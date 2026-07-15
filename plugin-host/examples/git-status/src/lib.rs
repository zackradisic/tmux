//! Pane-scoped git status for the tmux status line.
//!
//! Each pane instance publishes `@git_branch` and `@git_dirty` as user
//! options on its own pane. Status-line formats resolve `#{@...}` through
//! the active pane's options first, so the status line always reflects the
//! pane you are in and empties out in panes that are not inside a git
//! repository.
//!
//! Load (tmux.conf):
//!   load-plugin -s pane -c run-process -c write-options \
//!       ~/.tmux/plugins/git_status.wasm
//!   set -g status-right '#{@git_branch}#{?@git_dirty,*,} | %H:%M'
//!
//! Only the pane that is active in its window does git work; inactive
//! panes just tick. `-o interval_ms=<n>` tunes the poll (default 5000).
//!
//! Build: cargo build -p git-status --target wasm32-unknown-unknown --release

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
}

impl Plugin for GitStatus {
    const NAME: &'static str = "git-status";
    type Config = Config;

    fn init(ctx: &Ctx, config: Config) -> Result<Self, String> {
        let interval: u64 = config
            .interval_ms
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5000)
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

        // Instant refresh when this pane gains focus; the poll loop covers
        // everything else (cwd changes, commits, detached sessions).
        ctx.subscribe(&["pane-focus-in"]).map_err(|e| e.message.clone())?;

        ctx.spawn(async move {
            loop {
                refresh(pane).await;
                if sleep_ms(interval).await.is_err() {
                    return; // instance torn down
                }
            }
        });

        Ok(Self { pane })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if event.event == "pane-focus-in" {
            let pane = self.pane;
            ctx.spawn(async move {
                refresh(pane).await;
            });
        }
    }
}

async fn refresh(pane: PaneId) {
    // Live pane info; bail quietly if the pane died under us.
    let Ok(info) = resolve_pane(pane) else { return };

    // Only the active pane of the window does git work.
    if info.get("active").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    let Some(cwd) = info.get("cwd").and_then(|v| v.as_str()).map(String::from)
    else {
        publish(pane, "", false);
        return;
    };

    let Ok(out) = run_job(GIT_PROBE, Some(&cwd)).await else { return };

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

/// The host skips the redraw when values are unchanged, so writing every
/// poll is cheap.
fn publish(pane: PaneId, branch: &str, dirty: bool) {
    let _ = set_option_in(OptionTarget::Pane(pane), "@git_branch", branch);
    let _ = set_option_in(
        OptionTarget::Pane(pane),
        "@git_dirty",
        if dirty { "1" } else { "" },
    );
}

tmux_plugin!(GitStatus);
