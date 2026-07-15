//! Pane-scoped git status for the tmux status line.
//!
//! Each pane instance publishes `@git_branch` and `@git_dirty` as user
//! options on its own pane. Status-line formats resolve `#{@...}` through
//! the active pane's options first, so the status line always reflects the
//! pane you are in and empties out in panes that are not inside a git
//! repository.
//!
//! Refresh triggers:
//!  - `pane-prompt` (OSC 133;A from shell integration): a command just
//!    finished in this pane -> refresh immediately. Enable by making the
//!    shell emit the mark, e.g. in ~/.bashrc:
//!      PROMPT_COMMAND='printf "\e]133;A\a"'"${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
//!  - `pane-focus-in`: refresh now and poll every `interval_ms` (default
//!    10000) while the pane stays focused.
//!  - `pane-focus-out`: polling stops entirely (background panes cost
//!    nothing; they are refreshed again the moment they regain focus).
//!
//! A slow `git status` can never pile up: refreshes are skipped while one
//! is already in flight, and tmux itself never blocks (the probe runs as
//! an async job).
//!
//! Load (tmux.conf):
//!   load-plugin -s pane -c run-process -c write-options \
//!       ~/.tmux/plugins/git_status.wasm
//!   set -g status-right '#{@git_branch}#{?@git_dirty,*,} | %H:%M'
//!
//! Build: cargo build -p git-status --target wasm32-unknown-unknown --release

use std::cell::Cell;
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    interval_ms: Option<String>, // -o values arrive as strings
}

/// One job that reports branch and dirtiness with unambiguous markers
/// (detached HEAD falls back to the short commit hash). Runs with the
/// pane's directory as cwd, so nothing from the path is interpolated
/// into the command line.
const GIT_PROBE: &str = concat!(
    r#"echo "B:$(git symbolic-ref --short HEAD 2>/dev/null "#,
    r#"|| git rev-parse --short HEAD 2>/dev/null)"; "#,
    r#"echo "D:$(git status --porcelain 2>/dev/null | head -1)""#,
);

struct GitStatus {
    pane: PaneId,
    interval: u64,
    /// Poll-loop generation: bumping it makes any live loop exit at its
    /// next wakeup. Shared with the loop tasks via Rc (guest is
    /// single-threaded).
    epoch: Rc<Cell<u64>>,
    /// Refresh-in-flight guard: prompt bursts and overlapping triggers
    /// collapse into the probe already running.
    in_flight: Rc<Cell<bool>>,
}

impl GitStatus {
    /// Start (or restart) the focused-poll loop; any previous loop dies.
    fn start_polling(&self, ctx: &Ctx) {
        let my_epoch = self.epoch.get() + 1;
        self.epoch.set(my_epoch);

        let epoch = Rc::clone(&self.epoch);
        let in_flight = Rc::clone(&self.in_flight);
        let pane = self.pane;
        let interval = self.interval;
        ctx.spawn(async move {
            loop {
                refresh(pane, &in_flight).await;
                if sleep_ms(interval).await.is_err() {
                    return; // instance torn down
                }
                if epoch.get() != my_epoch {
                    return; // unfocused (or superseded by a newer loop)
                }
            }
        });
    }

    fn stop_polling(&self) {
        self.epoch.set(self.epoch.get() + 1);
    }
}

impl Plugin for GitStatus {
    const NAME: &'static str = "git-status";
    type Config = Config;

    fn init(ctx: &Ctx, config: Config) -> Result<Self, String> {
        let interval: u64 = config
            .interval_ms
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10_000)
            .max(250);

        let me = self_info().map_err(|e| e.message.clone())?;
        if me.pointer("/scope/type").and_then(|v| v.as_str()) != Some("pane") {
            return Err("git-status must be loaded with -s pane".into());
        }
        let pane = PaneId(
            me.pointer("/scope/id")
                .and_then(|v| v.as_u64())
                .ok_or("missing pane id in scope")? as u32,
        );

        ctx.subscribe(&["pane-focus-in", "pane-focus-out", "pane-prompt"])
            .map_err(|e| e.message.clone())?;

        let plugin = Self {
            pane,
            interval,
            epoch: Rc::new(Cell::new(0)),
            in_flight: Rc::new(Cell::new(false)),
        };

        // Focus events only fire on changes, so seed the initial state: if
        // this pane is currently its window's active pane, poll until a
        // focus-out says otherwise; either way publish once now.
        let active = resolve_pane(pane)
            .ok()
            .and_then(|i| i.get("active").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        if active {
            plugin.start_polling(ctx); // first loop iteration refreshes
        } else {
            let in_flight = Rc::clone(&plugin.in_flight);
            ctx.spawn(async move {
                refresh_even_if_inactive(pane, &in_flight).await;
            });
        }

        Ok(plugin)
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.event.as_str() {
            // Focused: refresh now (loop's first iteration) + keep polling.
            "pane-focus-in" => self.start_polling(ctx),
            // Unfocused: stop polling entirely.
            "pane-focus-out" => self.stop_polling(),
            // A command just finished in this pane (OSC 133;A).
            "pane-prompt" => {
                let pane = self.pane;
                let in_flight = Rc::clone(&self.in_flight);
                ctx.spawn(async move {
                    refresh(pane, &in_flight).await;
                });
            }
            _ => {}
        }
    }
}

async fn refresh(pane: PaneId, in_flight: &Cell<bool>) {
    // Background panes are not shown in the status line; they catch up on
    // their next focus-in or prompt-after-focus.
    let Ok(info) = resolve_pane(pane) else { return };
    if info.get("active").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    probe_and_publish(pane, info, in_flight).await;
}

/// Used once at init so a freshly-loaded plugin publishes something even
/// for panes that are not active right now.
async fn refresh_even_if_inactive(pane: PaneId, in_flight: &Cell<bool>) {
    let Ok(info) = resolve_pane(pane) else { return };
    probe_and_publish(pane, info, in_flight).await;
}

async fn probe_and_publish(
    pane: PaneId,
    info: serde_json::Value,
    in_flight: &Cell<bool>,
) {
    if in_flight.get() {
        return; // a probe is already running; it will publish shortly
    }
    let Some(cwd) = info.get("cwd").and_then(|v| v.as_str()).map(String::from)
    else {
        publish(pane, "", false);
        return;
    };

    in_flight.set(true);
    let result = run_job(GIT_PROBE, Some(&cwd)).await;
    in_flight.set(false);

    let Ok(out) = result else { return };
    let mut branch = "";
    let mut dirty = false;
    for line in out.output.lines() {
        if let Some(b) = line.strip_prefix("B:") {
            branch = b.trim();
        } else if let Some(d) = line.strip_prefix("D:") {
            dirty = !d.trim().is_empty();
        }
    }
    publish(pane, branch, dirty && !branch.is_empty());
}

/// The host skips the status redraw when values are unchanged, so
/// publishing every probe is cheap.
fn publish(pane: PaneId, branch: &str, dirty: bool) {
    let _ = set_option_in(OptionTarget::Pane(pane), "@git_branch", branch);
    let _ = set_option_in(
        OptionTarget::Pane(pane),
        "@git_dirty",
        if dirty { "1" } else { "" },
    );
}

tmux_plugin!(GitStatus);
