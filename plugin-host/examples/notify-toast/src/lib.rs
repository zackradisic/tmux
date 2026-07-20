//! Notification toasts as floating panes.
//!
//! Listens for `pane-notification` events (OSC 9 "message" or OSC 777
//! "notify;title;body" emitted by a program in any pane - e.g. a coding
//! agent's stop hook running `printf '\e]9;done\a'`) and shows them in a
//! single floating notification pane in the top-right corner of whatever
//! window the user is currently looking at.
//!
//! The pane aggregates: each notification is one line, formatted
//!   %<pane> [<session>:<window>] <text>
//! and expires on its own after `duration_ms`; the pane grows and shrinks
//! with its contents and disappears when the last line expires.
//!
//! Policy:
//!  - one notification pane per window, placed in each attached session's
//!    current window (floating panes are window-scoped, so the toast must
//!    go where the user is looking, not where the notifying program ran);
//!  - suppressed when the notifying pane's window *is* the current window
//!    (the user can already see it) - `plugin-log` says so;
//!  - it is a real pane: click to focus it, scroll it, `join-pane` it into
//!    the layout, kill it early - all normal pane operations work.
//!
//! Load (tmux.conf):
//!   load-plugin -s server -c run-command ~/.tmux/plugins/notify_toast.wasm
//!
//! Options (-o): duration_ms (default 6000), width (default 44),
//! show_when_visible=1 (show even when the source window is on display -
//! useful for testing in a single window).
//!
//! Build: cargo build -p notify-toast --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    duration_ms: Option<String>, // -o values arrive as strings
    #[serde(default)]
    width: Option<String>,
    /// "1" disables the source-is-visible suppression (testing/demos).
    #[serde(default)]
    show_when_visible: Option<String>,
}

/// Most notification lines shown at once; older ones are dropped early.
const MAX_LINES: usize = 6;

struct Entry {
    seq: u64,
    line: String,
}

/// One window's notification pane and its pending lines.
#[derive(Default)]
struct WinState {
    entries: VecDeque<Entry>,
    pane: Option<u64>,
    /// Repaint mutex: a repaint in flight absorbs later requests via
    /// `dirty` instead of racing to spawn a second pane.
    repainting: bool,
    dirty: bool,
}

type Wins = Rc<RefCell<HashMap<u64, WinState>>>;

struct NotifyToast {
    duration_ms: u64,
    width: u64,
    show_when_visible: bool,
    seq: u64,
    wins: Wins,
}

/// Make text safe for a single-quoted tmux argument rendered via
/// printf %b: single quotes swapped for U+2019, control characters
/// flattened, backslashes doubled so %b shows them literally (the
/// bridge's own \n separators stay meaningful).
fn sanitize(msg: &str, max_chars: usize) -> String {
    let flat: String = msg
        .chars()
        .map(|c| match c {
            '\'' => '\u{2019}',
            c if c.is_control() => ' ',
            c => c,
        })
        .take(max_chars)
        .collect();
    flat.replace('\\', "\\\\")
}

/// Best-effort "[session:window]" tag for the notifying pane.
fn origin_tag(src_window: Option<u32>) -> String {
    let Some(window) = src_window else { return String::new() };
    let Ok(wi) = resolve_window(WindowId(window)) else {
        return String::new();
    };
    let wname = wi.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let sname = wi
        .get("sessions")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_u64())
        .and_then(|sid| resolve_session(SessionId(sid as u32)).ok())
        .and_then(|s| {
            s.get("name").and_then(|v| v.as_str()).map(String::from)
        })
        .unwrap_or_else(|| "?".into());
    format!(" [{sname}:{wname}]")
}

fn pane_ids(window: &serde_json::Value) -> Vec<u64> {
    window
        .get("panes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default()
}

/*
 * Rebuild the notification pane for a window from its current entries:
 * kill the old pane, spawn a fresh one sized to the lines (or nothing if
 * all lines have expired). Serialized per window via the repainting flag;
 * calls arriving mid-repaint set `dirty` and are absorbed by the loop.
 */
async fn repaint(wins: &Wins, window: u64, width: u64, keeper_secs: u64) {
    {
        let mut m = wins.borrow_mut();
        let Some(ws) = m.get_mut(&window) else { return };
        if ws.repainting {
            ws.dirty = true;
            return;
        }
        ws.repainting = true;
    }

    loop {
        let (old, body, nlines) = {
            let m = wins.borrow();
            let Some(ws) = m.get(&window) else { return };
            let body = ws
                .entries
                .iter()
                .map(|e| format!(" {}", e.line))
                .collect::<Vec<_>>()
                .join("\\n");
            (ws.pane, body, ws.entries.len())
        };

        if let Some(old) = old {
            let _ = run_command(&format!("kill-pane -t %{old}")).await;
            if let Some(ws) = wins.borrow_mut().get_mut(&window) {
                ws.pane = None;
            }
        }

        if nlines > 0 {
            let height = nlines as u64 + 3;
            if let Ok(before) = resolve_window(WindowId(window as u32)) {
                let win_width =
                    before.get("width").and_then(|v| v.as_u64()).unwrap_or(80);
                let x = win_width.saturating_sub(width);
                let pre = pane_ids(&before);
                let cmd = format!(
                    "new-pane -d -x {width} -y {height} -X {x} -Y 0 \
                     -t '@{window}' -T 'notifications' -e 'TOAST_BODY={body}' \
                     'printf \"%b\" \"\\n$TOAST_BODY\\n\"; \
                     sleep {keeper_secs}'"
                );
                if run_command(&cmd).await.is_ok() {
                    let new_pane = resolve_window(WindowId(window as u32))
                        .ok()
                        .and_then(|after| {
                            pane_ids(&after)
                                .into_iter()
                                .find(|id| !pre.contains(id))
                        });
                    if let Some(ws) = wins.borrow_mut().get_mut(&window) {
                        ws.pane = new_pane;
                    }
                }
            }
        }

        let mut m = wins.borrow_mut();
        let Some(ws) = m.get_mut(&window) else { return };
        if ws.dirty {
            ws.dirty = false;
            drop(m);
            continue;
        }
        ws.repainting = false;
        let done = ws.entries.is_empty() && ws.pane.is_none();
        if done {
            m.remove(&window);
        }
        return;
    }
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
            show_when_visible: config.show_when_visible.as_deref()
                == Some("1"),
            seq: 0,
            wins: Rc::new(RefCell::new(HashMap::new())),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if event.event != "pane-notification" {
            return;
        }
        let Some(src_pane) = event.scope.pane else { return };
        let src_window = event.scope.window;
        let Some(msg) = event.data.get("text").and_then(|v| v.as_str()) else {
            return;
        };
        if msg.trim().is_empty() {
            return;
        }

        // "%5 [main:bash] the message", truncated to the pane width.
        let line = {
            let prefix = format!("%{src_pane}{}", origin_tag(src_window));
            let room =
                (self.width as usize).saturating_sub(prefix.len() + 5).max(8);
            format!("{prefix} {}", sanitize(msg, room))
        };

        self.seq += 1;
        let seq = self.seq;
        let duration_ms = self.duration_ms;
        let width = self.width;
        let keeper_secs = duration_ms.div_ceil(1000) * (MAX_LINES as u64) + 60;
        let show_when_visible = self.show_when_visible;
        let wins = Rc::clone(&self.wins);

        ctx.spawn(async move {
            let Ok(sessions) = list_sessions() else { return };
            let Some(sessions) = sessions.as_array().cloned() else { return };
            let mut targets: Vec<u64> = Vec::new();
            for s in &sessions {
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
                if !show_when_visible && src_window == Some(curw as u32) {
                    log(&format!(
                        "notification from %{src_pane}: source window is \
                         visible in session ${sid}, suppressed (set -o \
                         show_when_visible=1 to disable)"
                    ));
                    continue;
                }
                if !targets.contains(&curw) {
                    targets.push(curw);
                }
            }
            if targets.is_empty() {
                return;
            }

            for &window in &targets {
                {
                    let mut m = wins.borrow_mut();
                    let ws = m.entry(window).or_default();
                    ws.entries.push_back(Entry {
                        seq,
                        line: line.clone(),
                    });
                    while ws.entries.len() > MAX_LINES {
                        ws.entries.pop_front();
                    }
                }
                repaint(&wins, window, width, keeper_secs).await;
            }
            log(&format!(
                "notification from %{src_pane}: shown in {} window(s)",
                targets.len()
            ));

            // Expire this line everywhere it was shown, then repaint.
            if sleep_ms(duration_ms).await.is_err() {
                return; // instance torn down
            }
            for &window in &targets {
                {
                    let mut m = wins.borrow_mut();
                    let Some(ws) = m.get_mut(&window) else { continue };
                    ws.entries.retain(|e| e.seq != seq);
                }
                repaint(&wins, window, width, keeper_secs).await;
            }
        });
    }
}

tmux_plugin!(NotifyToast);
