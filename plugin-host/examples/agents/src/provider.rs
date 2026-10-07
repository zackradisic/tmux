//! The provider half: everything that looks at THIS server's machine.
//!
//! Detection reads pane commands and environments, the resolvers read the
//! harnesses' session files, the roster lives in this server's store, and
//! captures and content search read this server's grids. None of that
//! crosses a link, so the provider runs on every server (pushed there by a
//! `remote-attach` link) and a view anywhere asks it through services:
//!
//!   list    { history, archived }    -> Snapshot (enriched rows + clock)
//!   capture { id }                   -> the pane's text, or the saved one
//!   search  { needle, archived,      -> SearchReply (hits per agent id, and
//!             transcript }              with `transcript` the conversation
//!                                       hits plus the rows they belong to)
//!   turns   { id, from, to }         -> the stored conversation, by seq
//!   stats   { id }                   -> Stats (what the conversation adds
//!                                       up to, for the info card)
//!   act     { id, verb, name? }      -> "ok" (ack | archive | unarchive | rename)
//!
//! and follows the `changed` topic, which carries a fresh Snapshot after
//! every write. Times are the provider's clock; `now_ms` in the Snapshot
//! lets a view on another machine correct for skew.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

use crate::resolve::{self, Resolved};
use crate::store::{self, Agent, TurnRow};
use crate::transcript::{self, TranscriptHit};
use crate::Config;

/// The commands that mark a pane as an agent, if none are configured.
pub const DEFAULT_COMMANDS: &[&str] = &["claude", "codex", "pi", "opencode"];
/// The statuses a shim may report.
pub const STATUSES: &[&str] = &["working", "needs_input", "waiting", "done"];
/// Foreground commands that are really interpreters launching a script.
/// Codex ships as `node /usr/bin/codex`, so the pane's foreground command
/// is `node`; the agent name is the basename of the launched script, which
/// the process keeps in its `_` environment variable.
const INTERPRETERS: &[&str] =
    &["node", "bun", "deno", "python", "python3", "ruby"];
/// Lines searched per pane (from the bottom up) by content search. 0 =
/// the host default cap.
pub const SEARCH_LINES: u32 = 5000;
/// The service topic a provider publishes its roster on.
pub const TOPIC: &str = "changed";

/// A provider asks the views that follow it to open their picker on an
/// agent: the user pressed the open key in copy mode on THIS server's
/// pane (a mirrored one, from the workstation), where the picker they
/// look at is not.
pub const OPEN_TOPIC: &str = "open";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenReq {
    pub id: String,
}

pub fn broadcast_open(id: &str) {
    let _ = service::emit_json(OPEN_TOPIC, &OpenReq { id: id.to_string() });
}

// ---------------------------------------------------------------------------
// wire shapes
// ---------------------------------------------------------------------------

/// The provider's roster as a view sees it: rows plus the provider's clock.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub now_ms: i64,
    pub agents: Vec<Agent>,
}

/// What a view wants beyond the live rows. `history` adds the recent
/// finished and archived rows (capped); `archived` adds every archived
/// row instead, uncapped, for the archive-only view. An older provider
/// that does not know `archived` answers with history: the view still
/// filters to archived rows, only capped ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ListReq {
    #[serde(default)]
    pub history: bool,
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReq {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReq {
    pub needle: String,
    /// Also grep the saved captures of archived agents whose pane is
    /// gone, for the archive-only view.
    #[serde(default)]
    pub archived: bool,
    /// Also search the stored conversations (the transcript index), and
    /// return the rows of the agents that hit, so the view can show ones
    /// its roster does not hold. An older provider ignores this.
    #[serde(default)]
    pub transcript: bool,
}

/// Content-search hits, by agent id, with the matcher that found them;
/// and, when asked, the conversation hits with their rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchReply {
    pub mode: String,
    pub hits: HashMap<String, String>,
    #[serde(default)]
    pub transcript: Vec<TranscriptHit>,
    #[serde(default)]
    pub agents: Vec<Agent>,
}

/// One agent's conversation totals, for the info card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsReq {
    pub id: String,
}

/// One agent's stored conversation, turns `[from, to)` by seq.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnsReq {
    pub id: String,
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActReq {
    pub id: String,
    pub verb: String,
    #[serde(default)]
    pub name: Option<String>,
}

// ---------------------------------------------------------------------------
// detection + identity (observed from the live process)
// ---------------------------------------------------------------------------

/// What detection found for a pane.
enum Detected {
    Kind(String),
    /// The environment says claude, but no session file names the pane
    /// yet. Either a Claude that just started (its file comes within a
    /// second or two) or a variable the pane merely inherited. Worth
    /// another look soon, not a row.
    Pending,
    None,
}

/// A foreground command like "2.1.271": the macOS Claude launcher execs
/// `~/.local/share/claude/versions/<version>`, so the command name is the
/// version, not `claude`.
fn looks_like_version(base: &str) -> bool {
    let mut parts = base.split('.');
    let n = parts.clone().count();
    (2..=4).contains(&n) && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The agent kind a pane runs. Command name first (reliable, and the only
/// signal Codex offers), then the interpreter's launched script, then
/// the environment. `AI_AGENT` is a hint, not proof: Claude Code sets it
/// for its children, so a tmux server started from inside Claude Code
/// hands it to every pane it spawns, and the Claude process itself may
/// carry the server's copy. A claude row needs the harness's own session
/// file to name the pane (see `resolve::claude_claims_pane`), unless the
/// configuration says `trust_env`.
async fn detect(pane: u32, cfg: &Config) -> Detected {
    let commands = &cfg.commands;
    let mut version_like = false;
    if let Ok(cmd) =
        format_expand(OptionTarget::Pane(PaneId(pane)), "#{pane_current_command}")
    {
        let base = cmd.rsplit('/').next().unwrap_or(&cmd);
        if let Some(k) = commands.iter().find(|c| c.as_str() == base) {
            return Detected::Kind(k.clone());
        }
        // An interpreter-wrapped CLI: match the basename of the launched
        // script (the `_` var), not the interpreter. Gated to interpreters
        // so an idle shell's stale `_` cannot trip a false positive.
        if INTERPRETERS.contains(&base) {
            if let Ok(Some(under)) = pane_env(PaneId(pane), "_") {
                let ubase = under.rsplit('/').next().unwrap_or(&under);
                if let Some(k) = commands.iter().find(|c| c.as_str() == ubase)
                {
                    return Detected::Kind(k.clone());
                }
            }
        }
        version_like = looks_like_version(base);
    }
    let hint = pane_env(PaneId(pane), "AI_AGENT")
        .ok()
        .flatten()
        .map(|v| v.to_lowercase())
        .and_then(|v| {
            ["claude", "codex", "opencode", "pi"]
                .iter()
                .find(|k| v.contains(*k))
                .map(|k| k.to_string())
        });
    match hint.as_deref() {
        // An inherited AI_AGENT is not evidence, and neither is a session
        // file on its own: what runs in the pane decides. A shell, a
        // `sleep`, anything whose command is an ordinary program, is not
        // an agent however loudly a file claims the pane - a session file
        // outlives its Claude and the server reuses pane ids after a
        // restart, so a stale file will name a pane that now holds a
        // shell. Only a command we cannot read as itself - the macOS
        // launcher execs a version-named binary - earns a look at the
        // files, just below.
        Some("claude") if !cfg.trust_env => {}
        Some(k) => return Detected::Kind(k.to_string()),
        None => {}
    }
    if version_like {
        return if crate::resolve::claude_claims_pane(pane).await {
            Detected::Kind("claude".into())
        } else {
            // The process looks like a harness but has not written its
            // file yet; look again shortly.
            Detected::Pending
        };
    }
    if let Ok(Some(_)) = pane_env(PaneId(pane), "OPENCODE") {
        return Detected::Kind("opencode".into());
    }
    Detected::None
}

thread_local! {
    /// Per pane: how many delayed looks a Pending detection has had.
    static PENDING_LOOKS: std::cell::RefCell<HashMap<u32, u8>> =
        std::cell::RefCell::new(HashMap::new());
    /// (session, window) -> the user name for the agent that appears
    /// there: a fork started from the form gets a name of its own, since
    /// the harness gives the copy the original's.
    static PENDING_NAMES: std::cell::RefCell<HashMap<(String, String), String>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Name the agent that appears in this session's window `window`, once,
/// as the user would with the rename key.
pub fn name_window_when_seen(session: &str, window: &str, name: &str) {
    PENDING_NAMES.with(|p| {
        p.borrow_mut().insert((session.to_string(), window.to_string()), name.to_string());
    });
}

/// Delays between the looks at a Pending pane: a Claude writes its
/// session file within the first seconds of its life.
const PENDING_DELAYS_MS: [u64; 3] = [2000, 5000, 15000];

/// Is this pane a shadow of a pane on another server? Its agent belongs
/// to that server's provider, so this one leaves it alone.
fn is_shadow(pane: u32) -> bool {
    resolve_pane(PaneId(pane)).map(|p| p.remote).unwrap_or(false)
}

/// A provisional, pane-bound id for a freshly observed agent. A resolver
/// migrates the row to the durable harness id once the session file
/// appears (see `resolve`). Deterministic per pane, so re-detection of
/// the same pane does not churn rows.
fn provisional_id(kind: &str, pane: u32) -> String {
    format!("prov-{kind}-{pane}")
}

/// The pane's title, when it is a useful name (non-empty and not just the
/// running command or the directory). Used as the provisional display
/// name until a resolver supplies the harness's own name, and preferred
/// over it while the pane lives (see `enrich_live`).
pub fn pane_title(pane: u32, kind: &str) -> Option<String> {
    let expanded = format_expand(
        OptionTarget::Pane(PaneId(pane)),
        "#{pane_title}\t#{host_short}\t#{host}\t#{pane_current_path}",
    )
    .ok()?;
    let mut parts = expanded.splitn(4, '\t');
    let title = parts.next().unwrap_or("");
    let host_short = parts.next().unwrap_or("");
    let host = parts.next().unwrap_or("");
    let cwd = parts.next().unwrap_or("");
    clean_title(title, kind, host_short, host, cwd).map(str::to_string)
}

/// The useful part of a pane title, or nothing.
///
/// The default pane title is the host name (an OSC title the shell never
/// set), which is no better than the kind. Codex titles its pane
/// `<topic> | <dir>` once the thread has a topic and plain `<dir>` (the
/// working directory's last component) before that, so the directory is
/// peeled off and a title that is only the directory - or the cwd itself
/// - is no name either. What survives is a title an agent actually
/// wrote about its work.
fn clean_title<'a>(
    title: &'a str,
    kind: &str,
    host_short: &str,
    host: &str,
    cwd: &str,
) -> Option<&'a str> {
    let cwd = cwd.trim().trim_end_matches('/');
    let dir = cwd.rsplit('/').next().unwrap_or("");
    let mut title = title.trim();
    if !dir.is_empty() {
        if let Some((head, tail)) = title.rsplit_once(" | ") {
            if tail.trim() == dir {
                title = head.trim();
            }
        }
    }
    let is_dir = !dir.is_empty()
        && (title == dir
            || title == cwd
            // `~/Code/tab`: a path spelled another way. A topic has spaces;
            // a path, as a rule, has none.
            || (!title.contains(' ') && title.ends_with(&format!("/{dir}"))));
    if title.is_empty()
        || title.eq_ignore_ascii_case(kind)
        || title == host_short
        || title == host
        || is_dir
    {
        return None;
    }
    Some(title)
}

/// Session and window names for a pane, best effort.
fn labels(pane: u32) -> (Option<String>, Option<String>) {
    let panes = list_panes().unwrap_or_default();
    let Some(pi) = panes.iter().find(|p| p.id == pane) else {
        return (None, None);
    };
    let windows = list_windows().unwrap_or_default();
    let Some(wi) = windows.iter().find(|w| w.id == pi.window) else {
        return (None, None);
    };
    let session = wi
        .sessions
        .first()
        .and_then(|sid| resolve_session(SessionId(*sid)).ok())
        .map(|s| s.name);
    (session, Some(wi.name.clone()))
}

pub fn capture_tail(pane: u32) -> Option<String> {
    capture_pane(PaneId(pane), None, None).ok()
}

/// Classify a pane and reconcile the roster for it: create/revive/rebind
/// a live agent, or retire the one that was there if it is no longer an
/// agent. A shadow pane (mirrored from another server) is never an agent
/// here: its own server's provider owns it.
pub async fn classify(pane: u32, cfg: Rc<Config>) {
    if !owns_store() {
        return;
    }
    let now = now_ms() as i64;
    if is_shadow(pane) {
        let _ = store::end_by_pane(pane as i64, now, "closed").await;
        return;
    }
    let kind = match detect(pane, &cfg).await {
        Detected::Kind(k) => {
            PENDING_LOOKS.with(|p| p.borrow_mut().remove(&pane));
            k
        }
        Detected::Pending => {
            // A row already here stays: its file was seen once. Otherwise
            // look again after a while, a bounded number of times.
            if store::live_by_pane(pane as i64).await.ok().flatten().is_some() {
                return;
            }
            let look = PENDING_LOOKS.with(|p| {
                let mut p = p.borrow_mut();
                let n = p.entry(pane).or_insert(0);
                let look = *n;
                *n += 1;
                look
            });
            if let Some(delay) = PENDING_DELAYS_MS.get(look as usize) {
                let delay = *delay;
                let cfg = Rc::clone(&cfg);
                spawn(async move {
                    if sleep_ms(delay).await.is_ok() {
                        classify(pane, cfg).await;
                    }
                });
            } else {
                PENDING_LOOKS.with(|p| p.borrow_mut().remove(&pane));
            }
            return;
        }
        Detected::None => {
            PENDING_LOOKS.with(|p| p.borrow_mut().remove(&pane));
            end_pane(pane, now, "closed").await;
            return;
        }
    };
    // detect() awaited; the pane may have died meanwhile, and its
    // pane-destroyed handler found no row to end. A row for a dead pane
    // would be a ghost until the sweep, so look once more first.
    if resolve_pane(PaneId(pane)).is_err() {
        let _ = store::end_by_pane(pane as i64, now, "closed").await;
        return;
    }
    let (session, window) = labels(pane);
    let existing = store::live_by_pane(pane as i64).await.ok().flatten();
    // Keep the pane's current id (provisional or already-resolved); mint a
    // provisional one only for a pane we have not seen. A resolver
    // migrates the provisional id to the durable one later.
    let id = existing
        .as_ref()
        .map(|a| a.id.clone())
        .unwrap_or_else(|| provisional_id(&kind, pane));
    let name = pane_title(pane, &kind);
    let _ = store::activate(
        &id,
        &kind,
        pane as i64,
        session.as_deref(),
        window.as_deref(),
        name.as_deref(),
        now,
    )
    .await;
    // A name left for whatever agent appeared in this window (a fork
    // started from the form): applied once, and it follows the row
    // through its id migration like any user name.
    if let (Some(s), Some(w)) = (session.as_deref(), window.as_deref()) {
        let pending =
            PENDING_NAMES.with(|p| p.borrow_mut().remove(&(s.to_string(), w.to_string())));
        if let Some(user_name) = pending {
            let _ = store::rename_by_user(&id, Some(&user_name), now).await;
        }
    }
}

// ---------------------------------------------------------------------------
// resolve-at-render: enrich live rows from each harness's own session file
// ---------------------------------------------------------------------------

/// Enrich every live row in place from its harness source, and persist
/// the result (id migrations, names, statuses, real times). Runs once per
/// picker open and per refresh - never in the background.
pub async fn enrich_live(rows: &mut [Agent]) {
    for a in rows.iter_mut().filter(|a| a.live()) {
        let mut r = match a.kind.as_str() {
            "claude" => resolve::claude(a).await,
            "codex" => resolve::codex(a).await,
            _ => resolve::from_source(a).await,
        }
        .unwrap_or_default();
        // The live pane title tracks the conversation topic (Claude and
        // its kin write it there; Codex since 0.160 does too, behind a
        // ` | <dir>` suffix that `pane_title` peels), which beats the
        // session file's slug. Prefer it; keep the harness name (a Codex
        // nickname) only as a fallback.
        let title = a.pane.and_then(|p| pane_title(p as u32, &a.kind));
        r.name = title.or_else(|| r.name.take());
        apply(a, r).await;
    }
}

/// Must a resolved status yield to the one the row already carries? A
/// shim reports `needs_input` - the agent is waiting on the USER - and no
/// session file can say that much: the most a harness writes is `idle`
/// (plain `waiting` here) or `busy` (`working`). A question dialog is
/// open mid-turn, so the file says `busy` while the agent is in fact
/// blocked on you; an idle prompt says `idle`. Flattening either over the
/// report on every render is what made rows flip between bands, so a
/// resolved status never overrides a `needs_input` report; only activity
/// in the file dated after that report - the user answered, a new turn -
/// does.
fn keeps_status(a: &Agent, _resolved: &str, last_active_ms: Option<i64>) -> bool {
    a.status == "needs_input" && last_active_ms.unwrap_or(0) <= a.last_status_ms
}

/// Fold one resolver result onto a row: migrate the id first (so enrich
/// lands on the durable row), then persist the resolved fields, then
/// mirror both onto the in-memory `Agent` for this render.
async fn apply(a: &mut Agent, mut r: Resolved) {
    if let Some(real) = r.real_id.as_deref().filter(|id| *id != a.id) {
        migrate_id(a, real).await;
    }
    publish_id(a);
    let now = now_ms() as i64;
    // Did the harness's file move on since the row last saw it? Decided
    // before the row is updated below.
    let grew = r
        .last_active_ms
        .is_some_and(|new| a.last_active_ms.map_or(true, |old| new > old));
    // One decision, taken before either write: a status the row keeps must
    // not be persisted away behind the render's back.
    let status =
        r.status.take().filter(|v| !keeps_status(a, v, r.last_active_ms));
    let _ = store::enrich(
        &a.id,
        r.name.as_deref(),
        status.as_deref(),
        r.started_ms,
        r.last_active_ms,
        r.source_path.as_deref(),
        now,
    )
    .await;
    if let Some(v) = r.name {
        // Mirror the store's name_ms rule for this render: a real change
        // stamps now, unless it is the FIRST name and a user name already
        // stands, which must keep winning (stamp it older).
        if a.name.as_deref() != Some(v.as_str()) {
            let first = a.name.as_deref().unwrap_or("").is_empty();
            a.name_ms = Some(if first && a.user_name.is_some() { 0 } else { now });
        }
        a.name = Some(v);
    }
    if let Some(v) = status {
        a.status = v;
    }
    if r.started_ms.is_some() {
        a.started_ms = r.started_ms;
    }
    if r.last_active_ms.is_some() {
        a.last_active_ms = r.last_active_ms;
    }
    if let Some(v) = r.source_path {
        a.source_path = Some(v);
    }
    if let Some(tp) = r.transcript_path {
        if a.transcript_path.as_deref() != Some(tp.as_str()) {
            let _ = store::set_transcript(&a.id, &tp).await;
            a.transcript_path = Some(tp);
            // The conversation so far: a harness with no hooks (a Codex
            // found by the scan) would otherwise be read only when it
            // ends. Debounced, off this path.
            transcript::request(a.id.clone(), false);
        } else if a.kind == "codex" && grew && a.live() {
            // Codex has no shim to say a turn ended; its rollout's mtime
            // moving is the only word of new turns. The read is cursor-
            // based, so a half-written turn is finished on the next one.
            // Claude is left to its hooks: its file changes all through
            // a turn, and its turn-end report already triggers the read.
            transcript::request(a.id.clone(), false);
        }
    }
    if let Some(cwd) = r.cwd {
        if a.cwd.as_deref() != Some(cwd.as_str()) {
            let _ = store::set_cwd(&a.id, &cwd).await;
            a.cwd = Some(cwd);
        }
    }
}

thread_local! {
    /// Per pane, the id last written to its `@agent_id` option, so a
    /// render that changes nothing costs no host call.
    static PUBLISHED_ID: std::cell::RefCell<HashMap<u32, String>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Put the durable id on the pane as `@agent_id`, so a format can name
/// the agent in a pane: `skill -t <pane> show mailbox` prints it in the
/// guide's live block. A provisional id is not published; nothing can
/// address it. Needs `write-options`; without it this is a no-op.
fn publish_id(a: &Agent) {
    let Some(pane) = a.pane else { return };
    let pane = pane as u32;
    if a.id.starts_with("prov-") {
        return;
    }
    let same = PUBLISHED_ID.with(|m| m.borrow().get(&pane) == Some(&a.id));
    if same {
        return;
    }
    if set_option_in(OptionTarget::Pane(PaneId(pane)), "@agent_id", &a.id).is_ok() {
        PUBLISHED_ID.with(|m| m.borrow_mut().insert(pane, a.id.clone()));
    }
}

/// The agent left the pane: clear `@agent_id` so a shell that follows it
/// there is not mistaken for the agent.
fn unpublish_id(pane: u32) {
    let had = PUBLISHED_ID.with(|m| m.borrow_mut().remove(&pane).is_some());
    if had {
        let _ = set_option_in(OptionTarget::Pane(PaneId(pane)), "@agent_id", "");
    }
}

thread_local! {
    /// Id migrations already reported, so a failure that repeats on every
    /// render is said once rather than a thousand times.
    static MIGRATE_LOGGED: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

/// Move a row from a provisional id to the durable one. When the durable
/// id already has a row (a resumed session), merge into it; otherwise
/// rename in place.
///
/// The in-memory id follows ONLY a move the store took. It used to follow
/// unconditionally, which turned any refused move into a silent one: the
/// row kept its old id on disk while everything here addressed the new
/// one, so the name, the status, an ack and a rename all updated zero rows
/// and reported success. The row stayed nameless forever. Keeping the
/// provisional id costs nothing - it addresses a real row, and the next
/// render tries the move again.
async fn migrate_id(a: &mut Agent, real: &str) {
    let exists = store::id_exists(real).await.unwrap_or(false);
    let moved = match (exists, a.pane) {
        (true, Some(pane)) => {
            store::merge_id(
                &a.id,
                real,
                pane,
                a.session.as_deref(),
                a.window.as_deref(),
                now_ms() as i64,
            )
            .await
        }
        // A durable row exists but this one has no pane to give it: there
        // is nothing to merge, and the provisional id still addresses a
        // real row.
        (true, None) => return,
        (false, _) => store::rename_id(&a.id, real).await,
    };
    match moved {
        Ok(()) => {
            transcript::on_rename(&a.id, real);
            a.id = real.to_string();
        }
        Err(e) => {
            let key = format!("{}\u{1}{real}", a.id);
            let first = MIGRATE_LOGGED.with(|s| s.borrow_mut().insert(key));
            if first {
                log(&format!(
                    "agents: {} -> {real}: {}; keeping the provisional id",
                    a.id, e.message
                ));
            }
        }
    }
}

/// The `identify` verb: a pi/opencode hook reports its durable id and
/// session file for a pane. Migrate the pane's live row to that id and
/// record the source, so the next render can date it.
pub async fn on_identify(
    pane: u32,
    id: String,
    source: Option<String>,
    cfg: Rc<Config>,
) {
    if !owns_store() {
        return;
    }
    // The hook can beat detection (a fresh codex pane whose command has not
    // changed to `node` yet). Discover the pane first, so the id lands.
    if store::live_by_pane(pane as i64).await.ok().flatten().is_none() {
        classify(pane, cfg).await;
    }
    let Ok(Some(mut a)) = store::live_by_pane(pane as i64).await else {
        return;
    };
    if id != a.id {
        migrate_id(&mut a, &id).await;
    }
    let _ = store::enrich(
        &a.id,
        None,
        None,
        None,
        None,
        source.as_deref(),
        now_ms() as i64,
    )
    .await;
}

/// A shim's status report for a pane.
pub async fn report(pane: u32, status: String, task: Option<String>, cfg: Rc<Config>) {
    if !owns_store() {
        return;
    }
    let now = now_ms() as i64;
    if status == "done" {
        let live = store::live_by_pane(pane as i64).await.ok().flatten();
        if let Some(a) = &live {
            if let Some(text) = capture_tail(pane) {
                let _ = store::save_capture(&a.id, &text).await;
            }
        }
        let _ = store::finish_by_pane(pane as i64, now).await;
        // The agent is done: read the rest of its transcript, then
        // snapshot the index.
        if let Some(a) = live {
            transcript::request(a.id, true);
        }
    } else {
        // The trailing text of a report means different things either
        // side of `needs_input`. On a working/waiting report it is the
        // task - what the agent is doing. On `needs_input` it is the
        // harness's own reason for wanting the user (the Claude
        // `Notification` message, a permission prompt's subject), which
        // belongs on the row only while the agent is blocked, so it goes
        // to `note` and the next report clears it.
        let (task, note) = if status == "needs_input" {
            (None, task)
        } else {
            (task, None)
        };
        let n = store::set_status(
            pane as i64,
            &status,
            task.as_deref(),
            note.as_deref(),
            now,
        )
        .await
        .unwrap_or(0);
        if n == 0 {
            // The shim beat classify to it; discover the pane, then retry.
            classify(pane, Rc::clone(&cfg)).await;
            let _ = store::set_status(
                pane as i64,
                &status,
                task.as_deref(),
                note.as_deref(),
                now,
            )
            .await;
        }
        // A `working` report is a new turn - the user messaged the agent -
        // so bring an archived row back into the roster.
        if status == "working" {
            let _ = store::unarchive_by_pane(pane as i64).await;
        } else {
            // The turn is over (`waiting`, `needs_input`): the transcript
            // holds all of it. Read what is new, off this handler.
            spawn(transcript::request_pane(pane));
        }
    }
}

/// End the live agent on a pane, then read the rest of its transcript:
/// the file outlives the pane, and this is what makes a killed agent
/// searchable. The read runs off the caller's path.
async fn end_pane(pane: u32, now: i64, reason: &str) {
    unpublish_id(pane);
    let live = store::live_by_pane(pane as i64).await.ok().flatten();
    // A row that never got its durable id - the picker never rendered
    // while it lived, and rendering is what resolves - gets one last
    // look at its session file now, while that may still exist. The id
    // is what bringing the agent back needs, and what the transcript is
    // found by.
    let live = match live {
        Some(a) if a.id.starts_with("prov-") => {
            let mut rows = vec![a];
            enrich_live(&mut rows).await;
            rows.pop()
        }
        other => other,
    };
    let _ = store::end_by_pane(pane as i64, now, reason).await;
    if let Some(a) = live {
        if a.transcript_path.is_some() {
            transcript::request(a.id, true);
        }
    }
}

/// A pane went away: its agent is done.
pub async fn pane_gone(pane: u32) {
    if !owns_store() {
        return;
    }
    end_pane(pane, now_ms() as i64, "closed").await;
}

thread_local! {
    /// The store belongs to another server: this instance must not
    /// retire or create rows. See `claim_store`.
    static FOREIGN_STORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The setting that names the server a store belongs to.
const SOCKET_KEY: &str = "server_socket";

/// Is this store ours to write? The store lives in the plugin's data
/// directory, which every tmux server of this user shares, while pane
/// ids are per server: a second server (a scratch one started with the
/// same config) would see none of the first's panes, retire its every
/// live row, and then mint rows for its own panes under ids that
/// collide. So the first server to use a store writes its socket path
/// into it, and a server with a different socket leaves the store
/// alone: no sweep, no rows, a line in the log. The claim moves only
/// when the store is empty of live rows (the old server is gone).
async fn claim_store() -> bool {
    let socket = format_expand(OptionTarget::Server, "#{socket_path}")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(socket) = socket else { return true };
    let owner = store::get_setting(SOCKET_KEY).await.ok().flatten();
    match owner {
        Some(o) if o == socket => true,
        Some(o) => {
            let live = store::live_agents().await.map(|v| v.len()).unwrap_or(0);
            if live == 0 {
                let _ = store::set_setting(SOCKET_KEY, &socket).await;
                true
            } else {
                log(&format!(
                    "agents: the store belongs to the server at {o}; this one ({socket}) \
                     leaves it alone - start scratch servers with -f /dev/null"
                ));
                FOREIGN_STORE.with(|f| f.set(true));
                false
            }
        }
        None => {
            let _ = store::set_setting(SOCKET_KEY, &socket).await;
            true
        }
    }
}

/// Does this instance own the store? False on a server that found the
/// store claimed by another; every write path checks it.
pub fn owns_store() -> bool {
    !FOREIGN_STORE.with(|f| f.get())
}

/// On start (including after restart-server): rediscover agents in live
/// panes, retire live rows whose pane is gone, and prune old history.
pub async fn reconcile(cfg: Rc<Config>) {
    if !claim_store().await {
        return;
    }
    // The index first, so nothing ingested below is missed by it.
    transcript::load().await;
    let panes = list_panes().unwrap_or_default();
    for p in &panes {
        classify(p.id, Rc::clone(&cfg)).await;
    }
    sweep_gone().await;
    let pruned = store::prune(cfg.keep_days, cfg.history_days, now_ms() as i64)
        .await
        .unwrap_or(0);
    if pruned > 0 {
        // Their turns went with them; the index must not keep answering
        // for agents that are gone.
        transcript::rebuild().await;
    }
    // Transcripts that grew while the server was down (an agent that died
    // meanwhile has a tail nobody read).
    spawn(transcript::catch_up());
}

/// Retire live rows whose pane no longer exists. Returns how many went.
/// The event path can still lose a race (a pane that dies while its
/// classify awaits, after the liveness check), so this also runs on a
/// timer; see `SWEEP_MS`.
pub async fn sweep_gone() -> usize {
    if !owns_store() {
        return 0;
    }
    // A failed listing is not an empty server: nothing is known to be
    // gone, so nothing goes.
    let Ok(panes) = list_panes() else { return 0 };
    let live = store::unended().await.unwrap_or_default();
    let now = now_ms() as i64;
    let mut gone = 0;
    for a in live {
        if let Some(pane) = a.pane {
            if !panes.iter().any(|p| p.id as i64 == pane) {
                let _ = store::end_by_pane(pane, now, "gone").await;
                if a.transcript_path.is_some() {
                    transcript::request(a.id.clone(), true);
                }
                gone += 1;
            }
        }
    }
    gone
}

/// How often the provider sweeps for rows whose pane is gone.
pub const SWEEP_MS: u64 = 30_000;

/// The periodic sweep: forever, while the instance lives.
pub async fn sweep_loop() {
    loop {
        if sleep_ms(SWEEP_MS).await.is_err() {
            return;
        }
        if sweep_gone().await > 0 {
            broadcast_changed().await;
        }
    }
}

// ---------------------------------------------------------------------------
// content search over this server's grids
// ---------------------------------------------------------------------------

/// Pick the matcher from the query, fff-style: a query with regex
/// metacharacters is a regex; anything else is a plain substring. The
/// caller falls back to fuzzy when the chosen matcher finds nothing.
pub fn detect_mode(q: &str) -> SearchMode {
    const META: &[char] =
        &['^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\'];
    if q.chars().any(|c| META.contains(&c)) {
        SearchMode::Regex
    } else {
        SearchMode::Plain
    }
}

pub fn mode_label(m: SearchMode) -> &'static str {
    match m {
        SearchMode::Plain => "plain",
        SearchMode::Regex => "regex",
        SearchMode::Fuzzy => "fuzzy",
    }
}

fn mode_from_label(s: &str) -> SearchMode {
    match s {
        "regex" => SearchMode::Regex,
        "fuzzy" => SearchMode::Fuzzy,
        _ => SearchMode::Plain,
    }
}

/// Run content search over the live panes and the saved captures:
/// auto-detect the matcher, and fall back to fuzzy when it finds nothing
/// anywhere (an unmatched query, or a half-typed regex that will not
/// compile). Returns the mode that actually produced the hits, the
/// snippet per matching pane, and the snippet per matching capture (by
/// whatever the captures were keyed with).
///
/// The grids are grepped in tmux (`panes_search`); a capture is text this
/// side already holds, so it is grepped here, by [`text_search`] - which
/// has no regex matcher, so a regex query only ever hits a grid.
pub fn run_content_search(
    panes: &[PaneId],
    captures: &[(String, String)],
    needle: &str,
) -> (SearchMode, HashMap<u32, String>, HashMap<String, String>) {
    let grid = |mode: SearchMode| -> HashMap<u32, String> {
        panes_search(panes, needle, mode, false, SEARCH_LINES)
            .unwrap_or_default()
            .into_iter()
            .map(|h: SearchHit| (h.pane.0, h.snippet))
            .collect()
    };
    let text = |fuzzy: bool| -> HashMap<String, String> {
        captures
            .iter()
            .filter_map(|(k, t)| text_search(t, needle, fuzzy).map(|s| (k.clone(), s)))
            .collect()
    };
    let mode = detect_mode(needle);
    let (g, t) = (grid(mode), text(false));
    if !g.is_empty() || !t.is_empty() {
        return (mode, g, t);
    }
    let (g, t) = (grid(SearchMode::Fuzzy), text(true));
    if !g.is_empty() || !t.is_empty() {
        return (SearchMode::Fuzzy, g, t);
    }
    (mode, HashMap::new(), HashMap::new())
}

/// Grep a saved capture the way the grid search does, minus regex: a
/// case-insensitive substring, or with `fuzzy` an in-order subsequence.
/// The first matching line is the snippet.
pub fn text_search(text: &str, needle: &str, fuzzy: bool) -> Option<String> {
    let n = needle.to_lowercase();
    if n.is_empty() {
        return None;
    }
    let hit = |line: &str| {
        let l = line.to_lowercase();
        if fuzzy {
            let mut it = l.chars();
            n.chars().all(|c| it.any(|h| h == c))
        } else {
            l.contains(&n)
        }
    };
    text.lines()
        .find(|l| !l.trim().is_empty() && hit(l))
        .map(|l| l.trim_end().to_string())
}

/// Content search over this server's agents, keyed by agent id, for a
/// view elsewhere: the live grids, plus (when asked) the saved captures
/// of archived agents whose pane is gone.
async fn search_local(req: &SearchReq) -> SearchReply {
    let rows = store::live_agents().await.unwrap_or_default();
    let panes: Vec<PaneId> = rows
        .iter()
        .filter_map(|a| a.pane.map(|p| PaneId(p as u32)))
        .collect();
    let captures = if req.archived {
        store::archived_captures().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let (mode, by_pane, by_id) = run_content_search(&panes, &captures, &req.needle);
    let mut hits: HashMap<String, String> = rows
        .into_iter()
        .filter_map(|a| {
            let pane = a.pane? as u32;
            by_pane.get(&pane).map(|s| (a.id, s.clone()))
        })
        .collect();
    hits.extend(by_id);
    let (transcript_hits, agents) = if req.transcript && !req.needle.trim().is_empty() {
        let th = transcript::hits_with_snippets(&req.needle, transcript::HITS_MAX).await;
        let ids: Vec<String> = th.iter().map(|h| h.id.clone()).collect();
        (th, store::by_ids(&ids).await.unwrap_or_default())
    } else {
        (Vec::new(), Vec::new())
    };
    SearchReply { mode: mode_label(mode).to_string(), hits, transcript: transcript_hits, agents }
}

/// An agent is being archived: save the pane's text, so the archived
/// agent keeps a preview and a searchable body once its pane is gone
/// (`done` saves one too; this covers a pane later killed or lost
/// without ever reporting), and read its transcript, so what it talked
/// about is searchable from the moment it is set aside. Both on the
/// local `a` path and the remote `act archive`.
pub async fn on_archive(id: &str) {
    let Ok(Some(a)) = store::by_id(id).await else { return };
    if let Some(pane) = a.pane.filter(|_| a.live()) {
        if let Some(text) = capture_tail(pane as u32) {
            let _ = store::save_capture(&a.id, &text).await;
        }
    }
    if a.transcript_path.is_some() {
        let ended = !a.live();
        transcript::request(a.id, ended);
    }
}

/// [`SearchReply::mode`] back into a [`SearchMode`].
pub fn reply_mode(reply: &SearchReply) -> SearchMode {
    mode_from_label(&reply.mode)
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------

/// Register the methods a view calls. Needs `service-serve`; without it
/// the plugin still works alone on its server.
pub fn register_services() {
    for m in ["list", "capture", "search", "turns", "stats", "act"] {
        if let Err(e) = service::register(m) {
            log(&format!("agents: register {m}: {}", e.message));
            return;
        }
    }
}

/// The current roster, enriched from the session files, plus what
/// `req` asks for beyond it.
pub async fn snapshot(req: ListReq) -> Snapshot {
    let mut rows = store::live_agents().await.unwrap_or_default();
    enrich_live(&mut rows).await;
    if req.archived {
        // Every archived row, including the live ones (which the live
        // set above already holds, minus the archived - see the store).
        rows.extend(store::archived().await.unwrap_or_default());
    } else if req.history {
        rows.extend(store::history(crate::HISTORY_MAX).await.unwrap_or_default());
    }
    Snapshot { now_ms: now_ms() as i64, agents: rows }
}

/// Tell every subscribed view that the roster changed. Cheap (no
/// enrich): the rows as stored, with the clock.
pub async fn broadcast_changed() {
    let rows = store::live_agents().await.unwrap_or_default();
    let snap = Snapshot { now_ms: now_ms() as i64, agents: rows };
    let _ = service::emit_json(TOPIC, &snap);
}

/// Answer one service request from a view.
pub async fn handle(req: ServiceRequest, cfg: Rc<Config>) {
    match req.method.as_str() {
        "list" => {
            let q: ListReq = req.json().unwrap_or_default();
            let snap = snapshot(q).await;
            let _ = req.reply_json(&snap);
        }
        "capture" => {
            let Ok(q) = req.json::<CaptureReq>() else {
                let _ = req.fail("capture: bad request");
                return;
            };
            let live = store::by_id(&q.id).await.ok().flatten();
            let text = match live.as_ref().filter(|a| a.live()).and_then(|a| a.pane) {
                Some(pane) => capture_tail(pane as u32),
                None => store::get_capture(&q.id).await.ok().flatten(),
            };
            match text {
                Some(t) => {
                    let _ = req.reply(t.as_bytes());
                }
                None => {
                    let _ = req.fail("no capture");
                }
            }
        }
        "search" => {
            let Ok(q) = req.json::<SearchReq>() else {
                let _ = req.fail("search: bad request");
                return;
            };
            let reply = search_local(&q).await;
            let _ = req.reply_json(&reply);
        }
        "turns" => {
            let Ok(q) = req.json::<TurnsReq>() else {
                let _ = req.fail("turns: bad request");
                return;
            };
            let turns: Vec<TurnRow> =
                store::turns_range(&q.id, q.from, q.to).await.unwrap_or_default();
            let _ = req.reply_json(&turns);
        }
        "stats" => {
            let Ok(q) = req.json::<StatsReq>() else {
                let _ = req.fail("stats: bad request");
                return;
            };
            let st = store::stats(&q.id).await.unwrap_or_default();
            let _ = req.reply_json(&st);
        }
        "act" => {
            let Ok(q) = req.json::<ActReq>() else {
                let _ = req.fail("act: bad request");
                return;
            };
            let now = now_ms() as i64;
            let ok = match q.verb.as_str() {
                "ack" => store::acknowledge(&q.id, now).await.is_ok(),
                "unack" => store::unacknowledge(&q.id, now).await.is_ok(),
                "archive" => {
                    on_archive(&q.id).await;
                    store::set_life(&q.id, "archived").await.is_ok()
                }
                "unarchive" => store::set_life(&q.id, "active").await.is_ok(),
                "rename" => {
                    let n = q.name.as_deref().filter(|s| !s.is_empty());
                    store::rename_by_user(&q.id, n, now).await.is_ok()
                }
                // The view moved a row between the attention band and
                // `waiting` by hand. Only those two: a view has no
                // business declaring an agent to be mid-turn or finished,
                // which the pane and the shims say.
                "status" => match q.name.as_deref() {
                    Some(v @ ("needs_input" | "waiting")) => {
                        store::set_status_by_id(&q.id, v, now).await.is_ok()
                    }
                    _ => false,
                },
                _ => false,
            };
            if ok {
                let _ = req.reply(b"ok");
                broadcast_changed().await;
            } else {
                let _ = req.fail(&format!("act: {} failed", q.verb));
            }
        }
        other => {
            let _ = req.fail(&format!("unknown method {other}"));
        }
    }
    let _ = cfg;
}

#[cfg(test)]
mod title_tests {
    use super::clean_title;

    #[test]
    fn codex_topic_loses_its_directory_suffix() {
        assert_eq!(
            clean_title("Inspect Docker image builds | tab", "codex", "mbp", "mbp.local", "/Users/z/Code/tab"),
            Some("Inspect Docker image builds")
        );
    }

    #[test]
    fn a_bare_directory_is_no_name() {
        let cwd = "/Users/z/Code/tab/";
        assert_eq!(clean_title("tab", "codex", "mbp", "mbp.local", cwd), None);
        assert_eq!(clean_title("/Users/z/Code/tab", "codex", "mbp", "mbp.local", cwd), None);
        assert_eq!(clean_title("~/Code/tab", "codex", "mbp", "mbp.local", cwd), None);
        assert_eq!(clean_title("codex | tab", "codex", "mbp", "mbp.local", cwd), None);
        assert_eq!(clean_title("Codex", "codex", "mbp", "mbp.local", cwd), None);
    }

    #[test]
    fn host_and_empty_titles_are_rejected() {
        assert_eq!(clean_title("", "claude", "mbp", "mbp.local", "/x"), None);
        assert_eq!(clean_title("mbp", "claude", "mbp", "mbp.local", "/x"), None);
        assert_eq!(clean_title("mbp.local", "claude", "mbp", "mbp.local", "/x"), None);
    }

    #[test]
    fn a_topic_that_mentions_the_directory_survives() {
        assert_eq!(
            clean_title("✳ fix the tab build", "claude", "mbp", "mbp.local", "/Users/z/Code/tab"),
            Some("✳ fix the tab build")
        );
        assert_eq!(clean_title("tab | other", "codex", "mbp", "mbp.local", "/Users/z/Code/tab"), Some("tab | other"));
        assert_eq!(clean_title("topic", "codex", "mbp", "mbp.local", ""), Some("topic"));
    }
}
