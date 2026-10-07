//! Per-harness resolvers: read the agent's OWN session file to learn its
//! durable id, display name, turn status, and real timestamps. Membership
//! and liveness never come from here - only enrichment. Every resolver is
//! best-effort: when the source is absent it returns `None` and the row
//! keeps its provisional identity, its `#{pane_title}` name, and its
//! observed times.
//!
//! Sources, one per harness:
//!   * claude   - `~/.claude/sessions/<pid>.json`, matched by its `tmux`
//!                field (`session:@win.%pane`). Carries id, name, status,
//!                and epoch-ms `startedAt` / `updatedAt`. The richest.
//!   * codex    - the rollout `*.jsonl` the TUI holds open, found through
//!                `pane_fds`; its first line names the session_id, its
//!                file mtime dates the last turn.
//!   * pi /     - no external mapping exists, so a one-shot `identify`
//!     opencode   hook reports the id and file once (see the plugin's
//!                `identify` verb); the file mtime dates activity.


use tmux_plugin_sdk::prelude::*;

use crate::store::Agent;

/// What a resolver learned. Every field is optional; `enrich` folds only
/// the ones that are present onto the row.
#[derive(Default, Debug)]
pub struct Resolved {
    /// The durable harness id, when known. A value different from the
    /// row's current id triggers a provisional->real migration.
    pub real_id: Option<String>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub started_ms: Option<i64>,
    pub last_active_ms: Option<i64>,
    pub source_path: Option<String>,
    /// The harness's transcript file - the conversation itself, which the
    /// extractor reads (see `transcript`). Claude's is derived from the
    /// session's `cwd`; Codex's is the rollout, the same file as
    /// `source_path`.
    pub transcript_path: Option<String>,
    /// The working directory the session file names.
    pub cwd: Option<String>,
}

fn home() -> Option<String> {
    home_dir().ok().filter(|s| !s.is_empty())
}

/// The mtime of one file, in epoch ms, by listing its parent directory.
/// The listing carries seconds, so scale to ms for the store.
async fn file_mtime_ms(path: &str) -> Option<i64> {
    let (dir, name) = path.rsplit_once('/')?;
    let opts = ListOpts { mtime: true, dirs_only: false };
    let listing = fs_list_with(dir, opts).await.ok()?;
    for e in listing.iter() {
        if e.name == name {
            return Some(e.mtime * 1000);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// claude: ~/.claude/sessions/<pid>.json, indexed by pane
// ---------------------------------------------------------------------------

/// Resolve a Claude agent from its own session file. The file is named by
/// Claude's pid, so with `pane_pid` we read exactly one file directly -
/// no directory scan. When the pid does not name the right file (Claude
/// launched under a wrapper, or a stale pid after a restart), fall back to
/// scanning the directory and matching the `tmux` field.
pub async fn claude(a: &Agent) -> Option<Resolved> {
    claude_for_pane_in(a.pane? as u32, a.session.as_deref()).await
}

/// Does a Claude session file name this pane? That file is the proof a
/// Claude runs there: the command name is a version string on macOS and
/// `AI_AGENT` is inherited by every pane of a server started inside
/// Claude Code, but only Claude itself writes its pane into its file.
pub async fn claude_claims_pane(pane: u32) -> bool {
    claude_for_pane_in(pane, None).await.is_some()
}

/// `session` is the row's session name, for the one case the window id
/// cannot be checked: the pane is already gone (its agent is ending, and
/// this is the last chance to learn its id).
async fn claude_for_pane_in(pane: u32, session: Option<&str>) -> Option<Resolved> {
    let home = home()?;
    let dir = format!("{home}/.claude/sessions");
    // A session file outlives the server that made it: after a restart
    // the new server hands out the same pane ids again, so a stale file
    // can name a live pane by number alone. The window id must match
    // too. The session name is left out: a rename must not lose a row.
    // A pane that is gone has no window to check; then the file's
    // session name must be the row's, which a rename would have
    // followed too.
    let alive = resolve_pane(PaneId(pane)).ok();
    let window = alive.as_ref().map(|p| p.window);
    let claims = |p: u32, w: Option<u32>, s: &str| {
        p == pane
            && if alive.is_some() {
                w.is_none() || w == window
            } else {
                session.is_some_and(|name| name == s)
            }
    };

    // Direct hit: <pid>.json, verified by its tmux field.
    if let Ok(Some(pid)) = pane_pid(PaneId(pane)) {
        let path = format!("{dir}/{pid}.json");
        if let Ok((bytes, _)) = fs_read(&path, 0, 16 * 1024).await {
            if let Some((p, w, s, r)) = parse_claude(&path, &bytes, &home) {
                if claims(p, w, &s) {
                    return Some(r);
                }
            }
        }
    }

    // Fallback: scan the directory for the file whose tmux field names
    // this pane.
    let listing = fs_list(&dir).await.ok()?;
    let names: Vec<String> = listing
        .iter()
        .filter(|e| e.name.ends_with(".json"))
        .map(|e| e.name.to_string())
        .collect();
    for name in names {
        let path = format!("{dir}/{name}");
        let Ok((bytes, _)) = fs_read(&path, 0, 16 * 1024).await else {
            continue;
        };
        if let Some((p, w, s, r)) = parse_claude(&path, &bytes, &home) {
            if claims(p, w, &s) {
                return Some(r);
            }
        }
    }
    None
}

/// Parse one Claude session file into (pane, window, session name,
/// resolved). Returns None when the JSON is malformed or lacks a usable
/// `tmux` field; the window is None when the field carries no `@N` part.
fn parse_claude(
    path: &str,
    bytes: &[u8],
    home: &str,
) -> Option<(u32, Option<u32>, String, Resolved)> {
    let v = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    // tmux is "session:@window.%pane".
    let field = v.get("tmux").and_then(|x| x.as_str())?;
    let (rest, pane_part) = field.rsplit_once('.').unwrap_or(("", field));
    let pane = pane_part.strip_prefix('%').and_then(|n| n.parse::<u32>().ok())?;
    let window = rest
        .rsplit(':')
        .next()
        .and_then(|w| w.strip_prefix('@'))
        .and_then(|n| n.parse::<u32>().ok());
    let session_name = rest.rsplit_once(':').map(|(s, _)| s).unwrap_or("").to_string();
    let sid = v.get("sessionId").and_then(|x| x.as_str());
    // The transcript sits under the project directory named for the cwd.
    let cwd = v.get("cwd").and_then(|x| x.as_str()).filter(|c| !c.is_empty()).map(str::to_string);
    let transcript_path = match (sid, cwd.as_deref()) {
        (Some(sid), Some(cwd)) => Some(crate::transcript::claude_transcript_path(home, cwd, sid)),
        _ => None,
    };
    let status = match v.get("status").and_then(|x| x.as_str()) {
        Some("busy") => Some("working".to_string()),
        Some("idle") => Some("waiting".to_string()),
        _ => None,
    };
    Some((
        pane,
        window,
        session_name,
        Resolved {
            real_id: sid.map(|s| format!("claude:{s}")),
            name: v.get("name").and_then(|x| x.as_str()).map(str::to_string),
            status,
            started_ms: v.get("startedAt").and_then(|x| x.as_i64()),
            last_active_ms: v.get("updatedAt").and_then(|x| x.as_i64()),
            source_path: Some(path.to_string()),
            transcript_path,
            cwd,
        },
    ))
}

// ---------------------------------------------------------------------------
// codex: the rollout jsonl under ~/.codex/sessions
// ---------------------------------------------------------------------------

/// Resolve a Codex agent from its rollout. Three ways to find the file,
/// tried in order:
///
///   1. the path an `identify` hook recorded (`source_path`);
///   2. a `rollout-*.jsonl` among the pane's open fds - Codex up to the
///      0.15x line held the file open for its whole life;
///   3. a scan of `~/.codex/sessions/<y>/<m>/<d>/` for a rollout whose
///      `session_meta` names the pane's working directory and whose
///      start is nearest the row's. Codex 0.160 appends to the rollout
///      and closes it between writes, so nothing holds it open, and
///      without a hook the scan is the only way to its id. The directory
///      tree is dated, so only the days since the row appeared are read.
pub async fn codex(a: &Agent) -> Option<Resolved> {
    let path = match a.source_path.as_deref() {
        Some(p) if p.ends_with(".jsonl") => p.to_string(),
        _ => match codex_open_rollout(a) {
            Some(p) => p,
            None => return codex_scan(a).await,
        },
    };
    let (bytes, _) = fs_read(&path, 0, META_BYTES).await.ok()?;
    let meta = codex_meta(&bytes);
    Some(codex_resolved(path, meta).await)
}

/// A rollout the pane's foreground process holds open (the pre-0.160
/// layout).
fn codex_open_rollout(a: &Agent) -> Option<String> {
    let pane = a.pane? as u32;
    let fds = pane_fds(PaneId(pane)).ok().flatten()?;
    fds.into_iter().find(|p| is_rollout(p.rsplit('/').next().unwrap_or(p)))
}

fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

/// How much of a rollout holds its first line. The `session_meta` record
/// carries the base instructions, so it runs to several KB.
const META_BYTES: usize = 24 * 1024;

/// A rollout not written since this long before the row appeared is not
/// the row's. The file is created when the session starts, so a fresh
/// Codex needs only seconds of slack; a week covers a Codex that was
/// running before its row was (the plugin reloaded, the row swept and
/// made again) and has sat idle since. Dead sessions in the same
/// directory from the same days are candidates too, which is what the
/// nearest-start rule below is for.
const SCAN_SLACK_S: i64 = 7 * 86_400;

/// Candidate files read per scan, newest first. A bound on the cost of a
/// busy week in a shared directory, not a limit anyone should reach.
const SCAN_MAX_FILES: usize = 200;

/// A scan that found nothing is not repeated for this long: a Codex
/// with no rollout yet (nothing typed) is rendered many times before
/// the file appears.
const SCAN_RETRY_MS: i64 = 10_000;

thread_local! {
    /// Per pane: when the last empty scan ran.
    static SCAN_MISSED: std::cell::RefCell<std::collections::HashMap<u32, i64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// What the first line of a rollout says.
#[derive(Debug, Default, PartialEq)]
pub struct CodexMeta {
    pub session_id: Option<String>,
    pub nickname: Option<String>,
    pub cwd: Option<String>,
    /// The session's start, epoch ms, from the record's own timestamp.
    pub started_ms: Option<i64>,
    /// Spawned by another thread (a subagent). Never a pane's agent.
    pub subagent: bool,
}

/// Parse the `session_meta` record that heads a rollout.
pub fn codex_meta(bytes: &[u8]) -> CodexMeta {
    let text = String::from_utf8_lossy(bytes);
    let Some(line) = text.lines().next() else { return CodexMeta::default() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return CodexMeta::default();
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return CodexMeta::default();
    }
    let p = v.get("payload");
    let field = |k: &str| {
        p.and_then(|p| p.get(k))
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // The paginated layout (0.160+) names the thread in `id` and keeps
    // `session_id` too; a subagent's `session_id` is its PARENT's, so
    // `id` is the one to trust when both are present.
    let session_id = field("id").or_else(|| field("session_id"));
    let started_ms = field("timestamp")
        .or_else(|| v.get("timestamp").and_then(|t| t.as_str()).map(str::to_string))
        .and_then(|t| parse_rfc3339_ms(&t));
    let subagent = p.and_then(|p| p.get("parent_thread_id")).is_some_and(|x| !x.is_null())
        || p.and_then(|p| p.get("source")).is_some_and(|s| s.is_object());
    CodexMeta {
        session_id,
        nickname: field("agent_nickname"),
        cwd: field("cwd"),
        started_ms,
        subagent,
    }
}

/// Fold a rollout's meta, turn state and mtime into a resolver result.
async fn codex_resolved(path: String, meta: CodexMeta) -> Resolved {
    let status = match codex_turn(&path).await {
        Turn::Working => Some("working".to_string()),
        Turn::Waiting => Some("waiting".to_string()),
        Turn::NeedsInput => Some("needs_input".to_string()),
        Turn::Unknown => None,
    };
    Resolved {
        real_id: meta.session_id.map(|sid| format!("codex:{sid}")),
        name: meta.nickname,
        status,
        started_ms: meta.started_ms,
        last_active_ms: file_mtime_ms(&path).await,
        source_path: Some(path.clone()),
        transcript_path: Some(path),
        cwd: meta.cwd,
        ..Default::default()
    }
}

// --- turn state -----------------------------------------------------------

/// Where a Codex thread is in its turn, as its rollout tells it. Codex
/// has no shim to report `working`/`waiting`, so without this a row
/// would keep the `working` it was created with for its whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Turn {
    #[default]
    Unknown,
    Working,
    Waiting,
    NeedsInput,
}

/// Fold one `event_msg` type onto the turn state. `task_started` opens
/// a turn and `task_complete` / `turn_aborted` close it; an approval or
/// question request blocks on the user until the stream moves again (a
/// `token_count` follows every item the model produces).
pub fn fold_turn(state: Turn, event: &str) -> Turn {
    match event {
        "task_started" => Turn::Working,
        "task_complete" | "turn_aborted" | "shutdown_complete" => Turn::Waiting,
        "exec_approval_request"
        | "apply_patch_approval_request"
        | "request_user_input"
        | "request_permissions"
        | "elicitation_request"
        | "mcp_elicitation_request"
        | "dynamic_tool_call_request" => Turn::NeedsInput,
        "token_count" | "item_completed" if state == Turn::NeedsInput => Turn::Working,
        _ => state,
    }
}

/// The `event_msg` type a rollout line carries, read off its head: the
/// record type comes first, the payload type right after `"payload":{`.
/// None for a line that is not an event.
pub fn event_type(head: &[u8]) -> Option<&str> {
    let s = core::str::from_utf8(head).ok().or_else(|| {
        // The head may cut a multi-byte character; take what is whole.
        let mut end = head.len();
        while end > 0 && core::str::from_utf8(&head[..end]).is_err() {
            end -= 1;
        }
        core::str::from_utf8(&head[..end]).ok()
    })?;
    let after = &s[s.find("\"type\":\"event_msg\"")? + 18..];
    let payload = &after[after.find("\"payload\":{")? + 11..];
    let typed = payload.strip_prefix("\"type\":\"")?;
    let end = typed.find('"')?;
    Some(&typed[..end])
}

thread_local! {
    /// Per rollout: how far the event scan has read, and the state it
    /// reached. A render reads only what the file gained since.
    static TURNS: std::cell::RefCell<std::collections::HashMap<String, (u64, Turn)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The bytes of a line that hold both type fields.
const EVENT_HEAD: usize = 256;

/// The turn state of a rollout, from the turn-boundary events past the
/// last read. The scan runs on the host's fs worker with the needles as
/// a prefilter, so only the few boundary lines - not the conversation -
/// reach the guest; a `task_complete` carries the last reply, so a line
/// may be long.
async fn codex_turn(path: &str) -> Turn {
    let (mut cursor, mut state) =
        TURNS.with(|t| t.borrow().get(path).copied().unwrap_or((0, Turn::Unknown)));
    const KEEP: &[&[u8]] = &[
        b"\"type\":\"task_started\"",
        b"\"type\":\"task_complete\"",
        b"\"type\":\"turn_aborted\"",
        b"\"type\":\"shutdown_complete\"",
        b"_approval_request\"",
        b"\"type\":\"request_user_input\"",
        b"\"type\":\"request_permissions\"",
        b"elicitation_request\"",
        b"\"type\":\"dynamic_tool_call_request\"",
        b"\"type\":\"token_count\"",
    ];
    let needles: Vec<LineNeedle<'_>> =
        KEEP.iter().map(|b| LineNeedle { bytes: b, reject: false }).collect();
    // Bounded: a rollout that grew by more than this between renders is
    // finished on the next one.
    for _ in 0..64 {
        let Ok(lines) =
            fs_read_lines(path, cursor, &needles, EVENT_HEAD, 4 * 1024 * 1024, 64 * 1024).await
        else {
            break;
        };
        for (_, line) in lines.iter() {
            let head = &line[..line.len().min(EVENT_HEAD)];
            if let Some(ev) = event_type(head) {
                state = fold_turn(state, ev);
            }
        }
        let moved = lines.cursor > cursor;
        cursor = lines.cursor;
        if lines.eof || !moved {
            break;
        }
    }
    TURNS.with(|t| {
        t.borrow_mut().insert(path.to_string(), (cursor, state));
    });
    state
}

/// Find the pane's rollout by scanning the dated session tree.
///
/// Membership is the pane's working directory (the one Codex was started
/// in, which its `session_meta.cwd` records). Among the rollouts there
/// that are not subagents and not already another live row's, the one
/// whose start is nearest the row's first sighting wins; a session that
/// spans several files (the legacy file and the paginated windows that
/// replaced it share one id) is represented by its most recently written
/// file, which is the one Codex appends to now.
async fn codex_scan(a: &Agent) -> Option<Resolved> {
    let pane = a.pane? as u32;
    let now = now_ms() as i64;
    let recent_miss = SCAN_MISSED
        .with(|m| m.borrow().get(&pane).copied())
        .is_some_and(|t| now - t < SCAN_RETRY_MS);
    if recent_miss {
        return None;
    }
    let found = codex_scan_now(a, pane).await;
    if found.is_none() {
        let first = SCAN_MISSED.with(|m| m.borrow_mut().insert(pane, now).is_none());
        if first {
            log(&format!("agents: no rollout found for the codex in pane %{pane} yet"));
        }
    } else {
        SCAN_MISSED.with(|m| m.borrow_mut().remove(&pane));
    }
    found
}

async fn codex_scan_now(a: &Agent, pane: u32) -> Option<Resolved> {
    let cwd = format_expand(OptionTarget::Pane(PaneId(pane)), "#{pane_current_path}")
        .ok()?
        .trim()
        .trim_end_matches('/')
        .to_string();
    if cwd.is_empty() {
        return None;
    }
    let home = home()?;
    let root = format!("{home}/.codex/sessions");
    let since_s = a.first_seen_ms / 1000 - SCAN_SLACK_S;
    // The tree is laid out by LOCAL date; the row's time is epoch. The
    // UTC date one day before the slack's start is on or before the
    // local date everywhere, so every day that can hold a candidate is
    // listed, a day too many at worst.
    let floor = utc_date_floor(a.first_seen_ms - (SCAN_SLACK_S + 86_400) * 1000);

    let mut files: Vec<(String, i64)> = Vec::new();
    for day in days_since(&root, &floor).await {
        let opts = ListOpts { mtime: true, dirs_only: false };
        let Ok(listing) = fs_list_with(&day, opts).await else { continue };
        for e in listing.iter() {
            if matches!(e.kind, EntryKind::File | EntryKind::Unknown)
                && is_rollout(e.name)
                && e.mtime >= since_s
            {
                files.push((format!("{day}/{}", e.name), e.mtime * 1000));
            }
        }
    }
    files.sort_by(|x, y| y.1.cmp(&x.1));
    files.truncate(SCAN_MAX_FILES);

    // Newest file first, so the first file seen for a session id is the
    // one it is being written to.
    let mut best: Option<(i64, String, CodexMeta)> = None;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (path, mtime_ms) in files {
        let Ok((bytes, _)) = fs_read(&path, 0, META_BYTES).await else { continue };
        let meta = codex_meta(&bytes);
        let Some(sid) = meta.session_id.clone() else { continue };
        if meta.subagent || !seen.insert(sid.clone()) {
            continue;
        }
        if meta.cwd.as_deref().map(|c| c.trim_end_matches('/')) != Some(cwd.as_str()) {
            continue;
        }
        if claimed_elsewhere(&format!("codex:{sid}"), a.pane).await {
            continue;
        }
        let distance = (meta.started_ms.unwrap_or(mtime_ms) - a.first_seen_ms).abs();
        if best.as_ref().map_or(true, |(d, _, _)| distance < *d) {
            best = Some((distance, path, meta));
        }
    }
    let (_, path, meta) = best?;
    Some(codex_resolved(path, meta).await)
}

/// Is this id a different live pane's already? The scan must not hand
/// two panes in one directory the same session.
async fn claimed_elsewhere(id: &str, pane: Option<i64>) -> bool {
    match crate::store::by_id(id).await {
        Ok(Some(row)) => row.ended_ms.is_none() && row.pane.is_some() && row.pane != pane,
        _ => false,
    }
}

/// The day directories of `~/.codex/sessions` dated `floor` or later, as
/// paths. The tree is `<yyyy>/<mm>/<dd>`, zero padded, so names order
/// like dates and a whole year or month before the floor is skipped
/// without listing it.
async fn days_since(root: &str, floor: &(String, String, String)) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(years) = fs_list_with(root, ListOpts { mtime: false, dirs_only: true }).await else {
        return out;
    };
    let years: Vec<String> =
        years.iter().map(|e| e.name.to_string()).filter(|y| *y >= floor.0).collect();
    for y in years {
        let ydir = format!("{root}/{y}");
        let Ok(months) = fs_list_with(&ydir, ListOpts { mtime: false, dirs_only: true }).await else {
            continue;
        };
        let months: Vec<String> = months
            .iter()
            .map(|e| e.name.to_string())
            .filter(|m| y > floor.0 || *m >= floor.1)
            .collect();
        for m in months {
            let mdir = format!("{ydir}/{m}");
            let Ok(days) = fs_list_with(&mdir, ListOpts { mtime: false, dirs_only: true }).await else {
                continue;
            };
            let at_floor = y == floor.0 && m == floor.1;
            for d in days.iter() {
                if !at_floor || d.name >= floor.2.as_str() {
                    out.push(format!("{mdir}/{}", d.name));
                }
            }
        }
    }
    out
}

/// The UTC calendar date of an epoch-ms instant, as the zero-padded
/// `(yyyy, mm, dd)` strings the session tree uses for its directories.
pub fn utc_date_floor(ms: i64) -> (String, String, String) {
    // Howard Hinnant's civil_from_days.
    let days = ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (format!("{y:04}"), format!("{m:02}"), format!("{d:02}"))
}

/// `2026-10-05T20:48:58.079Z` (or with a numeric offset) to epoch ms.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        if part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
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
            let om = if b.get(i + 3) == Some(&b':') {
                num(i + 4, i + 6)?
            } else {
                num(i + 3, i + 5)?
            };
            offset_min = oh * 60 + om;
            if sign == b'+' {
                offset_min = -offset_min;
            }
        }
        _ => return None,
    }
    // days_from_civil.
    let (yy, mm) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec + offset_min * 60) * 1000) + millis)
}

// ---------------------------------------------------------------------------
// pi / opencode: the file an `identify` hook already recorded
// ---------------------------------------------------------------------------

/// The `identify` verb stored the durable id and session file when the
/// hook first ran. Here we only refresh activity from the file's mtime.
pub async fn from_source(a: &Agent) -> Option<Resolved> {
    let path = a.source_path.as_deref()?;
    Some(Resolved {
        last_active_ms: file_mtime_ms(path).await,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_to_ms() {
        assert_eq!(parse_rfc3339_ms("2026-10-05T20:48:58.079Z"), Some(1_791_233_338_079));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2026-10-05T13:48:58-07:00"), Some(1_791_233_338_000));
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }

    #[test]
    fn utc_floor_dates() {
        assert_eq!(utc_date_floor(1_791_233_338_079), ("2026".into(), "10".into(), "05".into()));
        assert_eq!(utc_date_floor(0), ("1970".into(), "01".into(), "01".into()));
        // 2024-02-29T12:00Z
        assert_eq!(utc_date_floor(1_709_208_000_000), ("2024".into(), "02".into(), "29".into()));
    }

    #[test]
    fn meta_of_a_user_thread() {
        let line = br#"{"timestamp":"2026-10-05T20:48:58.079Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a10dd3-2304-7263-aadb-f19583aa4ae3","id":"01a10dd3-2304-7263-aadb-f19583aa4ae3","timestamp":"2026-10-05T20:48:58.040Z","cwd":"/Users/z/Code/tab","originator":"codex-tui","cli_version":"0.160.1","source":"vscode","thread_source":"user"}}
{"timestamp":"x","type":"response_item"}"#;
        let m = codex_meta(line);
        assert_eq!(m.session_id.as_deref(), Some("01a10dd3-2304-7263-aadb-f19583aa4ae3"));
        assert_eq!(m.cwd.as_deref(), Some("/Users/z/Code/tab"));
        assert_eq!(m.started_ms, Some(1_791_233_338_040));
        assert!(!m.subagent);
        assert_eq!(m.nickname, None);
    }

    #[test]
    fn meta_of_a_subagent_is_flagged_and_keeps_its_own_id() {
        let line = br#"{"timestamp":"2026-10-05T20:51:16.763Z","ordinal":0,"type":"session_meta","payload":{"session_id":"01a10dd3-2304-7263-aadb-f19583aa4ae3","id":"01a10dd5-c85d-7c63-a9bd-966c3cf72637","parent_thread_id":"01a10dd3-2304-7263-aadb-f19583aa4ae3","cwd":"/Users/z/Code/tab","source":{"subagent":{"thread_spawn":{"agent_nickname":"Hypatia"}}},"agent_nickname":"Hypatia"}}"#;
        let m = codex_meta(line);
        assert_eq!(m.session_id.as_deref(), Some("01a10dd5-c85d-7c63-a9bd-966c3cf72637"));
        assert!(m.subagent);
        assert_eq!(m.nickname.as_deref(), Some("Hypatia"));
    }

    #[test]
    fn meta_of_the_old_layout() {
        let line = br#"{"timestamp":"2026-08-19T00:11:03.716Z","type":"session_meta","payload":{"session_id":"abc","cli_version":"0.147.0"}}"#;
        let m = codex_meta(line);
        assert_eq!(m.session_id.as_deref(), Some("abc"));
        assert!(!m.subagent);
        assert_eq!(m.cwd, None);
    }

    #[test]
    fn turn_boundaries() {
        let mut t = Turn::Unknown;
        for ev in ["token_count", "task_started", "item_completed", "token_count"] {
            t = fold_turn(t, ev);
        }
        assert_eq!(t, Turn::Working);
        assert_eq!(fold_turn(t, "task_complete"), Turn::Waiting);
        assert_eq!(fold_turn(Turn::Working, "turn_aborted"), Turn::Waiting);
        assert_eq!(fold_turn(Turn::Waiting, "task_started"), Turn::Working);
        let blocked = fold_turn(Turn::Working, "exec_approval_request");
        assert_eq!(blocked, Turn::NeedsInput);
        assert_eq!(fold_turn(blocked, "thread_settings_applied"), Turn::NeedsInput);
        assert_eq!(fold_turn(blocked, "token_count"), Turn::Working);
        assert_eq!(fold_turn(blocked, "task_complete"), Turn::Waiting);
    }

    #[test]
    fn event_type_off_the_head() {
        let l = br#"{"timestamp":"2026-10-05T20:52:38.734Z","ordinal":87,"type":"event_msg","payload":{"type":"task_complete","turn_id":"01a10dd5","last_agent_message":"..."}}"#;
        assert_eq!(event_type(&l[..120]), Some("task_complete"));
        let r = br#"{"timestamp":"2026-10-05T20:52:38.734Z","ordinal":88,"type":"response_item","payload":{"type":"message","role":"user"}}"#;
        assert_eq!(event_type(r), None);
        assert_eq!(event_type(br#"{"type":"event_msg","payload":{"turn_id":"x","type":"task_started"}}"#), None);
        // A head cut inside a multi-byte character still parses.
        let m = "{\"timestamp\":\"t\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"x\":\"ééééé";
        let b = m.as_bytes();
        assert_eq!(event_type(&b[..b.len() - 1]), Some("task_started"));
    }

    #[test]
    fn not_a_meta_record() {
        assert_eq!(codex_meta(br#"{"type":"response_item","payload":{"id":"zzz"}}"#), CodexMeta::default());
        assert_eq!(codex_meta(b"not json"), CodexMeta::default());
    }
}
