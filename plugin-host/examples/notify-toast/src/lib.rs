//! Notification toasts as floating panes.
//!
//! Listens for `pane-notification` events (OSC 9 "message" or OSC 777
//! "notify;title;body" emitted by a program in any pane - e.g. a coding
//! agent's stop hook running `printf '\e]9;done\a'`) and shows them in a
//! floating notification pane in the top-right corner.
//!
//! The feed is server-global; the panes are just views of it. One line
//! per notification -
//!   %<pane> [<session>:<window>] <text>
//! - each expiring on its own after `duration_ms`. The view follows the
//! user: it is rendered in every attached session's *current* window and
//! repainted on window switches, so the same feed is always where you
//! are looking. A window does not show lines whose source is that window
//! itself (you can already see the pane that sent them); set
//! show_when_visible=1 to disable that filter.
//!
//! The view is a real pane: click to focus it, scroll it, `join-pane` it
//! into the layout, kill it early - all normal pane operations work.
//!
//! Load (tmux.conf):
//!   load-plugin -s server -c run-command ~/.tmux/plugins/notify_toast.wasm
//!
//! Options (-o): duration_ms (default 6000), width (default 44),
//! show_when_visible=1.
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
    /// "1" also shows lines whose source window is the one on display.
    #[serde(default)]
    show_when_visible: Option<String>,
}

/// Most notification lines shown at once; older ones are dropped early.
const MAX_LINES: usize = 6;

struct Entry {
    seq: u64,
    line: String,
    src_window: Option<u64>,
}

/// One window's rendering of the feed (just the pane and a repaint
/// mutex; the feed itself is global).
#[derive(Default)]
struct View {
    pane: Option<u64>,
    /// Body currently on display, so an unchanged repaint is a no-op.
    /// Without this, repaint's kill+spawn could feed the very events
    /// that trigger repaints - convergence must not depend on which
    /// events tmux fires.
    shown: Option<String>,
    /// A repaint in flight absorbs later requests via `dirty` instead of
    /// racing to spawn a second pane.
    repainting: bool,
    dirty: bool,
}

#[derive(Default)]
struct Shared {
    entries: VecDeque<Entry>,
    views: HashMap<u64, View>,
}

type State = Rc<RefCell<Shared>>;

struct NotifyToast {
    duration_ms: u64,
    width: u64,
    show_when_visible: bool,
    seq: u64,
    state: State,
}

/// Make text safe for a single-quoted tmux argument rendered via
/// printf %b: single quotes swapped for U+2019, control characters
/// flattened, backslashes doubled so %b shows them literally (our own
/// \n separators stay meaningful).
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

/// The windows the feed should currently be rendered in: every attached
/// session's current window.
fn current_windows() -> Vec<u64> {
    let mut out = Vec::new();
    let Ok(sessions) = list_sessions() else { return out };
    let Some(sessions) = sessions.as_array() else { return out };
    for s in sessions {
        if s.get("attached").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        if let Some(curw) = s.get("current_window").and_then(|v| v.as_u64()) {
            if !out.contains(&curw) {
                out.push(curw);
            }
        }
    }
    out
}

/*
 * Reconcile one window's pane with the global feed: kill the old pane
 * and, if this window is an attached session's current window and the
 * (source-filtered) feed is non-empty, spawn a fresh pane showing it.
 * Self-contained and convergent - it recomputes everything each pass, so
 * calling it "too often" is harmless. Serialized per window via the
 * repainting flag; calls arriving mid-repaint set `dirty`.
 */
async fn repaint(
    state: &State,
    window: u64,
    width: u64,
    keeper_secs: u64,
    show_when_visible: bool,
) {
    {
        let mut st = state.borrow_mut();
        let view = st.views.entry(window).or_default();
        if view.repainting {
            view.dirty = true;
            return;
        }
        view.repainting = true;
    }

    loop {
        let on_display = current_windows().contains(&window);
        let (old, shown, body, nlines) = {
            let st = state.borrow();
            let lines: Vec<String> = if on_display {
                st.entries
                    .iter()
                    .filter(|e| {
                        show_when_visible || e.src_window != Some(window)
                    })
                    .map(|e| format!(" {}", e.line))
                    .collect()
            } else {
                Vec::new()
            };
            let view = st.views.get(&window);
            let old = view.and_then(|v| v.pane);
            let shown = view.and_then(|v| v.shown.clone());
            (old, shown, lines.join("\\n"), lines.len())
        };

        // Already showing exactly this? Don't touch anything (the pane
        // may have been closed behind our back, e.g. by the user - then
        // repaint it after all).
        let pane_alive = match old {
            Some(p) => resolve_pane(PaneId(p as u32)).is_ok(),
            None => false,
        };
        if shown.as_deref() == Some(body.as_str()) && (pane_alive || nlines == 0)
        {
            let mut st = state.borrow_mut();
            if let Some(view) = st.views.get_mut(&window) {
                if view.dirty {
                    view.dirty = false;
                    drop(st);
                    continue;
                }
                view.repainting = false;
            }
            return;
        }

        if let Some(old) = old {
            if pane_alive {
                let _ = run_command(&format!("kill-pane -t %{old}")).await;
            }
            if let Some(view) = state.borrow_mut().views.get_mut(&window) {
                view.pane = None;
                view.shown = None;
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
                    if let Some(view) =
                        state.borrow_mut().views.get_mut(&window)
                    {
                        view.pane = new_pane;
                        view.shown =
                            new_pane.is_some().then(|| body.clone());
                    }
                }
            }
        }

        let mut st = state.borrow_mut();
        let Some(view) = st.views.get_mut(&window) else { return };
        if view.dirty {
            view.dirty = false;
            drop(st);
            continue;
        }
        view.repainting = false;
        let gone = view.pane.is_none();
        if gone {
            st.views.remove(&window);
        }
        return;
    }
}

/// Reconcile every window that either shows a pane or should: the union
/// of tracked views and the attached sessions' current windows.
async fn sync_views(
    state: &State,
    width: u64,
    keeper_secs: u64,
    show_when_visible: bool,
) {
    let mut windows = current_windows();
    for &w in state.borrow().views.keys() {
        if !windows.contains(&w) {
            windows.push(w);
        }
    }
    for window in windows {
        repaint(state, window, width, keeper_secs, show_when_visible).await;
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

        ctx.subscribe(&[
            "pane-notification",
            // The view follows the user: repaint on anything that
            // changes which window is on display.
            "session-window-changed",
            "client-session-changed",
            "client-attached",
            "client-detached",
        ])
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
            state: State::default(),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        let width = self.width;
        let keeper_secs =
            self.duration_ms.div_ceil(1000) * (MAX_LINES as u64) + 60;
        let show_when_visible = self.show_when_visible;
        let state = Rc::clone(&self.state);

        match event.event.as_str() {
            "pane-notification" => {}
            // A window-switch (or attach/detach): move the view. Other
            // events (e.g. the implicit lifecycle deliveries - including
            // our own toasts' pane-created/destroyed) are ignored;
            // repaint is idempotent anyway, but no need to churn.
            "session-window-changed" | "client-session-changed"
            | "client-attached" | "client-detached" => {
                ctx.spawn(async move {
                    sync_views(&state, width, keeper_secs, show_when_visible)
                        .await;
                });
                return;
            }
            _ => return,
        }

        let Some(src_pane) = event.scope.pane else { return };
        let src_window = event.scope.window.map(u64::from);
        let Some(msg) = event.data.get("text").and_then(|v| v.as_str()) else {
            return;
        };
        if msg.trim().is_empty() {
            return;
        }

        // "%5 [main:bash] the message", truncated to the pane width.
        let line = {
            let prefix =
                format!("%{src_pane}{}", origin_tag(event.scope.window));
            let room =
                (self.width as usize).saturating_sub(prefix.len() + 5).max(8);
            format!("{prefix} {}", sanitize(msg, room))
        };

        self.seq += 1;
        let seq = self.seq;
        let duration_ms = self.duration_ms;

        ctx.spawn(async move {
            {
                let mut st = state.borrow_mut();
                st.entries.push_back(Entry { seq, line, src_window });
                while st.entries.len() > MAX_LINES {
                    st.entries.pop_front();
                }
            }
            log(&format!("notification from %{src_pane}: added to feed"));
            sync_views(&state, width, keeper_secs, show_when_visible).await;

            // Expire this line, then reconcile again.
            if sleep_ms(duration_ms).await.is_err() {
                return; // instance torn down
            }
            state.borrow_mut().entries.retain(|e| e.seq != seq);
            sync_views(&state, width, keeper_secs, show_when_visible).await;
        });
    }
}

tmux_plugin!(NotifyToast);
