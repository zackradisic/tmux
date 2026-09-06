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
    let pane = a.pane? as u32;
    let home = home()?;
    let dir = format!("{home}/.claude/sessions");

    // Direct hit: <pid>.json, verified by its tmux field.
    if let Ok(Some(pid)) = pane_pid(PaneId(pane)) {
        let path = format!("{dir}/{pid}.json");
        if let Ok((bytes, _)) = fs_read(&path, 0, 16 * 1024).await {
            if let Some((p, r)) = parse_claude(&path, &bytes) {
                if p == pane {
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
        if let Some((p, r)) = parse_claude(&path, &bytes) {
            if p == pane {
                return Some(r);
            }
        }
    }
    None
}

/// Parse one Claude session file into (pane, resolved). Returns None when
/// the JSON is malformed or lacks a usable `tmux` field.
fn parse_claude(path: &str, bytes: &[u8]) -> Option<(u32, Resolved)> {
    let v = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    // tmux is "session:@window.%pane"; take the %N pane id.
    let pane = v
        .get("tmux")
        .and_then(|x| x.as_str())?
        .rsplit('.')
        .next()
        .and_then(|p| p.strip_prefix('%'))
        .and_then(|n| n.parse::<u32>().ok())?;
    let sid = v.get("sessionId").and_then(|x| x.as_str());
    let status = match v.get("status").and_then(|x| x.as_str()) {
        Some("busy") => Some("working".to_string()),
        Some("idle") => Some("waiting".to_string()),
        _ => None,
    };
    Some((
        pane,
        Resolved {
            real_id: sid.map(|s| format!("claude:{s}")),
            name: v.get("name").and_then(|x| x.as_str()).map(str::to_string),
            status,
            started_ms: v.get("startedAt").and_then(|x| x.as_i64()),
            last_active_ms: v.get("updatedAt").and_then(|x| x.as_i64()),
            source_path: Some(path.to_string()),
        },
    ))
}

// ---------------------------------------------------------------------------
// codex: the rollout jsonl the TUI holds open
// ---------------------------------------------------------------------------

/// Find the open rollout file through `pane_fds`, read its session id from
/// the first line, and date it by the file's mtime.
pub async fn codex(a: &Agent) -> Option<Resolved> {
    let pane = a.pane? as u32;
    let fds = pane_fds(PaneId(pane)).ok().flatten()?;
    let path = fds.into_iter().find(|p| {
        p.ends_with(".jsonl") && p.rsplit('/').next().is_some_and(|n| {
            n.starts_with("rollout-")
        })
    })?;

    // The first line is a small session_meta record naming the session.
    let mut real_id = None;
    if let Ok((bytes, _)) = fs_read(&path, 0, 8 * 1024).await {
        let text = String::from_utf8_lossy(&bytes);
        if let Some(line) = text.lines().next() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(sid) = v
                    .get("payload")
                    .and_then(|p| p.get("session_id"))
                    .and_then(|x| x.as_str())
                {
                    real_id = Some(format!("codex:{sid}"));
                }
            }
        }
    }

    Some(Resolved {
        real_id,
        last_active_ms: file_mtime_ms(&path).await,
        source_path: Some(path),
        ..Default::default()
    })
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
