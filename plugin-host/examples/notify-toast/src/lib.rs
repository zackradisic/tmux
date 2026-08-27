//! Notification toasts as floating panes.
//!
//! Listens for `pane-notification` events (OSC 9 "message" or OSC 777
//! "notify;title;body" emitted by a program in any pane - e.g. a coding
//! agent's stop hook running `printf '\e]9;done\a'`) and shows them in a
//! floating notification pane in the top-right corner.
//!
//! The feed is server-global; the panes are just views of it. One line
//! per notifying pane -
//!   <session>: <pane title> [xN]
//! - naming the pane by what it was doing when it pinged (its title, or
//! the notification text when the program never set one). A pane that
//! pings again replaces its own line and bumps the count, so one pane
//! never fills the feed. Lines expire on their own after `duration_ms`.
//! The view follows the
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
//! plugin UI mode) showing the feed as a session > window > notification
//! tree, drawn like `choose-tree` (prefix + w). A window holding one
//! notification is drawn as a single row, since a row of its own would
//! only repeat what the notification says; the level appears as soon as
//! it groups more than one. The panel carries a live preview of
//! what the selected row points at. `j`/`k` (or arrows) move over every
//! row, `h`/`l` fold and unfold the session or window under the cursor
//! (`M--`/`M-+` fold and unfold all), `1`-`9` or `Enter` jump to a
//! notification's source pane (and drop the entry), `d` dismisses one -
//! or, on a session or window row, everything under it, folded away or
//! not - and `q`/`Escape` closes the panel.
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
//! Sets one option of its own, `@notify_toast_scratch`, on a notifying
//! pane: `set -F` is how a plugin expands a tmux format (`pane_format`).
//!
//! Build: cargo build -p notify-toast --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::abi::KIND_SERVER;
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

/// One pending notification. The pane strings are captured at arrival:
/// a notification is a point-in-time event, so what the pane was doing
/// when it pinged is what identifies it - even after it has moved on.
///
/// Every field added after the first release is optional on the way in:
/// a snapshot the new code cannot read fails the migration, and a failed
/// migration keeps the OLD instance running - so a plugin that drops
/// compatibility here can no longer be reloaded until its feed empties.
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    seq: u64,
    src_window: Option<u64>,
    src_pane: u64,
    /// The notification text itself. `line` was the pre-tree snapshot's
    /// whole rendered line; taking it here keeps such a feed readable.
    #[serde(default, alias = "line")]
    msg: String,
    /// `#{pane_current_command}` at arrival.
    #[serde(default)]
    cmd: String,
    /// `#{pane_title}` at arrival, empty when the program never set one
    /// (tmux defaults it to the host name, which is also what
    /// choose-tree treats as "no title").
    #[serde(default)]
    title: String,
    /// Names for when the source objects are gone by the time we draw.
    #[serde(default)]
    session_name: String,
    #[serde(default)]
    window_name: String,
    /// Pings this pane has sent since the row appeared. A pane holds one
    /// row however often it fires; 0 in a snapshot from before counting.
    #[serde(default)]
    count: u32,
}

impl Entry {
    /// What the pane was doing when it pinged: its title, or the
    /// notification text when it has none.
    fn label(&self) -> &str {
        if self.title.is_empty() {
            &self.msg
        } else {
            &self.title
        }
    }

    /// " x3" once a pane has pinged more than once, else nothing.
    fn tally(&self) -> String {
        match self.count {
            0 | 1 => String::new(),
            n => format!(" \u{d7}{n}"),
        }
    }
}

/// Expand a tmux format against a pane. There is no host call for format
/// expansion, but `set -F` expands the value server-side, so a scratch
/// option is a round trip that gets one back. The option is pane-scoped,
/// so two notifications arriving together cannot read each other's
/// answer, and it dies with the pane.
async fn pane_format(pane: u64, format: &str) -> String {
    const SCRATCH: &str = "@notify_toast_scratch";
    let cmd = format!("set -p -F -t %{pane} {SCRATCH} '{format}'");
    if let Err(e) = run_command(&cmd).await {
        log(&format!("pane_format %{pane}: {}", e.message));
        return String::new();
    }
    get_option_in(OptionTarget::Pane(PaneId(pane as u32)), SCRATCH)
        .unwrap_or_default()
}

const FMT_COMMAND: &str = "#{pane_current_command}";
/// choose-tree's own test for a title worth showing.
const FMT_TITLE: &str = "#{?#{&&:#{pane_title},\
                          #{!=:#{pane_title},#{host_short}}},#{pane_title},}";

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
    /// The open chooser, if any. It lives beside the feed rather than on
    /// the instance because it is a view of the feed like the panes are:
    /// sync_views redraws it, so a notification arriving while the panel
    /// is open cannot leave a stale tree on screen.
    chooser: Option<Chooser>,
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
    /// The selected node, by identity: rows come and go under the
    /// selection (expiry, dismissal, folding) and row numbers shift
    /// with them.
    selected: NodeId,
    /// Row index the selection was last drawn at, so a vanished node
    /// leaves the cursor where it stood.
    hint: usize,
    /// Nodes drawn collapsed.
    folded: HashSet<NodeId>,
    /// First tree row on screen; the tree can outgrow the panel.
    offset: usize,
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

fn ids(list: &[u32]) -> Vec<u64> {
    list.iter().map(|&v| u64::from(v)).collect()
}

fn pane_ids(window: &tmux_plugin_sdk::WindowInfo) -> Vec<u64> {
    ids(&window.panes)
}

/// One session as the tree needs it.
struct SessionInfo {
    name: String,
    attached: bool,
    current_window: Option<u64>,
    /// window id -> its index in this session
    indexes: HashMap<u64, u32>,
}

/// One window as the tree needs it.
struct WindowInfo {
    name: String,
    /// Pane ids in layout order.
    panes: Vec<u64>,
    sessions: Vec<u64>,
    active_pane: Option<u64>,
}

/// The server layout in three calls, so locating N entries costs no
/// further host calls.
#[derive(Default)]
struct Topo {
    pane_window: HashMap<u64, u64>,
    windows: HashMap<u64, WindowInfo>,
    sessions: HashMap<u64, SessionInfo>,
}

impl Topo {
    fn fetch() -> Self {
        let mut topo = Topo::default();
        if let Ok(panes) = list_panes() {
            for p in panes {
                topo.pane_window
                    .insert(u64::from(p.id), u64::from(p.window));
            }
        }
        if let Ok(windows) = list_windows() {
            for w in windows {
                topo.windows.insert(
                    u64::from(w.id),
                    WindowInfo {
                        panes: pane_ids(&w),
                        sessions: ids(&w.sessions),
                        active_pane: w.active_pane.map(u64::from),
                        name: w.name,
                    },
                );
            }
        }
        if let Ok(sessions) = list_sessions() {
            for s in sessions {
                let mut indexes = HashMap::new();
                for &(index, w) in &s.windows {
                    indexes.insert(u64::from(w), index);
                }
                topo.sessions.insert(
                    u64::from(s.id),
                    SessionInfo {
                        attached: s.attached,
                        current_window: s.current_window.map(u64::from),
                        indexes,
                        name: s.name,
                    },
                );
            }
        }
        topo
    }
}

/// Where an entry sits in the tree right now; the names fall back to the
/// ones captured at arrival once the objects are gone.
struct Located {
    session: Option<u64>,
    session_name: String,
    attached: bool,
    window: Option<u64>,
    window_name: String,
    window_index: u32,
    /// The window is its session's current one (choose-tree's `*`).
    window_current: bool,
    pane_index: u32,
}

fn locate(topo: &Topo, e: &Entry) -> Located {
    let window = topo.pane_window.get(&e.src_pane).copied().or(e.src_window);
    let w = window.and_then(|w| topo.windows.get(&w));
    let pane_index = w
        .and_then(|w| w.panes.iter().position(|p| *p == e.src_pane))
        .unwrap_or(0) as u32;
    // A window can be linked into several sessions; prefer an attached
    // one, since that is where the user is looking.
    let session = w.and_then(|w| {
        w.sessions
            .iter()
            .find(|s| {
                topo.sessions.get(s).is_some_and(|si| si.attached)
            })
            .or_else(|| w.sessions.first())
            .copied()
    });
    let si = session.and_then(|s| topo.sessions.get(&s));
    Located {
        session,
        session_name: si
            .map(|si| si.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| e.session_name.clone()),
        attached: si.is_some_and(|si| si.attached),
        window,
        // The name the window had when it pinged, like everything else
        // on the row. Live would be wrong twice over: `automatic-rename`
        // turns the window into `[tmux]` the moment our own panel takes
        // focus there, and a name that moves under us changes the node
        // identity, which would drop the user's folds mid-session.
        window_name: if e.window_name.is_empty() {
            w.map(|w| w.name.clone()).unwrap_or_default()
        } else {
            e.window_name.clone()
        },
        window_index: window
            .zip(si)
            .and_then(|(w, si)| si.indexes.get(&w).copied())
            .unwrap_or(0),
        window_current: window.is_some()
            && si.and_then(|si| si.current_window) == window,
        pane_index,
    }
}

/// One feed line: "<session>: <what the pane was doing>", cut to the
/// pane width. The chooser's tree carries everything else. The tally is
/// kept out of the truncation, since it is the part that changes.
fn toast_line(e: &Entry, topo: &Topo, width: u64) -> String {
    let loc = locate(topo, e);
    let session = if loc.session_name.is_empty() {
        "?"
    } else {
        &loc.session_name
    };
    let tally = e.tally();
    let room = (width as usize)
        .saturating_sub(4 + tally.chars().count())
        .max(8);
    format!("{}{tally}", sanitize(&format!("{session}: {}", e.label()), room))
}

/// The windows the feed should currently be rendered in: every attached
/// session's current window.
fn current_windows() -> Vec<u64> {
    let mut out = Vec::new();
    let Ok(sessions) = list_sessions() else { return out };
    for s in sessions {
        if !s.attached {
            continue;
        }
        if let Some(curw) = s.current_window.map(u64::from) {
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
        let topo = on_display.then(Topo::fetch).unwrap_or_default();
        let (old, shown, body, nlines) = {
            let st = state.borrow();
            let lines: Vec<String> = if on_display {
                st.entries
                    .iter()
                    .filter(|e| {
                        show_when_visible || e.src_window != Some(window)
                    })
                    .map(|e| format!(" {}", toast_line(e, &topo, width)))
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
                .iter()
                .filter(|p| {
                    u64::from(p.window) == window
                        && p.title == "notifications"
                        && Some(u64::from(p.id)) != old
                })
                .map(|p| u64::from(p.id))
                .collect();
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
                    let win_width = u64::from(wi.width);
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
                let win_width = u64::from(before.width);
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
    redraw_chooser(state);
}

/// Redraw the open chooser from the live feed. Every path that changes
/// the feed ends in sync_views, so this is where the panel keeps up with
/// notifications arriving and expiring underneath it.
fn redraw_chooser(state: &State) {
    let mut guard = state.borrow_mut();
    let st = &mut *guard;
    if let Some(ch) = st.chooser.as_mut() {
        chooser_render(ch, &st.entries);
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

/// A styled run inside a drawn row: an SGR body ("" = plain) and text.
/// Truncation counts characters, so styles never eat visible width.
#[derive(Clone)]
struct Seg(&'static str, String);

const DIM: &str = "2";
const RED: &str = "31";
const GREEN: &str = "32";

/// What a row stands for. Sessions key on their name (tmux keeps those
/// unique) so the selection and the fold state survive a rebuild - and
/// survive the ids going away when a source pane dies.
#[derive(Clone, PartialEq, Eq, Hash)]
enum NodeId {
    Session(String),
    Window(String, Option<u64>, String),
    Note(u64),
}

/// One drawn row of the tree. `depth` 0 = session, 1 = window, 2 = the
/// notification itself.
struct Row {
    depth: u32,
    /// Last among its siblings: picks the branch glyph.
    last: bool,
    /// Whether this row's parent was the last of its siblings, i.e.
    /// whether the ancestor column continues past us.
    parent_last: bool,
    /// Session and window rows carry the expander glyph.
    has_children: bool,
    expanded: bool,
    /// The jump key, 1-9; notification rows only.
    key: Option<usize>,
    name: String,
    text: Vec<Seg>,
    id: NodeId,
    parent: Option<NodeId>,
    /// The pane the preview mirrors while this row is selected: the
    /// notification's own pane, or the active pane of the window (or
    /// session) the row groups.
    preview: Option<u64>,
    /// Seq of the notification, for rows that are one.
    note: Option<u64>,
}

/// Every notification a node stands for. Read from the feed, not from
/// the drawn rows, so dismissing a folded node takes its hidden
/// children with it.
fn node_notes(id: &NodeId, entries: &VecDeque<Entry>, topo: &Topo) -> Vec<u64> {
    entries
        .iter()
        .filter(|e| {
            let loc = locate(topo, e);
            match id {
                NodeId::Session(name) => loc.session_name == *name,
                NodeId::Window(sname, wid, wname) => {
                    loc.session_name == *sname
                        && loc.window == *wid
                        && loc.window_name == *wname
                }
                NodeId::Note(seq) => e.seq == *seq,
            }
        })
        .map(|e| e.seq)
        .collect()
}

/// The feed as a session > window > notification tree, ordered like
/// choose-tree: sessions by name, windows by index, panes by position.
/// Nodes in `folded` are drawn collapsed - their children are left out
/// of the rows entirely, so every row index is a visible line.
fn build_rows(
    entries: &VecDeque<Entry>,
    topo: &Topo,
    folded: &HashSet<NodeId>,
) -> Vec<Row> {
    struct Note {
        pane_index: u32,
        seq: u64,
        pane: u64,
        /// Pings folded into this row, for the session tally.
        count: u32,
        text: Vec<Seg>,
    }
    struct Win {
        id: Option<u64>,
        index: u32,
        name: String,
        current: bool,
        /// The window's active pane, previewed when this row or its
        /// session is selected.
        preview: Option<u64>,
        notes: Vec<Note>,
    }
    struct Sess {
        name: String,
        attached: bool,
        preview: Option<u64>,
        windows: Vec<Win>,
    }
    let mut tree: Vec<Sess> = Vec::new();

    for e in entries {
        let loc = locate(topo, e);
        // Sessions group by name (tmux keeps those unique), so an entry
        // whose pane has died - which resolves to no session id, only
        // the name captured at arrival - still lands under the live
        // session it came from instead of a ghost node beside it.
        let si = match tree.iter().position(|s| s.name == loc.session_name) {
            Some(i) => i,
            None => {
                tree.push(Sess {
                    name: loc.session_name.clone(),
                    attached: false,
                    preview: None,
                    windows: Vec::new(),
                });
                tree.len() - 1
            }
        };
        tree[si].attached |= loc.attached;
        let windows = &mut tree[si].windows;
        let wi = match windows
            .iter()
            .position(|w| w.id == loc.window && w.name == loc.window_name)
        {
            Some(i) => i,
            None => {
                windows.push(Win {
                    id: loc.window,
                    index: loc.window_index,
                    name: loc.window_name.clone(),
                    current: loc.window_current,
                    preview: loc
                        .window
                        .and_then(|w| topo.windows.get(&w))
                        .and_then(|w| w.active_pane),
                    notes: Vec::new(),
                });
                windows.len() - 1
            }
        };
        windows[wi].current |= loc.window_current;
        windows[wi].notes.push(Note {
            pane_index: loc.pane_index,
            seq: e.seq,
            pane: e.src_pane,
            count: e.count.max(1),
            text: note_text(e),
        });
    }

    tree.sort_by(|a, b| a.name.cmp(&b.name));
    for s in tree.iter_mut() {
        s.windows.sort_by_key(|w| (w.index, w.id));
        for w in s.windows.iter_mut() {
            w.notes.sort_by_key(|n| (n.pane_index, n.seq));
        }
        // A session row previews its current window, like choose-tree.
        s.preview = topo
            .sessions
            .values()
            .find(|si| si.name == s.name)
            .and_then(|si| si.current_window)
            .and_then(|w| topo.windows.get(&w))
            .and_then(|w| w.active_pane)
            .or_else(|| s.windows.first().and_then(|w| w.preview));
    }

    let mut rows = Vec::new();
    let mut key = 1usize;
    let nsessions = tree.len();
    for (i, s) in tree.into_iter().enumerate() {
        // Pings, not rows: a pane that fired four times holds one row.
        let count: u32 = s
            .windows
            .iter()
            .flat_map(|w| w.notes.iter())
            .map(|n| n.count)
            .sum();
        let mut text = vec![Seg(
            DIM,
            format!(
                "{count} notification{}",
                if count == 1 { "" } else { "s" }
            ),
        )];
        if s.attached {
            text.push(Seg(DIM, " (attached)".into()));
        }
        let sid = NodeId::Session(s.name.clone());
        let session_open = !folded.contains(&sid);
        let session_last = i + 1 == nsessions;
        rows.push(Row {
            depth: 0,
            last: session_last,
            parent_last: true,
            has_children: true,
            expanded: session_open,
            key: None,
            name: if s.name.is_empty() { "?".into() } else { s.name.clone() },
            text,
            id: sid.clone(),
            parent: None,
            // Folded or not, the row previews where it points: the
            // session's current window, the window's active pane.
            preview: s.preview,
            note: None,
        });
        if !session_open {
            continue;
        }
        let nwindows = s.windows.len();
        for (j, w) in s.windows.into_iter().enumerate() {
            let mut text = vec![Seg(
                "",
                if w.name.is_empty() { "?".into() } else { w.name.clone() },
            )];
            if w.current {
                text.push(Seg(DIM, "*".into()));
            }
            let wid = NodeId::Window(s.name.clone(), w.id, w.name.clone());
            let window_open = !folded.contains(&wid);
            let window_last = j + 1 == nwindows;

            // A window that holds a single notification is drawn as one
            // row - "<index>: <window>: <what the pane was doing>" -
            // because a window row of its own would only repeat what
            // the notification already says. The level appears as soon
            // as it groups more than one.
            if w.notes.len() == 1 {
                let mut w = w;
                let n = w.notes.pop().unwrap();
                text.push(Seg(DIM, ": ".into()));
                text.extend(n.text);
                rows.push(Row {
                    depth: 1,
                    last: window_last,
                    parent_last: session_last,
                    has_children: false,
                    expanded: false,
                    key: (key <= 9).then(|| {
                        let k = key;
                        key += 1;
                        k
                    }),
                    name: w.index.to_string(),
                    text,
                    id: NodeId::Note(n.seq),
                    parent: Some(sid.clone()),
                    preview: Some(n.pane),
                    note: Some(n.seq),
                });
                continue;
            }

            rows.push(Row {
                depth: 1,
                last: window_last,
                parent_last: session_last,
                has_children: true,
                expanded: window_open,
                key: None,
                name: w.index.to_string(),
                text,
                id: wid.clone(),
                parent: Some(sid.clone()),
                preview: w.preview,
                note: None,
            });
            if !window_open {
                continue;
            }
            let nnotes = w.notes.len();
            for (k, n) in w.notes.into_iter().enumerate() {
                rows.push(Row {
                    depth: 2,
                    last: k + 1 == nnotes,
                    parent_last: window_last,
                    has_children: false,
                    expanded: false,
                    key: (key <= 9).then(|| {
                        let k = key;
                        key += 1;
                        k
                    }),
                    name: n.pane_index.to_string(),
                    text: n.text,
                    id: NodeId::Note(n.seq),
                    parent: Some(wid.clone()),
                    preview: Some(n.pane),
                    note: Some(n.seq),
                });
            }
        }
    }
    rows
}

/// What a notification row says: the pane title in quotes, or the
/// notification text when the program set no title, plus the tally.
///
/// choose-tree leads this with `#{pane_current_command}`, but here that
/// is the third `claude` on one branch - automatic-rename already names
/// the window after the same command, and the title says far more. The
/// command survives only as that window name.
fn note_text(e: &Entry) -> Vec<Seg> {
    let head = if e.title.is_empty() {
        e.msg.clone()
    } else {
        format!("\"{}\"", e.title)
    };
    let mut segs = vec![Seg("", head)];
    let tally = e.tally();
    if !tally.is_empty() {
        segs.push(Seg(DIM, tally));
    }
    segs
}

/// Draw one row, cut to `width` characters. Every segment re-applies the
/// style from scratch so the selection's reverse video survives the
/// segment styles drawn inside it.
fn draw_row(
    row: &Row,
    key_width: usize,
    name_width: usize,
    selected: bool,
    width: usize,
) -> String {
    let mut segs: Vec<Seg> = Vec::new();
    let key = match row.key {
        Some(k) => format!("({k})"),
        None => String::new(),
    };
    let mut prefix = format!("{key:<key_width$}");
    if row.depth > 0 {
        for _ in 0..row.depth - 1 {
            prefix.push_str(if row.parent_last {
                "    "
            } else {
                "\u{2502}   "
            });
        }
        prefix.push_str(if row.last {
            "\u{2514}\u{2500}> "
        } else {
            "\u{251c}\u{2500}> "
        });
    }
    segs.push(Seg(DIM, prefix));
    if row.has_children {
        if row.expanded {
            segs.push(Seg(RED, "- ".into()));
        } else {
            segs.push(Seg(GREEN, "+ ".into()));
        }
    }
    segs.push(Seg("", format!("{:>name_width$}", row.name)));
    if !row.text.is_empty() {
        segs.push(Seg(DIM, ": ".into()));
        segs.extend(row.text.iter().cloned());
    }

    let sgr = |style: &str| {
        let mut s = String::from("\x1b[0");
        if selected {
            s.push_str(";7");
        }
        if !style.is_empty() {
            s.push(';');
            s.push_str(style);
        }
        s.push('m');
        s
    };
    let mut out = String::new();
    let mut used = 0usize;
    for Seg(style, text) in segs.iter() {
        if used >= width {
            break;
        }
        let take: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(width - used)
            .collect();
        if take.is_empty() {
            continue;
        }
        used += take.chars().count();
        out.push_str(&sgr(style));
        out.push_str(&take);
    }
    // A selected row is highlighted across the whole list column.
    if selected && used < width {
        out.push_str(&sgr(""));
        out.push_str(&" ".repeat(width - used));
    }
    out.push_str("\x1b[0m");
    out
}

/// The retained preview rect for what the selected row points at.
fn chooser_preview_rect(ch: &Chooser, rows: &[Row]) -> Option<PreviewRect> {
    let pane = selected_row(ch, rows).and_then(|i| rows[i].preview)?;
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
    Some(PreviewRect { pane: PaneId(pane as u32), x, y: 0, w, h })
}

/// Row index of the selection. When the selected node has gone - it
/// expired, or its parent was folded over it - the cursor stays where it
/// was on screen rather than jumping to the top.
fn selected_row(ch: &Chooser, rows: &[Row]) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    rows.iter()
        .position(|r| r.id == ch.selected)
        .or_else(|| Some(ch.hint.min(rows.len() - 1)))
}

/// Full redraw of the chooser screen plus the preview rect. Rebuilt from
/// the live feed each time, so it is safe to call with a selection that
/// expiry has made stale.
fn chooser_render(ch: &mut Chooser, entries: &VecDeque<Entry>) {
    let topo = Topo::fetch();
    let rows = build_rows(entries, &topo, &ch.folded);
    let height = ch.height as usize;
    let list_w = chooser_list_width(ch.width) as usize;
    let visible = height.saturating_sub(1); // last row is the help line

    // Keep the selection on screen.
    let sel = selected_row(ch, &rows);
    if let Some(sel) = sel {
        ch.hint = sel;
        ch.selected = rows[sel].id.clone();
        if sel < ch.offset {
            ch.offset = sel;
        } else if visible > 0 && sel >= ch.offset + visible {
            ch.offset = sel + 1 - visible;
        }
    }
    if ch.offset + visible > rows.len() {
        ch.offset = rows.len().saturating_sub(visible);
    }

    let key_width = rows
        .iter()
        .filter_map(|r| r.key)
        .map(|k| k.to_string().len() + 3)
        .max()
        .unwrap_or(0);
    // choose-tree right-aligns the window and pane indexes per level.
    let name_width = |depth: u32| {
        rows.iter()
            .filter(|r| r.depth == depth)
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths = [0, name_width(1), name_width(2)];

    let mut out = String::from("\x1b[2J\x1b[H");
    if rows.is_empty() {
        out.push_str("\x1b[1;1H\x1b[2mno notifications\x1b[0m");
    }
    for (i, row) in rows.iter().enumerate().skip(ch.offset).take(visible) {
        out.push_str(&format!("\x1b[{};1H", i - ch.offset + 1));
        out.push_str(&draw_row(
            row,
            key_width,
            widths[row.depth as usize],
            sel == Some(i),
            list_w,
        ));
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
        "\x1b[{height};1H\x1b[2m j/k move \u{b7} h/l fold \u{b7} \
         1-9/Enter jump \u{b7} d dismiss \u{b7} q close\x1b[0m"
    ));

    let _ = mode_write(ch.mode, out.as_bytes());
    let _ = mode_preview(ch.mode, chooser_preview_rect(ch, &rows).as_ref());
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
    /// Read one field of the open chooser without holding the borrow -
    /// every caller here goes on to call something that borrows again.
    fn with_chooser<T>(&self, f: impl FnOnce(&Chooser) -> T) -> Option<T> {
        self.state.borrow().chooser.as_ref().map(f)
    }

    /// Ask the host to close the panel, if one is open. The instance
    /// keeps its Chooser until the `mode-closed` event arrives, except
    /// where a caller explicitly drops it first.
    fn close_chooser(&self) {
        if let Some(mode) = self.with_chooser(|ch| ch.mode) {
            let _ = mode_close(mode);
        }
    }

    fn open_chooser(
        &mut self,
        window: u64,
        carried: Option<(NodeId, HashSet<NodeId>)>,
        return_pane: Option<u64>,
    ) {
        if self.state.borrow().chooser.is_some() {
            return;
        }
        let (selected, folded) = match carried {
            Some((sel, folded)) => (Some(sel), folded),
            None => (None, HashSet::new()),
        };
        // An empty feed still opens (the keybinding path): the panel
        // shows "no notifications" and q closes it.
        let (nrows, first) = {
            let st = self.state.borrow();
            let topo = Topo::fetch();
            let rows = build_rows(&st.entries, &topo, &folded);
            let first = rows.first().map(|r| r.id.clone());
            (rows.len(), first)
        };
        let Ok(wi) = resolve_window(WindowId(window as u32)) else { return };
        let win_w = wi.width;
        let win_h = wi.height;
        let width = self
            .chooser_width
            .map(|s| s.resolve(win_w))
            .unwrap_or_else(|| win_w.saturating_sub(8).min(90))
            .clamp(24.min(win_w.saturating_sub(4).max(10)),
                win_w.saturating_sub(4).max(10));
        let height = self
            .chooser_height
            .map(|s| s.resolve(win_h))
            .unwrap_or_else(|| (nrows as u32 + 3).max(10))
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
                    selected: selected
                        .or(first)
                        .unwrap_or_else(|| NodeId::Note(0)),
                    hint: 0,
                    folded,
                    offset: 0,
                    width,
                    height,
                };
                self.state.borrow_mut().chooser = Some(ch);
                redraw_chooser(&self.state);
            }
            Err(e) => log(&format!("chooser open failed: {}", e.message)),
        }
    }

    /// Jump to entry `seq`'s source pane, drop the entry and close.
    /// `client` is who pressed the key: select-window/select-pane only
    /// mutate session/window state, so a cross-session jump must also
    /// switch-client THAT client to the source's session or nothing
    /// visibly happens.
    fn chooser_jump(&mut self, ctx: &Ctx, seq: u64, client: Option<u64>) {
        let target = {
            let st = self.state.borrow();
            st.entries
                .iter()
                .find(|e| e.seq == seq)
                .map(|e| (e.src_window, e.src_pane))
        };
        let Some((src_window, src_pane)) = target else { return };
        self.close_chooser();
        // Cleared now (not at mode-closed) so the jump's own focus
        // change wins; the late mode-closed for this id is then a no-op.
        self.state.borrow_mut().chooser = None;

        let state = Rc::clone(&self.state);
        let (width, keeper_secs, show_when_visible) = self.view_params();
        ctx.spawn(async move {
            // Which sessions contain the source window, and where is the
            // pressing client right now?
            let sessions: Vec<u64> = src_window
                .and_then(|w| resolve_window(WindowId(w as u32)).ok())
                .map(|wi| ids(&wi.sessions))
                .unwrap_or_default();
            let client_info = client.and_then(|cid| {
                list_clients().ok()?.iter().find_map(|c| {
                    (u64::from(c.id) == cid).then(|| {
                        (
                            // Empty string = absent (the client has no
                            // usable name to switch by).
                            (!c.name.is_empty()).then(|| c.name.clone()),
                            c.session.map(u64::from),
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

    /// Drop `seqs` from the feed - one notification, or everything under
    /// a session or window row. Closes when the feed empties.
    fn chooser_dismiss(&mut self, ctx: &Ctx, seqs: &[u64], next: Option<NodeId>) {
        if seqs.is_empty() {
            return;
        }
        {
            let mut st = self.state.borrow_mut();
            st.entries.retain(|e| !seqs.contains(&e.seq));
            if let (Some(ch), Some(next)) = (st.chooser.as_mut(), next) {
                ch.selected = next;
            }
        }
        if self.state.borrow().entries.is_empty() {
            self.close_chooser();
        }
        // sync_views below redraws the panel; this keeps the feed and
        // the panel in step even when the task runs a beat later.
        redraw_chooser(&self.state);

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
        // What the key asks for. Cursor and fold moves happen under
        // the borrow; anything that calls back into self is decided here
        // and run after the borrow ends, so the chooser is never
        // borrowed twice.
        enum Act {
            Nothing,
            Redraw,
            Close,
            Jump(u64),
            Dismiss(Vec<u64>, Option<NodeId>),
        }

        let topo = Topo::fetch();
        let act = {
            let mut guard = self.state.borrow_mut();
            let st = &mut *guard;
            let Some(ch) = st.chooser.as_mut() else { return };
            // The cursor walks every drawn row, sessions and windows
            // included; folded children are not rows at all.
            let rows = build_rows(&st.entries, &topo, &ch.folded);
            let n = rows.len();
            let select = |ch: &mut Chooser, i: usize| {
                ch.selected = rows[i].id.clone();
                ch.hint = i;
            };

            match selected_row(ch, &rows) {
                // Nothing but the "no notifications" line.
                None => match key {
                    "q" | "Escape" => Act::Close,
                    _ => Act::Nothing,
                },
                Some(at) => match key {
                    "j" | "Down" => {
                        select(ch, (at + 1) % n);
                        Act::Redraw
                    }
                    "k" | "Up" => {
                        select(ch, (at + n - 1) % n);
                        Act::Redraw
                    }
                    // Fold keys, as mode-tree binds them: on a leaf or
                    // an already folded row, h steps out to the parent
                    // and folds that instead, and l steps down into
                    // what it just opened.
                    "h" | "Left" | "-" => {
                        let row = &rows[at];
                        let fold = if row.has_children && row.expanded {
                            Some(at)
                        } else {
                            row.parent.as_ref().and_then(|p| {
                                rows.iter().position(|r| &r.id == p)
                            })
                        };
                        match fold {
                            Some(i) => {
                                ch.folded.insert(rows[i].id.clone());
                                select(ch, i);
                            }
                            None => select(ch, (at + n - 1) % n),
                        }
                        Act::Redraw
                    }
                    "l" | "Right" | "+" => {
                        if rows[at].has_children && !rows[at].expanded {
                            ch.folded.remove(&rows[at].id);
                        } else {
                            select(ch, (at + 1) % n);
                        }
                        Act::Redraw
                    }
                    // Fold or unfold every session at once, as
                    // mode-tree binds M-- and M-+.
                    "M--" | "M-+" => {
                        if key == "M-+" {
                            ch.folded.clear();
                        } else {
                            for row in rows.iter().filter(|r| r.depth == 0) {
                                ch.folded.insert(row.id.clone());
                            }
                        }
                        Act::Redraw
                    }
                    // Enter jumps to a notification; on a grouping row
                    // it folds, so the key is never dead.
                    "Enter" => match rows[at].note {
                        Some(seq) => Act::Jump(seq),
                        None => {
                            if !ch.folded.remove(&rows[at].id) {
                                ch.folded.insert(rows[at].id.clone());
                            }
                            Act::Redraw
                        }
                    },
                    // On a grouping row this dismisses the whole
                    // subtree, folded children included.
                    "d" => {
                        let seqs =
                            node_notes(&rows[at].id, &st.entries, &topo);
                        // Prefer the row below, else the one above.
                        let next = rows
                            .get(at + 1)
                            .filter(|r| !seqs.contains(&r.note.unwrap_or(0)))
                            .or_else(|| rows.get(at.wrapping_sub(1)))
                            .map(|r| r.id.clone());
                        Act::Dismiss(seqs, next)
                    }
                    "q" | "Escape" => Act::Close,
                    "MouseDown1Pane" => {
                        // Rows start at screen row 0, scrolled by
                        // `offset`.
                        let idx =
                            ch.offset + mouse_row.unwrap_or(0) as usize;
                        if mouse_row.is_some() && idx < n {
                            select(ch, idx);
                            Act::Redraw
                        } else {
                            Act::Nothing
                        }
                    }
                    k => match k
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=9).contains(n))
                        .and_then(|i| {
                            rows.iter().filter_map(|r| r.note).nth(i - 1)
                        }) {
                        Some(seq) => Act::Jump(seq),
                        None => Act::Nothing,
                    },
                },
            }
        };

        match act {
            Act::Nothing => {}
            Act::Redraw => redraw_chooser(&self.state),
            Act::Close => self.close_chooser(),
            Act::Jump(seq) => self.chooser_jump(ctx, seq, client),
            Act::Dismiss(seqs, next) => self.chooser_dismiss(ctx, &seqs, next),
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
        if me.scope_kind != KIND_SERVER {
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
        // One row per pane is the feed's invariant, so restore it rather
        // than assume it: a snapshot taken before the rule existed holds
        // a row per ping. Newest wins, counts add up.
        let mut entries: VecDeque<Entry> = VecDeque::new();
        for e in snap.entries {
            match entries.iter().position(|x| x.src_pane == e.src_pane) {
                Some(i) => {
                    let count = entries.remove(i).map_or(0, |x| x.count.max(1));
                    entries.push_back(Entry {
                        count: count + e.count.max(1),
                        ..e
                    });
                }
                None => entries.push_back(e),
            }
        }
        // Config-derived fields from the fresh init (current config);
        // only the carried state comes from the snapshot.
        let me = Self {
            seq: snap.seq,
            state: Rc::new(RefCell::new(Shared {
                entries,
                views: snap
                    .views
                    .into_iter()
                    .map(|(w, (pane, shown))| {
                        (w, View { pane, shown, ..View::default() })
                    })
                    .collect(),
                // The host force-closes our modes during the swap, so
                // the new generation starts without a chooser.
                chooser: None,
            })),
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

        match event.name().as_str() {
            "pane-notification" => {}
            // Key binding: toggle the chooser in the target window.
            "plugin-command" => {
                if event.get_str("text") != Some("chooser") {
                    return;
                }
                let Some(window) = event.scope.window.map(u64::from) else {
                    return;
                };
                if let Some(same) =
                    self.with_chooser(|ch| ch.window == window)
                {
                    self.close_chooser();
                    self.state.borrow_mut().chooser = None;
                    if same {
                        return; // toggle off
                    }
                }
                self.open_chooser(
                    window,
                    None,
                    event.scope.pane.map(u64::from),
                );
                return;
            }
            // Chooser events, targeted at this instance by mode id.
            "mode-key" => {
                let matches = self.with_chooser(|ch| {
                    event.get_i64("mode") == Some(ch.mode.0 as i64)
                });
                if matches != Some(true) {
                    return;
                }
                let Some(key) = event.get_str("key") else {
                    return;
                };
                let mouse_row = event
                    .get_i64("mouse_y")
                    .and_then(|v| u64::try_from(v).ok());
                let client = event
                    .get_i64("client")
                    .and_then(|v| u64::try_from(v).ok());
                let key = key.to_string();
                self.chooser_key(ctx, &key, mouse_row, client);
                return;
            }
            "mode-resize" => {
                {
                    let mut st = state.borrow_mut();
                    let Some(ch) = st.chooser.as_mut() else { return };
                    if event.get_i64("mode") != Some(ch.mode.0 as i64) {
                        return;
                    }
                    if let Some(w) = event
                        .get_i64("width")
                        .and_then(|v| u32::try_from(v).ok())
                    {
                        ch.width = w;
                    }
                    if let Some(h) = event
                        .get_i64("height")
                        .and_then(|v| u32::try_from(v).ok())
                    {
                        ch.height = h;
                    }
                }
                redraw_chooser(&state);
                return;
            }
            "mode-closed" => {
                let mine = self.with_chooser(|ch| {
                    event.get_i64("mode") == Some(ch.mode.0 as i64)
                });
                if mine == Some(true) {
                    let ch = self.state.borrow_mut().chooser.take().unwrap();
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
                // Decided under a short borrow, because the reopen
                // path calls back into self.
                let reopen = {
                    let mut st = state.borrow_mut();
                    let mut reopen = None;
                    if let Some(ch) = st.chooser.as_mut() {
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
                                        // Carry the cursor and the
                                        // folds over the reopen.
                                        reopen = Some((
                                            w,
                                            ch.selected.clone(),
                                            std::mem::take(&mut ch.folded),
                                        ));
                                        st.chooser = None;
                                    }
                                },
                                None => {
                                    let _ = mode_close(ch.mode);
                                    st.chooser = None;
                                }
                            }
                        }
                    }
                    reopen
                };
                if let Some((w, selected, folded)) = reopen {
                    self.open_chooser(w, Some((selected, folded)), None);
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
                if state.borrow().chooser.is_some() {
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
                let return_pane = event
                    .get_i64("old_pane")
                    .and_then(|v| u64::try_from(v).ok());
                log("notification pane clicked: opening chooser");
                self.open_chooser(window, None, return_pane);
                return;
            }
            _ => return,
        }

        let Some(src_pane) = event.scope.pane else { return };
        let src_window = event.scope.window.map(u64::from);
        let Some(msg) = event.get_str("text") else {
            return;
        };
        if msg.trim().is_empty() {
            return;
        }

        let msg = msg.trim().to_string();
        self.seq += 1;
        let seq = self.seq;
        let duration = self.duration;

        let src_pane = u64::from(src_pane);
        ctx.spawn(async move {
            // What the pane is, captured now: a notification names the
            // moment it fired, and the pane may be gone by the time the
            // user reads it.
            let entry = {
                let probe = Entry {
                    seq,
                    src_window,
                    src_pane,
                    msg,
                    cmd: pane_format(src_pane, FMT_COMMAND).await,
                    title: pane_format(src_pane, FMT_TITLE).await,
                    session_name: String::new(),
                    window_name: String::new(),
                    count: 0,
                };
                let loc = locate(&Topo::fetch(), &probe);
                Entry {
                    session_name: loc.session_name,
                    window_name: loc.window_name,
                    ..probe
                }
            };
            {
                let mut st = state.borrow_mut();
                // A pane holds one row however often it pings: a repeat
                // replaces the old entry, carrying its tally and taking
                // a fresh seq. The old entry's expiry task fires later,
                // finds that seq gone and does nothing.
                let count = st
                    .entries
                    .iter()
                    .find(|e| e.src_pane == src_pane)
                    .map_or(0, |e| e.count.max(1));
                st.entries.retain(|e| e.src_pane != src_pane);
                st.entries.push_back(Entry { count: count + 1, ..entry });
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
