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
//!   search  { needle, archived }     -> SearchReply (hits per agent id)
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
use crate::store::{self, Agent};
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
}

/// Content-search hits, by agent id, with the matcher that found them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchReply {
    pub mode: String,
    pub hits: HashMap<String, String>,
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
/// running command). Used as the provisional display name until a
/// resolver supplies the harness's own name.
pub fn pane_title(pane: u32, kind: &str) -> Option<String> {
    // The default pane title is the host name (an OSC title the shell
    // never set), which is no better than the kind. Reject it, as
    // notify-toast does, so only a title an agent actually wrote survives.
    let expanded = format_expand(
        OptionTarget::Pane(PaneId(pane)),
        "#{pane_title}\t#{host_short}\t#{host}",
    )
    .ok()?;
    let mut parts = expanded.splitn(3, '\t');
    let title = parts.next().unwrap_or("").trim();
    let host_short = parts.next().unwrap_or("");
    let host = parts.next().unwrap_or("");
    if title.is_empty()
        || title.eq_ignore_ascii_case(kind)
        || title == host_short
        || title == host
    {
        return None;
    }
    Some(title.to_string())
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
            let _ = store::end_by_pane(pane as i64, now, "closed").await;
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
        // its kin write it there), which beats the session file's slug.
        // Prefer it; keep the harness name only as a fallback. Codex is the
        // exception: its pane title is just the cwd, so prefer the
        // resolver's nickname there.
        let title = if a.kind == "codex" {
            None
        } else {
            a.pane.and_then(|p| pane_title(p as u32, &a.kind))
        };
        r.name = title.or_else(|| r.name.take());
        apply(a, r).await;
    }
}

/// Must a resolved status yield to the one the row already carries? A
/// shim reports `needs_input` - an idle turn that is waiting on the USER -
/// and no session file can say that much: the most a harness writes is
/// `idle`, which reads here as plain `waiting`. Flattening one into the
/// other on every render is what made a row flip between the two bands, so
/// a resolved `waiting` never overrides a `needs_input` report; only
/// activity in the file dated after that report - a new turn - does.
fn keeps_status(a: &Agent, resolved: &str, last_active_ms: Option<i64>) -> bool {
    a.status == "needs_input"
        && resolved == "waiting"
        && last_active_ms.unwrap_or(0) <= a.last_status_ms
}

/// Fold one resolver result onto a row: migrate the id first (so enrich
/// lands on the durable row), then persist the resolved fields, then
/// mirror both onto the in-memory `Agent` for this render.
async fn apply(a: &mut Agent, mut r: Resolved) {
    if let Some(real) = r.real_id.as_deref().filter(|id| *id != a.id) {
        migrate_id(a, real).await;
    }
    let now = now_ms() as i64;
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
        Ok(()) => a.id = real.to_string(),
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
    let now = now_ms() as i64;
    if status == "done" {
        if let Ok(Some(a)) = store::live_by_pane(pane as i64).await {
            if let Some(text) = capture_tail(pane) {
                let _ = store::save_capture(&a.id, &text).await;
            }
        }
        let _ = store::finish_by_pane(pane as i64, now).await;
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
        }
    }
}

/// A pane went away: its agent is done.
pub async fn pane_gone(pane: u32) {
    let _ = store::end_by_pane(pane as i64, now_ms() as i64, "closed").await;
}

/// On start (including after restart-server): rediscover agents in live
/// panes, retire live rows whose pane is gone, and prune old history.
pub async fn reconcile(cfg: Rc<Config>) {
    let panes = list_panes().unwrap_or_default();
    for p in &panes {
        classify(p.id, Rc::clone(&cfg)).await;
    }
    sweep_gone().await;
    let _ = store::prune(cfg.keep_days, now_ms() as i64).await;
}

/// Retire live rows whose pane no longer exists. Returns how many went.
/// The event path can still lose a race (a pane that dies while its
/// classify awaits, after the liveness check), so this also runs on a
/// timer; see `SWEEP_MS`.
pub async fn sweep_gone() -> usize {
    let panes = list_panes().unwrap_or_default();
    let live = store::unended().await.unwrap_or_default();
    let now = now_ms() as i64;
    let mut gone = 0;
    for a in live {
        if let Some(pane) = a.pane {
            if !panes.iter().any(|p| p.id as i64 == pane) {
                let _ = store::end_by_pane(pane, now, "gone").await;
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
    SearchReply { mode: mode_label(mode).to_string(), hits }
}

/// Save the pane's text before an archive, so the archived agent keeps a
/// preview and a searchable body once its pane is gone. `done` saves one
/// too; this covers a pane that is later killed or lost without ever
/// reporting. A row with no live pane is left as it is.
pub async fn capture_on_archive(id: &str) {
    let Ok(Some(a)) = store::by_id(id).await else { return };
    let Some(pane) = a.pane.filter(|_| a.live()) else { return };
    if let Some(text) = capture_tail(pane as u32) {
        let _ = store::save_capture(&a.id, &text).await;
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
    for m in ["list", "capture", "search", "act"] {
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
                    capture_on_archive(&q.id).await;
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
