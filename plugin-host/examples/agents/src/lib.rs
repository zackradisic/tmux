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
//! Two halves, one crate (see `provider.rs` and `view.rs`). The provider
//! half sees one server: it detects, resolves, keeps the store and answers
//! the `list`, `capture`, `search` and `act` services and publishes the
//! `changed` topic. The view half owns the picker and merges the local
//! roster with the rosters of the providers on every linked server
//! (`remote-attach` pushes this plugin there as a provider), grouped by
//! server. `Ctx::role()` says which halves this instance runs: `both` on
//! the local server, `provider` on a remote. Two servers on one machine
//! share `store.db` when they share `XDG_DATA_HOME`.
//!
//! Manifest:
//!
//!   [plugins.agents]
//!   path = "agents.wasm"
//!   scope = "server"
//!   caps = ["capture-pane", "run-command", "mode", "db", "env-read",
//!           "pane-fds", "fs-read", "fs-list", "service-serve",
//!           "service-call"]
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

mod provider;
mod resolve;
mod store;
mod view;

use provider::{DEFAULT_COMMANDS, STATUSES};
use view::{Picker, Remotes};

pub(crate) const HISTORY_MAX: i64 = 100;

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
pub(crate) struct PickKeys {
    pub jump: String,
    pub filter: String,
    pub archive: String,
    pub history: String,
    pub close: String,
    pub content: String,
    pub rename: String,
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

pub(crate) struct Config {
    pub keep_days: i64,
    pub commands: Vec<String>,
    pub keys: PickKeys,
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
// the plugin
// ---------------------------------------------------------------------------

struct Agents {
    cfg: Rc<Config>,
    role: Role,
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
    /// The rosters of the providers on linked servers (view half).
    remotes: Rc<RefCell<Remotes>>,
}

impl Plugin for Agents {
    const NAME: &'static str = "agents";
    type Config = AgentsConfig;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        let cfg = Rc::new(Config::from(&config)?);
        let role = ctx.role();
        let mut events = vec![
            "plugin-command",
            "pane-created",
            "pane-destroyed",
            "pane-command-changed",
        ];
        if role.views() {
            events.extend(["mode-key", "mode-resize", "mode-closed", "link-up", "link-down"]);
        }
        ctx.subscribe(&events).map_err(|e| e.message.clone())?;
        store::migrate_sync()?;
        let picker = Rc::new(RefCell::new(None));
        view::PICKER.with(|c| *c.borrow_mut() = Some(Rc::clone(&picker)));
        if role.provides() {
            provider::register_services();
            ctx.spawn(provider::reconcile(Rc::clone(&cfg)));
        }
        if role.views() {
            // Follow the providers on servers that are linked already.
            for s in service::servers().unwrap_or_default() {
                if !s.local && s.up {
                    view::follow(&s.name);
                }
            }
        }
        Ok(Self {
            cfg,
            role,
            picker,
            busy: Rc::new(Cell::new(false)),
            remotes: Rc::new(RefCell::new(Remotes::default())),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => self.on_command(ctx, &event),
            "pane-created" | "pane-command-changed" => {
                if let Some(pane) = event.scope.pane {
                    let cfg = Rc::clone(&self.cfg);
                    let picker = Rc::clone(&self.picker);
                    let remotes = Rc::clone(&self.remotes);
                    let provides = self.role.provides();
                    ctx.spawn(async move {
                        if provides {
                            provider::classify(pane, cfg).await;
                            provider::broadcast_changed().await;
                        }
                        view::refresh_if_open(&picker, &remotes).await;
                    });
                }
            }
            "pane-destroyed" => {
                if let Some(pane) = event.scope.pane {
                    let picker = Rc::clone(&self.picker);
                    let remotes = Rc::clone(&self.remotes);
                    let provides = self.role.provides();
                    ctx.spawn(async move {
                        if provides {
                            provider::pane_gone(pane).await;
                            provider::broadcast_changed().await;
                        }
                        view::refresh_if_open(&picker, &remotes).await;
                    });
                }
            }
            "link-up" => {
                // A server (re)appeared: follow its roster and fetch it.
                let Some(server) = event.get_str("server").map(str::to_string) else {
                    return;
                };
                view::follow(&server);
                self.remotes.borrow_mut().mark_up(&server);
                let picker = Rc::clone(&self.picker);
                let remotes = Rc::clone(&self.remotes);
                ctx.spawn(async move {
                    view::fetch_remotes(Rc::clone(&remotes), false).await;
                    view::refresh_if_open(&picker, &remotes).await;
                });
            }
            "link-down" => {
                let Some(server) = event.get_str("server").map(str::to_string) else {
                    return;
                };
                self.remotes.borrow_mut().mark_down(&server);
                let picker = Rc::clone(&self.picker);
                let remotes = Rc::clone(&self.remotes);
                ctx.spawn(async move {
                    view::refresh_if_open(&picker, &remotes).await;
                });
            }
            "mode-key" => {
                view::on_mode_key(&self.picker, &self.busy, &self.remotes, ctx, &event)
            }
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
                view::pick_render(p);
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

    /// A view somewhere asks this server's provider.
    fn on_service_request(&mut self, ctx: &Ctx, req: ServiceRequest) {
        if !self.role.provides() {
            let _ = req.fail("this instance is a view");
            return;
        }
        ctx.spawn(provider::handle(req, Rc::clone(&self.cfg)));
    }

    /// A provider on a linked server published its roster.
    fn on_service_event(&mut self, ctx: &Ctx, event: ServiceEvent) {
        if event.topic != provider::TOPIC || event.server == store::LOCAL {
            return;
        }
        let Ok(snap) = event.json::<provider::Snapshot>() else { return };
        self.remotes.borrow_mut().apply(&event.server, snap);
        let picker = Rc::clone(&self.picker);
        let remotes = Rc::clone(&self.remotes);
        ctx.spawn(async move {
            view::refresh_if_open(&picker, &remotes).await;
        });
    }
}

impl Agents {
    fn on_command(&self, ctx: &Ctx, event: &Event) {
        let text = event.get_str("text").unwrap_or("").trim().to_string();
        let verb = text.split_whitespace().next().unwrap_or("").to_string();
        if verb == "pick" {
            if !self.role.views() {
                let _ = display_message("agents: a provider has no picker");
                return;
            }
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
            let remotes = Rc::clone(&self.remotes);
            let client = event.scope.client.map(u64::from);
            let here = event.scope.pane;
            ctx.spawn(view::pick_open(picker, cfg, remotes, client, here));
            return;
        }
        if !self.role.provides() {
            let _ = display_message("agents: this instance is a view");
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
            let remotes = Rc::clone(&self.remotes);
            ctx.spawn(async move {
                provider::on_identify(pane, id, source, cfg).await;
                provider::broadcast_changed().await;
                view::refresh_if_open(&picker, &remotes).await;
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
            let remotes = Rc::clone(&self.remotes);
            ctx.spawn(async move {
                provider::report(pane, verb, task, cfg).await;
                provider::broadcast_changed().await;
                view::refresh_if_open(&picker, &remotes).await;
            });
            return;
        }
        let _ = display_message(&format!(
            "agents: unknown verb {verb:?} (pick|working|needs_input|waiting|done)"
        ));
    }
}

tmux_plugin!(Agents);
