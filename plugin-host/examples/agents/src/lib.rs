//! A roster of the agent CLI sessions running in your panes.
//!
//! Press the hotkey (bind it to `plugin-command agents pick`) to open a
//! floating chooser: one row per agent, grouped by state (needs input,
//! waiting, working, done) and, inside each group, most recently active
//! first. A live preview of the highlighted pane sits to the right.
//! `j`/`k` move, `gg`/`G` jump to the ends, Enter jumps to the pane, `a`
//! archives, `h` folds in the finished ones. `q` or Esc closes the picker.
//! Press `/`, or navigate the cursor up past the top row, to focus the
//! search box; Esc there unfocuses it and keeps the query (but an empty box
//! closes the picker, so a stray move up never swallows a close). `J`/`K` (shift) mark rows into a selection; `a` then archives the whole
//! selection at once (and un-archives when every marked row is archived).
//! Esc clears the selection before it closes the picker. `+`/`-` grow and
//! shrink the popup; the size is remembered across opens. The popup opens
//! at a fraction of the window by default. `r` renames the selected agent
//! (Enter accepts, Esc cancels, empty reverts to the harness name); your
//! name and the harness name compete by recency (see `display_name`).
//! `C-f` toggles content search: the filter then also greps each live
//! agent's pane CONTENTS, not just its name and status. The grep runs in
//! tmux over the live grid (the `panes_search` host call), so the pane
//! text never crosses the plugin ABI - only the needle in and the matches
//! out. A matching row shows the line it hit. The matcher is auto-detected
//! (a query with regex metacharacters runs as a regex, else a plain
//! substring), and falls back to fuzzy when it finds nothing; the active
//! mode shows in the header and footer.
//!
//! A waiting agent you have not gotten to yet is UNREAD: it entered
//! `waiting` more recently than your last acknowledgement. Jumping to its
//! pane or landing the cursor on its row acknowledges it. Unread rows sort
//! to the top of the waiting band, show a bright filled badge, and count
//! in the header.
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
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

mod resolve;
mod store;
use resolve::Resolved;
use store::Agent;

/// The picker opens at a fraction of the window, clamped to this box. A
/// manual resize (+/-) is remembered and overrides the default.
const MAX_WIDTH: u32 = 180;
const MAX_HEIGHT: u32 = 54;
const MIN_WIDTH: u32 = 72;
const MIN_HEIGHT: u32 = 16;
/// Fraction of the window the default size fills (in tenths).
const FILL_TENTHS: u32 = 9;
/// The step a single +/- resize moves the width and height.
const RESIZE_STEP_W: u32 = 12;
const RESIZE_STEP_H: u32 = 4;
/// Cap on rows the list draws; the window height drives the real count.
const LIST_MAX: usize = 60;
const HISTORY_MAX: i64 = 100;
/// Lines searched per pane (from the bottom up) by content search. 0 =
/// the host default cap.
const SEARCH_LINES: u32 = 5000;

/// The commands that mark a pane as an agent, if none are configured.
const DEFAULT_COMMANDS: &[&str] = &["claude", "codex", "pi", "opencode"];
/// The statuses a shim may report.
const STATUSES: &[&str] = &["working", "needs_input", "waiting", "done"];
/// Foreground commands that are really interpreters launching a script.
/// Codex ships as `node /usr/bin/codex`, so the pane's foreground command
/// is `node`; the agent name is the basename of the launched script, which
/// the process keeps in its `_` environment variable.
const INTERPRETERS: &[&str] =
    &["node", "bun", "deno", "python", "python3", "ruby"];

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
    pick_content: Option<String>,
    pick_rename: Option<String>,
}

#[derive(Clone)]
struct PickKeys {
    jump: String,
    filter: String,
    archive: String,
    history: String,
    close: String,
    content: String,
    rename: String,
}

impl Default for PickKeys {
    fn default() -> Self {
        Self {
            jump: "Enter".into(),
            filter: "/".into(),
            archive: "a".into(),
            history: "h".into(),
            close: "Escape".into(),
            content: "C-f".into(),
            rename: "r".into(),
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
                content: pick(&c.pick_content, d.content),
                rename: pick(&c.pick_rename, d.rename),
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
async fn on_identify(
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
        // A `working` report is a new turn - the user messaged the agent -
        // so bring an archived row back into the roster.
        if status == "working" {
            let _ = store::unarchive_by_pane(pane as i64).await;
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
                    if let Some(t) = b.as_ref().and_then(|p| p.timer) {
                        cancel(t);
                    }
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
                if let Some(t) = old.timer {
                    cancel(t);
                }
                let _ = mode_close(old.mode);
            }
            let cfg = Rc::clone(&self.cfg);
            let picker = Rc::clone(&self.picker);
            let client = event.scope.client.map(u64::from);
            let here = event.scope.pane;
            ctx.spawn(pick_open(picker, cfg, client, here));
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
            let cfg = Rc::clone(&self.cfg);
            let picker = Rc::clone(&self.picker);
            ctx.spawn(async move {
                on_identify(pane, id, source, cfg).await;
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
        // An agent to acknowledge (mark read) after the borrow drops: the
        // cursor landed on an unread waiting row, or the user jumped to it.
        let mut ack_id: Option<String> = None;
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
            // True when this key moved the selection: drives read-ack.
            let mut moved = false;
            // A pending `g` is consumed by this key; only a second `g`
            // keeps it (see the `g` branch).
            let g_pending = p.pending_g;
            p.pending_g = false;
            // Arrows and their control aliases move the selection in both
            // modes; they are never text.
            let is_down = matches!(key.as_str(), "Down" | "C-n" | "C-j");
            let is_up = matches!(key.as_str(), "Up" | "C-p" | "C-k");
            if p.renaming {
                // Rename mode: keys are text, except accept / cancel.
                if key == k.close {
                    p.renaming = false;
                    p.rename_buf.clear();
                    pick_render(p);
                } else if key == "Enter" {
                    if let Some(&i) = p.view.get(p.sel) {
                        let id = p.rows[i].id.clone();
                        after = PickAfter::Rename(id, p.rename_buf.trim().to_string());
                    }
                    p.renaming = false;
                } else if key == "BSpace" {
                    p.rename_buf.pop();
                    pick_render(p);
                } else if key == "C-u" {
                    p.rename_buf.clear();
                    pick_render(p);
                } else if key == "Space" {
                    p.rename_buf.push(' ');
                    pick_render(p);
                } else if key.chars().count() == 1
                    && !key.chars().next().unwrap().is_control()
                {
                    p.rename_buf.push_str(&key);
                    pick_render(p);
                }
            } else if p.filtering {
                // The search box is focused: keys are text, except the ones
                // that unfocus it or move the selection. Esc (or Enter)
                // unfocuses and KEEPS the query, fzf-style; it never closes
                // the picker from here (close is q, or Esc from the list).
                if key == k.close || key == "Enter" {
                    p.filtering = false;
                    pick_render(p);
                } else if is_down {
                    move_sel(p, 1);
                    moved = true;
                } else if is_up {
                    move_sel(p, -1);
                    moved = true;
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
                } else if key == k.content {
                    toggle_content(p);
                } else if key.chars().count() == 1
                    && !key.chars().next().unwrap().is_control()
                {
                    p.filter.push_str(&key);
                    pick_refilter(p);
                    pick_render(p);
                }
            } else if key == k.close || key == "q" {
                // Esc or q cancels a pending selection first; a second press,
                // with nothing marked, closes the picker.
                if p.marked.is_empty() {
                    after = PickAfter::Close(p.mode);
                } else {
                    p.marked.clear();
                    p.status = Some("selection cleared".into());
                    pick_render(p);
                }
            } else if key == k.filter {
                // `/` focuses the search box (so does navigating up).
                p.filtering = true;
                pick_render(p);
            } else if key == "g" {
                // Vim `gg`: the first `g` waits, the second goes to the top.
                if g_pending {
                    let n = p.view.len() as i32;
                    move_sel(p, -n);
                    moved = true;
                } else {
                    p.pending_g = true;
                }
            } else if key == "G" {
                // Vim `G`: go to the bottom.
                let n = p.view.len() as i32;
                move_sel(p, n);
                moved = true;
            } else if key == k.content {
                toggle_content(p);
            } else if key == k.rename {
                if let Some(&i) = p.view.get(p.sel) {
                    p.rename_buf = p.rows[i].user_name.clone().unwrap_or_default();
                    p.renaming = true;
                    pick_render(p);
                }
            } else if key == k.jump {
                if let Some(i) = sel {
                    let pane = p.rows[i].pane.filter(|_| p.rows[i].live());
                    if let Some(pane) = pane {
                        // Jumping to the pane acknowledges the agent.
                        p.rows[i].acked_ms = Some(p.now_ms as i64);
                        ack_id = Some(p.rows[i].id.clone());
                        after = PickAfter::Jump(pane as u32, p.mode);
                    } else {
                        p.status = Some("no live pane to jump to".into());
                        pick_render(p);
                    }
                }
            } else if key == k.archive {
                after = archive_after(p);
            } else if key == k.history {
                p.show_history = !p.show_history;
                after = PickAfter::Reload;
            } else if key == "J" {
                mark_and_move(p, 1);
                moved = true;
            } else if key == "K" {
                mark_and_move(p, -1);
                moved = true;
            } else if key == "+" || key == "=" {
                let w = (p.width + RESIZE_STEP_W).min(MAX_WIDTH);
                let h = (p.height + RESIZE_STEP_H).min(MAX_HEIGHT);
                p.width = w;
                p.height = h;
                pick_render(p);
                after = PickAfter::Resize(p.mode, w, h);
            } else if key == "-" || key == "_" {
                let w = p.width.saturating_sub(RESIZE_STEP_W).max(MIN_WIDTH);
                let h = p.height.saturating_sub(RESIZE_STEP_H).max(MIN_HEIGHT);
                p.width = w;
                p.height = h;
                pick_render(p);
                after = PickAfter::Resize(p.mode, w, h);
            } else if is_down || key == "j" {
                move_sel(p, 1);
                moved = true;
            } else if is_up || key == "k" {
                if p.sel == 0 {
                    // Already at the top: bring the cursor up into the
                    // search box.
                    p.filtering = true;
                    pick_render(p);
                } else {
                    move_sel(p, -1);
                    moved = true;
                }
            }
            // Landing the cursor on an unread waiting row acknowledges it
            // (only real navigation acks; opening the picker does not).
            if moved && ack_id.is_none() {
                if let Some(&i) = p.view.get(p.sel) {
                    if p.rows[i].unread() {
                        p.rows[i].acked_ms = Some(p.now_ms as i64);
                        ack_id = Some(p.rows[i].id.clone());
                        pick_render(p);
                    }
                }
            }
        }
        if let Some(id) = ack_id {
            ctx.spawn(async move {
                let _ = store::acknowledge(&id, now_ms() as i64).await;
            });
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
            PickAfter::Life(ids, life) => {
                ctx.spawn(apply_life(Rc::clone(&self.picker), ids, life));
            }
            PickAfter::Reload => {
                ctx.spawn(reload_picker(Rc::clone(&self.picker), false));
            }
            PickAfter::Resize(mode, w, h) => {
                // Resize the float now; remember the choice for next time.
                let _ = mode_resize(mode, w, h);
                ctx.spawn(async move {
                    let _ = store::set_setting("pick_w", &w.to_string()).await;
                    let _ = store::set_setting("pick_h", &h.to_string()).await;
                });
            }
            PickAfter::Rename(id, name) => {
                let picker = Rc::clone(&self.picker);
                ctx.spawn(async move {
                    let n = (!name.is_empty()).then_some(name.as_str());
                    let _ =
                        store::rename_by_user(&id, n, now_ms() as i64).await;
                    reload_picker(picker, false).await;
                });
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

/// Mark the current row into the selection, then move by `delta`. `J`/`K`
/// build a multi-row selection this way: each press ropes in the row under
/// the cursor and steps on, so N presses select N rows and leave the cursor
/// just past them.
fn mark_and_move(p: &mut Picker, delta: i32) {
    if let Some(&i) = p.view.get(p.sel) {
        p.marked.insert(p.rows[i].id.clone());
    }
    move_sel(p, delta);
}

/// Flip content search on or off, then re-filter. Turning it off drops
/// the cached snippets. Turning it on makes the next `pick_refilter` grep
/// the live panes.
fn toggle_content(p: &mut Picker) {
    p.content_search = !p.content_search;
    if !p.content_search {
        p.content_hits.clear();
    }
    p.status = Some(if p.content_search {
        "search pane contents: on".into()
    } else {
        "search pane contents: off".into()
    });
    pick_refilter(p);
    pick_render(p);
}

/// The archive key toggles: it archives its targets, or un-archives them
/// when they are all already archived (an archived row shows in the
/// history view). Targets are the marked selection, else the highlighted
/// row.
fn archive_after(p: &Picker) -> PickAfter {
    let targets: Vec<&Agent> = {
        let marked: Vec<&Agent> = p
            .view
            .iter()
            .filter_map(|&i| p.rows.get(i))
            .filter(|a| p.marked.contains(&a.id))
            .collect();
        if !marked.is_empty() {
            marked
        } else {
            p.view
                .get(p.sel)
                .and_then(|&i| p.rows.get(i))
                .into_iter()
                .collect()
        }
    };
    if targets.is_empty() {
        return PickAfter::None;
    }
    // All targets already archived -> un-archive; otherwise archive.
    let life = if targets.iter().all(|a| a.life == "archived") {
        "active"
    } else {
        "archived"
    };
    let ids: Vec<String> = targets.iter().map(|a| a.id.clone()).collect();
    PickAfter::Life(ids, life.to_string())
}

// ---------------------------------------------------------------------------
// the picker
// ---------------------------------------------------------------------------

enum PickAfter {
    None,
    Close(ModeId),
    Jump(u32, ModeId),
    Life(Vec<String>, String),
    Reload,
    Resize(ModeId, u32, u32),
    Rename(String, String),
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
    /// Agent ids marked for a bulk action, keyed by id so they survive a
    /// reload/refilter without a stale-index risk.
    marked: HashSet<String>,
    filter: String,
    filtering: bool,
    /// A rename in progress: the typed name for the selected agent.
    renaming: bool,
    rename_buf: String,
    /// When on, the filter also matches live pane CONTENTS: the grid of
    /// each live agent's pane is grep'd for the query, in tmux, through
    /// `panes_search`. Toggled with `C-f`.
    content_search: bool,
    /// The matching snippet per live pane id, from the last content
    /// search. Drives the row's snippet and the OR in the filter.
    content_hits: HashMap<u32, String>,
    /// The matcher the last content search actually used (auto-detected,
    /// with a fuzzy fallback). Shown in the footer/header.
    content_mode: SearchMode,
    now_ms: u64,
    show_history: bool,
    keys: PickKeys,
    status: Option<String>,
    /// A `g` was pressed and waits for a second `g` (vim `gg` = go top).
    pending_g: bool,
    /// A stable display rank per agent id, assigned in the recency order
    /// the FIRST time each agent is seen this session. Live refreshes sort
    /// by band then this rank, so an activity-time bump never reshuffles
    /// rows under the cursor; the recency order is set once, at open.
    order: HashMap<String, u64>,
    order_next: u64,
    /// The 2s refresh task, cancelled when the picker closes or reopens.
    timer: Option<TaskId>,
    /// The pane the picker was opened from. Its row gets a "you are here"
    /// border, so you can spot the agent you are currently sitting on.
    current_pane: Option<u32>,
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

/// Keep a dimension within the window (2 cells spare for the border) and
/// at or above `min`.
fn clamp_dim(v: u32, min: u32, avail: u32) -> u32 {
    v.min(avail.saturating_sub(2).max(min)).max(min)
}

/// The default picker size for a window: a fraction of it, clamped to the
/// MIN/MAX box.
fn default_size(ww: u32, wh: u32) -> (u32, u32) {
    let w = (ww * FILL_TENTHS / 10).min(MAX_WIDTH);
    let h = (wh * FILL_TENTHS / 10).min(MAX_HEIGHT);
    (clamp_dim(w, MIN_WIDTH, ww), clamp_dim(h, MIN_HEIGHT, wh))
}

async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    cfg: Rc<Config>,
    client: Option<u64>,
    here: Option<u32>,
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
    // Size to a fraction of the window (clamped to the MIN/MAX box), then
    // let a remembered manual size override it. mode_open clamps again.
    let (ww, wh) = resolve_window(WindowId(window))
        .map(|wi| (wi.width, wi.height))
        .unwrap_or((MAX_WIDTH, MAX_HEIGHT));
    let (mut width, mut height) = default_size(ww, wh);
    if let Ok(Some(v)) = store::get_setting("pick_w").await {
        if let Ok(n) = v.parse::<u32>() {
            width = clamp_dim(n, MIN_WIDTH, ww);
        }
    }
    if let Ok(Some(v)) = store::get_setting("pick_h").await {
        if let Ok(n) = v.parse::<u32>() {
            height = clamp_dim(n, MIN_HEIGHT, wh);
        }
    }
    let mut rows = store::live_agents().await.unwrap_or_default();
    enrich_live(&mut rows).await;
    let mut order: HashMap<String, u64> = HashMap::new();
    let mut order_next: u64 = 0;
    stable_sort(&mut order, &mut order_next, &mut rows);
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
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
        width,
        height,
        rows,
        view: Vec::new(),
        lines: Vec::new(),
        sel: 0,
        top: 0,
        marked: HashSet::new(),
        filter: String::new(),
        filtering: false,
        renaming: false,
        rename_buf: String::new(),
        content_search: false,
        content_hits: HashMap::new(),
        content_mode: SearchMode::Plain,
        now_ms: now_ms(),
        show_history: false,
        keys: cfg.keys.clone(),
        status: None,
        pending_g: false,
        order,
        order_next,
        timer: None,
        current_pane: here,
    };
    pick_refilter(&mut p);
    pick_render(&mut p);
    *picker.borrow_mut() = Some(p);
    // Keep times and file-sourced status fresh while the picker is open,
    // without a costly file scan on every event. Track the task so close
    // (or a reopen) can cancel it deterministically, instead of leaving it
    // to notice the picker is gone on its next tick.
    let tid = spawn(refresh_timer(Rc::clone(&picker), mode));
    if let Some(p) = picker.borrow_mut().as_mut() {
        p.timer = Some(tid);
    }
}

/// Reload rows from the database, preserving the highlight.
/// Rebuild the picker's rows. `enrich` reads the harness session files
/// (the costly part); event-driven refreshes pass false and only re-read
/// the DB (shim-pushed status, membership) then re-render. The enrich runs
/// on picker open and on a slow timer.
async fn reload_picker(picker: Rc<RefCell<Option<Picker>>>, enrich: bool) {
    let show_history =
        picker.borrow().as_ref().map(|p| p.show_history).unwrap_or(false);
    let mut rows = store::live_agents().await.unwrap_or_default();
    if enrich {
        enrich_live(&mut rows).await;
    }
    if show_history {
        rows.extend(store::history(HISTORY_MAX).await.unwrap_or_default());
    }
    let mut b = picker.borrow_mut();
    if let Some(p) = b.as_mut() {
        // Capture the selected agent (id AND pane) against the OLD rows
        // before we swap them in, so the highlight follows the agent. The
        // pane is the fallback: an id migration (prov -> durable) changes
        // the id but never the pane, so the cursor stays put across it.
        let keep = p
            .view
            .get(p.sel)
            .and_then(|&i| p.rows.get(i))
            .map(|a| (a.id.clone(), a.pane));
        // Stable order (band + frozen rank), so a refresh never reshuffles
        // rows under the cursor.
        stable_sort(&mut p.order, &mut p.order_next, &mut rows);
        p.rows = rows;
        p.now_ms = now_ms();
        // A refresh keeps the scroll where it is (only filter typing snaps
        // back to the top).
        pick_refilter_keep(p, keep, false);
        pick_render(p);
    }
}

async fn refresh_if_open(picker: &Rc<RefCell<Option<Picker>>>) {
    if picker.borrow().is_some() {
        // Events only re-read the DB; the file scan is left to the timer.
        reload_picker(Rc::clone(picker), false).await;
    }
}

/// While the picker stays open, re-read the harness session files on a
/// slow cadence so times and file-sourced status stay fresh without
/// re-scanning on every event. Ends when the picker closes or is replaced.
const REFRESH_MS: u64 = 2000;
async fn refresh_timer(picker: Rc<RefCell<Option<Picker>>>, mode: ModeId) {
    loop {
        if sleep_ms(REFRESH_MS).await.is_err() {
            return;
        }
        let live =
            picker.borrow().as_ref().is_some_and(|p| p.mode.0 == mode.0);
        if !live {
            return;
        }
        reload_picker(Rc::clone(&picker), true).await;
    }
}

async fn apply_life(
    picker: Rc<RefCell<Option<Picker>>>,
    ids: Vec<String>,
    life: String,
) {
    for id in &ids {
        let _ = store::set_life(id, &life).await;
    }
    {
        let mut b = picker.borrow_mut();
        if let Some(p) = b.as_mut() {
            let verb = if life == "active" { "unarchived" } else { "archived" };
            p.status = Some(if ids.len() == 1 {
                verb.to_string()
            } else {
                format!("{} {verb}", ids.len())
            });
            // The bulk action consumed the selection.
            p.marked.clear();
        }
    }
    reload_picker(picker, false).await;
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
/// Order rows for a live refresh WITHOUT reshuffling under the cursor.
/// New agents get a rank in ideal (band + unread + recency) order the
/// first time they appear; thereafter rows sort by band then that frozen
/// rank. Band is primary, so a status change that moves an agent to
/// another band still moves it - only the churn from activity-time bumps
/// is removed.
fn stable_sort(
    order: &mut HashMap<String, u64>,
    order_next: &mut u64,
    rows: &mut Vec<Agent>,
) {
    // Ideal order first, so a batch of new agents is ranked sensibly.
    sort_rows(rows);
    for a in rows.iter() {
        if !order.contains_key(&a.id) {
            order.insert(a.id.clone(), *order_next);
            *order_next += 1;
        }
    }
    let rank = |a: &Agent| order.get(&a.id).copied().unwrap_or(u64::MAX);
    rows.sort_by(|a, b| band(a).cmp(&band(b)).then(rank(a).cmp(&rank(b))));
}

fn sort_rows(rows: &mut [Agent]) {
    // 0 sorts before 1: unread first.
    let unread_rank = |a: &Agent| u8::from(!a.unread());
    rows.sort_by(|a, b| {
        band(a)
            .cmp(&band(b))
            .then(unread_rank(a).cmp(&unread_rank(b)))
            .then(b.active_ms().cmp(&a.active_ms()))
            .then(b.started().cmp(&a.started()))
            .then(display_name(a).cmp(&display_name(b)))
    });
}

/// The name to show. A user rename and the harness name compete by
/// recency: the more recently changed wins. The store's write path makes
/// the exception - the harness's FIRST name is stamped older than a rename
/// that preceded it, so a name you set while the agent was nameless keeps
/// winning. Falls back to the pane title (also in `name`), else
/// `kind · session`.
fn display_name(a: &Agent) -> String {
    let user = a.user_name.as_deref().filter(|s| !s.is_empty());
    let harness = a.name.as_deref().filter(|s| !s.is_empty());
    match (user, harness) {
        (Some(u), Some(h)) => {
            if a.user_name_ms.unwrap_or(0) >= a.name_ms.unwrap_or(0) {
                u.to_string()
            } else {
                h.to_string()
            }
        }
        (Some(u), None) => u.to_string(),
        (None, Some(h)) => h.to_string(),
        (None, None) => match a.session.as_deref() {
            Some(s) if !s.is_empty() => format!("{} · {}", a.kind, s),
            _ => a.kind.clone(),
        },
    }
}

/// Pick the matcher from the query, fff-style: a query with regex
/// metacharacters is a regex; anything else is a plain substring. The
/// caller falls back to fuzzy when the chosen matcher finds nothing.
fn detect_mode(q: &str) -> SearchMode {
    const META: &[char] =
        &['^', '$', '*', '+', '?', '(', ')', '[', ']', '{', '}', '|', '\\'];
    if q.chars().any(|c| META.contains(&c)) {
        SearchMode::Regex
    } else {
        SearchMode::Plain
    }
}

fn mode_label(m: SearchMode) -> &'static str {
    match m {
        SearchMode::Plain => "plain",
        SearchMode::Regex => "regex",
        SearchMode::Fuzzy => "fuzzy",
    }
}

/// Run content search over the live panes: auto-detect the matcher, and
/// fall back to fuzzy when it finds nothing (an unmatched query, or a
/// half-typed regex that will not compile). Returns the mode that
/// actually produced the hits and the snippet per matching pane.
fn run_content_search(
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
    // The currently-selected agent id, resolved against the CURRENT rows.
    // `.get` on both sides: a stale index (rows just replaced under us)
    // must never index out of bounds.
    let keep = p
        .view
        .get(p.sel)
        .and_then(|&i| p.rows.get(i))
        .map(|a| (a.id.clone(), a.pane));
    pick_refilter_keep(p, keep, true);
}

/// Rebuild `view`/`sel`/`lines`, restoring the highlight to `keep`'s agent
/// if it survived the filter. Callers that replace `rows` pass the id they
/// captured from the OLD rows, since the internal `view`/`sel` no longer
/// index the new set.
fn pick_refilter_keep(
    p: &mut Picker,
    keep: Option<(String, Option<i64>)>,
    reset_scroll: bool,
) {
    let needle = p.filter.trim().to_string();
    // Refresh the content-match set when content search is on. The grep
    // runs in tmux over the live grids (`panes_search`); only the needle
    // and the matches cross the ABI, so it is cheap enough per keystroke.
    if p.content_search && !needle.is_empty() {
        let panes: Vec<PaneId> = p
            .rows
            .iter()
            .filter(|a| a.live())
            .filter_map(|a| a.pane.map(|x| PaneId(x as u32)))
            .collect();
        let (mode, hits) = run_content_search(&panes, &needle);
        p.content_mode = mode;
        p.content_hits = hits;
    } else {
        p.content_hits = HashMap::new();
    }
    p.view = p
        .rows
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            rank(&haystack(a), &needle).is_some()
                || a.pane
                    .map(|pn| p.content_hits.contains_key(&(pn as u32)))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();
    p.sel = keep
        .and_then(|(id, pane)| {
            // Prefer the id; fall back to the pane, which survives an
            // id migration (prov -> durable) that the id would miss.
            p.view
                .iter()
                .position(|&i| p.rows[i].id == id)
                .or_else(|| {
                    pane.and_then(|pn| {
                        p.view.iter().position(|&i| p.rows[i].pane == Some(pn))
                    })
                })
        })
        .unwrap_or(0);
    if p.sel >= p.view.len() {
        p.sel = p.view.len().saturating_sub(1);
    }
    p.rebuild_lines();
    // Filter typing snaps to the top; a refresh keeps the scroll where it
    // is (then just nudges to keep the selection visible).
    if reset_scroll {
        p.top = 0;
    } else if p.top >= p.lines.len() {
        p.top = p.lines.len().saturating_sub(1);
    }
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

/// A short, readable key label: `C-f` shows as `^F`.
fn pretty_key(k: &str) -> String {
    if let Some(rest) = k.strip_prefix("C-") {
        format!("^{}", rest.to_uppercase())
    } else {
        keyname(k).to_string()
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

/// Coloured status glyph. Magenta for an archived agent, dim for a
/// finished one.
fn badge(a: &Agent) -> String {
    if a.life == "archived" {
        return "\x1b[35m◆\x1b[0m".into();
    }
    if !a.live() {
        return "\x1b[2m·\x1b[0m".into();
    }
    match a.status.as_str() {
        "needs_input" => "\x1b[1;33m!\x1b[0m".into(),
        "working" => "\x1b[32m●\x1b[0m".into(),
        // Unread waiting: bright, bold, filled. Read waiting: hollow.
        "waiting" if a.unread() => "\x1b[1;96m◉\x1b[0m".into(),
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
    let selected = if p.marked.is_empty() {
        String::new()
    } else {
        format!(", {} selected", p.marked.len())
    };
    let content_tag = if p.content_search {
        format!(", {}", mode_label(p.content_mode))
    } else {
        String::new()
    };
    let unread = p.rows.iter().filter(|a| a.unread()).count();
    let unread_tag = if unread > 0 {
        format!(", {unread} unread")
    } else {
        String::new()
    };
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1m agents\x1b[0m \x1b[2m({live} live{}{unread_tag}{content_tag}{selected})\x1b[0m",
        if p.show_history { ", +history" } else { "" },
    ));
    if p.renaming {
        // Rename mode takes over the prompt line, with a block cursor.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2mrename\x1b[0m {}\x1b[7m \x1b[0m",
            p.rename_buf
        ));
    } else if p.filtering {
        // Focused: show the query with a block cursor.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2msearch\x1b[0m {}\x1b[7m \x1b[0m",
            p.filter
        ));
    } else if p.filter.is_empty() {
        // Idle, no query: a hint. The box is reached by navigating up.
        out.push_str(
            "\x1b[2;1H  \x1b[2msearch\x1b[0m \x1b[2m(/ or ↑ to search)\x1b[0m",
        );
    } else {
        // Idle, query applied: show it, no cursor.
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2msearch\x1b[0m {}",
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
                    let marked = p.marked.contains(&a.id);
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
                    // An archived row (only in the history view) says so,
                    // so the `a` un-archive is obvious. Otherwise a
                    // content-search hit shows the matching line, else the
                    // reported task.
                    let snip = a
                        .pane
                        .and_then(|pn| p.content_hits.get(&(pn as u32)))
                        .filter(|_| p.content_search);
                    if a.life == "archived" {
                        label = format!("{label}  ·  archived");
                    } else if let Some(sn) = snip {
                        label = format!("{label}  ·  {}", sn.trim());
                    } else if let Some(t) =
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
                    let pad = list_w.saturating_sub(1);
                    if cur {
                        // The cursor row: reverse video across the width.
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[7m{:<pad$}\x1b[0m",
                            strip_sgr(&shown),
                        ));
                    } else if marked {
                        // A marked row: a full-width highlight band, distinct
                        // from the cursor's reverse. Explicit fg+bg so it
                        // reads on any theme; strip the badge colours so they
                        // do not reset the band mid-row.
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[97;44m{:<pad$}\x1b[0m",
                            strip_sgr(&shown),
                        ));
                    } else {
                        out.push_str(&format!("\x1b[{row};1H{shown}"));
                    }
                    // "You are here": the pane the picker was opened from
                    // gets a bright left border, drawn last so it shows over
                    // any row state (cursor, marked, or plain).
                    let here =
                        a.live() && a.pane.map(|pn| pn as u32) == p.current_pane;
                    if here {
                        let g = if cur { "▸" } else { "▎" };
                        out.push_str(&format!("\x1b[{row};1H\x1b[1;94m{g}\x1b[0m"));
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
    let ctok = if p.content_search {
        format!("{} {}", pretty_key(&k.content), mode_label(p.content_mode))
    } else {
        format!("{} contents", pretty_key(&k.content))
    };
    // The archive key un-archives when the highlighted row is archived.
    let cursor_archived = p
        .view
        .get(p.sel)
        .and_then(|&i| p.rows.get(i))
        .is_some_and(|a| a.life == "archived");
    let arch = if cursor_archived { "unarch" } else { "arch" };
    let footer = if p.renaming {
        "type a name · Enter accept · Esc cancel".to_string()
    } else if p.filtering {
        format!("type to search · {ctok} · Esc unfocus")
    } else {
        format!(
            "j/k move · gg/G ends · {} search · {} jump · {ctok} · {} rename · {} {arch} · +/- size · q/{} close",
            keyname(&k.filter),
            keyname(&k.jump),
            keyname(&k.rename),
            keyname(&k.archive),
            keyname(&k.close),
        )
    };
    let footer = clip(&footer, list_w.saturating_sub(4));
    // Light up the content-search hotkey while it is on.
    let footer = if p.content_search {
        footer.replacen(&ctok, &format!("\x1b[0;7m{ctok}\x1b[0;2m"), 1)
    } else {
        footer
    };
    out.push_str(&format!("\x1b[{h};1H  \x1b[2m{}\x1b[0m", footer));

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
