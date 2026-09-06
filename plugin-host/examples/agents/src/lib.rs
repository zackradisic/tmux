//! A roster of the agent CLI sessions running in your panes.
//!
//! Press the hotkey (bind it to `plugin-command agents pick`) to open a
//! floating chooser: one row per agent, grouped by state (needs input,
//! waiting, working, done) and, inside each group, most recently active
//! first. A live preview of the highlighted pane sits to the right.
//! `j`/`k` move, Enter jumps, `A` archives, `h` folds in the finished
//! ones, and `/` starts a filter (Enter accepts it, Esc cancels).
//!
//! Three signals drive it, each from its own trusted source:
//!
//!   * Membership + liveness is OBSERVED, never announced. A pane is an
//!     agent when its foreground command is one of `claude|codex|pi|
//!     opencode` (configurable) or it carries an `AI_AGENT`/`OPENCODE`
//!     marker in its environment (read with the `pane_env` host call).
//!     The plugin learns of changes from `pane-command-changed`,
//!     `pane-created` and `pane-destroyed`, so a killed or crashed CLI
//!     retires itself - no hook can leave a ghost behind. This is also
//!     how `done` is decided: the pane dying, not an exit hook.
//!   * Identity is two-phase. A freshly seen agent joins under a
//!     provisional, pane-bound id and its `#{pane_title}` name. A resolver
//!     then reads the harness's OWN session file and migrates the row to
//!     the durable id (see `resolve`), so a `restart-server` and even a
//!     resumed session re-link to the same history.
//!   * Name, real times, and turn state come from that same session file,
//!     read at render time - Claude's `~/.claude/sessions/<pid>.json`,
//!     Codex's open rollout, or the file a pi/opencode `identify` hook
//!     reported. A per-harness shim may still push a finer turn status
//!     through one wire:
//!       plugin-command -t $TMUX_PANE agents "<status> [task...]"
//!     but none is required for Claude or Codex.
//!
//! The roster lives in the plugin's SQLite database (`store.db`), so it
//! survives a restart and stays searchable. `init` re-scans every pane
//! and reconciles the live set against it.
//!
//! Manifest:
//!
//!   [plugins.agents]
//!   path = "agents.wasm"
//!   scope = "server"
//!   caps = ["capture-pane", "run-command", "mode", "db", "env-read",
//!           "pane-fds", "fs-read", "fs-list"]
//!   config = { keep_days = 14 }
//!
//!   [plugins.agents.caps.env-read]
//!   names = ["AI_AGENT", "OPENCODE"]
//!
//!   [plugins.agents.caps.fs-read]
//!   paths = ["~/.claude/sessions", "~/.codex/sessions", "~/.pi",
//!            "~/.local/share/opencode"]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

mod resolve;
mod store;
use resolve::Resolved;
use store::Agent;

const WIDTH: u32 = 110;
const HEIGHT: u32 = 22;
const LIST_MAX: usize = 14;
const HISTORY_MAX: i64 = 100;

/// The commands that mark a pane as an agent, if none are configured.
const DEFAULT_COMMANDS: &[&str] = &["claude", "codex", "pi", "opencode"];
/// The statuses a shim may report.
const STATUSES: &[&str] = &["working", "needs_input", "waiting", "done"];

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentsConfig {
    keep_days: Option<serde_json::Value>,
    commands: Option<Vec<String>>,
    pick_jump: Option<String>,
    pick_filter: Option<String>,
    pick_archive: Option<String>,
    pick_history: Option<String>,
    pick_close: Option<String>,
}

#[derive(Clone)]
struct PickKeys {
    jump: String,
    filter: String,
    archive: String,
    history: String,
    close: String,
}

impl Default for PickKeys {
    fn default() -> Self {
        Self {
            jump: "Enter".into(),
            filter: "/".into(),
            archive: "A".into(),
            history: "h".into(),
            close: "Escape".into(),
        }
    }
}

struct Config {
    keep_days: i64,
    commands: Vec<String>,
    keys: PickKeys,
}

impl Config {
    fn from(c: &AgentsConfig) -> Result<Config, String> {
        let keep_days = match &c.keep_days {
            None => 14,
            Some(v) => {
                let s = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                let n: i64 = s.trim().parse().map_err(|_| format!("bad keep_days {v}"))?;
                if n < 1 {
                    return Err("keep_days must be at least 1".into());
                }
                n
            }
        };
        let commands = match &c.commands {
            Some(v) if !v.is_empty() => v.clone(),
            _ => DEFAULT_COMMANDS.iter().map(|s| s.to_string()).collect(),
        };
        let d = PickKeys::default();
        let pick = |v: &Option<String>, def: String| {
            v.as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or(def)
        };
        Ok(Config {
            keep_days,
            commands,
            keys: PickKeys {
                jump: pick(&c.pick_jump, d.jump),
                filter: pick(&c.pick_filter, d.filter),
                archive: pick(&c.pick_archive, d.archive),
                history: pick(&c.pick_history, d.history),
                close: pick(&c.pick_close, d.close),
            },
        })
    }
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
    }
    if let Ok(Some(v)) = pane_env(PaneId(pane), "AI_AGENT") {
        let v = v.to_lowercase();
        for k in ["claude", "codex", "opencode", "pi"] {
            if v.contains(k) {
                return Some(k.to_string());
            }
        }
    }
    if let Ok(Some(_)) = pane_env(PaneId(pane), "OPENCODE") {
        return Some("opencode".into());
    }
    None
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
fn pane_title(pane: u32, kind: &str) -> Option<String> {
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

fn capture_tail(pane: u32) -> Option<String> {
    capture_pane(PaneId(pane), None, None).ok()
}

/// Classify a pane and reconcile the roster for it: create/revive/rebind
/// a live agent, or retire the one that was there if it is no longer an
/// agent.
async fn classify(pane: u32, cfg: Rc<Config>) {
    let now = now_ms() as i64;
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
async fn enrich_live(rows: &mut [Agent]) {
    // Claude needs one directory read for the whole set; index it up front.
    let claude = resolve::claude_index().await;
    for a in rows.iter_mut().filter(|a| a.live()) {
        let mut r = match a.kind.as_str() {
            "claude" => a
                .pane
                .and_then(|p| claude.get(&(p as u32)))
                .map(clone_resolved),
            "codex" => resolve::codex(a).await,
            _ => resolve::from_source(a).await,
        }
        .unwrap_or_default();
        // The live pane title tracks the conversation topic (Claude and
        // its kin write it there), which beats the session file's slug.
        // Prefer it; keep the harness name only as a fallback.
        let title = a.pane.and_then(|p| pane_title(p as u32, &a.kind));
        r.name = title.or_else(|| r.name.take());
        apply(a, r).await;
    }
}

/// A `Resolved` is not `Clone` (it is cheap to rebuild); copy the fields
/// out of the shared claude index entry.
fn clone_resolved(r: &Resolved) -> Resolved {
    Resolved {
        real_id: r.real_id.clone(),
        name: r.name.clone(),
        status: r.status.clone(),
        started_ms: r.started_ms,
        last_active_ms: r.last_active_ms,
        source_path: r.source_path.clone(),
    }
}

/// Fold one resolver result onto a row: migrate the id first (so enrich
/// lands on the durable row), then persist the resolved fields, then
/// mirror both onto the in-memory `Agent` for this render.
async fn apply(a: &mut Agent, r: Resolved) {
    if let Some(real) = r.real_id.as_deref().filter(|id| *id != a.id) {
        migrate_id(a, real).await;
    }
    let _ = store::enrich(
        &a.id,
        r.name.as_deref(),
        r.status.as_deref(),
        r.started_ms,
        r.last_active_ms,
        r.source_path.as_deref(),
    )
    .await;
    if let Some(v) = r.name {
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
async fn on_identify(pane: u32, id: String, source: Option<String>) {
    let Ok(Some(mut a)) = store::live_by_pane(pane as i64).await else {
        return;
    };
    if id != a.id {
        migrate_id(&mut a, &id).await;
    }
    let _ = store::enrich(&a.id, None, None, None, None, source.as_deref())
        .await;
}

/// A shim's status report for a pane.
async fn report(
    pane: u32,
    status: String,
    task: Option<String>,
    cfg: Rc<Config>,
    picker: Rc<RefCell<Option<Picker>>>,
) {
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
    }
    refresh_if_open(&picker).await;
}

/// On start (including after restart-server): rediscover agents in live
/// panes, retire live rows whose pane is gone, and prune old history.
async fn reconcile(cfg: Rc<Config>) {
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

/// Jump the pressing client to a pane, wherever it lives.
async fn jump(pane: u32) {
    let panes = list_panes().unwrap_or_default();
    if let Some(pi) = panes.iter().find(|p| p.id == pane) {
        let _ = run_command(&format!("select-window -t @{}", pi.window)).await;
        let windows = list_windows().unwrap_or_default();
        if let Some(sid) = windows
            .iter()
            .find(|w| w.id == pi.window)
            .and_then(|w| w.sessions.first())
        {
            let _ = run_command(&format!("switch-client -t ${sid}")).await;
        }
    }
    let _ = run_command(&format!("select-pane -t %{pane}")).await;
}

// ---------------------------------------------------------------------------
// the plugin
// ---------------------------------------------------------------------------

struct Agents {
    cfg: Rc<Config>,
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
}

impl Plugin for Agents {
    const NAME: &'static str = "agents";
    type Config = AgentsConfig;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let cfg = Rc::new(Config::from(&config)?);
        ctx.subscribe(&[
            "plugin-command",
            "pane-created",
            "pane-destroyed",
            "pane-command-changed",
            "mode-key",
            "mode-resize",
            "mode-closed",
        ])
        .map_err(|e| e.message.clone())?;
        store::migrate_sync()?;
        ctx.spawn(reconcile(Rc::clone(&cfg)));
        Ok(Self {
            cfg,
            picker: Rc::new(RefCell::new(None)),
            busy: Rc::new(Cell::new(false)),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => self.on_command(ctx, &event),
            "pane-created" | "pane-command-changed" => {
                if let Some(pane) = event.scope.pane {
                    let cfg = Rc::clone(&self.cfg);
                    let picker = Rc::clone(&self.picker);
                    ctx.spawn(async move {
                        classify(pane, cfg).await;
                        refresh_if_open(&picker).await;
                    });
                }
            }
            "pane-destroyed" => {
                if let Some(pane) = event.scope.pane {
                    let picker = Rc::clone(&self.picker);
                    ctx.spawn(async move {
                        let _ = store::end_by_pane(
                            pane as i64,
                            now_ms() as i64,
                            "closed",
                        )
                        .await;
                        refresh_if_open(&picker).await;
                    });
                }
            }
            "mode-key" => self.on_mode_key(ctx, &event),
            "mode-resize" => {
                let mut b = self.picker.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if event.get_i64("mode") != Some(p.mode.0 as i64) {
                    return;
                }
                if let Some(w) = event.get_i64("width") {
                    p.width = w as u32;
                }
                if let Some(h) = event.get_i64("height") {
                    p.height = h as u32;
                }
                pick_render(p);
            }
            "mode-closed" => {
                let mut b = self.picker.borrow_mut();
                if b.as_ref().is_some_and(|p| {
                    event.get_i64("mode") == Some(p.mode.0 as i64)
                }) {
                    *b = None;
                }
            }
            _ => {}
        }
    }
}

impl Agents {
    fn on_command(&self, ctx: &Ctx, event: &Event) {
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let verb = text.split_whitespace().next().unwrap_or("").to_string();
        if verb == "pick" {
            // Reopen even if a picker lingers. A mode whose window or
            // session was destroyed without a clean close leaves our
            // state as Some with no mode-closed event; refusing then would
            // wedge the hotkey. Drop the old one (best-effort close) and
            // open fresh, so pressing the key always shows a picker.
            if let Some(old) = self.picker.borrow_mut().take() {
                let _ = mode_close(old.mode);
            }
            let cfg = Rc::clone(&self.cfg);
            let picker = Rc::clone(&self.picker);
            let client = event.scope.client.map(u64::from);
            ctx.spawn(pick_open(picker, cfg, client));
            return;
        }
        if verb == "identify" {
            // "identify <session_id> [session_file]" from a pi/opencode
            // one-shot hook: bind the pane's row to its durable id.
            let Some(pane) = event.scope.pane else { return };
            let mut rest = text.split_whitespace().skip(1);
            let Some(id) = rest.next().map(str::to_string) else {
                return;
            };
            let source = rest.next().map(str::to_string);
            let picker = Rc::clone(&self.picker);
            ctx.spawn(async move {
                on_identify(pane, id, source).await;
                refresh_if_open(&picker).await;
            });
            return;
        }
        if STATUSES.contains(&verb.as_str()) {
            let Some(pane) = event.scope.pane else { return };
            let task = text
                .splitn(2, char::is_whitespace)
                .nth(1)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let cfg = Rc::clone(&self.cfg);
            let picker = Rc::clone(&self.picker);
            ctx.spawn(report(pane, verb, task, cfg, picker));
            return;
        }
        let _ = display_message(&format!(
            "agents: unknown verb {verb:?} (pick|working|needs_input|waiting|done)"
        ));
    }

    fn on_mode_key(&self, ctx: &Ctx, event: &Event) {
        let mode_id = event.get_i64("mode");
        let key = event.get_str("key").unwrap_or("").to_string();
        let mut after = PickAfter::None;
        {
            let mut b = self.picker.borrow_mut();
            let Some(p) = b.as_mut() else { return };
            if mode_id != Some(p.mode.0 as i64) {
                return;
            }
            if self.busy.get() {
                return;
            }
            p.status = None;
            let k = &p.keys.clone();
            let sel = p.view.get(p.sel).copied();
            // Arrows and their control aliases move the selection in both
            // modes; they are never text.
            let is_down = matches!(key.as_str(), "Down" | "C-n" | "C-j");
            let is_up = matches!(key.as_str(), "Up" | "C-p" | "C-k");
            if p.filtering {
                // Filter mode: keys are text, except accept / cancel / move.
                if key == k.close {
                    // Esc leaves filter mode and clears it (a second Esc,
                    // now in normal mode, closes the picker).
                    p.filtering = false;
                    p.filter.clear();
                    pick_refilter(p);
                    pick_render(p);
                } else if key == "Enter" {
                    // Accept the filter; stay on the picker in normal mode.
                    p.filtering = false;
                    pick_render(p);
                } else if is_down {
                    move_sel(p, 1);
                } else if is_up {
                    move_sel(p, -1);
                } else if key == "BSpace" {
                    p.filter.pop();
                    pick_refilter(p);
                    pick_render(p);
                } else if key == "C-u" {
                    p.filter.clear();
                    pick_refilter(p);
                    pick_render(p);
                } else if key == "Space" {
                    p.filter.push(' ');
                    pick_refilter(p);
                    pick_render(p);
                } else if key.chars().count() == 1
                    && !key.chars().next().unwrap().is_control()
                {
                    p.filter.push_str(&key);
                    pick_refilter(p);
                    pick_render(p);
                }
            } else if key == k.close {
                after = PickAfter::Close(p.mode);
            } else if key == k.filter {
                p.filtering = true;
                pick_render(p);
            } else if key == k.jump {
                if let Some(i) = sel {
                    let a = &p.rows[i];
                    if let Some(pane) = a.pane.filter(|_| a.live()) {
                        after = PickAfter::Jump(pane as u32, p.mode);
                    } else {
                        p.status = Some("no live pane to jump to".into());
                        pick_render(p);
                    }
                }
            } else if key == k.archive {
                after = life_after(p, sel, "archived");
            } else if key == k.history {
                p.show_history = !p.show_history;
                after = PickAfter::Reload;
            } else if is_down || key == "j" {
                move_sel(p, 1);
            } else if is_up || key == "k" {
                move_sel(p, -1);
            }
        }
        match after {
            PickAfter::None => {}
            PickAfter::Close(mode) => {
                let _ = mode_close(mode);
            }
            PickAfter::Jump(pane, mode) => {
                ctx.spawn(async move {
                    jump(pane).await;
                    let _ = mode_close(mode);
                });
            }
            PickAfter::Life(id, life) => {
                ctx.spawn(apply_life(Rc::clone(&self.picker), id, life));
            }
            PickAfter::Reload => {
                ctx.spawn(reload_picker(Rc::clone(&self.picker)));
            }
        }
    }
}

/// Move the selection by `delta` rows, clamped, then scroll and redraw.
fn move_sel(p: &mut Picker, delta: i32) {
    if p.view.is_empty() {
        return;
    }
    let last = (p.view.len() - 1) as i32;
    p.sel = (p.sel as i32 + delta).clamp(0, last) as usize;
    p.scroll_to_selection();
    pick_render(p);
}

/// Decide a lifecycle change for the highlighted row, under the borrow.
fn life_after(p: &Picker, sel: Option<usize>, life: &str) -> PickAfter {
    match sel {
        Some(i) => PickAfter::Life(p.rows[i].id.clone(), life.to_string()),
        None => PickAfter::None,
    }
}

// ---------------------------------------------------------------------------
// the picker
// ---------------------------------------------------------------------------

enum PickAfter {
    None,
    Close(ModeId),
    Jump(u32, ModeId),
    Life(String, String),
    Reload,
}

/// One rendered line: a band header, or a selectable row (by its position
/// in `view`). Headers make the list scroll in "display space", so the
/// selected row stays visible even with headers between the bands.
#[derive(Clone, Copy)]
enum Line {
    Header(u8),
    Item(usize),
}

struct Picker {
    mode: ModeId,
    width: u32,
    height: u32,
    rows: Vec<Agent>,
    view: Vec<usize>,
    lines: Vec<Line>,
    sel: usize,
    top: usize,
    filter: String,
    filtering: bool,
    now_ms: u64,
    show_history: bool,
    keys: PickKeys,
    status: Option<String>,
}

impl Picker {
    /// Rebuild the display lines from `view`, inserting a header whenever
    /// the band changes.
    fn rebuild_lines(&mut self) {
        self.lines.clear();
        let mut prev: Option<u8> = None;
        for (vpos, &ri) in self.view.iter().enumerate() {
            let b = band(&self.rows[ri]);
            if prev != Some(b) {
                self.lines.push(Line::Header(b));
                prev = Some(b);
            }
            self.lines.push(Line::Item(vpos));
        }
    }

    /// The display line of the selected row.
    fn sel_line(&self) -> usize {
        self.lines
            .iter()
            .position(|l| matches!(l, Line::Item(v) if *v == self.sel))
            .unwrap_or(0)
    }

    fn list_h(&self) -> usize {
        LIST_MAX.min((self.height as usize).saturating_sub(5)).max(1)
    }

    /// Keep the selected row's display line inside the scroll window.
    fn scroll_to_selection(&mut self) {
        let h = self.list_h();
        let sel_line = self.sel_line();
        if sel_line < self.top {
            self.top = sel_line;
        } else if sel_line >= self.top + h {
            self.top = sel_line + 1 - h;
        }
        // A header directly above the window wastes a line; pull it in.
        if self.top > 0 && matches!(self.lines.get(self.top), Some(Line::Item(_)))
            && matches!(self.lines.get(self.top - 1), Some(Line::Header(_)))
        {
            self.top -= 1;
        }
    }
}

async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    cfg: Rc<Config>,
    client: Option<u64>,
) {
    let window = client
        .and_then(|cid| {
            list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
        })
        .and_then(|c| c.session)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window)
        .or_else(|| list_windows().ok().and_then(|w| w.first().map(|x| x.id)));
    let Some(window) = window else {
        let _ = display_message("agents: no window to open the picker");
        return;
    };
    let mut rows = store::live_agents().await.unwrap_or_default();
    enrich_live(&mut rows).await;
    sort_rows(&mut rows);
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: WIDTH,
        height: HEIGHT,
        title: Some("agents".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ =
                display_message(&format!("agents: cannot open picker: {}", e.message));
            return;
        }
    };
    let mut p = Picker {
        mode,
        width: WIDTH,
        height: HEIGHT,
        rows,
        view: Vec::new(),
        lines: Vec::new(),
        sel: 0,
        top: 0,
        filter: String::new(),
        filtering: false,
        now_ms: now_ms(),
        show_history: false,
        keys: cfg.keys.clone(),
        status: None,
    };
    pick_refilter(&mut p);
    pick_render(&mut p);
    *picker.borrow_mut() = Some(p);
}

/// Reload rows from the database, preserving the highlight.
async fn reload_picker(picker: Rc<RefCell<Option<Picker>>>) {
    let show_history =
        picker.borrow().as_ref().map(|p| p.show_history).unwrap_or(false);
    let mut rows = store::live_agents().await.unwrap_or_default();
    enrich_live(&mut rows).await;
    if show_history {
        rows.extend(store::history(HISTORY_MAX).await.unwrap_or_default());
    }
    sort_rows(&mut rows);
    let mut b = picker.borrow_mut();
    if let Some(p) = b.as_mut() {
        p.rows = rows;
        p.now_ms = now_ms();
        pick_refilter(p);
        pick_render(p);
    }
}

async fn refresh_if_open(picker: &Rc<RefCell<Option<Picker>>>) {
    if picker.borrow().is_some() {
        reload_picker(Rc::clone(picker)).await;
    }
}

async fn apply_life(
    picker: Rc<RefCell<Option<Picker>>>,
    id: String,
    life: String,
) {
    let _ = store::set_life(&id, &life).await;
    {
        let mut b = picker.borrow_mut();
        if let Some(p) = b.as_mut() {
            p.status = Some(format!("marked {life}"));
        }
    }
    reload_picker(picker).await;
}

// ---------------------------------------------------------------------------
// state bands: the roster is grouped by what needs attention, then by
// recency inside each group.
// ---------------------------------------------------------------------------

/// The band an agent belongs to, lowest first. `needs_input` sits at the
/// top (a human is blocking it), then the ones still in a turn, then the
/// finished ones.
fn band(a: &Agent) -> u8 {
    if !a.live() {
        return 3; // done / history
    }
    match a.status.as_str() {
        "needs_input" => 0,
        "waiting" => 1,
        "working" => 2,
        "done" => 3,
        _ => 2,
    }
}

fn band_label(b: u8) -> &'static str {
    match b {
        0 => "needs input",
        1 => "waiting",
        2 => "working",
        _ => "done",
    }
}

/// Order the roster: by band, then most-recently-active first, then most
/// recently started, then name - so the order never jitters between
/// renders.
fn sort_rows(rows: &mut [Agent]) {
    rows.sort_by(|a, b| {
        band(a)
            .cmp(&band(b))
            .then(b.active_ms().cmp(&a.active_ms()))
            .then(b.started().cmp(&a.started()))
            .then(display_name(a).cmp(&display_name(b)))
    });
}

/// The name to show: the harness's own name, else the pane title, else a
/// `kind · session` fallback. `activate`/resolvers store the first two in
/// `name`; this only adds the last-resort composition.
fn display_name(a: &Agent) -> String {
    if let Some(n) = a.name.as_deref().filter(|s| !s.is_empty()) {
        return n.to_string();
    }
    match a.session.as_deref() {
        Some(s) if !s.is_empty() => format!("{} · {}", a.kind, s),
        _ => a.kind.clone(),
    }
}

/// session_creator's ranking: prefix beats substring beats subsequence.
fn rank(hay: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(4);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if h.starts_with(&n) {
        return Some(0);
    }
    if h.contains(&n) {
        return Some(1);
    }
    // No fuzzy subsequence tier: the haystack joins the name with the
    // kind/status/session words, whose common letters make a subsequence
    // match almost everything. Substring is the right strictness here.
    None
}

fn haystack(a: &Agent) -> String {
    format!(
        "{} {} {} {} {} {} {} {}",
        display_name(a),
        a.kind,
        a.status,
        a.life,
        a.session.as_deref().unwrap_or(""),
        a.window.as_deref().unwrap_or(""),
        a.task.as_deref().unwrap_or(""),
        a.reason.as_deref().unwrap_or(""),
    )
}

/// Keep the band order of `p.rows`; the filter only includes or excludes.
fn pick_refilter(p: &mut Picker) {
    let keep = p.view.get(p.sel).map(|&i| p.rows[i].id.clone());
    let needle = p.filter.trim();
    p.view = p
        .rows
        .iter()
        .enumerate()
        .filter(|(_, a)| rank(&haystack(a), needle).is_some())
        .map(|(i, _)| i)
        .collect();
    p.sel = keep
        .and_then(|id| p.view.iter().position(|&i| p.rows[i].id == id))
        .unwrap_or(0);
    if p.sel >= p.view.len() {
        p.sel = p.view.len().saturating_sub(1);
    }
    p.rebuild_lines();
    p.top = 0;
    p.scroll_to_selection();
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

fn keyname(k: &str) -> &str {
    match k {
        "Escape" => "Esc",
        "Enter" => "Enter",
        other => other,
    }
}

fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Coloured status glyph. Dim for a finished agent.
fn badge(a: &Agent) -> String {
    if !a.live() {
        return "\x1b[2m·\x1b[0m".into();
    }
    match a.status.as_str() {
        "needs_input" => "\x1b[1;33m!\x1b[0m".into(),
        "working" => "\x1b[32m●\x1b[0m".into(),
        "waiting" => "\x1b[36m◍\x1b[0m".into(),
        "done" => "\x1b[2m·\x1b[0m".into(),
        _ => "?".into(),
    }
}

fn pick_render(p: &mut Picker) {
    let w = p.width as usize;
    let h = p.height as usize;
    // 60% of the width for the list, but never let the clamp's min exceed
    // its max: a narrow mode (a split pane) would panic `clamp(30, <30)`
    // and trap the guest. Below ~50 cols give the list almost everything
    // and skip the side preview.
    let list_w = if w <= 50 {
        w.saturating_sub(2).max(1)
    } else {
        (w * 6 / 10).clamp(30, w - 20)
    };
    let mut out = String::from("\x1b[2J\x1b[H");

    let live = p.rows.iter().filter(|a| a.live()).count();
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1m agents\x1b[0m \x1b[2m({live} live{})\x1b[0m",
        if p.show_history { ", +history" } else { "" }
    ));
    if p.filtering {
        // Active: show the query with a block cursor.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2mfilter\x1b[0m {}\x1b[7m \x1b[0m",
            p.filter
        ));
    } else if p.filter.is_empty() {
        // Idle, no query: a hint.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2mfilter\x1b[0m \x1b[2m(press {} to filter)\x1b[0m",
            keyname(&p.keys.filter),
        ));
    } else {
        // Idle, query applied: show it, no cursor.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2mfilter\x1b[0m {}",
            p.filter
        ));
    }
    out.push_str(&format!(
        "\x1b[3;1H  \x1b[2m{}\x1b[0m",
        "─".repeat(list_w.saturating_sub(2))
    ));

    let list_h = p.list_h();
    if p.view.is_empty() {
        out.push_str("\x1b[4;1H  \x1b[2m(no agents)\x1b[0m");
    } else {
        for (line_i, li) in
            (p.top..(p.top + list_h).min(p.lines.len())).enumerate()
        {
            let row = 4 + line_i;
            match p.lines[li] {
                Line::Header(b) => {
                    out.push_str(&format!(
                        "\x1b[{row};1H \x1b[1;2m{}\x1b[0m",
                        clip(band_label(b), list_w.saturating_sub(2)),
                    ));
                }
                Line::Item(vpos) => {
                    let a = &p.rows[p.view[vpos]];
                    let cur = vpos == p.sel;
                    let marker = if cur { "▸" } else { " " };
                    let age = fmt_age(
                        p.now_ms.saturating_sub(a.active_ms() as u64) / 1000,
                    );
                    // kind + age are fixed columns at the right edge; the
                    // name (with the task, when there is one) fills all the
                    // width that is left.
                    let right =
                        format!("{:<8} {:>6}", clip(&a.kind, 8), age);
                    // prefix = "▸ ● " (marker + badge), gap = 2 before right.
                    let label_w = list_w
                        .saturating_sub(1)
                        .saturating_sub(4 + 2 + right.chars().count())
                        .max(8);
                    let mut label = display_name(a);
                    if let Some(t) =
                        a.task.as_deref().filter(|s| !s.is_empty())
                    {
                        label = format!("{label}  ·  {t}");
                    }
                    let label = clip(&label, label_w);
                    // The badge carries its own colour codes; keep it out
                    // of the width budget by composing plain text first,
                    // then splicing the badge over the marker's gap.
                    let plain =
                        format!("{marker}   {label:<label_w$}  {right}");
                    let plain = clip(&plain, list_w.saturating_sub(1));
                    // Splice the coloured badge back in over the two
                    // spaces after the marker.
                    let shown =
                        plain.replacen("   ", &format!(" {} ", badge(a)), 1);
                    if cur {
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[7m{:<pad$}\x1b[0m",
                            strip_sgr(&shown),
                            pad = list_w.saturating_sub(1)
                        ));
                    } else {
                        out.push_str(&format!("\x1b[{row};1H{shown}"));
                    }
                }
            }
        }
    }

    // Vertical separator between the list and the preview.
    for r in 1..=h {
        out.push_str(&format!("\x1b[{r};{c}H\x1b[2m│\x1b[0m", c = list_w + 1));
    }

    if let Some(s) = &p.status {
        out.push_str(&format!(
            "\x1b[{r};1H\x1b[36m  {}\x1b[0m",
            clip(s, list_w.saturating_sub(4)),
            r = h.saturating_sub(1)
        ));
    }
    let k = &p.keys;
    let footer = if p.filtering {
        "type to filter · Enter accept · Esc cancel".to_string()
    } else {
        format!(
            "j/k move · {} jump · {} filter · {} arch · {} hist · {} close",
            keyname(&k.jump),
            keyname(&k.filter),
            keyname(&k.archive),
            keyname(&k.history),
            keyname(&k.close),
        )
    };
    out.push_str(&format!(
        "\x1b[{h};1H  \x1b[2m{}\x1b[0m",
        clip(&footer, list_w.saturating_sub(4))
    ));

    let _ = mode_write(p.mode, out.as_bytes());
    let _ = mode_preview(p.mode, preview_rect(p, list_w).as_ref());
}

/// The live pane of the highlighted row, shown to the right of the list.
fn preview_rect(p: &Picker, list_w: usize) -> Option<PreviewRect> {
    let a = p.rows.get(p.view.get(p.sel).copied()?)?;
    let pane = a.pane.filter(|_| a.live())? as u32;
    let x = (list_w + 1) as u32;
    let w = (p.width as usize).saturating_sub(list_w + 1) as u32;
    let h = p.height.saturating_sub(1);
    if w == 0 || h == 0 {
        return None;
    }
    Some(PreviewRect { pane: PaneId(pane), x, y: 0, w, h })
}

/// Drop SGR escape sequences so the reverse-video selection line does not
/// carry a colour that would reset the inversion mid-row.
fn strip_sgr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            while let Some(&n) = chars.peek() {
                chars.next();
                if n == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

tmux_plugin!(Agents);
