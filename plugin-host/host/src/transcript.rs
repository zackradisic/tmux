//! `transcript_extract`: an agent harness's transcript in, its
//! conversation out - on the fs worker, so the plugin's side of it is
//! decoding a few KB of finished turns.
//!
//! A transcript is a JSONL file the harness appends to as it works. Most
//! of its bytes are tool results and bookkeeping; the conversation (what
//! the user typed, what the agent said, which tools it called) is a few
//! per cent. This module scans the file from an offset (the same line
//! scan as `fs_read_lines`, with the harness's needles), parses the
//! records that pass, and condenses each into [`Turn`]s: a prompt, a
//! reply, or one line per tool call - the tool and what it touched, with
//! an edit's line counts, never the file content or the tool's output.
//!
//! Why the host knows the formats at all: the parse is the one part of
//! the agents plugin's work that scales with transcript size, and on the
//! main thread it would cost the frame budget once several agents finish
//! turns together. `claude_notify` set the precedent for a harness-aware
//! import. The parse is typed and borrowing (strings stay slices of the
//! record unless they hold escapes; a tool's `input` stays raw JSON and
//! its line counts are read off the escapes), about 800 MB/s over the
//! records that pass the prefilter.
//!
//! The block a call returns (the completion's data; `v0` = the cursor to
//! resume from, `v1` = eof):
//!
//! ```text
//! u16 version_len | u8 version[version_len]          (the harness's, or 0)
//! per turn: u8 kind | i64 ts_ms | u64 offset | u32 len | u32 text_len |
//!           u8 text[text_len] | u16 path_len | u8 path[path_len]
//! kind:  0 user, 1 assistant, 2 tool        ts_ms: i64::MIN = none
//! ```
//!
//! `offset`/`len` locate the record in the transcript, so a preview can
//! seek back to it for what is not kept. Records are grouped by line: a
//! line's turns are all in the block or none, and the cursor never stops
//! inside one.

use std::borrow::Cow;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use crate::fsworker::Needle;

/// How many bytes of a record the prefilter looks at. The needles are
/// fields near the start of every record shape seen so far.
pub const HEAD: usize = 1024;
/// A candidate record longer than this is skipped unparsed, whatever
/// its head said: nothing conversational is that long.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
/// A call's block stops growing past this (soft: a line's turns are
/// never split, and the first line always fits).
pub const BLOCK_MAX: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub fn from_name(name: &str) -> Option<Harness> {
        match name {
            "claude" => Some(Harness::Claude),
            "codex" => Some(Harness::Codex),
            _ => None,
        }
    }

    /// The prefilter: a line is a candidate when a keep needle is in its
    /// head and no reject needle is. A superset of what parses; the
    /// parse decides.
    pub fn needles(self) -> Vec<Needle> {
        let n = |b: &[u8], reject: bool| Needle { bytes: b.to_vec(), reject };
        match self {
            // The message's role, which every user and assistant record
            // carries near its start whatever the field order of the
            // version that wrote it (the top-level `"type"` tag can come a
            // kilobyte in, after the body). A user record whose content is
            // a tool result is the tool's output coming back.
            Harness::Claude => vec![
                n(b"\"role\":\"user\"", false),
                n(b"\"role\":\"assistant\"", false),
                n(b"\"type\":\"tool_result\"", true),
            ],
            Harness::Codex => vec![
                n(b"\"type\":\"message\"", false),
                n(b"\"type\":\"function_call\"", false),
                n(b"\"type\":\"session_meta\"", false),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User = 0,
    Assistant = 1,
    Tool = 2,
}

/// One unit of conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub kind: Kind,
    pub text: String,
    pub path: Option<String>,
    pub ts_ms: Option<i64>,
    pub offset: u64,
    pub len: u32,
}

/// What a batch of records yielded besides turns.
#[derive(Debug, Default)]
pub struct Fed {
    pub turns: Vec<Turn>,
    pub version: Option<String>,
}

// ---------------------------------------------------------------------------
// the block
// ---------------------------------------------------------------------------

pub fn block_header(out: &mut Vec<u8>, version: Option<&str>) {
    let v = version.unwrap_or("").as_bytes();
    let v = &v[..v.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(v.len() as u16).to_le_bytes());
    out.extend_from_slice(v);
}

/// The encoded size of a turn, to decide whether it fits.
pub fn turn_size(t: &Turn) -> usize {
    1 + 8 + 8 + 4 + 4 + t.text.len() + 2 + t.path.as_ref().map_or(0, String::len)
}

pub fn put_turn(out: &mut Vec<u8>, t: &Turn) {
    out.push(t.kind as u8);
    out.extend_from_slice(&t.ts_ms.unwrap_or(i64::MIN).to_le_bytes());
    out.extend_from_slice(&t.offset.to_le_bytes());
    out.extend_from_slice(&t.len.to_le_bytes());
    let text = t.text.as_bytes();
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text);
    let path = t.path.as_deref().unwrap_or("").as_bytes();
    let path = &path[..path.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(path.len() as u16).to_le_bytes());
    out.extend_from_slice(path);
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

fn strip_nl(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\n') => &line[..line.len() - 1],
        _ => line,
    }
}

/// Parse an RFC 3339 timestamp (`2026-09-16T08:20:43.045Z`, or with an
/// offset) into epoch milliseconds. Hand-rolled: the harness formats are
/// regular and a date crate is not worth pulling in for it.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut i = 19;
    let mut millis = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let frac = &s[start..i];
        let take = frac.len().min(3);
        if take > 0 {
            millis = frac[..take].parse::<i64>().ok()? * 10i64.pow(3 - take as u32);
        }
    }
    let mut offset_min = 0i64;
    match b.get(i) {
        Some(b'Z') | None => {}
        Some(&sign @ (b'+' | b'-')) => {
            let oh = num(i + 1, i + 3)?;
            let om = if b.get(i + 3) == Some(&b':') { num(i + 4, i + 6)? } else { num(i + 3, i + 5)? };
            offset_min = oh * 60 + om;
            if sign == b'+' {
                offset_min = -offset_min;
            }
        }
        _ => return None,
    }
    // Days since the epoch, proleptic Gregorian (days_from_civil).
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * mo + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec + offset_min * 60;
    Some(secs * 1000 + millis)
}

/// The last path component or two, for a tool line that should stay
/// readable at a glance. The full path travels in [`Turn::path`].
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn first_line(s: &str, max: usize) -> String {
    let l = s.lines().next().unwrap_or("").trim();
    let mut out: String = l.chars().take(max).collect();
    if l.chars().count() > max {
        out.push('…');
    }
    out
}

/// Strip `<system-reminder>…</system-reminder>` spans from a prompt:
/// they are the harness talking to the agent, not the user.
fn strip_injected(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</system-reminder>") {
            Some(end) => rest = &rest[start + end + "</system-reminder>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// A prompt the user did not write: a slash command's transcript record,
/// a local command's output, the caveat wrapper.
fn is_command_noise(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<local-command-")
        || t.starts_with("<command-name>")
        || t.starts_with("<command-message>")
        || t.starts_with("<bash-input>")
        || t.starts_with("<bash-stdout>")
        || t.starts_with("<bash-stderr>")
        || t.starts_with("<user-prompt-submit-hook>")
}

/// A user message that is one XML-ish wrapper from the app, not a prompt:
/// starts with `<tag>` and ends with the matching `</tag>`.
fn is_tag_wrapper(text: &str) -> bool {
    let t = text.trim();
    let Some(rest) = t.strip_prefix('<') else { return false };
    let Some(end) = rest.find('>') else { return false };
    let tag = &rest[..end];
    if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return false;
    }
    t.ends_with(&format!("</{tag}>"))
}

/// One record of a transcript: its turns, appended to `fed`. `record`
/// includes the trailing newline when there is one.
pub fn extract_line(h: Harness, record: &[u8], off: u64, fed: &mut Fed) {
    match h {
        Harness::Claude => claude_line(record, off, fed),
        Harness::Codex => codex_line(record, off, fed),
    }
}

/// Complete lines in `buf`, the first at file offset `base`.
pub fn extract_buf(h: Harness, buf: &[u8], base: u64) -> Fed {
    let mut fed = Fed::default();
    let mut pos = 0usize;
    for nl in memchr::memchr_iter(b'\n', buf) {
        extract_line(h, &buf[pos..=nl], base + pos as u64, &mut fed);
        pos = nl + 1;
    }
    if pos < buf.len() {
        extract_line(h, &buf[pos..], base + pos as u64, &mut fed);
    }
    fed
}

// ---------------------------------------------------------------------------
// Claude Code: ~/.claude/projects/<cwd slug>/<session id>.jsonl
// ---------------------------------------------------------------------------

/// A Claude transcript record, the fields the extractor reads. Every
/// string borrows from the record where it can; unknown fields are
/// ignored, so a new field never breaks the parse.
#[derive(Deserialize)]
struct ClRecord<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta", default)]
    is_meta: bool,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    version: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    message: Option<ClMessage<'a>>,
}

#[derive(Deserialize)]
struct ClMessage<'a> {
    /// A string (a typed prompt) or an array of blocks. Kept raw here
    /// and split by its first byte: an untagged enum would buffer the
    /// value and lose the borrow the blocks' `input` needs.
    #[serde(default, borrow)]
    content: Option<&'a RawValue>,
}

enum ClContent<'a> {
    Text(Cow<'a, str>),
    Blocks(Vec<ClBlock<'a>>),
    Other,
}

impl<'a> ClContent<'a> {
    fn parse(raw: &'a RawValue) -> ClContent<'a> {
        let s = raw.get();
        match s.as_bytes().first() {
            Some(b'"') => serde_json::from_str::<Cow<'a, str>>(s)
                .map(ClContent::Text)
                .unwrap_or(ClContent::Other),
            Some(b'[') => serde_json::from_str::<Vec<ClBlock<'a>>>(s)
                .map(ClContent::Blocks)
                .unwrap_or(ClContent::Other),
            _ => ClContent::Other,
        }
    }
}

#[derive(Deserialize)]
struct ClBlock<'a> {
    #[serde(rename = "type", default, borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    /// Raw JSON, not a tree: see [`condense_claude`].
    #[serde(default, borrow)]
    input: Option<&'a RawValue>,
}

/// The fields of a tool's input the condenser reads. The big ones (a
/// file's content, an edit's old and new text) stay raw: only their line
/// counts are wanted, and those come from the escaped JSON without
/// unescaping it (see [`raw_str_lines`]).
#[derive(Deserialize, Default)]
struct ClInput<'a> {
    #[serde(default, borrow)]
    file_path: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    notebook_path: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    path: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    pattern: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    description: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    command: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    prompt: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    url: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    query: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    content: Option<&'a RawValue>,
    #[serde(default, borrow)]
    old_string: Option<&'a RawValue>,
    #[serde(default, borrow)]
    new_string: Option<&'a RawValue>,
    #[serde(default, borrow)]
    edits: Option<Vec<ClEdit<'a>>>,
}

#[derive(Deserialize)]
struct ClEdit<'a> {
    #[serde(default, borrow)]
    old_string: Option<&'a RawValue>,
    #[serde(default, borrow)]
    new_string: Option<&'a RawValue>,
}

/// The line count of a JSON string literal, from its escaped form: the
/// `\n` escapes, counted without unescaping (a backslash escaped as `\\`
/// does not start one). Same answer as `str::lines().count()` on the
/// decoded string: a trailing newline adds no line.
fn raw_str_lines(raw: Option<&RawValue>) -> usize {
    let Some(r) = raw else { return 0 };
    let s = r.get().as_bytes();
    if s.len() < 2 || s[0] != b'"' {
        return 0;
    }
    let body = &s[1..s.len() - 1];
    if body.is_empty() {
        return 0;
    }
    let mut newlines = 0usize;
    let mut i = 0;
    let mut ends_with_nl = false;
    while i < body.len() {
        if body[i] == b'\\' {
            if i + 1 < body.len() && body[i + 1] == b'n' {
                newlines += 1;
                ends_with_nl = i + 2 == body.len();
            } else {
                ends_with_nl = false;
            }
            i += 2;
        } else {
            ends_with_nl = false;
            i += 1;
        }
    }
    if ends_with_nl { newlines } else { newlines + 1 }
}

fn claude_line(record: &[u8], off: u64, fed: &mut Fed) {
    let len = record.len() as u32;
    let line = strip_nl(record);
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_slice::<ClRecord<'_>>(line) else { return };
    if v.is_meta || v.is_sidechain {
        return;
    }
    if fed.version.is_none() {
        fed.version = v.version.map(|s| s.into_owned());
    }
    let ts_ms = v.timestamp.as_deref().and_then(parse_rfc3339_ms);
    let Some(content) = v.message.and_then(|m| m.content).map(ClContent::parse) else {
        return;
    };
    match v.ty.as_deref() {
        Some("user") => {
            let text = match content {
                ClContent::Text(s) => s.into_owned(),
                ClContent::Blocks(blocks) => blocks
                    .iter()
                    .filter(|b| b.ty.as_deref() == Some("text"))
                    .filter_map(|b| b.text.as_deref())
                    .collect::<Vec<_>>()
                    .join("\n"),
                ClContent::Other => return,
            };
            let text = strip_injected(&text);
            if text.is_empty() || is_command_noise(&text) {
                return;
            }
            fed.turns.push(Turn { kind: Kind::User, text, path: None, ts_ms, offset: off, len });
        }
        Some("assistant") => {
            let ClContent::Blocks(blocks) = content else { return };
            let mut texts: Vec<&str> = Vec::new();
            let mut tools: Vec<(String, Option<String>)> = Vec::new();
            for b in &blocks {
                match b.ty.as_deref() {
                    Some("text") => {
                        if let Some(t) = b.text.as_deref() {
                            if !t.trim().is_empty() {
                                texts.push(t);
                            }
                        }
                    }
                    Some("tool_use") => {
                        let name = b.name.as_deref().unwrap_or("tool");
                        let input = b.input.map(|r| r.get()).unwrap_or("null");
                        tools.push(condense_claude(name, input));
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() {
                fed.turns.push(Turn {
                    kind: Kind::Assistant,
                    text: texts.join("\n").trim().to_string(),
                    path: None,
                    ts_ms,
                    offset: off,
                    len,
                });
            }
            for (text, path) in tools {
                fed.turns.push(Turn { kind: Kind::Tool, text, path, ts_ms, offset: off, len });
            }
        }
        _ => {}
    }
}

/// One line for a Claude tool call: the tool and what it touched. Never
/// the content it wrote or the output it got. `input` is the tool's
/// input as raw JSON. Returns the line and the file path it names, when
/// it names one.
pub fn condense_claude(name: &str, input: &str) -> (String, Option<String>) {
    let inp: ClInput<'_> = serde_json::from_str(input).unwrap_or_default();
    let path = inp
        .file_path
        .as_deref()
        .or(inp.notebook_path.as_deref())
        .or(inp.path.as_deref())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let shown = path.as_deref().map(short_path);
    let field = |o: &Option<Cow<'_, str>>| -> String { o.as_deref().unwrap_or("").to_string() };
    let line = match name {
        "Read" | "LS" | "NotebookRead" | "NotebookEdit" => {
            format!("{name} {}", shown.unwrap_or_default())
        }
        "Glob" | "Grep" => format!(
            "{name} {}{}",
            field(&inp.pattern),
            shown.map(|p| format!(" in {p}")).unwrap_or_default()
        ),
        "Write" => format!("{name} {} +{}", shown.unwrap_or_default(), raw_str_lines(inp.content)),
        "Edit" => format!(
            "{name} {} +{} −{}",
            shown.unwrap_or_default(),
            raw_str_lines(inp.new_string),
            raw_str_lines(inp.old_string)
        ),
        "MultiEdit" => {
            let (mut plus, mut minus) = (0, 0);
            for e in inp.edits.as_deref().unwrap_or(&[]) {
                plus += raw_str_lines(e.new_string);
                minus += raw_str_lines(e.old_string);
            }
            format!("{name} {} +{plus} −{minus}", shown.unwrap_or_default())
        }
        "Bash" => {
            let what = inp
                .description
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| first_line(d, 120))
                .unwrap_or_else(|| first_line(&field(&inp.command), 120));
            format!("{name} {what}")
        }
        "Agent" | "Task" => format!(
            "{name} {}: {}",
            first_line(&field(&inp.description), 60),
            first_line(&field(&inp.prompt), 120)
        ),
        "WebFetch" => format!("{name} {}", field(&inp.url)),
        "WebSearch" => format!("{name} {}", first_line(&field(&inp.query), 120)),
        _ => {
            // Unknown tool: its name and the head of its input, so a new
            // tool renders as a line rather than breaking the extractor.
            let compact = if input == "null" { "" } else { input };
            let mut head: String = compact.chars().take(120).collect();
            if compact.chars().count() > 120 {
                head.push('…');
            }
            format!("{name} {head}").trim_end().to_string()
        }
    };
    (line, path)
}

// ---------------------------------------------------------------------------
// Codex: the rollout jsonl (~/.codex/sessions/.../rollout-*.jsonl)
// ---------------------------------------------------------------------------

fn s_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn codex_line(record: &[u8], off: u64, fed: &mut Fed) {
    let len = record.len() as u32;
    let line = strip_nl(record);
    if line.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
    let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms);
    let Some(payload) = v.get("payload") else { return };
    match (v.get("type").and_then(Value::as_str), payload.get("type").and_then(Value::as_str)) {
        (Some("session_meta"), _) => {
            if fed.version.is_none() {
                fed.version = s_field(payload, "cli_version").map(str::to_string);
            }
        }
        (Some("response_item"), Some("message")) => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
            let kind = match role {
                "user" => Kind::User,
                "assistant" => Kind::Assistant,
                // developer / system: the harness's own instructions.
                _ => return,
            };
            let Some(blocks) = payload.get("content").and_then(Value::as_array) else { return };
            let text = blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.get("type").and_then(Value::as_str),
                        Some("input_text") | Some("output_text") | Some("text")
                    )
                })
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim().to_string();
            // A prompt the app injected: context wrappers such as
            // <app-context>, <environment_context>, <recommended_plugins>.
            if text.is_empty() || (kind == Kind::User && is_tag_wrapper(&text)) {
                return;
            }
            fed.turns.push(Turn { kind, text, path: None, ts_ms, offset: off, len });
        }
        (Some("response_item"), Some("function_call")) => {
            let name = s_field(payload, "name").unwrap_or("tool");
            let args = s_field(payload, "arguments")
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .unwrap_or(Value::Null);
            let (text, path) = condense_codex(name, &args);
            fed.turns.push(Turn { kind: Kind::Tool, text, path, ts_ms, offset: off, len });
        }
        _ => {}
    }
}

/// One line for a Codex tool call. Shell commands show the command; a
/// patch shows its files and line counts; anything else its name and the
/// head of its arguments.
pub fn condense_codex(name: &str, args: &Value) -> (String, Option<String>) {
    match name {
        "shell" | "exec_command" | "local_shell" | "container.exec" | "shell_command" => {
            let cmd = match args.get("command").or_else(|| args.get("cmd")) {
                Some(Value::Array(parts)) => {
                    parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
                }
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            (format!("shell {}", first_line(&cmd, 120)), None)
        }
        "apply_patch" => {
            let patch = s_field(args, "input").or_else(|| s_field(args, "patch")).unwrap_or("");
            let mut files: Vec<String> = Vec::new();
            let (mut plus, mut minus) = (0usize, 0usize);
            for l in patch.lines() {
                if let Some(p) = l
                    .strip_prefix("*** Update File: ")
                    .or_else(|| l.strip_prefix("*** Add File: "))
                    .or_else(|| l.strip_prefix("*** Delete File: "))
                {
                    files.push(p.trim().to_string());
                } else if l.starts_with('+') && !l.starts_with("+++") {
                    plus += 1;
                } else if l.starts_with('-') && !l.starts_with("---") {
                    minus += 1;
                }
            }
            let shown = files.iter().map(|f| short_path(f)).collect::<Vec<_>>().join(", ");
            (format!("apply_patch {shown} +{plus} −{minus}"), files.into_iter().next())
        }
        _ => {
            let compact = match args {
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let mut head: String = compact.chars().take(120).collect();
            if compact.chars().count() > 120 {
                head.push('…');
            }
            (format!("{name} {head}").trim_end().to_string(), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_rfc3339_ms("2026-09-16T08:20:43.045Z"), Some(1789546843045));
        assert_eq!(parse_rfc3339_ms("2026-09-16T10:20:43+02:00"), Some(1789546843000));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }

    #[test]
    fn claude_turns_and_offsets() {
        let l1 = br#"{"type":"user","message":{"role":"user","content":"hello there"},"timestamp":"2026-09-16T08:20:43.045Z","version":"2.1.273"}"#;
        let l2 = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"I will read it."},{"type":"tool_use","name":"Read","input":{"file_path":"/a/b/c.rs"}},{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/c.rs","old_string":"x\ny","new_string":"z"}}]}}"#;
        let l3 = br#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"huge output"}]}}"#;
        let l4 = br#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>x</local-command-caveat>"}}"#;
        let l5 = br#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<system-reminder>ignore</system-reminder>real prompt"}]}}"#;
        let mut buf = Vec::new();
        for l in [&l1[..], &l2[..], &l3[..], &l4[..], &l5[..]] {
            buf.extend_from_slice(l);
            buf.push(b'\n');
        }
        let fed = extract_buf(Harness::Claude, &buf, 1000);
        assert_eq!(fed.version.as_deref(), Some("2.1.273"));
        let t = &fed.turns;
        assert_eq!(t.len(), 5, "{t:?}");
        assert_eq!(t[0].kind, Kind::User);
        assert_eq!(t[0].text, "hello there");
        assert_eq!(t[0].offset, 1000);
        assert_eq!(t[0].len as usize, l1.len() + 1);
        assert_eq!(t[0].ts_ms, Some(1789546843045));
        assert_eq!(t[1].kind, Kind::Assistant);
        assert_eq!(t[1].text, "I will read it.");
        assert_eq!(t[1].offset, 1000 + l1.len() as u64 + 1);
        assert_eq!(t[2].kind, Kind::Tool);
        assert_eq!(t[2].text, "Read b/c.rs");
        assert_eq!(t[2].path.as_deref(), Some("/a/b/c.rs"));
        assert_eq!(t[3].text, "Edit b/c.rs +1 −2");
        assert_eq!(t[4].kind, Kind::User);
        assert_eq!(t[4].text, "real prompt");
    }

    #[test]
    fn claude_body_first_record() {
        // Claude 2.1.2xx writes `message` before `type` in assistant records.
        let l = br#"{"parentUuid":"x","isSidechain":false,"message":{"model":"m","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Body first."},{"type":"tool_use","name":"Read","input":{"file_path":"/a.rs"}}]},"type":"assistant","timestamp":"2026-09-08T00:00:00.000Z"}
"#;
        let fed = extract_buf(Harness::Claude, l, 0);
        let texts: Vec<&str> = fed.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["Body first.", "Read /a.rs"]);
    }

    #[test]
    fn condense_rules() {
        assert_eq!(condense_claude("Write", r#"{"file_path":"/x/y.rs","content":"a\nb\nc\n"}"#).0, "Write x/y.rs +3");
        assert_eq!(condense_claude("Bash", r#"{"command":"ls -la","description":"List files"}"#).0, "Bash List files");
        assert_eq!(condense_claude("Grep", r#"{"pattern":"fn main","path":"/src"}"#).0, "Grep fn main in /src");
        assert_eq!(
            condense_claude("MultiEdit", r#"{"file_path":"/x/y.rs","edits":[{"old_string":"a","new_string":"b\nc"},{"old_string":"","new_string":"d"}]}"#).0,
            "MultiEdit x/y.rs +3 −1"
        );
        assert_eq!(condense_claude("Brand New Tool", r#"{"weird":"input"}"#).0, r#"Brand New Tool {"weird":"input"}"#);
        // Line counts come from the escaped JSON: an escaped backslash
        // before an n is not a newline, and a trailing newline adds none.
        assert_eq!(condense_claude("Write", r#"{"file_path":"/f","content":"x\\ny"}"#).0, "Write /f +1");
        assert_eq!(condense_claude("Write", r#"{"file_path":"/f","content":""}"#).0, "Write /f +0");
        assert_eq!(condense_claude("Edit", r#"{"file_path":"/f","old_string":"a\nb","new_string":"q\"r\ns\nt"}"#).0, "Edit /f +3 −2");
    }

    #[test]
    fn codex_turns() {
        let l1 = br#"{"timestamp":"2026-08-19T00:11:03.716Z","type":"session_meta","payload":{"session_id":"abc","cli_version":"0.147.0"}}"#;
        let l2 = br#"{"timestamp":"2026-08-19T00:11:03.896Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<app-context>x</app-context>"}]}}"#;
        let l3 = br#"{"timestamp":"2026-08-19T00:11:03.896Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\ncwd\n</environment_context>"}]}}"#;
        let l4 = br#"{"timestamp":"2026-08-19T00:11:04.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix the build"}]}}"#;
        let l5 = br#"{"timestamp":"2026-08-19T00:11:05.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"cargo build\"]}"}}"#;
        let l6 = br#"{"timestamp":"2026-08-19T00:11:06.000Z","type":"response_item","payload":{"type":"function_call","name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** Update File: src/main.rs\\n@@\\n-old\\n+new\\n+more\\n*** End Patch\"}"}}"#;
        let l7 = br#"{"timestamp":"2026-08-19T00:11:07.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}}"#;
        let mut buf = Vec::new();
        for l in [&l1[..], &l2[..], &l3[..], &l4[..], &l5[..], &l6[..], &l7[..]] {
            buf.extend_from_slice(l);
            buf.push(b'\n');
        }
        let fed = extract_buf(Harness::Codex, &buf, 0);
        assert_eq!(fed.version.as_deref(), Some("0.147.0"));
        let texts: Vec<&str> = fed.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["fix the build", "shell bash -lc cargo build", "apply_patch src/main.rs +2 −1", "Done."]
        );
        assert_eq!(fed.turns[2].path.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn block_roundtrip_shape() {
        let t = Turn {
            kind: Kind::Tool,
            text: "Read x".into(),
            path: Some("/x".into()),
            ts_ms: None,
            offset: 7,
            len: 9,
        };
        let mut out = Vec::new();
        block_header(&mut out, Some("2.1.0"));
        put_turn(&mut out, &t);
        assert_eq!(out.len(), 2 + 5 + turn_size(&t));
        assert_eq!(&out[..2], &5u16.to_le_bytes());
        assert_eq!(out[7], 2); // kind
        assert_eq!(&out[8..16], &i64::MIN.to_le_bytes());
    }
}
