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
//! are looking - including the window the notification came from (set
//! show_when_visible=0 to hide lines whose source window is the one on
//! display).
//!
//! The view is a real pane: click to focus it, scroll it, `join-pane` it
//! into the layout, kill it early - all normal pane operations work.
//!
//! Clicking a toast opens the **chooser**: a centered floating panel (a
//! plugin UI mode) listing the feed with a live preview of the selected
//! notification's source pane. `j`/`k` (or arrows) move the selection,
//! `1`-`9` or `Enter` jump to the source pane (and drop the entry), `d`
//! dismisses an entry, `q`/`Escape` closes the panel.
//!
//! Load (tmux.conf):
//!   load-plugin -s server -c run-command -c mode \
//!       ~/.tmux/plugins/notify_toast.wasm
//!
//! Options (-o): duration_ms (default 6000; 0 or "infinite" = lines never
//! expire), width (default 44), show_when_visible (default 1; 0 =
//! suppress in the source window), chooser_width / chooser_height
//! (chooser panel size: cells, or "NN%" of the window; default: most of
//! the window width capped at 90, height sized to the feed).
//!
//! Build: cargo build -p notify-toast --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    duration_ms: Option<String>, // -o values arrive as strings
    #[serde(default)]
    width: Option<String>,
    /// "0" hides lines whose source window is the one on display
    /// (default is to show them everywhere).
    #[serde(default)]
    show_when_visible: Option<String>,
    /// Chooser panel size: a cell count or "NN%" of the window. Values
    /// arrive as strings from -o and as native numbers from a manifest,
    /// so these accept any JSON value.
    #[serde(default)]
    chooser_width: Option<serde_json::Value>,
    #[serde(default)]
    chooser_height: Option<serde_json::Value>,
}

/// A configured dimension: absolute cells or a percentage of the window.
#[derive(Clone, Copy, Serialize, Deserialize)]
enum SizeSpec {
    Cells(u32),
    Percent(u32),
}

impl SizeSpec {
    fn parse(v: Option<&serde_json::Value>) -> Option<Self> {
        let v = v?;
        if let Some(n) = v.as_u64() {
            return Some(SizeSpec::Cells(n as u32));
        }
        let s = v.as_str()?.trim();
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse().ok().map(SizeSpec::Percent);
        }
        s.parse().ok().map(SizeSpec::Cells)
    }

    fn resolve(self, total: u32) -> u32 {
        match self {
            SizeSpec::Cells(n) => n,
            SizeSpec::Percent(p) => total * p.min(100) / 100,
        }
    }
}

/// Most notification lines shown at once; older ones are dropped early.
const MAX_LINES: usize = 6;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    line: String,
    src_window: Option<u64>,
    src_pane: u64,
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

/// The expanded chooser: one open plugin UI mode at a time.
struct Chooser {
    mode: ModeId,
    /// Window the panel currently lives in (updated when it follows the
    /// user to another window).
    window: u64,
    /// Where focus came from (the click event's old_pane): restored on
    /// an interactive close so the toast view - which the click made
    /// active - does not end up focused, which would both swallow the
    /// next click (no pane change) and look like a click to us.
    return_pane: Option<u64>,
    selected: usize,
    width: u32,
    height: u32,
}

struct NotifyToast {
    /// None = lines never expire (dismiss from the chooser).
    duration: Option<u64>,
    width: u64,
    show_when_visible: bool,
    chooser_width: Option<SizeSpec>,
    chooser_height: Option<SizeSpec>,
    seq: u64,
    state: State,
    chooser: Option<Chooser>,
}

/// State carried across a live code reload. The view panes are real tmux
/// panes only our `views` map knows about: without this, a reload
/// orphans them (the fresh instance spawns new panes next to the old
/// ones, which - with an infinite duration - never go away). State only:
/// config-derived fields come from the fresh init in restore(), so a
/// config change applied together with a code reload wins.
#[derive(Serialize, Deserialize)]
struct Snapshot {
    seq: u64,
    entries: Vec<Entry>,
    /// window -> (pane, body on display); repaint flags start fresh.
    views: HashMap<u64, (Option<u64>, Option<String>)>,
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
            let body = if lines.is_empty() {
                String::new()
            } else {
                // First row: the expand affordance, right-aligned
                // (clicking anywhere in the pane opens the chooser).
                let pad = " ".repeat((width as usize).saturating_sub(5));
                format!("{pad}\u{2261}\\n{}", lines.join("\\n"))
            };
            (old, shown, body, lines.len())
        };

        // Self-healing: kill any "notifications"-titled pane in this
        // window that is not the tracked view. Orphans arise when a
        // spawn was in flight across a plugin reload (the command runs,
        // the completion is dropped, the new generation never learns the
        // pane id) - without this they duplicate forever. The chooser
        // float is excluded by its distinct title.
        if let Ok(panes) = list_panes() {
            let orphans: Vec<u64> = panes
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|p| {
                            p.get("window").and_then(|v| v.as_u64())
                                == Some(window)
                                && p.get("title").and_then(|v| v.as_str())
                                    == Some("notifications")
                                && p.get("id").and_then(|v| v.as_u64()) != old
                        })
                        .filter_map(|p| p.get("id").and_then(|v| v.as_u64()))
                        .collect()
                })
                .unwrap_or_default();
            for orphan in orphans {
                log(&format!("repaint @{window}: killing orphan %{orphan}"));
                let _ = run_command(&format!("kill-pane -t %{orphan}")).await;
            }
        }

        // Already showing exactly this? Just enforce the position (the
        // window may have been resized - tmux does not reposition floats,
        // so a toast spawned at one width sits stranded at another) and
        // don't touch anything else (the pane may have been closed behind
        // our back, e.g. by the user - then repaint it after all).
        let pane_alive = match old {
            Some(p) => resolve_pane(PaneId(p as u32)).is_ok(),
            None => false,
        };
        if shown.as_deref() == Some(body.as_str()) && (pane_alive || nlines == 0)
        {
            if let (Some(p), true) = (old, pane_alive && nlines > 0) {
                if let Ok(wi) = resolve_window(WindowId(window as u32)) {
                    let win_width = wi
                        .get("width")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(80);
                    let x = win_width.saturating_sub(width);
                    if let Err(e) = run_command(&format!(
                        "move-pane -t %{p} -X {x} -Y 0"
                    ))
                    .await
                    {
                        log(&format!(
                            "repaint @{window}: reposition failed: {}",
                            e.message
                        ));
                    }
                }
            }
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
                let r = run_command(&format!("kill-pane -t %{old}")).await;
                log(&format!(
                    "repaint @{window}: killed %{old} ok={}",
                    r.is_ok()
                ));
            } else {
                log(&format!("repaint @{window}: %{old} already gone"));
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
                     'printf \"%b\" \"$TOAST_BODY\"; \
                     sleep {keeper_secs}'"
                );
                match run_command(&cmd).await {
                    Ok(()) => {
                        let new_pane = resolve_window(WindowId(window as u32))
                            .ok()
                            .and_then(|after| {
                                pane_ids(&after)
                                    .into_iter()
                                    .find(|id| !pre.contains(id))
                            });
                        if new_pane.is_none() {
                            log(&format!(
                                "repaint @{window}: DIFF MISS (pre={pre:?})"
                            ));
                        } else {
                            log(&format!(
                                "repaint @{window}: spawned %{}",
                                new_pane.unwrap()
                            ));
                        }
                        if let Some(view) =
                            state.borrow_mut().views.get_mut(&window)
                        {
                            view.pane = new_pane;
                            view.shown =
                                new_pane.is_some().then(|| body.clone());
                        }
                    }
                    Err(e) => log(&format!(
                        "repaint @{window}: spawn FAILED: {}",
                        e.message
                    )),
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

// ---- chooser: the expanded view, a plugin UI mode ----

/// Columns the list occupies; the preview takes the rest (none when the
/// panel is too narrow).
fn chooser_list_width(width: u32) -> u32 {
    if width >= 46 {
        width * 2 / 5
    } else {
        width
    }
}

/// The retained preview rect for the selected entry's source pane.
fn chooser_preview_rect(
    ch: &Chooser,
    entries: &VecDeque<Entry>,
) -> Option<PreviewRect> {
    if entries.is_empty() {
        return None;
    }
    let sel = ch.selected.min(entries.len() - 1);
    let list_w = chooser_list_width(ch.width);
    if list_w >= ch.width {
        return None;
    }
    let x = list_w + 1;
    let w = ch.width.saturating_sub(x);
    let h = ch.height.saturating_sub(2);
    if w == 0 || h == 0 {
        return None;
    }
    Some(PreviewRect {
        pane: PaneId(entries[sel].src_pane as u32),
        x,
        y: 0,
        w,
        h,
    })
}

/// Full redraw of the chooser screen plus the preview rect. Recomputes
/// from the live feed each time, so it is safe to call with a selection
/// that expiry has made stale.
fn chooser_render(ch: &Chooser, entries: &VecDeque<Entry>) {
    let height = ch.height as usize;
    let list_w = chooser_list_width(ch.width) as usize;
    let sel = ch.selected.min(entries.len().saturating_sub(1));

    let mut out = String::from("\x1b[2J\x1b[H");
    if entries.is_empty() {
        out.push_str("\x1b[2;2H\x1b[2mno notifications\x1b[0m");
    }
    for (i, e) in entries.iter().enumerate() {
        let row = i + 2; // 1-based; row 1 is a margin
        if row + 1 > height {
            break;
        }
        let text: String = format!("{}. {}", i + 1, e.line)
            .chars()
            .take(list_w.saturating_sub(2))
            .collect();
        if i == sel {
            out.push_str(&format!("\x1b[{row};1H\x1b[7m {text}\x1b[0m"));
        } else {
            out.push_str(&format!("\x1b[{row};1H {text}"));
        }
    }
    if list_w < ch.width as usize {
        for row in 1..height {
            out.push_str(&format!(
                "\x1b[{row};{col}H\x1b[2m\u{2502}\x1b[0m",
                col = list_w + 1
            ));
        }
    }
    out.push_str(&format!(
        "\x1b[{height};1H\x1b[2m j/k move \u{b7} 1-9/Enter jump \u{b7} \
         d dismiss \u{b7} q close\x1b[0m"
    ));

    let _ = mode_write(ch.mode, out.as_bytes());
    let _ = mode_preview(ch.mode, chooser_preview_rect(ch, entries).as_ref());
}

/// Config -> (duration, width, show_when_visible, chooser_w, chooser_h);
/// shared by init and on_config_changed so both parse identically.
fn parse_config(
    config: &Config,
) -> (Option<u64>, u64, bool, Option<SizeSpec>, Option<SizeSpec>) {
    let duration = match config.duration_ms.as_deref() {
        Some("0") | Some("infinite") => None,
        other => Some(
            other
                .and_then(|s| s.parse().ok())
                .unwrap_or(6_000)
                .clamp(1_000, 300_000),
        ),
    };
    let width = config
        .width
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(44)
        .clamp(20, 120);
    (
        duration,
        width,
        config.show_when_visible.as_deref() != Some("0"),
        SizeSpec::parse(config.chooser_width.as_ref()),
        SizeSpec::parse(config.chooser_height.as_ref()),
    )
}

impl NotifyToast {
    fn open_chooser(
        &mut self,
        window: u64,
        selected: usize,
        return_pane: Option<u64>,
    ) {
        if self.chooser.is_some() {
            return;
        }
        // An empty feed still opens (the keybinding path): the panel
        // shows "no notifications" and q closes it.
        let nentries = self.state.borrow().entries.len();
        let Ok(wi) = resolve_window(WindowId(window as u32)) else { return };
        let win_w =
            wi.get("width").and_then(|v| v.as_u64()).unwrap_or(80) as u32;
        let win_h =
            wi.get("height").and_then(|v| v.as_u64()).unwrap_or(24) as u32;
        let width = self
            .chooser_width
            .map(|s| s.resolve(win_w))
            .unwrap_or_else(|| win_w.saturating_sub(8).min(90))
            .clamp(24.min(win_w.saturating_sub(4).max(10)),
                win_w.saturating_sub(4).max(10));
        let height = self
            .chooser_height
            .map(|s| s.resolve(win_h))
            .unwrap_or_else(|| (nentries as u32 + 6).max(10))
            .clamp(5, win_h.saturating_sub(2).max(5));

        match mode_open(&ModeOpts {
            window: Some(WindowId(window as u32)),
            width,
            height,
            x: None, // centered
            y: None,
            // Distinct from the toast panes' title: the orphan sweep in
            // repaint matches exact "notifications".
            title: Some("notifications \u{25b8}".into()),
        }) {
            Ok(mode) => {
                let ch = Chooser {
                    mode,
                    window,
                    return_pane,
                    selected: selected.min(nentries.saturating_sub(1)),
                    width,
                    height,
                };
                chooser_render(&ch, &self.state.borrow().entries);
                self.chooser = Some(ch);
            }
            Err(e) => log(&format!("chooser open failed: {}", e.message)),
        }
    }

    /// Jump to entry `idx`'s source pane, drop the entry and close.
    /// `client` is who pressed the key: select-window/select-pane only
    /// mutate session/window state, so a cross-session jump must also
    /// switch-client THAT client to the source's session or nothing
    /// visibly happens.
    fn chooser_jump(&mut self, ctx: &Ctx, idx: usize, client: Option<u64>) {
        let Some(ch) = self.chooser.as_ref() else { return };
        let target = {
            let st = self.state.borrow();
            st.entries.get(idx).map(|e| (e.seq, e.src_window, e.src_pane))
        };
        let Some((seq, src_window, src_pane)) = target else { return };
        let _ = mode_close(ch.mode);
        // Cleared now (not at mode-closed) so the jump's own focus
        // change wins; the late mode-closed for this id is then a no-op.
        self.chooser = None;

        let state = Rc::clone(&self.state);
        let (width, keeper_secs, show_when_visible) = self.view_params();
        ctx.spawn(async move {
            // Which sessions contain the source window, and where is the
            // pressing client right now?
            let sessions: Vec<u64> = src_window
                .and_then(|w| resolve_window(WindowId(w as u32)).ok())
                .and_then(|wi| {
                    wi.get("sessions").and_then(|v| v.as_array()).map(|a| {
                        a.iter().filter_map(|v| v.as_u64()).collect()
                    })
                })
                .unwrap_or_default();
            let client_info = client.and_then(|cid| {
                list_clients().ok()?.as_array()?.iter().find_map(|c| {
                    (c.get("id").and_then(|v| v.as_u64()) == Some(cid))
                        .then(|| {
                            (
                                c.get("name")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                c.get("session").and_then(|v| v.as_u64()),
                            )
                        })
                })
            });

            // Prefer the session the client is already on; otherwise the
            // first session linked to the window, switching the client
            // over to it.
            let dest = client_info
                .as_ref()
                .and_then(|(_, s)| *s)
                .filter(|s| sessions.contains(s))
                .or_else(|| sessions.first().copied());
            if let (Some((Some(name), cur)), Some(dest)) =
                (client_info.as_ref(), dest)
            {
                if *cur != Some(dest) {
                    let r = run_command(&format!(
                        "switch-client -c '{name}' -t '${dest}'"
                    ))
                    .await;
                    if let Err(e) = r {
                        log(&format!("jump: switch-client failed: {}",
                            e.message));
                    }
                }
            }
            if let Some(window) = src_window {
                // Session-qualified so grouped sessions (which share
                // windows) pick the session we just switched to.
                let t = match dest {
                    Some(s) => format!("'${s}:@{window}'"),
                    None => format!("'@{window}'"),
                };
                if let Err(e) =
                    run_command(&format!("select-window -t {t}")).await
                {
                    log(&format!("jump: select-window failed: {}", e.message));
                }
            }
            if let Err(e) =
                run_command(&format!("select-pane -t %{src_pane}")).await
            {
                log(&format!("jump: select-pane failed: {}", e.message));
            }
            state.borrow_mut().entries.retain(|e| e.seq != seq);
            sync_views(&state, width, keeper_secs, show_when_visible).await;
        });
    }

    /// Drop entry `idx` from the feed; close when it was the last one.
    fn chooser_dismiss(&mut self, ctx: &Ctx, idx: usize) {
        let seq = {
            let st = self.state.borrow();
            st.entries.get(idx).map(|e| e.seq)
        };
        let Some(seq) = seq else { return };
        self.state.borrow_mut().entries.retain(|e| e.seq != seq);

        let empty = self.state.borrow().entries.is_empty();
        if let Some(ch) = self.chooser.as_mut() {
            if empty {
                let _ = mode_close(ch.mode);
            } else {
                ch.selected = ch.selected.min(
                    self.state.borrow().entries.len() - 1,
                );
                chooser_render(ch, &self.state.borrow().entries);
            }
        }

        let state = Rc::clone(&self.state);
        let (width, keeper_secs, show_when_visible) = self.view_params();
        ctx.spawn(async move {
            sync_views(&state, width, keeper_secs, show_when_visible).await;
        });
    }

    fn chooser_key(
        &mut self,
        ctx: &Ctx,
        key: &str,
        mouse_row: Option<u64>,
        client: Option<u64>,
    ) {
        let nentries = self.state.borrow().entries.len();
        let Some(ch) = self.chooser.as_mut() else { return };
        let sel = ch.selected.min(nentries.saturating_sub(1));

        match key {
            "j" | "Down" if nentries > 0 => {
                ch.selected = (sel + 1) % nentries;
                chooser_render(ch, &self.state.borrow().entries);
            }
            "k" | "Up" if nentries > 0 => {
                ch.selected = (sel + nentries - 1) % nentries;
                chooser_render(ch, &self.state.borrow().entries);
            }
            "Enter" if nentries > 0 => self.chooser_jump(ctx, sel, client),
            "d" if nentries > 0 => self.chooser_dismiss(ctx, sel),
            "q" | "Escape" => {
                let _ = mode_close(ch.mode);
            }
            "MouseDown1Pane" => {
                // List rows start at screen row 1 (0-based).
                let Some(row) = mouse_row else { return };
                let idx = (row as usize).wrapping_sub(1);
                if idx < nentries {
                    ch.selected = idx;
                    chooser_render(ch, &self.state.borrow().entries);
                }
            }
            k => {
                if let Some(idx) =
                    k.parse::<usize>().ok().filter(|n| (1..=9).contains(n))
                {
                    if idx <= nentries {
                        self.chooser_jump(ctx, idx - 1, client);
                    }
                }
            }
        }
    }

    fn view_params(&self) -> (u64, u64, bool) {
        let keeper_secs = match self.duration {
            Some(d) => d.div_ceil(1000) * (MAX_LINES as u64) + 60,
            None => 2_147_483_647,
        };
        (self.width, keeper_secs, self.show_when_visible)
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
        let (duration, width, show_when_visible, chooser_width, chooser_height) =
            parse_config(&config);

        ctx.subscribe(&[
            "pane-notification",
            // The view follows the user: repaint on anything that
            // changes which window is on display.
            "session-window-changed",
            "client-session-changed",
            "client-attached",
            "client-detached",
            // Click-to-dismiss: focusing a toast pane fires this.
            "window-pane-changed",
            // Key bindings: `plugin-command notify_toast chooser`.
            "plugin-command",
            // Re-anchor the toast when its window changes size (tmux
            // does not reposition floating panes on resize).
            "window-resized",
        ])
        .map_err(|e| e.message.clone())?;

        Ok(Self {
            duration,
            width,
            show_when_visible,
            chooser_width,
            chooser_height,
            seq: 0,
            state: State::default(),
            chooser: None,
        })
    }

    /// Absorb config changes in place: a restart would orphan the view
    /// panes (only our views map knows about them) and drop the feed.
    /// Already-armed expiry timers keep their old duration; new entries
    /// use the new one.
    fn on_config_changed(&mut self, _ctx: &Ctx, config: Config) -> bool {
        (
            self.duration,
            self.width,
            self.show_when_visible,
            self.chooser_width,
            self.chooser_height,
        ) = parse_config(&config);
        log("config absorbed");
        true
    }

    fn snapshot(&self) -> Option<serde_json::Value> {
        // The chooser is deliberately absent: the host force-closes our
        // modes during the swap, so the new generation must start
        // without one.
        let st = self.state.borrow();
        serde_json::to_value(Snapshot {
            seq: self.seq,
            entries: st.entries.iter().cloned().collect(),
            views: st
                .views
                .iter()
                .map(|(w, v)| (*w, (v.pane, v.shown.clone())))
                .collect(),
        })
        .ok()
    }

    fn restore(
        fresh: Self,
        _old_version: i32,
        state: serde_json::Value,
    ) -> Option<Self> {
        let snap: Snapshot = serde_json::from_value(state).ok()?;
        // Config-derived fields from the fresh init (current config);
        // only the carried state comes from the snapshot.
        let me = Self {
            seq: snap.seq,
            state: Rc::new(RefCell::new(Shared {
                entries: snap.entries.into_iter().collect(),
                views: snap
                    .views
                    .into_iter()
                    .map(|(w, (pane, shown))| {
                        (w, View { pane, shown, ..View::default() })
                    })
                    .collect(),
            })),
            chooser: None,
            ..fresh
        };

        // The old instance's expiry tasks died with it: re-arm one per
        // restored entry (each gets a fresh full duration; the elapsed
        // part is not carried), then reconcile the adopted panes once.
        let (width, keeper_secs, show_when_visible) = me.view_params();
        let ctx = Ctx::new();
        if let Some(duration_ms) = me.duration {
            for e in me.state.borrow().entries.iter() {
                let seq = e.seq;
                let state = Rc::clone(&me.state);
                ctx.spawn(async move {
                    if sleep_ms(duration_ms).await.is_err() {
                        return;
                    }
                    state.borrow_mut().entries.retain(|x| x.seq != seq);
                    sync_views(&state, width, keeper_secs, show_when_visible)
                        .await;
                });
            }
        }
        let state = Rc::clone(&me.state);
        ctx.spawn(async move {
            sync_views(&state, width, keeper_secs, show_when_visible).await;
        });
        Some(me)
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        let (width, keeper_secs, show_when_visible) = self.view_params();
        let state = Rc::clone(&self.state);

        match event.event.as_str() {
            "pane-notification" => {}
            // Key binding: toggle the chooser in the target window.
            "plugin-command" => {
                if event.data.get("text").and_then(|v| v.as_str())
                    != Some("chooser")
                {
                    return;
                }
                let Some(window) = event.scope.window.map(u64::from) else {
                    return;
                };
                if let Some(ch) = self.chooser.as_ref() {
                    let same = ch.window == window;
                    let _ = mode_close(ch.mode);
                    self.chooser = None;
                    if same {
                        return; // toggle off
                    }
                }
                self.open_chooser(window, 0, event.scope.pane.map(u64::from));
                return;
            }
            // Chooser events, targeted at this instance by mode id.
            "mode-key" => {
                let matches = self.chooser.as_ref().is_some_and(|ch| {
                    event.data.get("mode").and_then(|v| v.as_u64())
                        == Some(ch.mode.0)
                });
                if !matches {
                    return;
                }
                let Some(key) =
                    event.data.get("key").and_then(|v| v.as_str())
                else {
                    return;
                };
                let mouse_row = event.data.pointer("/mouse/y").and_then(
                    serde_json::Value::as_u64,
                );
                let client =
                    event.data.get("client").and_then(|v| v.as_u64());
                let key = key.to_string();
                self.chooser_key(ctx, &key, mouse_row, client);
                return;
            }
            "mode-resize" => {
                let Some(ch) = self.chooser.as_mut() else { return };
                if event.data.get("mode").and_then(|v| v.as_u64())
                    != Some(ch.mode.0)
                {
                    return;
                }
                if let Some(w) =
                    event.data.get("width").and_then(|v| v.as_u64())
                {
                    ch.width = w as u32;
                }
                if let Some(h) =
                    event.data.get("height").and_then(|v| v.as_u64())
                {
                    ch.height = h as u32;
                }
                chooser_render(ch, &state.borrow().entries);
                return;
            }
            "mode-closed" => {
                if self.chooser.as_ref().is_some_and(|ch| {
                    event.data.get("mode").and_then(|v| v.as_u64())
                        == Some(ch.mode.0)
                }) {
                    let ch = self.chooser.take().unwrap();
                    // Hand focus back to where the opening click came
                    // from: tmux's fallback would otherwise focus the
                    // toast view, whose next click could not fire (no
                    // pane change). Dead panes fail harmlessly.
                    if let Some(rp) = ch.return_pane {
                        ctx.spawn(async move {
                            let _ = run_command(&format!(
                                "select-pane -t %{rp}"
                            ))
                            .await;
                        });
                    }
                }
                return;
            }
            // A window-switch (or attach/detach): move the view. Other
            // events (e.g. the implicit lifecycle deliveries - including
            // our own toasts' pane-created/destroyed) are ignored;
            // repaint is idempotent anyway, but no need to churn.
            // A resized window strands its toast at the old offset:
            // repaint re-anchors it (position-only when content matches).
            "window-resized" => {
                let Some(w) = event.scope.window.map(u64::from) else {
                    return;
                };
                let tracked =
                    state.borrow().views.get(&w).is_some_and(|v| v.pane.is_some());
                if !tracked {
                    return;
                }
                ctx.spawn(async move {
                    repaint(&state, w, width, keeper_secs, show_when_visible)
                        .await;
                });
                return;
            }
            "session-window-changed" | "client-session-changed"
            | "client-attached" | "client-detached" => {
                // The chooser follows the user: when its window stops
                // being on display, move the float to the window now
                // shown. The move keeps the pane, the mode id and the
                // rendered screen (at most a mode-resize follows), so
                // no state needs carrying. Fallback to close-and-reopen
                // if the move is refused (it would empty the old
                // window); plain close when there is nowhere to follow
                // (e.g. the last client detached).
                if let Some(ch) = self.chooser.as_mut() {
                    let current = current_windows();
                    if !current.contains(&ch.window) {
                        let target = event
                            .scope
                            .window
                            .map(u64::from)
                            .filter(|w| current.contains(w))
                            .or_else(|| current.first().copied());
                        match target {
                            Some(w) => match mode_move(
                                ch.mode,
                                Some(WindowId(w as u32)),
                            ) {
                                Ok(()) => ch.window = w,
                                Err(_) => {
                                    let _ = mode_close(ch.mode);
                                    let selected = ch.selected;
                                    self.chooser = None;
                                    self.open_chooser(w, selected, None);
                                }
                            },
                            None => {
                                let _ = mode_close(ch.mode);
                                self.chooser = None;
                            }
                        }
                    }
                }
                ctx.spawn(async move {
                    sync_views(&state, width, keeper_secs, show_when_visible)
                        .await;
                });
                return;
            }
            // Clicking (or otherwise focusing) a notification pane opens
            // the chooser in that window. Two exclusions keep this from
            // misfiring: the chooser's own pane is not a view pane (a
            // mode pane taking focus never matches), and while a chooser
            // exists ALL of these are ignored - tmux's focus fallback
            // makes the toast active whenever a float leaves its window
            // (close, kill or a follow move), and that is
            // indistinguishable from a click by pane alone.
            "window-pane-changed" => {
                if self.chooser.is_some() {
                    return;
                }
                let Some(p) = event.scope.pane else { return };
                let window = state
                    .borrow()
                    .views
                    .iter()
                    .find(|(_, v)| v.pane == Some(u64::from(p)))
                    .map(|(w, _)| *w);
                let Some(window) = window else { return };
                let return_pane =
                    event.data.get("old_pane").and_then(|v| v.as_u64());
                log("notification pane clicked: opening chooser");
                self.open_chooser(window, 0, return_pane);
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
        let duration = self.duration;

        let src_pane = u64::from(src_pane);
        ctx.spawn(async move {
            {
                let mut st = state.borrow_mut();
                st.entries.push_back(Entry {
                    seq,
                    line,
                    src_window,
                    src_pane,
                });
                while st.entries.len() > MAX_LINES {
                    st.entries.pop_front();
                }
            }
            log(&format!("notification from %{src_pane}: added to feed"));
            sync_views(&state, width, keeper_secs, show_when_visible).await;

            // Expire this line, then reconcile again. With an infinite
            // duration the line stays until the pane is dismissed.
            let Some(duration_ms) = duration else { return };
            if sleep_ms(duration_ms).await.is_err() {
                return; // instance torn down
            }
            state.borrow_mut().entries.retain(|e| e.seq != seq);
            sync_views(&state, width, keeper_secs, show_when_visible).await;
        });
    }
}

tmux_plugin!(NotifyToast);
