//! The provider half: everything that looks at THIS server's machine.
//!
//! Detection reads pane commands and environments, the resolvers read the
//! harnesses' session files, the roster lives in this server's store, and
//! captures and content search read this server's grids. None of that
//! crosses a link, so the provider runs on every server (pushed there by a
//! `remote-attach` link) and a view anywhere asks it through services:
//!
//!   list    { history }              -> Snapshot (enriched rows + clock)
//!   capture { id }                   -> the pane's text, or the saved one
//!   search  { needle, mode, lines }  -> SearchReply (hits per agent id)
//!   act     { id, verb, name? }      -> "ok" (ack | archive | unarchive | rename)
//!
//! and follows the `changed` topic, which carries a fresh Snapshot after
//! every write. Times are the provider's clock; `now_ms` in the Snapshot
//! lets a view on another machine correct for skew.

use std::collections::HashMap;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListReq {
    #[serde(default)]
    pub history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReq {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchReq {
    pub needle: String,
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

/// The agent kind a pane runs, or None. Command name first (reliable,
/// and the only signal Codex offers), then the environment markers.
fn detect(pane: u32, commands: &[String]) -> Option<String> {
    if let Ok(cmd) =
        format_expand(OptionTarget::Pane(PaneId(pane)), "#{pane_current_command}")
    {
        let base = cmd.rsplit('/').next().unwrap_or(&cmd);
        if let Some(k) = commands.iter().find(|c| c.as_str() == base) {
            return Some(k.clone());
        }
        // An interpreter-wrapped CLI: match the basename of the launched
        // script (the `_` var), not the interpreter. Gated to interpreters
        // so an idle shell's stale `_` cannot trip a false positive.
        if INTERPRETERS.contains(&base) {
            if let Ok(Some(under)) = pane_env(PaneId(pane), "_") {
                let ubase = under.rsplit('/').next().unwrap_or(&under);
                if let Some(k) = commands.iter().find(|c| c.as_str() == ubase)
                {
                    return Some(k.clone());
                }
            }
        }
    }
    if let Some(v) = marker(pane, "AI_AGENT") {
        let v = v.to_lowercase();
        for k in ["claude", "codex", "opencode", "pi"] {
            if v.contains(k) {
                return Some(k.to_string());
            }
        }
    }
    if marker(pane, "OPENCODE").is_some() {
        return Some("opencode".into());
    }
    None
}

/// An environment marker of the pane's foreground process, unless the
/// pane inherited it. Claude Code sets `AI_AGENT` for its children, so a
/// tmux server started from inside Claude Code hands the variable to
/// every pane it spawns, and an idle shell or a `sleep` would look like
/// an agent. The `#{NAME}` format reads the session and global
/// environment, which is what the pane's shell got; a value equal to
/// that is inherited and proves nothing.
fn marker(pane: u32, name: &str) -> Option<String> {
    let value = pane_env(PaneId(pane), name).ok()??;
    let inherited =
        format_expand(OptionTarget::Pane(PaneId(pane)), &format!("#{{{name}}}")).ok()?;
    if inherited == value {
        return None;
    }
    Some(value)
}

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
    let Some(kind) = detect(pane, &cfg.commands) else {
        let _ = store::end_by_pane(pane as i64, now, "closed").await;
        return;
    };
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

/// Fold one resolver result onto a row: migrate the id first (so enrich
/// lands on the durable row), then persist the resolved fields, then
/// mirror both onto the in-memory `Agent` for this render.
async fn apply(a: &mut Agent, r: Resolved) {
    if let Some(real) = r.real_id.as_deref().filter(|id| *id != a.id) {
        migrate_id(a, real).await;
    }
    let now = now_ms() as i64;
    let _ = store::enrich(
        &a.id,
        r.name.as_deref(),
        r.status.as_deref(),
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
    if let Some(v) = r.status {
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

/// Move a row from a provisional id to the durable one. When the durable
/// id already has a row (a resumed session), merge into it; otherwise
/// rename in place. Updates the in-memory id either way.
async fn migrate_id(a: &mut Agent, real: &str) {
    let exists = store::id_exists(real).await.unwrap_or(false);
    if exists {
        if let Some(pane) = a.pane {
            let _ = store::merge_id(
                &a.id,
                real,
                pane,
                a.session.as_deref(),
                a.window.as_deref(),
                now_ms() as i64,
            )
            .await;
        }
    } else {
        let _ = store::rename_id(&a.id, real).await;
    }
    a.id = real.to_string();
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
        let n = store::set_status(pane as i64, &status, task.as_deref(), now)
            .await
            .unwrap_or(0);
        if n == 0 {
            // The shim beat classify to it; discover the pane, then retry.
            classify(pane, Rc::clone(&cfg)).await;
            let _ =
                store::set_status(pane as i64, &status, task.as_deref(), now).await;
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
    let live = store::live_agents().await.unwrap_or_default();
    let now = now_ms() as i64;
    for a in live {
        if let Some(pane) = a.pane {
            if !panes.iter().any(|p| p.id as i64 == pane) {
                let _ = store::end_by_pane(pane, now, "gone").await;
            }
        }
    }
    let _ = store::prune(cfg.keep_days, now).await;
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

/// Run content search over the live panes: auto-detect the matcher, and
/// fall back to fuzzy when it finds nothing (an unmatched query, or a
/// half-typed regex that will not compile). Returns the mode that
/// actually produced the hits and the snippet per matching pane.
pub fn run_content_search(
    panes: &[PaneId],
    needle: &str,
) -> (SearchMode, HashMap<u32, String>) {
    let collect = |hits: Vec<SearchHit>| -> HashMap<u32, String> {
        hits.into_iter().map(|h| (h.pane.0, h.snippet)).collect()
    };
    let mode = detect_mode(needle);
    let hits = panes_search(panes, needle, mode, false, SEARCH_LINES)
        .unwrap_or_default();
    if !hits.is_empty() {
        return (mode, collect(hits));
    }
    let fz = panes_search(panes, needle, SearchMode::Fuzzy, false, SEARCH_LINES)
        .unwrap_or_default();
    if !fz.is_empty() {
        return (SearchMode::Fuzzy, collect(fz));
    }
    (mode, HashMap::new())
}

/// Content search over this server's live agents, keyed by agent id, for
/// a view elsewhere.
async fn search_local(needle: &str) -> SearchReply {
    let rows = store::live_agents().await.unwrap_or_default();
    let panes: Vec<PaneId> = rows
        .iter()
        .filter_map(|a| a.pane.map(|p| PaneId(p as u32)))
        .collect();
    let (mode, by_pane) = run_content_search(&panes, needle);
    let hits = rows
        .into_iter()
        .filter_map(|a| {
            let pane = a.pane? as u32;
            by_pane.get(&pane).map(|s| (a.id, s.clone()))
        })
        .collect();
    SearchReply { mode: mode_label(mode).to_string(), hits }
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

/// The current roster, enriched from the session files.
pub async fn snapshot(history: bool) -> Snapshot {
    let mut rows = store::live_agents().await.unwrap_or_default();
    enrich_live(&mut rows).await;
    if history {
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
            let snap = snapshot(q.history).await;
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
            let reply = search_local(&q.needle).await;
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
                "archive" => store::set_life(&q.id, "archived").await.is_ok(),
                "unarchive" => store::set_life(&q.id, "active").await.is_ok(),
                "rename" => {
                    let n = q.name.as_deref().filter(|s| !s.is_empty());
                    store::rename_by_user(&q.id, n, now).await.is_ok()
                }
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
