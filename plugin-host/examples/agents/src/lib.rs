//! A roster of the agent CLI sessions running in your panes.
//!
//! Press the hotkey (bind it to `plugin-command agents pick`) to open a
//! floating chooser: one row per agent, grouped by state (needs input,
//! waiting, working, done) and, inside each group, most recently active
//! first. A live preview of the highlighted pane sits to the right.
//! `j`/`k` move, `gg`/`G` jump to the ends, Enter jumps to the pane, `a`
//! archives, `.` folds in the finished ones. `q` or Esc closes the picker.
//! `l` (or Right, or a click on the preview) hands the keyboard to the
//! preview: every key then goes to the agent's pane, so a prompt can be
//! typed and sent without leaving the picker, and the preview shows the
//! echo. `C-]` takes the keyboard back; so does a click on the list, and
//! so does a directional `select-pane` on the float - tmux hands the
//! picker the direction as a `mode-nav` event, so whatever `prefix h` /
//! `prefix l` / `prefix j` / `prefix k` already do for panes, they do
//! inside the picker: left is the list, right is the preview, up and
//! down move the highlight even while the preview has the keyboard. The
//! prefix table runs before the mode sees a key, which is what makes
//! this reachable when every plain key is the agent's. The mouse works
//! on the list too: a click selects a row, a double click jumps to it,
//! the wheel scrolls, a click on the search line focuses it. Over the
//! preview the wheel scrolls the pane's own history (the blit is moved
//! back through it by the host, `mode_preview_scroll`); the row under
//! the preview says how far back it is, wheel down returns to live, and
//! so does any key typed into the pane.
//! `n` starts another agent: a form over the picker (see `newagent`),
//! prefilled from the highlighted row - its session, its pane's folder,
//! its harness - that makes a window in that session running the
//! harness; `C-t` cycles to a new session or a git worktree instead.
//! The cursor opens on the agent whose pane the picker was opened from
//! (the "you are here" row); it lands there even when that row's roster
//! arrives after the popup is up, unless the cursor has been moved since.
//! Press `/` to focus the search box; Esc there unfocuses it and keeps the
//! query. `k` at the top row stays at the top - only `/` reaches the box.
//! `J`/`K` (shift) mark rows into a selection; `a` then archives the whole
//! selection at once (and un-archives when every marked row is archived).
//! `Space` opens an action menu on the selected row - every action the
//! picker has, with the ones that do not apply to that row dimmed. Its
//! items send their own key back into the picker, so the menu is a view
//! of the keymap rather than a second copy of it.
//! `c` copies the agent's durable id - the harness's own session id, with
//! the roster's `kind:` prefix off - to the clipboard of the client that
//! pressed it, so `claude --resume <id>` is a paste away.
//! `w` moves the row out of the "needs input" band into `waiting` when
//! the roster flagged something you have already dealt with, and raises a
//! `waiting` row into the band when you know it needs you and no hook
//! said so. It takes a marked selection too.
//! Esc clears the selection before it closes the picker. `+`/`-` grow and
//! shrink the popup; the size is remembered across opens. The popup opens
//! at a fraction of the window by default. `R` renames the selected agent
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
//! The search box also searches every agent's CONVERSATION, live or long
//! dead: what you typed, what the agent said, which files it touched. A
//! query brings in the finished agents it matches whether or not history
//! is on, best match first, each row showing the line that matched, and
//! the preview opens on the matching turn. The conversation comes from
//! the harness's own transcript file (Claude's `~/.claude/projects/...`
//! jsonl, Codex's rollout), read from a saved cursor when a turn ends,
//! when the agent is archived and when it ends (debounced per agent),
//! condensed to prompts, replies and one line per tool call, and
//! kept in the store for `history_days` - past the harness's own cleanup
//! of the file. It is searched through an inverted index held in memory,
//! so the answer is inside the keystroke. See `transcript.rs` and
//! `index.rs`; the formats live in the host (`transcript_extract`). A
//! finished agent's preview shows that conversation with its Markdown
//! rendered, opened on the match; `Tab` shows it for a live one too, in
//! place of its pane. `l`, Right or a click on it gives it the keyboard:
//! `j`/`k` scroll, `n`/`N` step through the matches, `g`/`G` top and end,
//! Esc back to the list. `i` swaps the preview for an info card: the id,
//! harness and model, where the agent runs (or ran), its working
//! directory and git branch (from the transcript records, or the session
//! file; `y` copies the directory), when it started, what it did (prompts,
//! replies, tool calls, the files it touched most), its transcript, and
//! the command that resumes it.
//!
//! An agent that stopped for you and you have not gotten to yet is
//! UNREAD: it entered `needs_input` or `waiting` more recently than your
//! last acknowledgement. Jumping to its pane or typing into it
//! acknowledges it, and so does `r`; `u` makes it unread again. Moving
//! the cursor over a row does NOT: scrolling past is not reading. Unread
//! rows sort to the top of their band, show their badge on a coloured
//! block with the name in bold, and count in the header.
//!
//! With the Claude shim installed, an agent that stops goes straight to
//! `needs_input`: under `--dangerously-skip-permissions` there are no
//! permission prompts, so "it stopped" IS the signal that it wants you.
//! Mid-turn, a question dialog (`AskUserQuestion`) or a plan waiting for
//! approval (`ExitPlanMode`) is the exact signal: the shim's PreToolUse
//! hook reports `needs_input` with the question as the row's note.
//! `waiting` is the band you move a row to by hand (`w`) once you have
//! judged it; Claude's Notification hook is not used.
//!
//! Three signals drive it, each from its own trusted source:
//!
//!   * Membership + liveness is OBSERVED, never announced. A pane is an
//!     agent when its foreground command is one of `claude|codex|pi|
//!     opencode` (configurable), or it carries an `AI_AGENT`/`OPENCODE`
//!     marker in its environment (read with the `pane_env` host call).
//!     For claude the marker is a hint only: a server started from
//!     inside Claude Code hands `AI_AGENT` to every pane, so a claude
//!     row also needs Claude's own session file to name the pane
//!     (`~/.claude/sessions/<pid>.json`, `tmux` field). `trust_env = "1"`
//!     in the config makes the marker proof again (the tests use it).
//!     The plugin learns of changes from `pane-command-changed`,
//!     `pane-created` and `pane-destroyed`, so a killed or crashed CLI
//!     retires itself - no hook can leave a ghost behind. A pane that
//!     dies while its classification is still in flight is caught by a
//!     liveness check before the row is written, and a sweep every 30 s
//!     retires any row whose pane is gone all the same. This is also
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
//!           "service-call", "send-keys", "run-process", "fs-read-any"]
//!
//!   # A table, not `config = { ... }`: an inline table cannot take the
//!   # launch section below.
//!   [plugins.agents.config]
//!   keep_days = 14      # a finished agent with no stored conversation
//!   history_days = 365  # one with a conversation (its turns, searchable)
//!
//!   # Named launchers for the new-agent form's command field. The name
//!   # is what you type; the line is what runs (through the default
//!   # shell, non-interactive - so spell the flags out, no aliases). A
//!   # launcher named like a harness (claude) is what a new agent from
//!   # one of that harness's rows runs by default. The first word of the
//!   # command is expanded, so `claude-opus --resume <id>` works.
//!   [plugins.agents.config.launch]
//!   claude = "IS_SANDBOX=1 claude --dangerously-skip-permissions"
//!   claude-opus = "claude --dangerously-skip-permissions --model opus"
//!
//! `send-keys` is the picker's interrupt and its typing into the preview;
//! `run-process` and `fs-read-any` are the new-agent form's completion
//! (`formkit::complete::CAPS`, with `fs-list`).
//!
//! The scoped lists (the variables env-read may read, the directories
//! fs-read may reach) do not live in plugins.toml. They come from the
//! sidecar `agents.toml` next to the deployed `agents.wasm` (the release
//! bundle ships it; a dev tree copies `examples/agents/agents.toml` next
//! to the built wasm). Without the sidecar the host trusts the grants as
//! given, and fs-read reaches only the plugin's own sandbox, so the
//! session files that prove a claude are out of reach: grant
//! `fs-read-any` then, or deploy the sidecar.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

mod index;
mod newagent;
mod provider;
mod push;
mod resolve;
mod store;
mod transcript;
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
    history_days: Option<serde_json::Value>,
    commands: Option<Vec<String>>,
    /// Named launchers for the new-agent form: a table of `name = "shell
    /// line"`, or one string of such lines (the `-o` form).
    launch: Option<serde_json::Value>,
    trust_env: Option<String>,
    pick_jump: Option<String>,
    pick_filter: Option<String>,
    pick_archive: Option<String>,
    pick_attention: Option<String>,
    pick_copy: Option<String>,
    pick_menu: Option<String>,
    pick_history: Option<String>,
    pick_archived: Option<String>,
    pick_close: Option<String>,
    pick_content: Option<String>,
    pick_rename: Option<String>,
    pick_interrupt: Option<String>,
    pick_kill: Option<String>,
    pick_focus: Option<String>,
    pick_unfocus: Option<String>,
    pick_new: Option<String>,
    pick_read: Option<String>,
    pick_unread: Option<String>,
}

#[derive(Clone)]
pub(crate) struct PickKeys {
    pub jump: String,
    pub filter: String,
    pub archive: String,
    /// Move the row between the attention band and `waiting` by hand.
    pub attention: String,
    /// Copy the agent's durable id to the clipboard.
    pub copy: String,
    /// Open the action menu on the selected row.
    pub menu: String,
    pub history: String,
    /// Show only the archived rows (turns the history view on).
    pub archived: String,
    pub close: String,
    pub content: String,
    pub rename: String,
    /// Send an interrupt (C-c) to the agent, leaving the pane alone.
    pub interrupt: String,
    /// Kill the agent's pane outright. Asks first.
    pub kill: String,
    /// Hand the keyboard to the preview: keys then go to the agent's
    /// pane. Right and a click on the preview do the same.
    pub focus: String,
    /// Take the keyboard back from the preview. The one key that never
    /// reaches the pane while typing, so it must be one no agent wants:
    /// telnet's escape character by default.
    pub unfocus: String,
    /// Open the new-agent form, prefilled from the highlighted row.
    pub new: String,
    /// Mark the row (or the marked selection) read.
    pub read: String,
    /// Mark it unread again.
    pub unread: String,
}

impl Default for PickKeys {
    fn default() -> Self {
        Self {
            jump: "Enter".into(),
            filter: "/".into(),
            archive: "a".into(),
            attention: "w".into(),
            copy: "c".into(),
            menu: "Space".into(),
            // `.` shows hidden things in nnn and yazi; here it shows the
            // finished rows. `h` used to, but `h`/`l` are left/right now.
            history: ".".into(),
            archived: "A".into(),
            close: "Escape".into(),
            content: "C-f".into(),
            // `r`/`u` read and unread; rename moved to `R`.
            rename: "R".into(),
            interrupt: "x".into(),
            kill: "X".into(),
            focus: "l".into(),
            unfocus: "C-]".into(),
            new: "n".into(),
            read: "r".into(),
            unread: "u".into(),
        }
    }
}

pub(crate) struct Config {
    pub keep_days: i64,
    /// How long an agent with a stored conversation is kept (its turns
    /// and its row), well past the harness's own transcript cleanup.
    pub history_days: i64,
    pub commands: Vec<String>,
    /// The new-agent form's command field completes from these: the
    /// launchers by name (each with the line it runs), then every
    /// detected harness command that no launcher shadows.
    pub launchers: Vec<(String, String)>,
    /// An `AI_AGENT` marker alone makes a claude row (no session file
    /// needed). Off by default; the regress tests turn it on.
    pub trust_env: bool,
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
        let history_days = match &c.history_days {
            None => 365,
            Some(v) => {
                let s = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
                let n: i64 =
                    s.trim().parse().map_err(|_| format!("bad history_days {v}"))?;
                if n < 1 {
                    return Err("history_days must be at least 1".into());
                }
                n
            }
        };
        let commands = match &c.commands {
            Some(v) if !v.is_empty() => v.clone(),
            _ => DEFAULT_COMMANDS.iter().map(|s| s.to_string()).collect(),
        };
        // Launchers: a TOML table (`[plugins.agents.config.launch]`,
        // name = "line") or one string of `name = line` rows, which is
        // what `-o launch=...` can carry. Names sort; a harness command
        // with no launcher of its own follows, running as itself.
        let mut launchers: Vec<(String, String)> = match &c.launch {
            Some(serde_json::Value::Object(m)) => m
                .iter()
                .filter_map(|(k, v)| {
                    let line = v.as_str().map(str::trim).unwrap_or("");
                    (!line.is_empty()).then(|| (k.trim().to_string(), line.to_string()))
                })
                .collect(),
            Some(serde_json::Value::String(s)) => parse_launch_lines(s),
            Some(other) => return Err(format!("bad launch {other}: a table or a string")),
            None => Vec::new(),
        };
        launchers.sort();
        for cmd in &commands {
            if !launchers.iter().any(|(n, _)| n == cmd) {
                launchers.push((cmd.clone(), cmd.clone()));
            }
        }
        let d = PickKeys::default();
        let pick = |v: &Option<String>, def: String| {
            v.as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or(def)
        };
        let trust_env = c
            .trust_env
            .as_deref()
            .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
            .unwrap_or(false);
        Ok(Config {
            keep_days,
            history_days,
            commands,
            launchers,
            trust_env,
            keys: PickKeys {
                jump: pick(&c.pick_jump, d.jump),
                filter: pick(&c.pick_filter, d.filter),
                archive: pick(&c.pick_archive, d.archive),
                attention: pick(&c.pick_attention, d.attention),
                copy: pick(&c.pick_copy, d.copy),
                menu: pick(&c.pick_menu, d.menu),
                history: pick(&c.pick_history, d.history),
                archived: pick(&c.pick_archived, d.archived),
                close: pick(&c.pick_close, d.close),
                content: pick(&c.pick_content, d.content),
                rename: pick(&c.pick_rename, d.rename),
                interrupt: pick(&c.pick_interrupt, d.interrupt),
                kill: pick(&c.pick_kill, d.kill),
                focus: pick(&c.pick_focus, d.focus),
                unfocus: pick(&c.pick_unfocus, d.unfocus),
                new: pick(&c.pick_new, d.new),
                read: pick(&c.pick_read, d.read),
                unread: pick(&c.pick_unread, d.unread),
            },
        })
    }
}

/// `name = shell line` rows, one per line; blank lines and `#` comments
/// are skipped, and so is a row with no `=` or an empty side.
fn parse_launch_lines(s: &str) -> Vec<(String, String)> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (name, line) = l.split_once('=')?;
            let (name, line) = (name.trim(), line.trim());
            (!name.is_empty() && !line.is_empty()).then(|| (name.to_string(), line.to_string()))
        })
        .collect()
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
            events.extend([
                "mode-key",
                "mode-nav",
                "mode-resize",
                "mode-closed",
                "link-up",
                "link-down",
            ]);
        }
        ctx.subscribe(&events).map_err(|e| e.message.clone())?;
        store::migrate_sync()?;
        let picker = Rc::new(RefCell::new(None));
        view::PICKER.with(|c| *c.borrow_mut() = Some(Rc::clone(&picker)));
        if role.provides() {
            provider::register_services();
            // Follow the local mailbox: a message to a Claude agent here is
            // pushed into its session (see push.rs).
            push::follow();
            ctx.spawn(provider::reconcile(Rc::clone(&cfg)));
            ctx.spawn(provider::sweep_loop());
        }
        if role.views() {
            // Follow the providers on servers this side linked to (never an
            // inbound peer: its roster is not ours to pull).
            for s in service::servers().unwrap_or_default() {
                if !s.local && s.up && s.linked {
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
                // A server (re)appeared. Only a server this side linked to
                // is followed and fetched; a peer that linked to us is not
                // ours to pull a roster from.
                let Some(server) = event.get_str("server").map(str::to_string) else {
                    return;
                };
                let linked = service::servers()
                    .unwrap_or_default()
                    .into_iter()
                    .any(|s| s.name == server && s.linked);
                if !linked {
                    return;
                }
                view::follow(&server);
                self.remotes.borrow_mut().mark_up(&server);
                let picker = Rc::clone(&self.picker);
                let remotes = Rc::clone(&self.remotes);
                ctx.spawn(async move {
                    // fetch_remotes repaints per server as it lands.
                    let req = view::list_req_of(&picker);
                    view::fetch_remotes(Rc::clone(&picker), Rc::clone(&remotes), req)
                        .await;
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
            // The new-agent form is a second float of this instance; its
            // events are told apart from the picker's by mode id.
            "mode-key" if newagent::owns(event.get_i64("mode")) => newagent::on_key(ctx, &event),
            "mode-nav" if newagent::owns(event.get_i64("mode")) => {}
            "mode-resize" if newagent::owns(event.get_i64("mode")) => newagent::on_resize(&event),
            "mode-closed" if newagent::owns(event.get_i64("mode")) => newagent::on_closed(),
            "mode-key" => {
                view::on_mode_key(&self.picker, &self.busy, &self.remotes, ctx, &event)
            }
            "mode-nav" => view::on_mode_nav(&self.picker, &self.busy, ctx, &event),
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

    /// A provider on a linked server published its roster, or the local
    /// mailbox stored a message.
    fn on_service_event(&mut self, ctx: &Ctx, event: ServiceEvent) {
        if event.topic == push::TOPIC && event.server == store::LOCAL {
            if !self.role.provides() {
                return;
            }
            let Ok(d) = event.json::<push::Delivered>() else { return };
            let picker = Rc::clone(&self.picker);
            let remotes = Rc::clone(&self.remotes);
            ctx.spawn(async move {
                push::deliver(d).await;
                view::refresh_if_open(&picker, &remotes).await;
            });
            return;
        }
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
        if verb == "menu-key" {
            // "menu-key <key> [x y]": an item of the action menu, handing
            // its key back to the picker. The menu is drawn by tmux, so
            // this is the only way back in. The optional cell makes a
            // mouse key whole (a click needs somewhere to land), which is
            // how a script - or a test - clicks the picker.
            if !self.role.views() {
                return;
            }
            let mut words = text.split_whitespace().skip(1);
            let Some(key) = words.next() else { return };
            let mouse = match (
                words.next().and_then(|v| v.parse::<u32>().ok()),
                words.next().and_then(|v| v.parse::<u32>().ok()),
            ) {
                (Some(x), Some(y)) => Some((x, y)),
                _ => None,
            };
            view::on_menu_key(
                &self.picker,
                &self.busy,
                &self.remotes,
                ctx,
                key.to_string(),
                mouse,
                event.scope.client.map(u64::from),
            );
            return;
        }
        if verb == "message" {
            // "message <agent-id> <text>": leave a message in the agent's
            // mailbox, on this server or the agent's own. The view knows
            // which server an id is on, from its roster.
            if !self.role.views() {
                let _ = display_message("agents: a provider has no roster to address");
                return;
            }
            let mut rest = text.splitn(3, char::is_whitespace).skip(1);
            let Some(id) = rest.next().map(str::to_string) else {
                let _ = display_message("agents: message <agent-id> <text>");
                return;
            };
            let body = rest.next().map(str::to_string).unwrap_or_default();
            if body.trim().is_empty() {
                let _ = display_message("agents: an empty message");
                return;
            }
            let remotes = Rc::clone(&self.remotes);
            let picker = Rc::clone(&self.picker);
            let pane = event.scope.pane;
            let provides = self.role.provides();
            ctx.spawn(async move {
                // Materialize durable ids so a message addressed by one
                // resolves without a picker having opened first (enrich
                // persists the migration and the session-file path).
                if provides {
                    let mut rows = store::live_agents().await.unwrap_or_default();
                    provider::enrich_live(&mut rows).await;
                }
                // The sender is the agent in the calling pane when there is
                // one, so the reader sees an agent id, not a pane number.
                let from = match pane {
                    Some(p) => store::live_by_pane(i64::from(p))
                        .await
                        .ok()
                        .flatten()
                        .map(|a| a.id)
                        .unwrap_or_else(|| format!("%{p}")),
                    None => "command".to_string(),
                };
                view::message_by_id(picker, remotes, &id, &from, body.trim()).await;
            });
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
