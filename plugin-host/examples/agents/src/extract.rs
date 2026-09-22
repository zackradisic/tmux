//! Turn extraction: a harness's transcript file in, the conversation out.
//!
//! A transcript is a JSONL file the harness appends to as it works. Most
//! of its bytes are tool results (the files it read, the output it saw)
//! and bookkeeping records; the conversation - what you typed, what the
//! agent said, which tools it called - is under two per cent. An
//! extractor reads the file in chunks of complete lines and yields that
//! conversation as [`Turn`]s, each with the byte range of the record it
//! came from, so a preview can seek back to the original for the parts
//! that are not kept (a tool's output, the full diff of an edit).
//!
//! Every line is prefiltered before it is parsed: a cheap substring look
//! at the head of the record decides whether it can hold conversation at
//! all. On a Claude transcript that leaves about one per cent of the
//! bytes for the JSON parser. The scan and that prefilter are the cost,
//! not the parse, so they run in the host: `fs_read_lines` takes each
//! extractor's [`Extractor::needles`] and hands back only the lines that
//! pass (see `transcript`). The guest applies the exact rule again in
//! [`Extractor::candidate`] - the host filter is a superset - and the
//! same code path serves a plain buffer for the tests. A record of a
//! type the extractor does not know is skipped, not an error: harnesses
//! add record types between versions.
//!
//! Tool calls are condensed to one line each: the tool and what it
//! touched (a path, a command's description), plus for an edit the count
//! of lines in and out. Never the file content or the tool's result.
//!
//! One extractor per harness, behind [`Extractor`]. Claude Code and Codex
//! are here; a harness with no extractor is not indexed.

use std::borrow::Cow;

use memchr::memmem;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

/// One unit of conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub kind: TurnKind,
    /// The text: a prompt, an assistant message, or a condensed tool line.
    pub text: String,
    /// For a tool turn that touched a file: the path, so it is searchable
    /// as a path and not only as words.
    pub path: Option<String>,
    /// The record's own timestamp, epoch ms, when it carries one.
    pub ts_ms: Option<i64>,
    /// Where the record sits in the transcript: byte offset and length,
    /// newline included. A preview seeks here for what was not kept.
    pub offset: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    User,
    Assistant,
    Tool,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::User => "user",
            TurnKind::Assistant => "assistant",
            TurnKind::Tool => "tool",
        }
    }

    pub fn parse(s: &str) -> Option<TurnKind> {
        match s {
            "user" => Some(TurnKind::User),
            "assistant" => Some(TurnKind::Assistant),
            "tool" => Some(TurnKind::Tool),
            _ => None,
        }
    }
}

/// What a whole feed learned besides the turns.
#[derive(Debug, Default)]
pub struct Fed {
    pub turns: Vec<Turn>,
    /// The harness version the records name, when they do.
    pub version: Option<String>,
}

/// A harness's transcript reader. [`feed_line`](Self::feed_line) takes
/// one complete record (newline included) with its file offset; `feed`
/// takes a buffer of complete lines and splits it, for a caller that has
/// the bytes in hand.
pub trait Extractor {
    /// One record. Appends whatever conversation it holds to `fed`.
    fn feed_line(&mut self, line: &[u8], offset: u64, fed: &mut Fed);
    /// Can a record whose head looks like this hold conversation? The
    /// exact rule; the host's needles are a superset of it.
    fn candidate(&self, head: &[u8]) -> bool;
    /// The needles the host prefilters lines with (see `fs_read_lines`):
    /// a line is read back when any keep needle is in its head and no
    /// reject needle is. Must accept every line `candidate` would.
    fn needles(&self) -> Vec<(&'static [u8], bool)>;

    /// Complete lines in `buf`, the first at file offset `base`. What the
    /// tests use; the plugin gets its lines from the host.
    #[cfg_attr(not(test), allow(dead_code))]
    fn feed(&mut self, buf: &[u8], base: u64) -> Fed {
        let mut fed = Fed::default();
        let mut pos = 0usize;
        for nl in memchr::memchr_iter(b'\n', buf) {
            self.feed_line(&buf[pos..=nl], base + pos as u64, &mut fed);
            pos = nl + 1;
        }
        if pos < buf.len() {
            self.feed_line(&buf[pos..], base + pos as u64, &mut fed);
        }
        fed
    }
}

/// A record without its trailing newline.
fn strip_nl(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\n') => &line[..line.len() - 1],
        _ => line,
    }
}

/// The extractor for an agent kind, if it has one.
pub fn for_kind(kind: &str) -> Option<Box<dyn Extractor>> {
    match kind {
        "claude" => Some(Box::new(Claude)),
        "codex" => Some(Box::new(Codex)),
        _ => None,
    }
}

/// How many bytes of a record the prefilter looks at. The needles are
/// fields that sit near the start of every record shape seen so far
/// (`"role":…` inside `message`, which Claude writes before or after the
/// body depending on version - the top-level `"type"` tag can come a
/// kilobyte in, after the content, so it is NOT the needle). The window
/// is generous because a record may carry ids before its message.
pub const HEAD: usize = 1024;

fn head(line: &[u8]) -> &[u8] {
    &line[..line.len().min(HEAD)]
}

/// The prefilter's needles, each with its searcher built once.
struct Needles {
    user: memmem::Finder<'static>,
    assistant: memmem::Finder<'static>,
    tool_result: memmem::Finder<'static>,
    item: memmem::Finder<'static>,
    message: memmem::Finder<'static>,
    call: memmem::Finder<'static>,
    meta: memmem::Finder<'static>,
}

thread_local! {
    static NEEDLES: Needles = Needles {
        user: memmem::Finder::new(CL_USER),
        assistant: memmem::Finder::new(CL_ASSISTANT),
        tool_result: memmem::Finder::new(CL_TOOL_RESULT),
        item: memmem::Finder::new(CX_ITEM),
        message: memmem::Finder::new(CX_MESSAGE),
        call: memmem::Finder::new(CX_CALL),
        meta: memmem::Finder::new(CX_META),
    };
}

/// Parse an RFC 3339 timestamp (`2026-09-16T08:20:43.045Z`, or with an
/// offset) into epoch milliseconds. Hand-rolled: the harness formats are
/// regular and a date crate is not worth its size in the guest.
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
        // Up to three digits count; the rest are finer than ms.
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
    // Days since the epoch, proleptic Gregorian (Howard Hinnant's
    // days_from_civil).
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
/// readable at a glance. The full path is kept in [`Turn::path`].
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

/// Strip `<system-reminder>…</system-reminder>` spans and other injected
/// wrappers from a prompt: they are the harness talking to the agent, not
/// the user.
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

// ---------------------------------------------------------------------------
// Claude Code: ~/.claude/projects/<cwd slug>/<session id>.jsonl
// ---------------------------------------------------------------------------

pub struct Claude;

/// The message's role, which every user and assistant record carries
/// near its start whatever the field order of the version that wrote it.
const CL_USER: &[u8] = b"\"role\":\"user\"";
const CL_ASSISTANT: &[u8] = b"\"role\":\"assistant\"";
/// A user record whose content is a tool result: the block's type tag,
/// which follows the role within a few dozen bytes.
const CL_TOOL_RESULT: &[u8] = b"\"type\":\"tool_result\"";

impl Extractor for Claude {
    fn candidate(&self, head: &[u8]) -> bool {
        // A user record whose content is a tool result is the tool's
        // output coming back, not the user; those are the big lines.
        NEEDLES.with(|n| {
            (n.user.find(head).is_some() && n.tool_result.find(head).is_none())
                || n.assistant.find(head).is_some()
        })
    }

    fn needles(&self) -> Vec<(&'static [u8], bool)> {
        vec![(CL_USER, false), (CL_ASSISTANT, false), (CL_TOOL_RESULT, true)]
    }

    fn feed_line(&mut self, record: &[u8], off: u64, fed: &mut Fed) {
        let len = record.len() as u32;
        let line = strip_nl(record);
        if line.is_empty() || !self.candidate(head(line)) {
            return;
        }
        // A typed, borrowing parse: strings stay slices of the record
        // unless they hold escapes, and a tool's `input` is kept as raw
        // JSON and never built into a tree - it is the bulk of an
        // assistant record (a Write carries the whole file) and the
        // condenser needs a few fields and line counts from it.
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
                fed.turns.push(Turn { kind: TurnKind::User, text, path: None, ts_ms, offset: off, len });
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
                        kind: TurnKind::Assistant,
                        text: texts.join("\n").trim().to_string(),
                        path: None,
                        ts_ms,
                        offset: off,
                        len,
                    });
                }
                for (text, path) in tools {
                    fed.turns.push(Turn { kind: TurnKind::Tool, text, path, ts_ms, offset: off, len });
                }
            }
            _ => {}
        }
    }
}

/// A Claude transcript record, the fields the extractor reads. Every
/// string borrows from the record where it can (`Cow`); unknown fields
/// are ignored, so a new field never breaks the parse.
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

fn s_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
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
    let field = |o: &Option<Cow<'_, str>>| -> String {
        o.as_deref().unwrap_or("").to_string()
    };
    let line = match name {
        "Read" | "LS" | "NotebookRead" | "NotebookEdit" => format!("{name} {}", shown.unwrap_or_default()),
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

pub struct Codex;

const CX_ITEM: &[u8] = b"\"type\":\"response_item\"";
const CX_MESSAGE: &[u8] = b"\"type\":\"message\"";
const CX_CALL: &[u8] = b"\"type\":\"function_call\"";
const CX_META: &[u8] = b"\"type\":\"session_meta\"";

impl Extractor for Codex {
    fn candidate(&self, head: &[u8]) -> bool {
        NEEDLES.with(|n| {
            (n.item.find(head).is_some()
                && (n.message.find(head).is_some() || n.call.find(head).is_some()))
                || n.meta.find(head).is_some()
        })
    }

    fn needles(&self) -> Vec<(&'static [u8], bool)> {
        // A superset of `candidate`: the guest applies the exact rule.
        vec![(CX_MESSAGE, false), (CX_CALL, false), (CX_META, false)]
    }

    fn feed_line(&mut self, record: &[u8], off: u64, fed: &mut Fed) {
        let len = record.len() as u32;
        let line = strip_nl(record);
        if line.is_empty() || !self.candidate(head(line)) {
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
                    "user" => TurnKind::User,
                    "assistant" => TurnKind::Assistant,
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
                if text.is_empty() || (kind == TurnKind::User && is_tag_wrapper(&text)) {
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
                fed.turns.push(Turn { kind: TurnKind::Tool, text, path, ts_ms, offset: off, len });
            }
            _ => {}
        }
    }
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

/// One line for a Codex tool call. Shell commands show the command; a
/// patch shows its files and line counts; anything else its name and the
/// head of its arguments.
pub fn condense_codex(name: &str, args: &Value) -> (String, Option<String>) {
    match name {
        "shell" | "exec_command" | "local_shell" | "container.exec" | "shell_command" => {
            let cmd = match args.get("command").or_else(|| args.get("cmd")) {
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            (format!("shell {}", first_line(&cmd, 120)), None)
        }
        "apply_patch" => {
            let patch = s_field(args, "input")
                .or_else(|| s_field(args, "patch"))
                .unwrap_or("");
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

    /// Timing, not a test: `AGENTS_BENCH_FILE=<transcript> cargo test -p
    /// agents --lib bench_feed -- --ignored --nocapture`. Feeds only the
    /// records the host prefilter would hand over (the candidates), and
    /// reports the parse rate over those bytes - the guest's cost per
    /// byte received.
    #[test]
    #[ignore]
    fn bench_feed() {
        let Ok(path) = std::env::var("AGENTS_BENCH_FILE") else { return };
        let data = std::fs::read(&path).expect("read");
        let mut kept: Vec<u8> = Vec::new();
        for line in data.split_inclusive(|&b| b == b'\n') {
            if Claude.candidate(head(strip_nl(line))) {
                kept.extend_from_slice(line);
            }
        }
        let t = std::time::Instant::now();
        let mut n = 0;
        for _ in 0..5 {
            n = Claude.feed(&kept, 0).turns.len();
        }
        let per = t.elapsed() / 5;
        eprintln!(
            "bench_feed: {} MB file, {:.2} MB handed over, {n} turns, {:?} per pass = {:.0} MB/s over the handed bytes",
            data.len() / 1_000_000,
            kept.len() as f64 / 1e6,
            per,
            kept.len() as f64 / 1e6 / per.as_secs_f64()
        );
    }

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
        let fed = Claude.feed(&buf, 1000);
        assert_eq!(fed.version.as_deref(), Some("2.1.273"));
        let t = &fed.turns;
        assert_eq!(t.len(), 5, "{t:?}");
        assert_eq!(t[0].kind, TurnKind::User);
        assert_eq!(t[0].text, "hello there");
        assert_eq!(t[0].offset, 1000);
        assert_eq!(t[0].len as usize, l1.len() + 1);
        assert_eq!(t[0].ts_ms, Some(1789546843045));
        assert_eq!(t[1].kind, TurnKind::Assistant);
        assert_eq!(t[1].text, "I will read it.");
        assert_eq!(t[1].offset, 1000 + l1.len() as u64 + 1);
        assert_eq!(t[2].kind, TurnKind::Tool);
        assert_eq!(t[2].text, "Read b/c.rs");
        assert_eq!(t[2].path.as_deref(), Some("/a/b/c.rs"));
        assert_eq!(t[3].text, "Edit b/c.rs +1 −2");
        assert_eq!(t[4].kind, TurnKind::User);
        assert_eq!(t[4].text, "real prompt");
    }

    #[test]
    fn claude_prefilter_drops_tool_results() {
        let head = br#"{"parentUuid":"x","type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_1","type":"tool_result","content":"..."#;
        assert!(!Claude.candidate(head));
        assert!(Claude.candidate(br#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#));
        // The version that writes the body first: the top-level type tag
        // is far past the head, the role is not.
        assert!(Claude.candidate(br#"{"parentUuid":"x","isSidechain":false,"message":{"model":"m","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"..."#));
        assert!(!Claude.candidate(br#"{"type":"file-history-snapshot","snapshot":{}}"#));
    }

    #[test]
    fn claude_body_first_record() {
        // Claude 2.1.2xx writes `message` before `type` in assistant records.
        let l = br#"{"parentUuid":"x","isSidechain":false,"message":{"model":"m","id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"Body first."},{"type":"tool_use","name":"Read","input":{"file_path":"/a.rs"}}]},"type":"assistant","timestamp":"2026-09-08T00:00:00.000Z"}
"#;
        let fed = Claude.feed(l, 0);
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
        let fed = Codex.feed(&buf, 0);
        assert_eq!(fed.version.as_deref(), Some("0.147.0"));
        let texts: Vec<&str> = fed.turns.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["fix the build", "shell bash -lc cargo build", "apply_patch src/main.rs +2 −1", "Done."]
        );
        assert_eq!(fed.turns[2].path.as_deref(), Some("src/main.rs"));
    }
}
