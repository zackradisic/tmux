//! Notification toasts as floating panes.
//!
//! Listens for `pane-notification` events (OSC 9 "message" or OSC 777
//! "notify;title;body" emitted by a program in any pane - e.g. a coding
//! agent's stop hook running `printf '\e]9;done\a'`) and shows the message
//! as a small floating pane in the top-right corner of whatever window the
//! user is currently looking at.
//!
//! Policy:
//!  - one toast per attached session, placed in that session's current
//!    window (floating panes are window-scoped, so the toast must go where
//!    the user is looking, not where the notifying program ran);
//!  - suppressed when the notifying pane's window *is* the current window
//!    (the user can already see it);
//!  - the toast self-dismisses after `duration_ms` (its command exits and
//!    the pane closes); a newer toast for the same window replaces it;
//!  - it is a real pane: click to focus it, scroll it, `join-pane` it into
//!    the layout, kill it early - all normal pane operations work.
//!
//! Load (tmux.conf):
//!   load-plugin -s server -c run-command ~/.tmux/plugins/notify_toast.wasm
//!
//! Options (-o): duration_ms (default 6000), width (default 44).
//!
//! Build: cargo build -p notify-toast --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    duration_ms: Option<String>, // -o values arrive as strings
    #[serde(default)]
    width: Option<String>,
}

struct NotifyToast {
    duration_ms: u64,
    width: u64,
    /// Live toast per window (window id -> toast pane id), so a newer
    /// notification replaces a stale toast instead of stacking under it.
    toasts: Rc<RefCell<HashMap<u64, u64>>>,
}

/// Make a notification message safe to embed in a single-quoted tmux
/// argument: the only dangerous character is the single quote itself
/// (swapped for U+2019), and control characters are flattened to spaces.
/// The message reaches the toast via an environment variable, so the shell
/// never parses it.
fn sanitize(msg: &str, max_chars: usize) -> String {
    msg.chars()
        .map(|c| match c {
            '\'' => '\u{2019}',
            c if c.is_control() => ' ',
            c => c,
        })
        .take(max_chars)
        .collect()
}

impl Plugin for NotifyToast {
    const NAME: &'static str = "notify-toast";
    type Config = Config;

    fn init(ctx: &Ctx, config: Config) -> Result<Self, String> {
        let me = self_info().map_err(|e| e.message.clone())?;
        if me.pointer("/scope/type").and_then(|v| v.as_str()) != Some("server") {
            return Err("notify-toast must be loaded with -s server".into());
        }

        ctx.subscribe(&["pane-notification"])
            .map_err(|e| e.message.clone())?;

        Ok(Self {
            duration_ms: config
                .duration_ms
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6_000)
                .clamp(1_000, 300_000),
            width: config
                .width
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(44)
                .clamp(20, 120),
            toasts: Rc::new(RefCell::new(HashMap::new())),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if event.event != "pane-notification" {
            return;
        }
        let Some(src_pane) = event.scope.pane else { return };
        let src_window = event.scope.window;
        let Some(msg) = event
            .data
            .get("text")
            .and_then(|v| v.as_str())
            .map(|m| sanitize(m, 200))
        else {
            return;
        };
        if msg.trim().is_empty() {
            return;
        }

        let duration_ms = self.duration_ms;
        let width = self.width;
        let toasts = Rc::clone(&self.toasts);
        ctx.spawn(async move {
            let Ok(sessions) = list_sessions() else { return };
            let Some(sessions) = sessions.as_array().cloned() else { return };
            for s in sessions {
                if s.get("attached").and_then(|v| v.as_bool()) != Some(true) {
                    continue;
                }
                let (Some(sid), Some(curw)) = (
                    s.get("id").and_then(|v| v.as_u64()),
                    s.get("current_window").and_then(|v| v.as_u64()),
                ) else {
                    continue;
                };
                // The user is already looking at the notifying window.
                if src_window == Some(curw as u32) {
                    continue;
                }
                show_toast(
                    &toasts, sid, curw, src_pane, &msg, width, duration_ms,
                )
                .await;
            }
        });
    }
}

async fn show_toast(
    toasts: &RefCell<HashMap<u64, u64>>,
    session: u64,
    window: u64,
    src_pane: u32,
    msg: &str,
    width: u64,
    duration_ms: u64,
) {
    let Ok(before) = resolve_window(WindowId(window as u32)) else { return };
    let win_width = before.get("width").and_then(|v| v.as_u64()).unwrap_or(80);
    let pre: Vec<u64> = pane_ids(&before);

    // Replace a still-live toast in this window rather than stacking.
    // (Separate statement: an if-let scrutinee's RefMut would live across
    // the await in edition 2021.)
    let old = toasts.borrow_mut().remove(&window);
    if let Some(old) = old {
        let _ = run_command(&format!("kill-pane -t %{old}")).await;
    }

    // Height 5 = border + blank + message + source line. The message goes
    // through an environment variable (-e) so neither tmux's parser nor the
    // shell ever interprets it; sanitize() already removed single quotes.
    let x = win_width.saturating_sub(width);
    let secs = duration_ms.div_ceil(1000);
    let cmd = format!(
        "new-pane -d -x {width} -y 5 -X {x} -Y 0 -t '${session}:' \
         -T 'notification' -e 'TOAST_MSG={msg}' -e 'TOAST_SRC=%{src_pane}' \
         'printf \"\\n %s\\n [%s]\" \"$TOAST_MSG\" \"$TOAST_SRC\"; \
         sleep {secs}'"
    );
    if run_command(&cmd).await.is_err() {
        return;
    }

    // The toast normally dismisses itself (its command exits and the pane
    // closes); find its id so a follow-up toast can replace it early and,
    // as a safety net (remain-on-exit), reap it after a grace period.
    let Ok(after) = resolve_window(WindowId(window as u32)) else { return };
    let Some(new_pane) =
        pane_ids(&after).into_iter().find(|id| !pre.contains(id))
    else {
        return;
    };
    toasts.borrow_mut().insert(window, new_pane);

    if sleep_ms(duration_ms + 2_000).await.is_err() {
        return; // instance torn down
    }
    let mut map = toasts.borrow_mut();
    if map.get(&window) == Some(&new_pane) {
        map.remove(&window);
        drop(map);
        let _ = run_command(&format!("kill-pane -t %{new_pane}")).await;
    }
}

fn pane_ids(window: &serde_json::Value) -> Vec<u64> {
    window
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default()
}

tmux_plugin!(NotifyToast);
