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
//! bytes for the JSON parser, so the parser's speed does not matter; the
//! newline scan does. A record of a type the extractor does not know is
//! skipped, not an error: harnesses add record types between versions.
//!
//! Tool calls are condensed to one line each: the tool and what it
//! touched (a path, a command's description), plus for an edit the count
//! of lines in and out. Never the file content or the tool's result.
//!
//! One extractor per harness, behind [`Extractor`]. Claude Code and Codex
//! are here; a harness with no extractor is not indexed.

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

/// A harness's transcript reader. `feed` gets complete lines only (the
/// caller cuts at the last newline and carries the tail); `base` is the
/// file offset of the first byte of `buf`, so turns can say where they
/// came from.
pub trait Extractor {
    fn feed(&mut self, buf: &[u8], base: u64) -> Fed;
    /// Can a record whose head looks like this hold conversation? The
    /// caller uses it to drop an over-long line unparsed (a megabyte of
    /// tool output) without buffering the rest of it.
    fn candidate(&self, head: &[u8]) -> bool;
}

/// The extractor for an agent kind, if it has one.
pub fn for_kind(kind: &str) -> Option<Box<dyn Extractor>> {
    match kind {
        "claude" => Some(Box::new(Claude)),
        "codex" => Some(Box::new(Codex)),
        _ => None,
    }
}

/// How many bytes of a record the prefilter looks at. The type tags sit
/// in the first few hundred bytes of every record shape seen so far; the
/// window is generous because a record may carry ids before its type.
pub const HEAD: usize = 512;

fn head(line: &[u8]) -> &[u8] {
    &line[..line.len().min(HEAD)]
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Split `buf` into its lines, each with its offset from `base`. The
/// trailing newline is part of the record's length; a final line without
/// one is still yielded (the caller promised complete lines, but a file
/// may end without a newline).
fn lines(buf: &[u8], base: u64) -> impl Iterator<Item = (&[u8], u64, u32)> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= buf.len() {
            return None;
        }
        let rest = &buf[pos..];
        let (line, len) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) => (&rest[..i], i + 1),
            None => (rest, rest.len()),
        };
        let off = base + pos as u64;
        pos += len;
        Some((line, off, len as u32))
    })
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

/// Lines in a string, as a diff would count them: a trailing newline
/// does not add an empty line, and an empty string is zero.
fn line_count(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.lines().count()
    }
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

const CL_USER: &[u8] = b"\"type\":\"user\"";
const CL_ASSISTANT: &[u8] = b"\"type\":\"assistant\"";
const CL_TOOL_RESULT: &[u8] = b"\"tool_result\"";

impl Extractor for Claude {
    fn candidate(&self, head: &[u8]) -> bool {
        // A user record whose content is a tool result is the tool's
        // output coming back, not the user; those are the big lines.
        (contains(head, CL_USER) && !contains(head, CL_TOOL_RESULT)) || contains(head, CL_ASSISTANT)
    }

    fn feed(&mut self, buf: &[u8], base: u64) -> Fed {
        let mut fed = Fed::default();
        for (line, off, len) in lines(buf, base) {
            if line.is_empty() || !self.candidate(head(line)) {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
            if v.get("isMeta").and_then(Value::as_bool) == Some(true)
                || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            if fed.version.is_none() {
                fed.version = v.get("version").and_then(Value::as_str).map(str::to_string);
            }
            let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms);
            let Some(content) = v.pointer("/message/content") else { continue };
            match ty {
                "user" => {
                    let text = match content {
                        Value::String(s) => s.clone(),
                        Value::Array(blocks) => blocks
                            .iter()
                            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|b| b.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => continue,
                    };
                    let text = strip_injected(&text);
                    if text.is_empty() || is_command_noise(&text) {
                        continue;
                    }
                    fed.turns.push(Turn { kind: TurnKind::User, text, path: None, ts_ms, offset: off, len });
                }
                "assistant" => {
                    let Value::Array(blocks) = content else { continue };
                    let mut texts: Vec<&str> = Vec::new();
                    let mut tools: Vec<(String, Option<String>)> = Vec::new();
                    for b in blocks {
                        match b.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    if !t.trim().is_empty() {
                                        texts.push(t);
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                                let input = b.get("input").cloned().unwrap_or(Value::Null);
                                tools.push(condense_claude(name, &input));
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
        fed
    }
}

fn s_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// One line for a Claude tool call: the tool and what it touched. Never
/// the content it wrote or the output it got. Returns the line and the
/// file path it names, when it names one.
pub fn condense_claude(name: &str, input: &Value) -> (String, Option<String>) {
    let path = s_field(input, "file_path")
        .or_else(|| s_field(input, "notebook_path"))
        .or_else(|| s_field(input, "path"))
        .map(str::to_string);
    let shown = path.as_deref().map(short_path);
    let line = match name {
        "Read" | "LS" | "NotebookRead" => format!("{name} {}", shown.unwrap_or_default()),
        "Glob" => format!(
            "{name} {}{}",
            s_field(input, "pattern").unwrap_or(""),
            shown.map(|p| format!(" in {p}")).unwrap_or_default()
        ),
        "Grep" => format!(
            "{name} {}{}",
            s_field(input, "pattern").unwrap_or(""),
            shown.map(|p| format!(" in {p}")).unwrap_or_default()
        ),
        "Write" => format!(
            "{name} {} +{}",
            shown.unwrap_or_default(),
            line_count(s_field(input, "content").unwrap_or(""))
        ),
        "Edit" => format!(
            "{name} {} +{} −{}",
            shown.unwrap_or_default(),
            line_count(s_field(input, "new_string").unwrap_or("")),
            line_count(s_field(input, "old_string").unwrap_or(""))
        ),
        "MultiEdit" => {
            let (mut plus, mut minus) = (0, 0);
            if let Some(edits) = input.get("edits").and_then(Value::as_array) {
                for e in edits {
                    plus += line_count(s_field(e, "new_string").unwrap_or(""));
                    minus += line_count(s_field(e, "old_string").unwrap_or(""));
                }
            }
            format!("{name} {} +{plus} −{minus}", shown.unwrap_or_default())
        }
        "NotebookEdit" => format!("{name} {}", shown.unwrap_or_default()),
        "Bash" => {
            let what = s_field(input, "description")
                .map(|d| first_line(d, 120))
                .unwrap_or_else(|| first_line(s_field(input, "command").unwrap_or(""), 120));
            format!("{name} {what}")
        }
        "Agent" | "Task" => format!(
            "{name} {}: {}",
            first_line(s_field(input, "description").unwrap_or(""), 60),
            first_line(s_field(input, "prompt").unwrap_or(""), 120)
        ),
        "WebFetch" => format!("{name} {}", s_field(input, "url").unwrap_or("")),
        "WebSearch" => format!("{name} {}", first_line(s_field(input, "query").unwrap_or(""), 120)),
        _ => {
            // Unknown tool: its name and the head of its input, so a new
            // tool renders as a line rather than breaking the extractor.
            let compact = match input {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
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
        (contains(head, CX_ITEM) && (contains(head, CX_MESSAGE) || contains(head, CX_CALL)))
            || contains(head, CX_META)
    }

    fn feed(&mut self, buf: &[u8], base: u64) -> Fed {
        let mut fed = Fed::default();
        for (line, off, len) in lines(buf, base) {
            if line.is_empty() || !self.candidate(head(line)) {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            let ts_ms = v.get("timestamp").and_then(Value::as_str).and_then(parse_rfc3339_ms);
            let Some(payload) = v.get("payload") else { continue };
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
                        _ => continue,
                    };
                    let Some(blocks) = payload.get("content").and_then(Value::as_array) else { continue };
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
                        continue;
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
        fed
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
        assert!(Claude.candidate(br#"{"type":"assistant","message":{}}"#));
        assert!(!Claude.candidate(br#"{"type":"file-history-snapshot","snapshot":{}}"#));
    }

    #[test]
    fn condense_rules() {
        let v: Value = serde_json::from_str(r#"{"file_path":"/x/y.rs","content":"a\nb\nc\n"}"#).unwrap();
        assert_eq!(condense_claude("Write", &v).0, "Write x/y.rs +3");
        let v: Value = serde_json::from_str(r#"{"command":"ls -la","description":"List files"}"#).unwrap();
        assert_eq!(condense_claude("Bash", &v).0, "Bash List files");
        let v: Value = serde_json::from_str(r#"{"pattern":"fn main","path":"/src"}"#).unwrap();
        assert_eq!(condense_claude("Grep", &v).0, "Grep fn main in /src");
        let v: Value = serde_json::from_str(r#"{"file_path":"/x/y.rs","edits":[{"old_string":"a","new_string":"b\nc"},{"old_string":"","new_string":"d"}]}"#).unwrap();
        assert_eq!(condense_claude("MultiEdit", &v).0, "MultiEdit x/y.rs +3 −1");
        let v: Value = serde_json::from_str(r#"{"weird":"input"}"#).unwrap();
        assert_eq!(condense_claude("Brand New Tool", &v).0, r#"Brand New Tool {"weird":"input"}"#);
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
