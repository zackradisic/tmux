//! The verbs that answer on the caller's terminal: `ls`, `show`,
//! `transcript`, `search` and `whoami`. An agent in a pane reads the
//! roster and its peers' conversations with these instead of opening the
//! store's SQLite file by hand. A plugin-command's answer must be given
//! before the handler returns (that is when the host prints it), so
//! everything here reads the store synchronously and nothing waits on a
//! remote: a remote agent's row comes from the view's replica of its
//! roster, its conversation does not cross the link.

use listkit::text::{fmt_age, one_line, tilde_of};
use tmux_plugin_sdk::prelude::*;

use crate::store::{self, Agent, TurnRow};
use crate::view::{self, Remotes};
use crate::{newagent, push, transcript};

/// How many turns `transcript` prints when not told.
const TURNS_DEFAULT: i64 = 30;
/// A turn's text past this many characters is cut (a long reply, a
/// pasted file): the whole thing is in the transcript file.
const TURN_CHARS: usize = 4000;
/// How many agents `search` names when not told.
const HITS_DEFAULT: usize = 10;
/// Ended rows `ls --all` shows at most.
const HISTORY_MAX: i64 = 200;

pub fn is_verb(v: &str) -> bool {
    matches!(v, "ls" | "list" | "show" | "transcript" | "search" | "whoami")
}

/// The answer for the verb line `text`, from the `-t` pane, plus the
/// agent whose transcript should be read again in the background.
pub fn answer(text: &str, pane: Option<u32>, remotes: &Remotes) -> (String, Option<String>) {
    let words = newagent::words(text);
    let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
    match words.first().map(String::as_str).unwrap_or("") {
        "ls" | "list" => (ls(&args, remotes), None),
        "show" => (show(&args, remotes), None),
        "transcript" => transcript(&args, remotes),
        "search" => (search(&args), None),
        "whoami" => (whoami(pane), None),
        v => (format!("agents: unknown verb {v:?}"), None),
    }
}

/// The rows the verbs read: this server's live agents (their `@project`
/// stamped), every linked server's from the replica, and with `all` the
/// ended and archived ones here.
fn rows(remotes: &Remotes, all: bool) -> Vec<Agent> {
    let mut rows = store::live_agents_sync().unwrap_or_default();
    crate::provider::stamp_project_opts(&mut rows);
    rows.extend(remotes.rows());
    if all {
        rows.extend(store::history_sync(HISTORY_MAX).unwrap_or_default());
    }
    rows
}

/// The row for an id: the live ones first (any server), then the store's
/// history. A bare harness id (no `kind:`) is tried under each kind.
fn find(id: &str, remotes: &Remotes) -> Option<Agent> {
    let (bare, server) = match id.rsplit_once('@') {
        Some((i, s)) => (i, Some(s)),
        None => (id, None),
    };
    let candidates: Vec<String> = if bare.contains(':') {
        vec![bare.to_string()]
    } else {
        transcript::HARNESSES.iter().map(|k| format!("{k}:{bare}")).collect()
    };
    let live = rows(remotes, false);
    for c in &candidates {
        if let Some(a) = live.iter().find(|a| a.id == *c && server.is_none_or(|s| a.server == s)) {
            return Some(a.clone());
        }
    }
    if server.is_some() {
        return None;
    }
    candidates.iter().find_map(|c| store::by_id_sync(c).ok().flatten())
}

/// The state column: the status word, `ended` for a finished agent.
fn state_of(a: &Agent) -> String {
    if !a.live() {
        return "ended".to_string();
    }
    a.status.clone()
}

fn state_rank(s: &str) -> u8 {
    match s {
        "needs_input" => 0,
        "working" => 1,
        "waiting" => 2,
        "done" => 3,
        _ => 4,
    }
}

fn ls(args: &[&str], remotes: &Remotes) -> String {
    let mut project: Option<String> = None;
    let mut all = false;
    let mut json = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "--project" | "-p" => project = it.next().map(|s| s.to_string()),
            "--all" | "-a" => all = true,
            "--json" => json = true,
            other => return format!("agents ls: unknown flag {other:?} (--project <name>, --all, --json)"),
        }
    }
    let home = home_dir().ok().filter(|h| !h.is_empty());
    let mut list: Vec<(String, String, String, Agent)> = rows(remotes, all)
        .into_iter()
        .map(|a| (view::project_of(&a), state_of(&a), view::display_name(&a), a))
        .filter(|(p, ..)| project.as_deref().is_none_or(|want| p.eq_ignore_ascii_case(want)))
        .collect();
    list.sort_by(|x, y| {
        x.0.to_lowercase()
            .cmp(&y.0.to_lowercase())
            .then(state_rank(&x.1).cmp(&state_rank(&y.1)))
            .then(x.2.to_lowercase().cmp(&y.2.to_lowercase()))
    });
    if list.is_empty() {
        return match &project {
            Some(p) => format!("no agents in project {p:?}"),
            None => "no agents".to_string(),
        };
    }
    if json {
        return list
            .iter()
            .map(|(p, st, name, a)| {
                serde_json::json!({
                    "id": a.id,
                    "address": address(a),
                    "name": name,
                    "kind": a.kind,
                    "state": st,
                    "task": a.task,
                    "project": p,
                    "cwd": a.cwd,
                    "branch": a.git_branch,
                    "session": a.session,
                    "window": a.window,
                    "pane": a.pane.map(|n| format!("%{n}")),
                    "server": if a.is_local() { "local" } else { a.server.as_str() },
                    "started_ms": a.started(),
                    "last_active_ms": a.last_active_ms,
                    "inbox": inbox_state(a),
                    "inbox_socket": inbox_socket(a),
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let id_w = list.iter().map(|(_, _, _, a)| a.id.chars().count()).max().unwrap_or(0);
    let st_w = list.iter().map(|(_, s, _, _)| s.len()).max().unwrap_or(0);
    let pj_w = list.iter().map(|(p, ..)| p.chars().count()).max().unwrap_or(0);
    let nm_w = list.iter().map(|(_, _, n, _)| n.chars().count()).max().unwrap_or(0).min(40);
    let mut gone = false;
    let mut lines: Vec<String> = list
        .iter()
        .map(|(p, st, name, a)| {
            let mut line = format!(
                "{:<id_w$}  {:<st_w$}  {:<pj_w$}  {:<nm_w$}  {}",
                a.id,
                st,
                p,
                clip_chars(name, 40),
                a.cwd.as_deref().map(|c| tilde_of(&home, c)).unwrap_or_default()
            );
            if !a.is_local() {
                line.push_str("  @");
                line.push_str(&a.server);
            }
            if matches!(push::inbox_sync(a), push::Inbox::Gone(_)) {
                gone = true;
                line.push_str("  ");
                line.push_str(INBOX_GONE);
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>();
    if gone {
        lines.push(format!(
            "{INBOX_GONE}: its inbox socket was removed (the /tmp cleaner), so a message \
             waits in its box until its next `mailbox inbox`; `claude --resume <uuid>` \
             gives it a new socket"
        ));
    }
    lines.join("\n")
}

/// The row marker for a Claude agent whose inbox socket is gone.
const INBOX_GONE: &str = "⚠ inbox gone";

fn inbox_state(a: &Agent) -> Option<&'static str> {
    match push::inbox_sync(a) {
        push::Inbox::None => None,
        push::Inbox::Live(_) => Some("live"),
        push::Inbox::Gone(_) => Some("gone"),
        push::Inbox::Unchecked(_) => Some("unchecked"),
    }
}

fn inbox_socket(a: &Agent) -> Option<String> {
    match push::inbox_sync(a) {
        push::Inbox::None => None,
        push::Inbox::Live(s) | push::Inbox::Gone(s) | push::Inbox::Unchecked(s) => Some(s),
    }
}

/// The `inbox` line of show and whoami: the socket a push writes to,
/// and what to do when it is gone. `you` phrases it for the agent itself.
fn inbox_line(a: &Agent, you: bool) -> Option<String> {
    let bare = a.id.strip_prefix("claude:").unwrap_or(&a.id);
    Some(match push::inbox_sync(a) {
        push::Inbox::None => return None,
        push::Inbox::Live(s) => format!("{s}  (a message is pushed into the session)"),
        push::Inbox::Unchecked(s) => format!("{s}  (not checked: outside this plugin's fs-read paths)"),
        push::Inbox::Gone(s) if you => format!(
            "{s}  GONE: messages are not pushed to you any more; read them with \
             `tmux2 plugin-command mailbox \"inbox {}\"`, or restart with `claude --resume {bare}` \
             for a new socket",
            a.id
        ),
        push::Inbox::Gone(s) => format!(
            "{s}  GONE: a message waits in its box until its next `mailbox inbox`; \
             `claude --resume {bare}` gives it a new socket"
        ),
    })
}

/// For `message`: the line the sender hears when the local Claude agent
/// it addresses cannot be woken. None when the push can go ahead, or the
/// agent is not here (the reader's server knows its own sockets).
pub fn gone_note(id: &str) -> Option<String> {
    let a = store::by_id_sync(id).ok().flatten()?;
    let push::Inbox::Gone(sock) = push::inbox_sync(&a) else { return None };
    let bare = a.id.strip_prefix("claude:").unwrap_or(&a.id);
    Some(format!(
        "agents: {id} will not be woken: its inbox socket {sock} is gone (removed by the \
         /tmp cleaner?). The message is stored; the agent reads it on its next \
         `mailbox inbox`, and `claude --resume {bare}` gives it a new socket."
    ))
}

/// The id a message to this agent is addressed to: the bare id here,
/// `id@server` on a linked host.
fn address(a: &Agent) -> String {
    if a.is_local() {
        a.id.clone()
    } else {
        format!("{}@{}", a.id, a.server)
    }
}

fn clip_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn show(args: &[&str], remotes: &Remotes) -> String {
    let Some(id) = args.first() else {
        return "agents show: show <agent-id>".to_string();
    };
    let Some(a) = find(id, remotes) else {
        return format!("agents: no agent {id:?} (ls lists them; a remote one is <id>@<server>)");
    };
    let now = now_ms();
    let home = home_dir().ok().filter(|h| !h.is_empty());
    let mut rows: Vec<(&str, String)> = Vec::new();
    rows.push(("id", address(&a)));
    rows.push(("name", view::display_name(&a)));
    let mut harness = a.kind.clone();
    if let Some(v) = a.harness_version.as_deref() {
        harness.push(' ');
        harness.push_str(v);
    }
    if let Some(m) = a.model.as_deref() {
        harness.push_str(" · ");
        harness.push_str(m);
    }
    rows.push(("harness", harness));
    let where_ = if a.live() {
        format!(
            "{}:{} · {} · {}",
            a.session.as_deref().unwrap_or("?"),
            a.window.as_deref().unwrap_or("?"),
            a.pane.map(|n| format!("%{n}")).unwrap_or_else(|| "no pane".into()),
            if a.is_local() { "this server".to_string() } else { a.server.clone() }
        )
    } else {
        format!(
            "ended {} ago ({}) · was {}:{}",
            fmt_age(now.saturating_sub(a.ended_ms.unwrap_or(0).max(0) as u64) / 1000),
            a.reason.as_deref().unwrap_or("gone"),
            a.session.as_deref().unwrap_or("?"),
            a.window.as_deref().unwrap_or("?"),
        )
    };
    rows.push(("where", where_));
    let (proj, src) = view::project_with_source(&a);
    rows.push(("project", format!("{proj}  ({src})")));
    rows.push(("cwd", a.cwd.as_deref().map(|c| tilde_of(&home, c)).unwrap_or_else(|| "unknown".into())));
    rows.push(("branch", a.git_branch.clone().unwrap_or_else(|| "unknown".into())));
    rows.push((
        "started",
        format!(
            "{} ago · last active {} ago",
            fmt_age(now.saturating_sub(a.started().max(0) as u64) / 1000),
            fmt_age(now.saturating_sub(a.active_ms().max(0) as u64) / 1000)
        ),
    ));
    let mut status = state_of(&a);
    if a.life == "archived" {
        status.push_str(" · archived");
    }
    if let Some(t) = a.task.as_deref().filter(|s| !s.is_empty()) {
        status.push_str(" · ");
        status.push_str(&one_line(t));
    }
    if let Some(n) = a.note.as_deref().filter(|s| !s.is_empty()) {
        status.push_str(" · ");
        status.push_str(&one_line(n));
    }
    rows.push(("status", status));
    if a.is_local() {
        if let Ok(st) = store::stats_sync(&a.id) {
            if st.prompts + st.replies + st.tools > 0 {
                rows.push((
                    "turns",
                    format!(
                        "{} prompts · {} replies · {} tool calls ({} edits, {} reads, {} commands)",
                        st.prompts, st.replies, st.tools, st.edits, st.reads, st.commands
                    ),
                ));
            }
            if !st.files.is_empty() {
                let files: Vec<String> = st
                    .files
                    .iter()
                    .map(|(f, n)| {
                        let short = f.rsplit('/').next().unwrap_or(f);
                        if *n > 1 { format!("{short} ×{n}") } else { short.to_string() }
                    })
                    .collect();
                rows.push(("files", files.join(" · ")));
            }
        }
        if let Some(tp) = a.transcript_path.as_deref() {
            rows.push(("transcript", format!("{} · {} KB read", tilde_of(&home, tp), a.transcript_cursor.max(0) / 1024)));
        }
        if let Some(l) = inbox_line(&a, false) {
            rows.push(("inbox", l));
        }
    }
    let bare = a.id.strip_prefix(&format!("{}:", a.kind)).unwrap_or(&a.id);
    let resume = match a.kind.as_str() {
        "claude" => Some(format!("claude --resume {bare}")),
        "codex" => Some(format!("codex resume {bare}")),
        _ => None,
    };
    if let Some(r) = resume {
        rows.push(("resume", r));
    }
    let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    rows.iter().map(|(k, v)| format!("{k:<w$}  {v}")).collect::<Vec<_>>().join("\n")
}

fn transcript(args: &[&str], remotes: &Remotes) -> (String, Option<String>) {
    let mut id: Option<&str> = None;
    let mut n = TURNS_DEFAULT;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "-n" | "--last" => match it.next().and_then(|v| v.parse::<i64>().ok()) {
                Some(v) if v > 0 => n = v,
                _ => return ("agents transcript: -n takes a count".to_string(), None),
            },
            "--all" | "-a" => n = i64::MAX,
            other if other.starts_with('-') => {
                return (format!("agents transcript: unknown flag {other:?} (-n <turns>, --all)"), None)
            }
            other => id = Some(other),
        }
    }
    let Some(id) = id else {
        return ("agents transcript: transcript <agent-id> [-n <turns>] [--all]".to_string(), None);
    };
    let Some(a) = find(id, remotes) else {
        return (format!("agents: no agent {id:?} (ls lists them)"), None);
    };
    if !a.is_local() {
        return (
            format!(
                "agents: {} runs on {}; its conversation is there:\n  ssh {} tmux2 plugin-command agents 'transcript {}'",
                a.id, a.server, a.server, a.id
            ),
            None,
        );
    }
    let (turns, total) = store::turns_tail_sync(&a.id, n).unwrap_or_default();
    let name = view::display_name(&a);
    let mut out = String::new();
    if turns.is_empty() {
        out.push_str(&format!("# {name} ({}) · no turns stored yet", a.id));
        if a.transcript_path.is_none() {
            out.push_str(" (no transcript known for it)");
        }
        return (out, Some(a.id));
    }
    out.push_str(&format!(
        "# {name} ({}) · {} of {total} turns · as stored at its last turn end\n",
        a.id,
        turns.len()
    ));
    let now = now_ms();
    for t in &turns {
        out.push_str(&turn_block(t, now));
    }
    (out.trim_end().to_string(), Some(a.id))
}

/// One turn as it prints: a heading with its age for a prompt or a
/// reply, one bulleted line for a tool call.
fn turn_block(t: &TurnRow, now: u64) -> String {
    let age = t
        .ts_ms
        .filter(|&ms| ms > 0)
        .map(|ms| format!(" · {} ago", fmt_age(now.saturating_sub(ms as u64) / 1000)))
        .unwrap_or_default();
    match t.kind.as_str() {
        "tool" => format!("· {}\n", one_line(&t.text)),
        kind => {
            let who = if kind == "user" { "user" } else { "agent" };
            let mut text = t.text.trim().to_string();
            if text.chars().count() > TURN_CHARS {
                text = text.chars().take(TURN_CHARS).collect();
                text.push_str("\n[… cut; the transcript file has the rest]");
            }
            format!("\n## {who}{age}\n{text}\n")
        }
    }
}

fn search(args: &[&str]) -> String {
    let mut limit = HITS_DEFAULT;
    let mut words: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match *a {
            "-n" => match it.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(v) if v > 0 => limit = v,
                _ => return "agents search: -n takes a count".to_string(),
            },
            w => words.push(w),
        }
    }
    let query = words.join(" ");
    let Some(m) = transcript::fts_match(&query) else {
        return "agents search: search <words...>".to_string();
    };
    let raw_cap = limit.saturating_mul(40).clamp(limit, 4000);
    let rows = store::search_fts_sync(&m, raw_cap).unwrap_or_default();
    let (terms, _) = crate::index::query_terms(&query);
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for (id, seq, _) in rows {
        if !seen.insert(id.clone()) {
            continue;
        }
        let turns = store::turns_range_sync(&id, seq, seq + transcript::DOC_WINDOW).unwrap_or_default();
        let snippet = transcript::snippet_for(&turns, &id, seq, &terms);
        let name = store::by_id_sync(&id).ok().flatten().map(|a| view::display_name(&a)).unwrap_or_default();
        lines.push(format!("{id}  {name}  {}", one_line(&snippet)));
        if lines.len() >= limit {
            break;
        }
    }
    if lines.is_empty() {
        return format!("no conversation matches {query:?}");
    }
    lines.join("\n")
}

fn whoami(pane: Option<u32>) -> String {
    let Some(pane) = pane else {
        return "agents whoami: pass -t \"$TMUX_PANE\"".to_string();
    };
    let Some(mut a) = store::live_by_pane_sync(i64::from(pane)).ok().flatten() else {
        return format!("agents: no agent in %{pane}");
    };
    crate::provider::stamp_project_opts(std::slice::from_mut(&mut a));
    let peer = format_expand(OptionTarget::Pane(PaneId(pane)), "#{peer_name}")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let (proj, src) = view::project_with_source(&a);
    let mut rows = vec![
        ("id", a.id.clone()),
        ("name", view::display_name(&a)),
        ("project", format!("{proj}  ({src})")),
    ];
    if let Some(p) = peer {
        rows.push(("address", format!("{}@{p}  (for a reply from another server)", a.id)));
    }
    if let Some(l) = inbox_line(&a, true) {
        rows.push(("inbox", l));
    }
    rows.iter().map(|(k, v)| format!("{k:<8}  {v}")).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(kind: &str, text: &str, ts: Option<i64>) -> TurnRow {
        TurnRow { rowid: 0, id: "claude:x".into(), seq: 0, kind: kind.into(), ts_ms: ts, text: text.into(), path: None, offset: 0, len: 0 }
    }

    #[test]
    fn turns_print_as_blocks() {
        let now = 10_000_000;
        assert_eq!(turn_block(&turn("user", " hi there ", Some(9_940_000)), now), "\n## user · 1m ago\nhi there\n");
        assert_eq!(turn_block(&turn("assistant", "ok", None), now), "\n## agent\nok\n");
        assert_eq!(turn_block(&turn("tool", "Edit run.py +1 −1\nmore", None), now), "· Edit run.py +1 −1 more\n");
        let long = "x".repeat(TURN_CHARS + 10);
        let b = turn_block(&turn("assistant", &long, None), now);
        assert!(b.contains("[… cut; the transcript file has the rest]"));
        assert!(b.chars().count() < TURN_CHARS + 100);
    }

    #[test]
    fn clipping_keeps_short_names_whole() {
        assert_eq!(clip_chars("short", 40), "short");
        let long = "a".repeat(50);
        let c = clip_chars(&long, 40);
        assert_eq!(c.chars().count(), 40);
        assert!(c.ends_with('…'));
    }
}
