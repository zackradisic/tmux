//! The view half: the picker. It merges the local provider's roster (read
//! straight from this server's store) with the rosters of the providers on
//! linked servers (fetched through services and kept in [`Remotes`]), and
//! shows them grouped by server, then by state band.
//!
//! Rows carry their server; ids are unique per server only, so marks and
//! ranks key on [`Agent::key`]. Ages use each provider's clock: the
//! snapshot's `now_ms` gives the skew to correct by. A remote row's pane
//! is a pane on the other machine: Enter jumps to its local shadow when a
//! `remote-attach` link mirrors it, and the preview shows the shadow grid
//! live, else the provider's captured text.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

use crate::index;
use crate::provider::{self, mode_label, run_content_search, ListReq, Snapshot, StatsReq, TurnsReq};
use crate::store::{self, Agent, Stats, TurnRow, LOCAL};
use crate::transcript::{self, TranscriptHit};
use crate::{Config, PickKeys, HISTORY_MAX};

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
/// While the picker stays open, re-read the harness session files (and
/// the remote rosters) on this cadence.
const REFRESH_MS: u64 = 2000;
/// While at least one remote fetch is outstanding, repaint on this
/// cadence so the per-server spinner turns. It runs ONLY while something
/// is in flight, so an idle picker never repaints on it.
const SPIN_MS: u64 = 100;
/// A server that answered this recently is not fetched again: the refresh
/// tick and a link-up event can land on the same server at once.
const FETCH_DEBOUNCE_MS: u64 = 300;
/// Opening the picker reuses a roster this fresh instead of refetching.
/// It is the cadence an open picker refreshes at anyway, so reopening in
/// a hurry never shows anything an open picker would not have shown.
const OPEN_FRESH_MS: u64 = REFRESH_MS;
/// At most this many remote calls are on the wire at once. The point of
/// fetching together is to pay the max round trip instead of the sum;
/// past a handful of servers that stops being true (each hop is an ssh
/// process on this machine), so the rest queue behind these.
const MAX_INFLIGHT: usize = 4;
/// A fetch stays invisible until it has been outstanding this long. The
/// 2s tick refetches every server, and a healthy remote answers well
/// inside this, so the spinner does not blink at you twice a second for
/// nothing - it appears only when a server is actually being slow.
const SPIN_GRACE_MS: u64 = 500;
/// A fetch outstanding this long has stopped being a blink; the header
/// says how long it has been waiting instead of only spinning.
const FETCH_STUCK_MS: u64 = 3000;
/// An in-flight mark older than this is not believed (the host fails a
/// service call at 30s), so a lost task cannot wedge a server forever.
const FETCH_STALE_MS: u64 = 35_000;
/// Spinner frames, one per [`SPIN_MS`].
const SPIN_FRAMES: [&str; 10] =
    ["\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}"];

// ---------------------------------------------------------------------------
// remote rosters
// ---------------------------------------------------------------------------

/// What a provider on another server last reported.
#[derive(Debug, Clone, Default)]
pub struct RemoteRows {
    pub agents: Vec<Agent>,
    /// local clock - provider clock, at the last snapshot.
    pub skew_ms: i64,
    /// When the last snapshot arrived (local clock).
    pub fetched_ms: u64,
    /// When the server's link went down (local clock); None while up.
    pub down_since: Option<u64>,
    /// When the fetch now in flight started (local clock); None when
    /// nothing is outstanding. Doubles as the in-flight flag the
    /// debounce reads and as the spinner's clock for this server.
    pub fetching_since: Option<u64>,
}

/// The remote rosters, by server name. Shared between the plugin (which
/// feeds it from events) and the picker (which reads it).
#[derive(Debug, Default)]
pub struct Remotes {
    pub servers: HashMap<String, RemoteRows>,
    /// Servers whose copy of this plugin this side does not accept, with
    /// the reason to show ("agents 0.2.0 there, 0.1.0 here").
    pub mismatch: HashMap<String, String>,
    /// A spinner task is turning; one is enough for every server.
    pub spinning: bool,
    /// When the fetch round a picker-open started began (local clock).
    /// Opening again while one is outstanding starts nothing new.
    pub open_round_since: Option<u64>,
}

impl Remotes {
    /// Take a provider's snapshot for a server.
    pub fn apply(&mut self, server: &str, snap: Snapshot) {
        let now = now_ms();
        let e = self.servers.entry(server.to_string()).or_default();
        e.skew_ms = now as i64 - snap.now_ms;
        e.fetched_ms = now;
        e.down_since = None;
        self.mismatch.remove(server);
        e.agents = snap
            .agents
            .into_iter()
            .map(|mut a| {
                a.server = server.to_string();
                a
            })
            .collect();
    }

    pub fn mark_down(&mut self, server: &str) {
        let e = self.servers.entry(server.to_string()).or_default();
        if e.down_since.is_none() {
            e.down_since = Some(now_ms());
        }
    }

    pub fn mark_up(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.down_since = None;
        }
    }

    /// The server's copy runs a service version this side rejects: no
    /// rows from it, one line that says why.
    pub fn mark_mismatch(&mut self, server: &str, why: String) {
        if let Some(e) = self.servers.get_mut(server) {
            e.agents.clear();
        }
        self.mismatch.insert(server.to_string(), why);
    }

    /// Claim a server for a fetch, or refuse it. Refused when one is
    /// already in flight, or when the last snapshot landed less than
    /// `min_age_ms` ago: the open, the 2s tick and a link-up event all
    /// reach for the same servers, and a second claim would mean a
    /// second ssh hop for a roster we are already holding or already
    /// waiting on. A mark older than [`FETCH_STALE_MS`] outlived any
    /// call the host would still be holding, so it is not believed.
    fn begin_fetch(&mut self, server: &str, min_age_ms: u64) -> bool {
        let now = now_ms();
        let e = self.servers.entry(server.to_string()).or_default();
        if e.fetching_since
            .is_some_and(|t| now.saturating_sub(t) < FETCH_STALE_MS)
        {
            return false;
        }
        if now.saturating_sub(e.fetched_ms) < min_age_ms {
            return false;
        }
        e.fetching_since = Some(now);
        true
    }

    /// A claimed server's call is starting now: restart its clock, so a
    /// server that waited for a slot does not show the wait as if the
    /// remote were slow to answer.
    fn start_fetch(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.fetching_since = Some(now_ms());
        }
    }

    fn end_fetch(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.fetching_since = None;
        }
    }

    /// Per server with a fetch in flight: when it started.
    fn fetching(&self) -> HashMap<String, u64> {
        self.servers
            .iter()
            .filter_map(|(k, v)| v.fetching_since.map(|t| (k.clone(), t)))
            .collect()
    }

    /// Every remote row, in server-name order.
    fn rows(&self) -> Vec<Agent> {
        let mut names: Vec<&String> = self.servers.keys().collect();
        names.sort();
        names
            .into_iter()
            .flat_map(|n| self.servers[n].agents.iter().cloned())
            .collect()
    }
}

/// Fetch the roster of every connected remote server. `req` says what to
/// ask for beyond the live rows (history, the archive).
///
/// The calls run TOGETHER, not one after another: these are expensive
/// hops (a ProxyCommand, a Tailscale link, a box in a cloud region), so
/// serially the wait is their sum rather than their max. Each snapshot is
/// applied and repainted the moment it lands, so a fast server is not
/// held back by a slow one. Servers already being fetched, or fetched a
/// moment ago, are left alone (see [`Remotes::begin_fetch`]).
pub async fn fetch_remotes(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    req: ListReq,
) {
    fetch_all(picker, remotes, req, FETCH_DEBOUNCE_MS).await
}

/// [`fetch_remotes`] without the debounce: after acting on a remote row
/// we want that server's new state now, however recently it answered. A
/// fetch already in flight is still not doubled - the act itself took a
/// round trip, so the outstanding call is almost certainly newer than it.
pub async fn fetch_remotes_now(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    req: ListReq,
) {
    fetch_all(picker, remotes, req, 0).await
}

/// The fetch a picker-open starts. Opening the picker is a key press, so
/// it can happen a dozen times in a few seconds; none of that may turn
/// into a dozen rounds of ssh. Two brakes: a round already running from
/// an earlier open starts nothing at all, and a server whose roster is
/// younger than [`OPEN_FRESH_MS`] is left alone.
pub async fn fetch_remotes_on_open(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    req: ListReq,
) {
    // Decide under the borrow, act after it. start_spinner takes the
    // same RefCell, so calling it from inside this block panics with
    // "already borrowed" - which disabled the plugin after three opens.
    let outstanding = {
        let mut r = remotes.borrow_mut();
        let now = now_ms();
        // A round outstanding longer than any call the host would still
        // be holding is not believed - it must never wedge the open.
        if r.open_round_since
            .is_some_and(|t| now.saturating_sub(t) < FETCH_STALE_MS)
        {
            true
        } else {
            r.open_round_since = Some(now);
            false
        }
    };
    if outstanding {
        // Still show the spinner: the outstanding round owns the
        // servers this open would have asked for.
        start_spinner(&picker, &remotes);
        return;
    }
    fetch_all(Rc::clone(&picker), Rc::clone(&remotes), req, OPEN_FRESH_MS).await;
    remotes.borrow_mut().open_round_since = None;
}

/// Claim what is worth fetching, then work it off at most
/// [`MAX_INFLIGHT`] calls at a time. Claiming up front (before any await)
/// is what keeps a second caller - another tick, another open - from
/// asking the same server twice.
async fn fetch_all(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    req: ListReq,
    min_age_ms: u64,
) {
    let list = service::servers().unwrap_or_default();
    let mine = list
        .iter()
        .find(|s| s.local)
        .map(|s| s.version.clone())
        .unwrap_or_default();
    let mut queue: Vec<String> = Vec::new();
    // Only servers this side linked to: never fetch a roster from an
    // inbound peer (it would be a gated remote -> initiator call).
    for s in list.into_iter().filter(|s| !s.local && s.up && s.linked) {
        if !s.accepted {
            remotes.borrow_mut().mark_mismatch(
                &s.name,
                format!("agents {} there, {} here; run tmux update", s.version, mine),
            );
            continue;
        }
        if !remotes.borrow_mut().begin_fetch(&s.name, min_age_ms) {
            continue;
        }
        queue.push(s.name);
    }
    // Even with nothing to start, a fetch from another task may still be
    // outstanding and want a spinner; the task exits on its own when
    // none is.
    start_spinner(&picker, &remotes);
    if queue.is_empty() {
        return;
    }
    // Oldest roster first, so the group most out of date comes back
    // first when there are more servers than slots.
    queue.sort_by_key(|n| {
        remotes.borrow().servers.get(n).map(|e| e.fetched_ms).unwrap_or(0)
    });
    queue.reverse(); // workers pop from the end
    let n = MAX_INFLIGHT.min(queue.len());
    let queue = Rc::new(RefCell::new(queue));
    let futs: Vec<Pin<Box<dyn Future<Output = ()>>>> = (0..n)
        .map(|_| {
            Box::pin(fetch_worker(
                Rc::clone(&picker),
                Rc::clone(&remotes),
                Rc::clone(&queue),
                req,
            )) as Pin<Box<dyn Future<Output = ()>>>
        })
        .collect();
    JoinAll { futs }.await;
}

/// One slot on the wire: take the next claimed server, fetch it, repeat
/// until the queue is empty.
async fn fetch_worker(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    queue: Rc<RefCell<Vec<String>>>,
    req: ListReq,
) {
    loop {
        let next = queue.borrow_mut().pop();
        let Some(server) = next else { return };
        fetch_one(Rc::clone(&picker), Rc::clone(&remotes), server, req).await;
    }
}

/// One server's roster, applied and repainted as it lands.
async fn fetch_one(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    server: String,
    req: ListReq,
) {
    remotes.borrow_mut().start_fetch(&server);
    let target = format!("@{server}");
    let res = service::call_json::<_, Snapshot>(&target, "list", &req)
    .await;
    {
        let mut r = remotes.borrow_mut();
        r.end_fetch(&server);
        match res {
            Ok(snap) => r.apply(&server, snap),
            Err(e) => {
                // Not linked for plugins (an old remote, or no provider
                // yet): nothing to show, but nothing to break either.
                if e.code == ErrorCode::Unreachable {
                    r.mark_down(&server);
                } else if e.code == ErrorCode::Version {
                    r.mark_mismatch(&server, e.message.clone());
                }
            }
        }
    }
    refresh_if_open(&picker, &remotes).await;
}

/// Poll a handful of futures together to completion. The guest carries no
/// futures crate; this is the whole of it - re-poll whatever is still
/// pending on each wake, finish when nothing is.
struct JoinAll {
    futs: Vec<Pin<Box<dyn Future<Output = ()>>>>,
}

impl Future for JoinAll {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let futs = &mut self.get_mut().futs;
        futs.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if futs.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Turn the per-server spinner while any fetch is outstanding. One task
/// at a time (the `spinning` flag), and it ends the moment nothing is in
/// flight, so an idle picker is not repainting forever on a 100ms tick.
fn start_spinner(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    if picker.borrow().is_none() {
        return;
    }
    {
        let mut r = remotes.borrow_mut();
        if r.spinning {
            return;
        }
        r.spinning = true;
    }
    let picker = Rc::clone(picker);
    let remotes = Rc::clone(remotes);
    spawn(async move {
        loop {
            if sleep_ms(SPIN_MS).await.is_err() {
                break;
            }
            let fetching = remotes.borrow().fetching();
            if fetching.is_empty() {
                break;
            }
            let now = now_ms();
            // Nothing has been outstanding long enough to draw yet: keep
            // ticking (one of these may get slow) but do not repaint. A
            // fetch that finishes inside the grace costs no renders at
            // all, which is the whole point - the 2s tick refetches
            // every server and must not blink a spinner each time.
            if !fetching.values().any(|t| now.saturating_sub(*t) >= SPIN_GRACE_MS) {
                continue;
            }
            let mut b = picker.borrow_mut();
            let Some(p) = b.as_mut() else { break };
            // Only the frame and the server headers move: re-render off
            // the rows already in hand - no DB read, no file scan, and
            // no refilter (content search would re-grep every tick).
            p.now_ms = now;
            p.fetching = fetching;
            p.multi = is_multi(&p.rows, &p.fetching, now);
            p.rebuild_lines();
            pick_render(p);
        }
        remotes.borrow_mut().spinning = false;
    });
}

/// Follow the roster topic of a server, so changes arrive without a poll.
pub fn follow(server: &str) {
    let _ = service::subscribe(&format!("@{server}"), provider::TOPIC);
    let _ = service::subscribe(&format!("@{server}"), provider::OPEN_TOPIC);
}

/// The client to open a picker for when no key press names one: the
/// first attached client, which on a workstation is the user's.
pub fn any_client() -> Option<u64> {
    list_clients().ok()?.into_iter().next().map(|c| u64::from(c.id))
}

/// Ask a remote provider to act on one of its rows. The reply is a plain
/// "ok", so this is a byte call, not a JSON one.
/// The provider's answer, or its refusal as text a status line can
/// show. A refusal has one common cause worth naming: the provider on
/// that server is an older agents build that does not know the verb. Its
/// `act` falls through to "act: <verb> failed", which read on this side
/// as nothing happening - the row stayed put and the log alone said why.
async fn act_remote(
    server: &str,
    id: &str,
    verb: &str,
    name: Option<&str>,
) -> Result<(), String> {
    let target = format!("@{server}");
    let req = provider::ActReq {
        id: id.to_string(),
        verb: verb.to_string(),
        name: name.map(str::to_string),
    };
    let bytes = serde_json::to_vec(&req).unwrap_or_default();
    match service::call(&target, "act", &bytes).await {
        Ok(_) => Ok(()),
        Err(e) => {
            log(&format!("agents: {verb} on {server}: {}", e.message));
            let why = if e.message.contains("failed") {
                format!("{server} refused {verb}: its agents plugin is older - update it")
            } else {
                format!("{server}: {}", e.message)
            };
            Err(why)
        }
    }
}

// ---------------------------------------------------------------------------
// the picker
// ---------------------------------------------------------------------------

pub enum PickAfter {
    None,
    Close(ModeId),
    /// Jump to a local pane (a row's own pane, or the mirror of a remote
    /// one); close the mode after.
    Jump(u32, ModeId),
    /// Set the life of these (server, id) rows.
    Life(Vec<(String, String)>, String),
    /// Set the turn status of these (server, id) rows by hand.
    Status(Vec<(String, String)>, String),
    /// Put this text on the pressing client's clipboard.
    Copy(String),
    /// A wheel notch over the live preview: into the pane it shows, at
    /// that pane's cell (x, y), as if the pointer were there.
    Wheel(u32, String, u32, u32),
    /// Open the action menu on the pressing client.
    Menu,
    Reload,
    Resize(ModeId, u32, u32),
    /// Rename (server, id) to the name.
    Rename(String, String, String),
    /// Send a message to (server, id) through its mailbox.
    Message(String, String, String),
    /// Interrupt the agent in this local pane: C-c, nothing else. For a
    /// remote row this is its shadow, whose fd is one end of the link's
    /// socketpair, so the key travels to the remote like a typed one.
    Interrupt(u32),
    /// Kill this local pane. `kill-pane` on a shadow runs on the remote
    /// server (see tmux.1, REMOTE SESSIONS), so it kills the real pane.
    KillPane(u32),
    /// Type this key into the local pane the preview shows: the keyboard
    /// is the preview's. Same route as `Interrupt`, one key per press.
    Type(u32, String),
    /// Open the new-agent form over the picker, prefilled from the
    /// highlighted row (see `newagent`).
    /// The new-agent form; `true` opens it as a fork of the row.
    NewAgent(bool),
    /// Mark these (server, id) rows read (true) or unread (false).
    Read(Vec<(String, String)>, bool),
}

/// One rendered line: a server header (only when rows come from more than
/// one server), a band header, or a selectable row (by its position in
/// `view`). Headers make the list scroll in "display space", so the
/// selected row stays visible even with headers between the bands.
#[derive(Clone)]
pub enum Line {
    Server(String),
    Header(u8),
    Item(usize),
}

pub struct Picker {
    pub mode: ModeId,
    pub width: u32,
    pub height: u32,
    pub rows: Vec<Agent>,
    pub view: Vec<usize>,
    pub lines: Vec<Line>,
    pub sel: usize,
    pub top: usize,
    /// Row keys (server + id) marked for a bulk action, so they survive a
    /// reload/refilter without a stale-index risk.
    pub marked: HashSet<String>,
    pub filter: String,
    pub filtering: bool,
    /// A rename in progress: the typed name for the selected agent.
    pub renaming: bool,
    pub rename_buf: String,
    /// When on, the filter also matches live pane CONTENTS: the grid of
    /// each live agent's pane is grep'd for the query, in tmux, through
    /// `panes_search` (locally) or the provider's `search` (remotely).
    pub content_search: bool,
    /// The matching snippet per row key ([`Agent::key`]), from the last
    /// content search. Drives the row's snippet and the OR in the filter.
    pub content_hits: HashMap<String, String>,
    /// The saved captures of the local archived rows whose pane is gone,
    /// by row key; loaded with the rows in the archive-only view, so a
    /// content search can grep what those rows no longer show anywhere.
    pub captures: Vec<(String, String)>,
    /// The matcher the last content search actually used (auto-detected,
    /// with a fuzzy fallback). Shown in the footer/header.
    pub content_mode: SearchMode,
    /// The query the remote searches were sent for; a reply for another
    /// query is stale and dropped.
    pub content_query: String,
    pub now_ms: u64,
    pub show_history: bool,
    /// Only the archived rows: the archive as a list of its own, rather
    /// than a dozen set-aside agents mixed into a hundred finished ones.
    /// Implies `show_history`.
    pub archived_only: bool,
    /// Whether history was on before the archive view was entered, so
    /// leaving it puts the roster back the way it was.
    pub history_before_archive: bool,
    pub keys: PickKeys,
    /// The new-agent form's launchers (name, shell line): the configured
    /// ones, then the detected harness commands, see `Config::launchers`.
    pub launchers: Vec<(String, String)>,
    pub status: Option<String>,
    /// A `g` was pressed and waits for a second `g` (vim `gg` = go top).
    pub pending_g: bool,
    /// A kill was asked for on this local pane and waits for a second
    /// press to confirm. Killing a pane cannot be undone, and the row the
    /// cursor sits on moves under a refresh, so the pane is remembered
    /// here rather than re-read from the selection on the second press.
    pub pending_kill: Option<u32>,
    /// A stable display rank per row key, assigned in the recency order
    /// the FIRST time each agent is seen this session. Live refreshes sort
    /// by server, band then this rank, so an activity-time bump never
    /// reshuffles rows under the cursor.
    pub order: HashMap<String, u64>,
    pub order_next: u64,
    /// The 2s refresh task, cancelled when the picker closes or reopens.
    pub timer: Option<TaskId>,
    /// The pane the picker was opened from. Its row gets a "you are here"
    /// border, so you can spot the agent you are currently sitting on, and
    /// the cursor opens on it.
    pub current_pane: Option<u32>,
    /// The store row for `current_pane` when its agent is no longer live:
    /// a finished or archived row has no live pane to match, so the
    /// cursor finds it by id in the history (or archive) view the picker
    /// opened into for it.
    pub here_id: Option<String>,
    /// The server user's home, for `~` paths in the `~dir` filter and its
    /// dropdown (one host call at open, not one per row per keystroke).
    pub home: Option<String>,
    /// The dropdown under the search box while a filter token is being
    /// typed: the values on the roster that complete it, with how many
    /// rows each has. Empty when there is nothing to offer, or after Esc
    /// dismissed it for the token being typed.
    pub completions: Vec<(String, usize)>,
    pub completion_idx: usize,
    pub completion_hidden: bool,
    /// The cursor has yet to land on `current_pane`'s row. Set at open;
    /// cleared once it lands, or once the user moves the cursor
    /// themselves. While set, each refresh tries again: a remote row's
    /// roster can land after the picker is already on screen.
    pub seek_here: bool,
    /// Per server: local clock minus the provider's clock.
    pub skew: HashMap<String, i64>,
    /// Per server: when its link went down (local clock), while it is.
    pub down: HashMap<String, u64>,
    /// Per server: why this side rejects its copy of the plugin.
    pub mismatch: HashMap<String, String>,
    /// Composing a message to the selected agent.
    pub composing: bool,
    pub msg_buf: String,
    /// Unread message count per (server, agent id), from each server's
    /// mailbox plugin. Absent or zero means no badge.
    pub unread: HashMap<(String, String), i64>,
    /// Per server: when the fetch now in flight for it started (local
    /// clock). Drives the header's spinner.
    pub fetching: HashMap<String, u64>,
    /// (server, remote pane) -> the local shadow pane that mirrors it.
    pub mirrors: HashMap<(String, u32), u32>,
    /// The captured text of the highlighted remote row that has no local
    /// mirror, by row key, once the provider answered.
    pub remote_capture: Option<(String, Vec<String>)>,
    /// Rows come from more than one server: show server headers.
    pub multi: bool,
    /// The keyboard belongs to the preview: every key goes to the
    /// highlighted agent's pane (see `PickAfter::Type`), except the one
    /// that takes it back. The list still refreshes underneath, but no
    /// key moves the cursor - a selection that moved under your prompt
    /// would send the rest of it to another agent.
    pub preview_focus: bool,
    /// The query the conversation hits below are for.
    pub transcript_query: String,
    /// Conversation hits for that query, by row key: the turn to open on,
    /// the score, the line that matched. Local ones are computed inside
    /// the keystroke (see `transcript_search`); their snippets, and the
    /// remote servers' hits, land a moment later.
    pub transcript_hits: HashMap<String, TranscriptHit>,
    /// Rows a conversation hit brought in that the roster did not hold
    /// (finished agents with history off, or past its cap). Appended to
    /// `rows` after the roster's own on every refilter; gone with the
    /// query.
    pub hit_rows: Vec<Agent>,
    /// How many of `rows` are the roster's own: the rest are hit rows,
    /// and are cut off before they are merged again.
    pub roster_len: usize,
    /// The conversation in the preview, when the highlighted row shows
    /// one (a finished agent, a remote row with no mirror, or a live row
    /// with `show_transcript`).
    pub transcript: Option<TranscriptView>,
    /// Show the highlighted live row's conversation instead of its pane.
    pub show_transcript: bool,
    /// The keyboard belongs to the conversation in the preview: keys
    /// scroll it and step through its matches (see `dispatch_key`).
    pub transcript_focus: bool,
    /// Show the info card for the highlighted row in the preview (`i`).
    pub show_info: bool,
    /// The quick reference in place of the preview (`?`).
    pub show_help: bool,
    /// The card's fetched half, for the row it was fetched for.
    pub info: Option<InfoCard>,
}

/// What the info card fetches beyond the row itself.
pub struct InfoCard {
    pub key: String,
    pub fetched_ms: u64,
    pub stats: Option<Stats>,
    /// A live local pane's directory right now (fresher than the store).
    pub live_cwd: Option<String>,
    /// `git status` says the tree has changes (local rows with a cwd).
    pub dirty: Option<bool>,
}

/// A conversation rendered for the preview.
pub struct TranscriptView {
    /// The row it belongs to.
    pub key: String,
    /// When the turns were fetched (local clock): a live agent's
    /// conversation grows, so it is fetched again on the refresh cadence.
    pub fetched_ms: u64,
    pub turns: Vec<TurnRow>,
    /// The turn it opened on (a hit's), or -1 for the end.
    pub open_seq: i64,
    /// Scroll position, in rendered lines.
    pub top: usize,
    /// The width the lines were rendered for.
    pub width: usize,
    /// The rendered lines, as styled cells; emitted to the terminal at
    /// draw time, so the match the cursor is on can be drawn brighter.
    pub lines: Vec<Vec<Styled>>,
    /// The rendered lines that hold a match of the query, in order, and
    /// which of them `n`/`N` last landed on.
    pub matches: Vec<usize>,
    pub match_idx: Option<usize>,
    /// The saved capture that stands in when there are no turns.
    pub capture: Vec<String>,
}

impl Picker {
    /// The width of the list column; the preview takes the rest. 60% of
    /// the width, but never let the clamp's min exceed its max: a narrow
    /// mode (a split pane) would panic `clamp(30, <30)` and trap the
    /// guest. Below ~50 cols give the list almost everything and skip the
    /// side preview.
    fn list_w(&self) -> usize {
        let w = self.width as usize;
        if w <= 50 {
            w.saturating_sub(2).max(1)
        } else {
            (w * 6 / 10).clamp(30, w - 20)
        }
    }

    /// The preview cannot keep the keyboard without a pane to type into:
    /// the agent may have finished, or a refresh replaced the rows and
    /// the highlighted one is a remote row with no mirror here.
    fn sync_focus(&mut self) {
        if self.preview_focus && live_pane_of_selection(self).is_none() {
            self.preview_focus = false;
        }
    }
    /// What to ask the store and every provider for beyond the live rows.
    pub fn list_req(&self) -> ListReq {
        ListReq { history: self.show_history, archived: self.archived_only }
    }

    /// Rebuild the display lines from `view`, inserting a server header
    /// whenever the server changes (multi-server only) and a band header
    /// whenever the band changes.
    fn rebuild_lines(&mut self) {
        self.lines.clear();
        let mut prev_server: Option<&str> = None;
        let mut prev: Option<u8> = None;
        for (vpos, &ri) in self.view.iter().enumerate() {
            let a = &self.rows[ri];
            if self.multi && prev_server != Some(a.server.as_str()) {
                self.lines.push(Line::Server(a.server.clone()));
                prev_server = Some(a.server.as_str());
                prev = None;
            }
            let b = band(a);
            if prev != Some(b) {
                self.lines.push(Line::Header(b));
                prev = Some(b);
            }
            self.lines.push(Line::Item(vpos));
        }
        // A server whose copy this side rejects has no rows, and nor
        // does one being fetched for the first time; give both a line
        // anyway, so the reason (or the spinner) is on screen. A fetch
        // still inside its grace is not one of them: a line that appears
        // and vanishes within 500ms is worse than no line.
        let now = self.now_ms;
        let mut odd: Vec<&String> = self
            .mismatch
            .keys()
            .chain(
                self.fetching
                    .iter()
                    .filter(|(_, t)| now.saturating_sub(**t) >= SPIN_GRACE_MS)
                    .map(|(k, _)| k),
            )
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|s| !self.rows.iter().any(|a| a.server == **s))
            .collect();
        odd.sort();
        for s in odd {
            self.lines.push(Line::Server(s.clone()));
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
        // Headers directly above the window waste lines; pull them in.
        while self.top > 0
            && matches!(self.lines.get(self.top), Some(Line::Item(_)) | Some(Line::Header(_)))
            && matches!(
                self.lines.get(self.top - 1),
                Some(Line::Header(_)) | Some(Line::Server(_))
            )
        {
            self.top -= 1;
        }
    }

    /// Put the cursor on the row for the pane the picker was opened from,
    /// if that row is on screen and the cursor has not been moved since
    /// open. Returns whether it landed.
    fn select_here(&mut self) -> bool {
        if !self.seek_here || (self.current_pane.is_none() && self.here_id.is_none()) {
            return false;
        }
        let pos = self.view.iter().position(|&i| self.is_here(&self.rows[i]));
        let Some(pos) = pos else { return false };
        self.sel = pos;
        self.seek_here = false;
        self.scroll_to_selection();
        true
    }

    /// Is this the row for the pane the picker was opened from? A live
    /// row through its local pane (a remote row through its mirror; an
    /// unmirrored remote row never, since its pane id is another
    /// server's); a finished or archived row through the id resolved at
    /// open, since it has no live pane to match.
    pub fn is_here(&self, a: &Agent) -> bool {
        // An id resolved at open (a finished or archived row of the pane,
        // or the id `pick id` was given) matches wherever the row is.
        if self.here_id.as_deref() == Some(a.id.as_str()) {
            return true;
        }
        if self.current_pane.is_none() || !a.live() {
            return false;
        }
        self.local_pane_of(a) == self.current_pane
    }

    /// The highlighted row.
    pub fn selected(&self) -> Option<&Agent> {
        self.rows.get(*self.view.get(self.sel)?)
    }

    /// The local pane a row's pane shows in: the pane itself for a local
    /// row, the shadow pane for a mirrored remote row.
    pub fn local_pane_of(&self, a: &Agent) -> Option<u32> {
        let pane = a.pane.filter(|_| a.live())? as u32;
        if a.is_local() {
            return Some(pane);
        }
        self.mirrors.get(&(a.server.clone(), pane)).copied()
    }

    /// Milliseconds since the agent was last active, on the local clock.
    fn age_ms(&self, a: &Agent) -> u64 {
        let skew = self.skew.get(&a.server).copied().unwrap_or(0);
        let active = (a.active_ms() + skew).max(0) as u64;
        self.now_ms.saturating_sub(active)
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

/// The shadow panes this server holds for panes elsewhere:
/// (host, remote pane id) -> local pane id.
fn find_mirrors() -> HashMap<(String, u32), u32> {
    let mut out = HashMap::new();
    for p in list_panes().unwrap_or_default() {
        if !p.remote {
            continue;
        }
        let Ok(rid) = format_expand(OptionTarget::Pane(PaneId(p.id)), "#{pane_remote_id}")
        else {
            continue;
        };
        if let Some(n) = rid.strip_prefix('%').and_then(|s| s.parse::<u32>().ok()) {
            out.insert((p.host.clone(), n), p.id);
        }
    }
    out
}

/// What [`gather_rows`] hands back: the rows plus the per-server state
/// the picker renders with.
struct Gathered {
    rows: Vec<Agent>,
    /// The saved captures of the local archived rows whose pane is gone,
    /// by row key. Only loaded for the archive-only view.
    captures: Vec<(String, String)>,
    skew: HashMap<String, i64>,
    down: HashMap<String, u64>,
    mismatch: HashMap<String, String>,
    fetching: HashMap<String, u64>,
}

/// Local rows (from the store) plus every remote row, with the per-server
/// clock skew and link state the picker renders with. `req` is what the
/// view wants beyond the live rows, the same request the providers get.
async fn gather_rows(
    remotes: &Rc<RefCell<Remotes>>,
    req: ListReq,
    enrich: bool,
) -> Gathered {
    let mut rows = store::live_agents().await.unwrap_or_default();
    if enrich {
        provider::enrich_live(&mut rows).await;
    }
    let mut captures = Vec::new();
    if req.archived {
        rows.extend(store::archived().await.unwrap_or_default());
        captures = store::archived_captures()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(id, text)| (store::row_key(LOCAL, &id), text))
            .collect();
    } else if req.history {
        rows.extend(store::history(HISTORY_MAX).await.unwrap_or_default());
    }
    let r = remotes.borrow();
    rows.extend(r.rows());
    let skew = r.servers.iter().map(|(k, v)| (k.clone(), v.skew_ms)).collect();
    let down = r
        .servers
        .iter()
        .filter_map(|(k, v)| v.down_since.map(|t| (k.clone(), t)))
        .collect();
    Gathered {
        rows,
        captures,
        skew,
        down,
        mismatch: r.mismatch.clone(),
        fetching: r.fetching(),
    }
}

/// The open picker's [`Picker::list_req`], or the plain live roster when
/// no picker is open.
pub fn list_req_of(picker: &Rc<RefCell<Option<Picker>>>) -> ListReq {
    picker.borrow().as_ref().map(Picker::list_req).unwrap_or_default()
}

/// Server headers are worth the lines once rows come from more than this
/// server - or once a remote has been slow enough to show a spinner, so
/// it has a header to sit on before its first row arrives.
fn is_multi(rows: &[Agent], fetching: &HashMap<String, u64>, now: u64) -> bool {
    rows.iter().any(|a| !a.is_local())
        || fetching.values().any(|t| now.saturating_sub(*t) >= SPIN_GRACE_MS)
}

/// When a server's outstanding fetch started, once it has been
/// outstanding long enough to be worth showing (see [`SPIN_GRACE_MS`]).
/// `fetching` holds every call in flight; this is the subset the picker
/// admits to, so a fetch that finishes quickly is never drawn at all.
fn spin_since(fetching: &HashMap<String, u64>, server: &str, now: u64) -> Option<u64> {
    fetching
        .get(server)
        .copied()
        .filter(|t| now.saturating_sub(*t) >= SPIN_GRACE_MS)
}

pub async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    cfg: Rc<Config>,
    remotes: Rc<RefCell<Remotes>>,
    client: Option<u64>,
    here: Option<u32>,
    seek: bool,
    seek_id: Option<String>,
) {
    // The window to float over: the pressing client's current one; else
    // the window of the pane the command targeted (a script run from a
    // copy-mode binding has no attached client, only the pane); else the
    // first attached client's, so a request that arrives over the link
    // lands where the user looks; the first window only as a last resort.
    let client_window = |cid: u64| {
        list_clients()
            .ok()?
            .into_iter()
            .find(|c| u64::from(c.id) == cid)?
            .session
            .and_then(|s| resolve_session(SessionId(s)).ok())
            .and_then(|v| v.current_window)
    };
    let window = client
        .and_then(client_window)
        .or_else(|| here.and_then(|p| resolve_pane(PaneId(p)).ok().map(|pi| pi.window)))
        .or_else(|| any_client().and_then(client_window))
        .or_else(|| list_windows().ok().and_then(|w| w.first().map(|x| x.id)));
    // Opening on an id, the pane we were pressed in is nobody's business
    // beyond that: the cursor and the here border go to the id's row, not
    // to the agent that happens to live where the key was pressed.
    let here = if seek_id.is_some() { None } else { here };
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
    // Open the window BEFORE touching the remotes. Waiting for every
    // server to answer first made the picker appear only as fast as the
    // slowest hop; the rosters already in hand are at most REFRESH_MS
    // old, and every row carries its own age, so nothing shown lies.
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
    // With `seek`, the pane we were opened from may hold an agent that has
    // finished or been archived (still running or not). Its row lives in
    // the history (or the archive), so open that view for it and let the
    // cursor land there. Without it the default view opens, and the cursor
    // lands on the pane's row only when that view has it.
    let mut req = ListReq::default();
    let mut here_id = None;
    let mut archived_only = false;
    if let (true, Some(pane)) = (seek, here) {
        if let Ok(Some(a)) = store::latest_by_pane(i64::from(pane)).await {
            // An archived row is out of the live list whether or not its
            // pane still runs; a finished one is in the history.
            if a.life == "archived" {
                req.history = true;
                req.archived = true;
                archived_only = true;
                here_id = Some(a.id.clone());
            } else if !a.live() {
                req.history = true;
                here_id = Some(a.id.clone());
            }
        }
    }
    if let Some(id) = &seek_id {
        here_id = Some(id.clone());
    }
    let Gathered { mut rows, mut captures, mut skew, mut down, mut mismatch, mut fetching } =
        gather_rows(&remotes, req.clone(), true).await;
    // An id to open on that no roster holds: it may have finished or been
    // archived, here or on a linked server, so ask every server for its
    // history once (which holds the archived rows too; asking for the
    // archive instead would fetch only those).
    let mut missing_id = None;
    if let Some(id) = &seek_id {
        if !rows.iter().any(|a| a.id == *id) {
            req.history = true;
            let g = gather_rows(&remotes, req.clone(), true).await;
            rows = g.rows;
            captures = g.captures;
            skew = g.skew;
            down = g.down;
            mismatch = g.mismatch;
            fetching = g.fetching;
            match rows.iter().find(|a| a.id == *id) {
                Some(a) if a.life == "archived" => archived_only = true,
                Some(_) => {}
                None => missing_id = Some(id.clone()),
            }
        }
    }
    let mut order: HashMap<String, u64> = HashMap::new();
    let mut order_next: u64 = 0;
    stable_sort(&mut order, &mut order_next, &mut rows);
    let multi = is_multi(&rows, &fetching, now_ms());
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
        captures,
        content_mode: SearchMode::Plain,
        content_query: String::new(),
        now_ms: now_ms(),
        show_history: req.history,
        archived_only,
        history_before_archive: false,
        keys: cfg.keys.clone(),
        launchers: cfg.launchers.clone(),
        status: None,
        pending_g: false,
        pending_kill: None,
        order,
        order_next,
        timer: None,
        current_pane: here,
        here_id,
        home: home_dir().ok().filter(|h| !h.is_empty()),
        completions: Vec::new(),
        completion_idx: 0,
        completion_hidden: false,
        seek_here: here.is_some() || seek_id.is_some(),
        skew,
        down,
        mismatch,
        fetching,
        composing: false,
        msg_buf: String::new(),
        unread: HashMap::new(),
        mirrors: if multi { find_mirrors() } else { HashMap::new() },
        remote_capture: None,
        multi,
        preview_focus: false,
        transcript_query: String::new(),
        transcript_hits: HashMap::new(),
        hit_rows: Vec::new(),
        roster_len: 0,
        transcript: None,
        show_transcript: false,
        transcript_focus: false,
        show_info: false,
        show_help: false,
        info: None,
    };
    if let Some(id) = missing_id {
        p.status = Some(format!("no agent {id} on any server"));
    }
    p.roster_len = p.rows.len();
    pick_refilter(&mut p);
    // Open on the agent you are sitting in, when it has a row.
    p.select_here();
    pick_render(&mut p);
    *picker.borrow_mut() = Some(p);
    // Keep times and file-sourced status fresh while the picker is open,
    // without a costly file scan on every event. Track the task so close
    // (or a reopen) can cancel it deterministically, instead of leaving it
    // to notice the picker is gone on its next tick.
    let tid = spawn(refresh_timer(Rc::clone(&picker), Rc::clone(&remotes), mode));
    if let Some(p) = picker.borrow_mut().as_mut() {
        p.timer = Some(tid);
    }
    // Now go ask the remotes. Detached: each snapshot repaints through
    // refresh_if_open as it lands, and the servers still outstanding
    // spin in their headers meanwhile. Reopening in a hurry does not
    // stack up rounds of ssh - see fetch_remotes_on_open.
    spawn(fetch_remotes_on_open(Rc::clone(&picker), Rc::clone(&remotes), ListReq::default()));
    fetch_unread(Rc::clone(&picker));
    request_preview(&picker);
}

/// Rebuild the picker's rows, preserving the highlight. `enrich` reads
/// the harness session files (the costly part); event-driven refreshes
/// pass false and only re-read the DB (shim-pushed status, membership)
/// then re-render. The enrich runs on picker open and on a slow timer.
pub async fn reload_picker(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    enrich: bool,
) {
    let req = list_req_of(&picker);
    let Gathered { mut rows, captures, skew, down, mismatch, fetching } =
        gather_rows(&remotes, req, enrich).await;
    let multi = is_multi(&rows, &fetching, now_ms());
    let mirrors = if multi { find_mirrors() } else { HashMap::new() };
    let mut b = picker.borrow_mut();
    if let Some(p) = b.as_mut() {
        // Capture the selected agent (key AND pane) against the OLD rows
        // before we swap them in, so the highlight follows the agent. The
        // pane is the fallback: an id migration (prov -> durable) changes
        // the id but never the pane, so the cursor stays put across it.
        let keep = p.selected().map(|a| (a.key(), a.server.clone(), a.pane));
        // Stable order (server + band + frozen rank), so a refresh never
        // reshuffles rows under the cursor.
        stable_sort(&mut p.order, &mut p.order_next, &mut rows);
        p.roster_len = rows.len();
        p.rows = rows;
        p.captures = captures;
        p.now_ms = now_ms();
        p.skew = skew;
        p.down = down;
        p.mismatch = mismatch;
        p.fetching = fetching;
        p.mirrors = mirrors;
        p.multi = multi;
        // A refresh keeps the scroll where it is (only filter typing snaps
        // back to the top).
        pick_refilter_keep(p, keep, false);
        // The here row may only now have arrived (a remote roster landing
        // after open); land on it unless the cursor has been moved since.
        p.select_here();
        pick_render(p);
    }
    drop(b);
    fetch_unread(Rc::clone(&picker));
    request_preview(&picker);
}

pub async fn refresh_if_open(
    picker: &Rc<RefCell<Option<Picker>>>,
    remotes: &Rc<RefCell<Remotes>>,
) {
    if picker.borrow().is_some() {
        // Events only re-read the DB; the file scan is left to the timer.
        reload_picker(Rc::clone(picker), Rc::clone(remotes), false).await;
    }
}

/// While the picker stays open, re-read the harness session files and the
/// remote rosters on a slow cadence so times and file-sourced status stay
/// fresh without re-scanning on every event. Ends when the picker closes
/// or is replaced.
async fn refresh_timer(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    mode: ModeId,
) {
    loop {
        if sleep_ms(REFRESH_MS).await.is_err() {
            return;
        }
        let (live, req) = {
            let b = picker.borrow();
            match b.as_ref() {
                Some(p) if p.mode.0 == mode.0 => (true, p.list_req()),
                _ => (false, ListReq::default()),
            }
        };
        if !live {
            return;
        }
        fetch_remotes(Rc::clone(&picker), Rc::clone(&remotes), req).await;
        reload_picker(Rc::clone(&picker), Rc::clone(&remotes), true).await;
    }
}

/// A remote row without a local mirror shows the provider's captured text
/// Whatever the highlighted row needs in the preview that is not a live
/// blit: its conversation, or a remote provider's captured text.
fn request_preview(picker: &Rc<RefCell<Option<Picker>>>) {
    request_transcript(picker);
    request_capture(picker);
    request_info(picker);
}

/// The info card is up: fetch what it shows beyond the row - the
/// conversation's totals (from the store, or a remote provider's
/// `stats`), a live local pane's directory, and whether that directory
/// has uncommitted changes. Once per row; a live row again on the
/// refresh cadence.
fn request_info(picker: &Rc<RefCell<Option<Picker>>>) {
    let want = {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        if !p.show_info {
            return;
        }
        let Some(a) = p.selected() else { return };
        let key = a.key();
        let fresh_for = if a.live() { REFRESH_MS } else { u64::MAX };
        if p.info.as_ref().is_some_and(|c| {
            c.key == key && now_ms().saturating_sub(c.fetched_ms) < fresh_for
        }) {
            return;
        }
        let local_pane = if a.is_local() { p.local_pane_of(a) } else { None };
        (key, a.server.clone(), a.id.clone(), a.is_local(), local_pane, a.cwd.clone(), p.mode)
    };
    let picker = Rc::clone(picker);
    spawn(async move {
        let (key, server, id, local, local_pane, cwd, mode) = want;
        let stats = if local {
            store::stats(&id).await.ok()
        } else {
            service::call_json::<_, Stats>(&format!("@{server}"), "stats", &StatsReq { id: id.clone() })
                .await
                .ok()
        };
        let live_cwd = local_pane.and_then(|pane| {
            format_expand(OptionTarget::Pane(PaneId(pane)), "#{pane_current_path}")
                .ok()
                .filter(|s| !s.is_empty())
        });
        // Dirty? One git status, only for a local row whose directory we
        // know, and only when the path is plain enough to quote.
        let dir = live_cwd.clone().or(cwd);
        let dirty = match dir.filter(|_| local) {
            Some(d) if !d.contains('\'') => run_job(
                format!("git -C '{d}' status --porcelain --untracked-files=no 2>/dev/null | head -c 1"),
                None,
            )
            .await
            .ok()
            .map(|j| !j.output.is_empty()),
            _ => None,
        };
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if p.mode.0 != mode.0 {
            return;
        }
        p.info = Some(InfoCard { key, fetched_ms: now_ms(), stats, live_cwd, dirty });
        pick_render(p);
    });
}

/// The highlighted row shows its conversation in the preview (no live
/// pane to blit, or `show_transcript`): fetch the turns, once per row
/// and opening turn. Local rows read the store; a remote row asks its
/// provider's `turns`. A local row with no turns falls back to its saved
/// capture, so a finished agent from before the transcript existed still
/// shows something.
fn request_transcript(picker: &Rc<RefCell<Option<Picker>>>) {
    let want = {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        let Some(a) = p.selected() else { return };
        if p.local_pane_of(a).is_some() && !p.show_transcript {
            return;
        }
        let key = a.key();
        let open_seq = p.transcript_hits.get(&key).map(|h| h.seq).unwrap_or(-1);
        let fresh_for = if a.live() { REFRESH_MS } else { u64::MAX };
        if p.transcript.as_ref().is_some_and(|tv| {
            tv.key == key
                && tv.open_seq == open_seq
                && now_ms().saturating_sub(tv.fetched_ms) < fresh_for
        }) {
            return;
        }
        (key, a.server.clone(), a.id.clone(), a.is_local(), open_seq, p.mode)
    };
    let picker = Rc::clone(picker);
    spawn(async move {
        let (key, server, id, local, open_seq, mode) = want;
        let turns: Vec<TurnRow> = if local {
            store::turns_range(&id, 0, i64::MAX).await.unwrap_or_default()
        } else {
            let req = TurnsReq { id: id.clone(), from: 0, to: i64::MAX };
            service::call_json::<_, Vec<TurnRow>>(&format!("@{server}"), "turns", &req)
                .await
                .unwrap_or_default()
        };
        let capture: Vec<String> = if turns.is_empty() && local {
            store::get_capture(&id)
                .await
                .ok()
                .flatten()
                .map(|t| t.lines().map(str::to_string).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if p.mode.0 != mode.0 || p.selected().map(|a| a.key()) != Some(key.clone()) {
            return;
        }
        // A refetch of the same conversation (a live row, on the refresh
        // cadence) keeps where the user is: the scroll position and the
        // match they stepped to. Rendering would reset both.
        let keep = p
            .transcript
            .as_ref()
            .filter(|tv| tv.key == key && tv.open_seq == open_seq && !tv.lines.is_empty())
            .map(|tv| (tv.top, tv.match_idx));
        p.transcript = Some(TranscriptView {
            key,
            fetched_ms: now_ms(),
            turns,
            open_seq,
            top: 0,
            width: 0,
            lines: Vec::new(),
            matches: Vec::new(),
            match_idx: None,
            capture,
        });
        if let Some((top, match_idx)) = keep {
            ensure_transcript_rendered(p);
            if let Some(tv) = p.transcript.as_mut() {
                tv.top = top;
                tv.match_idx = match_idx.filter(|&i| i < tv.matches.len());
            }
        }
        pick_render(p);
    });
}

/// in the preview area: ask for it (once per highlighted row).
fn request_capture(picker: &Rc<RefCell<Option<Picker>>>) {
    let want = {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        let Some(a) = p.selected() else { return };
        if a.is_local() || p.local_pane_of(a).is_some() {
            return;
        }
        let key = a.key();
        if p.remote_capture.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        (key, a.server.clone(), a.id.clone(), p.mode)
    };
    let picker = Rc::clone(picker);
    spawn(async move {
        let (key, server, id, mode) = want;
        let target = format!("@{server}");
        let req = provider::CaptureReq { id };
        let text = match service::call(&target, "capture", &serde_json::to_vec(&req).unwrap_or_default()).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) => format!("(no capture: {})", e.message),
        };
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if p.mode.0 != mode.0 {
            return;
        }
        let lines = text.lines().map(str::to_string).collect();
        p.remote_capture = Some((key, lines));
        pick_render(p);
    });
}

pub async fn apply_life(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    ids: Vec<(String, String)>,
    life: String,
) {
    for (server, id) in &ids {
        if server == LOCAL {
            if life == "archived" {
                provider::on_archive(id).await;
            }
            let _ = store::set_life(id, &life).await;
        } else {
            let verb = if life == "active" { "unarchive" } else { "archive" };
            let _ = act_remote(server, id, verb, None).await;
        }
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
    // The same request the view is showing: an archive from the history
    // view must not empty the remote half of it until the next tick.
    let req = list_req_of(&picker);
    fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
    reload_picker(picker, remotes, false).await;
}

/// Mark rows read or unread by hand, wherever they live. A remote row's
/// ack belongs to its own provider, over the `act` RPC (`ack`/`unack`).
pub async fn apply_read(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    ids: Vec<(String, String)>,
    read: bool,
) {
    let now = now_ms() as i64;
    let mut done = 0usize;
    let mut refused: Option<String> = None;
    for (server, id) in &ids {
        let r = if server == LOCAL {
            let r = if read {
                store::acknowledge(id, now).await
            } else {
                store::unacknowledge(id, now).await
            };
            r.map(|_| ()).map_err(|e| e.message)
        } else {
            act_remote(server, id, if read { "ack" } else { "unack" }, None).await
        };
        match r {
            Ok(()) => done += 1,
            Err(why) => {
                if refused.is_none() {
                    refused = Some(why);
                }
            }
        }
    }
    {
        let mut b = picker.borrow_mut();
        if let Some(p) = b.as_mut() {
            let verb = if read { "marked read" } else { "marked unread" };
            p.status = Some(match (done, refused) {
                (1, None) => verb.to_string(),
                (n, None) => format!("{n} {verb}"),
                (0, Some(why)) => why,
                (n, Some(why)) => format!("{n} {verb}; {why}"),
            });
            p.marked.clear();
        }
    }
    let req = list_req_of(&picker);
    fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
    reload_picker(picker, remotes, false).await;
}

/// Move rows between the attention band and `waiting` by hand, wherever
/// they live. A remote row's status belongs to its own provider, so the
/// change goes over the `act` RPC rather than into this server's store.
pub async fn apply_status(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    ids: Vec<(String, String)>,
    status: String,
) {
    let now = now_ms() as i64;
    let mut moved = 0usize;
    // The first refusal, verbatim. A row that does not move must say
    // why on the status line, because "0 moved" against a remote row
    // looks exactly like a wedged picker.
    let mut refused: Option<String> = None;
    for (server, id) in &ids {
        let r = if server == LOCAL {
            store::set_status_by_id(id, &status, now)
                .await
                .map(|_| ())
                .map_err(|e| e.message)
        } else {
            act_remote(server, id, "status", Some(&status)).await
        };
        match r {
            Ok(()) => moved += 1,
            Err(why) => {
                if refused.is_none() {
                    refused = Some(why);
                }
            }
        }
    }
    {
        let mut b = picker.borrow_mut();
        if let Some(p) = b.as_mut() {
            let verb = if status == "waiting" { "moved to waiting" } else { "flagged" };
            p.status = Some(match (moved, refused) {
                (1, None) => verb.to_string(),
                (n, None) => format!("{n} {verb}"),
                (0, Some(why)) => why,
                (n, Some(why)) => format!("{n} {verb}; {why}"),
            });
            p.marked.clear();
        }
    }
    let req = list_req_of(&picker);
    fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
    reload_picker(picker, remotes, false).await;
}

/// Text that is safe to drop into a tmux command string as a
/// single-quoted argument. tmux's single quotes take no escapes, so a
/// quote inside one cannot be escaped - it can only be removed. `#` goes
/// too: a menu name is format-expanded, and `#{...}` from an agent's own
/// title is not something to hand to the format parser.
fn menu_safe(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !matches!(c, '\'' | '"' | '#' | '\\' | ';' | '$'))
        .collect();
    clip(cleaned.trim(), max)
}

/// One row of the action menu: a label, the picker key it stands for,
/// and whether it applies to the selected agent right now. A name
/// starting with `-` is what tmux draws dimmed and refuses to select, so
/// an action that does not apply is still SHOWN - the menu is the place
/// you go to find out what you can do, and a silently missing line
/// answers nothing.
fn menu_item(label: &str, key: &str, enabled: bool) -> String {
    let name = if enabled {
        label.to_string()
    } else {
        format!("-{label}")
    };
    format!(" '{}' '{}' \"plugin-command agents 'menu-key {}'\"", name, key, key)
}

/// The action menu for the selected row: every picker action, with the
/// ones that do not apply dimmed, opened on the client that asked for it.
///
/// The items do not DO anything themselves - each one sends its key back
/// into the picker through `menu-key`. That is the whole design: the menu
/// is a view of the keymap, so an action can never behave one way from a
/// key and another from the menu, and a new key costs one line here.
async fn open_menu(picker: Rc<RefCell<Option<Picker>>>, client: Option<u64>) {
    let Some((title, items)) = ({
        let b = picker.borrow();
        b.as_ref().and_then(|p| {
            let a = p.selected()?;
            let k = &p.keys;
            let live = live_pane_of_selection(p).is_some();
            let flagged = a.status == "needs_input";
            let movable =
                a.live() && matches!(a.status.as_str(), "needs_input" | "waiting");
            let mut items = String::new();
            items.push_str(&menu_item("jump to pane", &k.jump, live));
            items.push_str(&menu_item("type into pane", &k.focus, live));
            items.push_str(&menu_item("new agent here", &k.new, true));
            items.push_str(&menu_item("fork this agent", "f", true));
            items.push_str(&menu_item("message", "m", true));
            let stopped =
                a.live() && matches!(a.status.as_str(), "needs_input" | "waiting");
            items.push_str(&menu_item("mark read", &k.read, stopped && a.unread()));
            items.push_str(&menu_item("mark unread", &k.unread, stopped && !a.unread()));
            items.push_str(&menu_item("copy id", &k.copy, durable_id(a).is_some()));
            items.push_str(&menu_item("info", "i", true));
            items.push_str(&menu_item("help", "?", true));
            items.push_str(&menu_item("rename", &k.rename, true));
            items.push_str(" ''");
            let band = if flagged { "move to waiting" } else { "flag: needs input" };
            items.push_str(&menu_item(band, &k.attention, movable));
            let arch = if a.life == "archived" { "un-archive" } else { "archive" };
            items.push_str(&menu_item(arch, &k.archive, true));
            items.push_str(" ''");
            items.push_str(&menu_item("interrupt (C-c)", &k.interrupt, live));
            items.push_str(&menu_item("kill pane", &k.kill, live));
            // The one view toggle in a menu of row actions: a key that
            // is not in the footer has to be findable somewhere.
            items.push_str(" ''");
            let hist = if p.show_history {
                "hide finished (history)"
            } else {
                "show finished (history)"
            };
            items.push_str(&menu_item(hist, &k.history, true));
            Some((menu_safe(&display_name(a), 30), items))
        })
    }) else {
        return;
    };
    let target = client
        .and_then(|cid| {
            list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
        })
        .map(|c| c.name)
        .filter(|n| {
            n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        })
        .map(|n| format!(" -c '{n}'"))
        .unwrap_or_default();
    // No -x/-y: display-menu then centres on the client's window, which
    // is where the picker floats. Anchoring it to the float itself would
    // need the mode's pane id, which the host does not hand out.
    let cmd = format!("display-menu{target} -T ' {title} '{items}");
    if let Err(e) = run_command(&cmd).await {
        if let Some(p) = picker.borrow_mut().as_mut() {
            p.status = Some(format!("menu failed: {}", e.message));
            pick_render(p);
        }
    }
}

/// Put text on the clipboard of the client that pressed the key.
///
/// `set-buffer -w` does both halves: a tmux paste buffer (for
/// `paste-buffer` in another pane) and the terminal's own clipboard over
/// OSC 52, which is the one that reaches the browser or editor. The `-t`
/// names the client, because "the current client" from a plugin command
/// is whichever one tmux guesses is best - fine when one client is
/// attached, wrong the moment two are.
async fn copy_to_clipboard(
    picker: Rc<RefCell<Option<Picker>>>,
    text: String,
    client: Option<u64>,
) {
    let name = client
        .and_then(|cid| {
            list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
        })
        .map(|c| c.name)
        .filter(|n| {
            n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        });
    let target = name.map(|n| format!(" -t '{n}'")).unwrap_or_default();
    let cmd = format!("set-buffer -w{target} -- '{text}'");
    let msg = match run_command(&cmd).await {
        Ok(_) => format!("copied {text}"),
        Err(e) => format!("copy failed: {}", e.message),
    };
    if let Some(p) = picker.borrow_mut().as_mut() {
        p.status = Some(msg);
        pick_render(p);
    }
}

/// Acknowledge (mark read) a row, wherever it lives.
async fn acknowledge(server: String, id: String) {
    if server == LOCAL {
        let _ = store::acknowledge(&id, now_ms() as i64).await;
    } else {
        let _ = act_remote(&server, &id, "ack", None).await;
    }
}

/// Jump the pressing client to a pane, wherever it lives.
pub async fn jump(pane: u32) {
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
// keys
// ---------------------------------------------------------------------------

pub fn on_mode_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    busy: &Rc<Cell<bool>>,
    remotes: &Rc<RefCell<Remotes>>,
    ctx: &Ctx,
    event: &Event,
) {
    let mode_id = event.get_i64("mode");
    let key = event.get_str("key").unwrap_or("").to_string();
    // The client whose key this was. A clipboard belongs to a terminal,
    // not to the server, so a copy has to name it - and so does a menu.
    let client = event.get_i64("client").map(|v| v as u64);
    // A mouse key lands on a cell of the mode screen (0-based).
    let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
        (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
        _ => None,
    };
    dispatch_key(picker, busy, remotes, ctx, mode_id, key, mouse, client);
}

/// A directional `select-pane` on the float: a prefix binding, most
/// likely (`prefix h` is `select-pane -L` in the common config). The
/// picker has two sides, so left is the list and right is the preview;
/// up and down move the highlight whichever side has the keyboard - a
/// prefix key can never be text, so this is the one way to switch the
/// agent you are typing into, or to leave the preview, without spending
/// a key of the agent's own.
pub fn on_mode_nav(
    picker: &Rc<RefCell<Option<Picker>>>,
    busy: &Rc<Cell<bool>>,
    ctx: &Ctx,
    event: &Event,
) {
    let dir = event.get_str("dir").unwrap_or("").to_string();
    let mut ack: Option<(String, String)> = None;
    {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if event.get_i64("mode") != Some(p.mode.0 as i64) {
            return;
        }
        if busy.get() {
            return;
        }
        p.status = None;
        match dir.as_str() {
            "left" => {
                p.preview_focus = false;
                p.transcript_focus = false;
                pick_render(p);
            }
            "right" => {
                if transcript_shown(p) {
                    p.transcript_focus = true;
                    ensure_transcript_rendered(p);
                    pick_render(p);
                } else {
                    ack = focus_preview(p);
                }
            }
            "up" | "down" => {
                let typing = p.preview_focus;
                move_sel(p, if dir == "up" { -1 } else { 1 });
                // The keyboard stays with the preview, which now shows
                // another agent - unless that row has no pane to type
                // into, in which case the focus drops and the footer says
                // so (rendered again, since move_sel drew the old state).
                p.sync_focus();
                if typing {
                    pick_render(p);
                }
            }
            _ => return,
        }
    }
    if let Some((server, id)) = ack {
        ctx.spawn(acknowledge(server, id));
    }
    request_preview(picker);
}

/// A key the action menu sent back in. The menu's items are the picker's
/// own keys, re-entered here, so the two can never disagree about what a
/// key does - the menu is a way to SEE the keys, not a second
/// implementation of them.
pub fn on_menu_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    busy: &Rc<Cell<bool>>,
    remotes: &Rc<RefCell<Remotes>>,
    ctx: &Ctx,
    key: String,
    mouse: Option<(u32, u32)>,
    client: Option<u64>,
) {
    let mode_id = picker.borrow().as_ref().map(|p| p.mode.0 as i64);
    // No picker: the menu outlived it (its window went away, or someone
    // ran the command by hand). Nothing to act on.
    if mode_id.is_none() {
        return;
    }
    dispatch_key(picker, busy, remotes, ctx, mode_id, key, mouse, client);
}

/// Text pasted into the picker (a bracketed paste, or `paste-buffer` on
/// the float). While the preview has the keyboard it goes to the agent's
/// pane, as typed keys do; otherwise into the search box, which takes
/// the focus, so a pasted agent id or filter token lands where it works.
pub fn on_mode_paste(picker: &Rc<RefCell<Option<Picker>>>, event: &Event) {
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    if event.get_i64("mode") != Some(p.mode.0 as i64) {
        return;
    }
    let Some(text) = event.get_str("text") else { return };
    let text = text.to_string();
    if p.preview_focus {
        if let Some(pane) = live_pane_of_selection(p) {
            let _ = send_text(PaneId(pane), &text);
        }
        return;
    }
    // Verbatim, with line breaks as spaces: a paste that tmux detected
    // from typing speed arrives one character per event, so nothing may
    // be added around a chunk - a separator per chunk put a space after
    // every character of a pasted id.
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect();
    if flat.is_empty() {
        return;
    }
    p.filtering = true;
    p.composing = false;
    p.renaming = false;
    p.filter.push_str(&flat);
    filter_edited(p);
}

/// The agent ids on a pane's screen (the last screens of its history
/// too), nearest the bottom first, each once: `claude:<uuid>`,
/// `codex:...`, `pi:...`, `opencode:...`.
pub fn ids_on_screen(text: &str) -> Vec<String> {
    const KINDS: [&str; 4] = ["claude:", "codex:", "pi:", "opencode:"];
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some((at, kind)) = KINDS
            .iter()
            .filter_map(|k| rest.find(k).map(|i| (i, *k)))
            .min_by_key(|(i, _)| *i)
        {
            // A word boundary before the kind, so `oldclaude:` is not one.
            let before = rest[..at].chars().last();
            let after = &rest[at + kind.len()..];
            let n = after
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
                .count();
            if before.is_none_or(|c| !c.is_alphanumeric()) && n >= 8 {
                found.push(format!("{kind}{}", &after[..n]));
            }
            rest = &after[n.min(after.len())..];
        }
    }
    // Nearest the bottom first, and once each.
    let mut out: Vec<String> = Vec::new();
    for id in found.into_iter().rev() {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// `pick ids`: the agent ids on the screen of the pane the key was
/// pressed in. One opens the picker on it; several are offered as a
/// menu whose items do that; none says so. A linked server's mirror is
/// a local pane with the remote grid in it, so this works there too,
/// where copy mode would run on the other server.
pub async fn pick_ids(
    picker: Rc<RefCell<Option<Picker>>>,
    cfg: Rc<Config>,
    remotes: Rc<RefCell<Remotes>>,
    client: Option<u64>,
    here: Option<u32>,
) {
    let Some(pane) = here else {
        let _ = display_message("agents: no pane to look at");
        return;
    };
    let text = capture_pane(PaneId(pane), Some(-200), None).unwrap_or_default();
    let ids = ids_on_screen(&text);
    match ids.len() {
        0 => {
            let _ = display_message("agents: no agent id on this screen");
        }
        1 => pick_open(picker, cfg, remotes, client, here, false, ids.into_iter().next()).await,
        _ => {
            let target = client
                .and_then(|cid| {
                    list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
                })
                .map(|c| c.name)
                .filter(|n| {
                    n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
                })
                .map(|n| format!(" -c '{n}'"))
                .unwrap_or_default();
            let mut items = String::new();
            for (i, id) in ids.iter().take(9).enumerate() {
                // Ids are the kind, a colon, hex and dashes: safe as typed.
                items.push_str(&format!(
                    " '{id}' '{}' \"plugin-command agents 'pick id {id}'\"",
                    i + 1
                ));
            }
            let cmd = format!("display-menu{target} -T ' agents on this screen '{items}");
            if let Err(e) = run_command(&cmd).await {
                let _ = display_message(&format!("agents: menu failed: {}", e.message));
            }
        }
    }
}

/// A mouse key, by name: a click, a release, a drag, a wheel notch, with
/// or without a modifier prefix. Never text, and never forwarded to a
/// pane - `send_key` would take the name, but a mouse key without its
/// event is a key the pane cannot make sense of.
fn is_mouse_key(key: &str) -> bool {
    key.contains("Mouse") || key.contains("Wheel") || key.contains("Click")
}

#[allow(clippy::too_many_arguments)]
fn dispatch_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    busy: &Rc<Cell<bool>>,
    remotes: &Rc<RefCell<Remotes>>,
    ctx: &Ctx,
    mode_id: Option<i64>,
    key: String,
    mouse: Option<(u32, u32)>,
    client: Option<u64>,
) {
    let mut after = PickAfter::None;
    // A row to acknowledge (mark read) after the borrow drops: the cursor
    // landed on an unread waiting row, or the user jumped to it.
    let mut ack: Option<(String, String)> = None;
    {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if mode_id != Some(p.mode.0 as i64) {
            return;
        }
        if busy.get() {
            return;
        }
        p.status = None;
        let k = &p.keys.clone();
        // A pending `g` is consumed by this key; only a second `g`
        // keeps it (see the `g` branch).
        let g_pending = p.pending_g;
        p.pending_g = false;
        // Any key that is not the kill key again cancels a pending kill.
        let kill_pending = p.pending_kill.take();
        // Arrows and their control aliases move the selection in both
        // modes; they are never text.
        let is_down = matches!(key.as_str(), "Down" | "C-n" | "C-j");
        let is_up = matches!(key.as_str(), "Up" | "C-p" | "C-k");
        if is_mouse_key(&key) {
            // The mouse means the same thing whatever has the keyboard:
            // a click lands where it lands. Without a cell (a mouse key
            // typed by name) there is nowhere for it to land.
            if let Some((x, y)) = mouse {
                mouse_key(p, &key, x, y, &mut after, &mut ack);
            }
        } else if p.preview_focus {
            // The preview has the keyboard: every key goes to the pane
            // it shows, except the one that takes the keyboard back.
            if key == k.unfocus {
                p.preview_focus = false;
                pick_render(p);
            } else {
                match live_pane_of_selection(p) {
                    Some(pane) => after = PickAfter::Type(pane, key.clone()),
                    None => {
                        // The pane went away under the prompt: say so,
                        // rather than typing into nothing.
                        p.preview_focus = false;
                        p.status = Some(unreachable_reason(p));
                        pick_render(p);
                    }
                }
            }
        } else if p.transcript_focus {
            // The conversation has the keyboard: scroll it, step through
            // its matches, or hand the keyboard back.
            transcript_key(p, &key, is_up, is_down);
        } else if p.composing {
            // Compose mode: keys are text, except accept / cancel.
            if key == k.close {
                p.composing = false;
                p.msg_buf.clear();
                pick_render(p);
            } else if key == "Enter" {
                if let Some(a) = p.selected() {
                    let text = p.msg_buf.trim().to_string();
                    if !text.is_empty() {
                        after = PickAfter::Message(a.server.clone(), a.id.clone(), text);
                    }
                }
                p.composing = false;
                p.msg_buf.clear();
            } else if key == "BSpace" {
                p.msg_buf.pop();
                pick_render(p);
            } else if key == "C-u" {
                p.msg_buf.clear();
                pick_render(p);
            } else if key == "Space" {
                p.msg_buf.push(' ');
                pick_render(p);
            } else if key.chars().count() == 1
                && !key.chars().next().unwrap().is_control()
            {
                p.msg_buf.push_str(&key);
                pick_render(p);
            }
        } else if p.renaming {
            // Rename mode: keys are text, except accept / cancel.
            if key == k.close {
                p.renaming = false;
                p.rename_buf.clear();
                pick_render(p);
            } else if key == "Enter" {
                if let Some(a) = p.selected() {
                    after = PickAfter::Rename(
                        a.server.clone(),
                        a.id.clone(),
                        p.rename_buf.trim().to_string(),
                    );
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
            // With the dropdown open, Tab/Down and BTab/Up walk it, Enter
            // takes the value, Esc puts it away for this token.
            let dropdown = !p.completions.is_empty();
            if dropdown && (key == "Tab" || is_down) {
                p.completion_idx = (p.completion_idx + 1) % p.completions.len();
                pick_render(p);
            } else if dropdown && (key == "BTab" || is_up) {
                p.completion_idx =
                    (p.completion_idx + p.completions.len() - 1) % p.completions.len();
                pick_render(p);
            } else if dropdown && key == "Enter" {
                accept_completion(p);
                pick_render(p);
            } else if dropdown && key == k.close {
                p.completion_hidden = true;
                p.completions.clear();
                pick_render(p);
            } else if key == k.close || key == "Enter" {
                p.filtering = false;
                p.completions.clear();
                pick_render(p);
            } else if is_down {
                move_sel(p, 1);
            } else if is_up {
                move_sel(p, -1);
            } else if key == "BSpace" {
                p.filter.pop();
                filter_edited(p);
            } else if key == "C-u" {
                p.filter.clear();
                filter_edited(p);
            } else if key == "Space" {
                p.filter.push(' ');
                filter_edited(p);
            } else if key == k.content {
                toggle_content(p);
            } else if key == "\\\\" {
                // A backslash: tmux names the key with two.
                p.filter.push('\\');
                filter_edited(p);
            } else if key.chars().count() == 1
                && !key.chars().next().unwrap().is_control()
            {
                p.filter.push_str(&key);
                filter_edited(p);
            }
        } else if key == k.close || key == "q" {
            // Esc or q puts the info card away first, then cancels a
            // pending selection; with neither, it closes the picker.
            if p.show_help {
                p.show_help = false;
                pick_render(p);
            } else if p.show_info {
                p.show_info = false;
                pick_render(p);
            } else if p.marked.is_empty() {
                after = PickAfter::Close(p.mode);
            } else {
                p.marked.clear();
                p.status = Some("selection cleared".into());
                pick_render(p);
            }
        } else if key == k.filter {
            // `/` focuses the search box, and is the only way in.
            p.filtering = true;
            p.completion_hidden = false;
            update_completions(p);
            pick_render(p);
        } else if key == "s" || key == "S" || key == "d" {
            // Narrow to the highlighted row's session (s), server (S) or
            // folder (d), as a token in the search box; the same key
            // again takes the token out.
            if let Some(tok) = narrow_token(p, &key) {
                toggle_token(p, &tok);
            }
        } else if key == "g" {
            // Vim `gg`: the first `g` waits, the second goes to the top.
            if g_pending {
                let n = p.view.len() as i32;
                move_sel(p, -n);
            } else {
                p.pending_g = true;
            }
        } else if key == "G" {
            // Vim `G`: go to the bottom.
            let n = p.view.len() as i32;
            move_sel(p, n);
        } else if key == k.interrupt {
            // Interrupt, do not kill: send C-c and let the agent decide
            // what that means. Claude with work in flight answers with its
            // own "are you sure?", and the live preview shows it, so the
            // second press is an informed one rather than a guess.
            match live_pane_of_selection(p) {
                Some(pane) => {
                    after = PickAfter::Interrupt(pane);
                    p.status = Some(format!("interrupt sent to %{pane}"));
                }
                None => {
                    p.status = Some(unreachable_reason(p));
                }
            }
            pick_render(p);
        } else if key == k.kill {
            // Killing the pane takes the agent's process and its scrollback
            // with it, so it asks first. The second press must be on the
            // same pane the first one named.
            match live_pane_of_selection(p) {
                Some(pane) => {
                    if kill_pending == Some(pane) {
                        after = PickAfter::KillPane(pane);
                        p.status = Some(format!("killing %{pane}"));
                    } else {
                        p.pending_kill = Some(pane);
                        p.status = Some(format!(
                            "kill %{pane}? {} again to confirm",
                            pretty_key(&k.kill)
                        ));
                    }
                }
                None => {
                    p.status = Some(unreachable_reason(p));
                }
            }
            pick_render(p);
        } else if key == k.content {
            toggle_content(p);
        } else if key == "i" {
            // The info card in place of the preview, and back.
            p.show_info = !p.show_info;
            p.show_help = false;
            pick_render(p);
        } else if key == "?" {
            // The quick reference in place of the preview, and back.
            p.show_help = !p.show_help;
            p.show_info = false;
            pick_render(p);
        } else if key == "y" && p.show_info {
            // Copy the working directory, the way c copies the id.
            let cwd = p
                .info
                .as_ref()
                .and_then(|c| c.live_cwd.clone())
                .or_else(|| p.selected().and_then(|a| a.cwd.clone()));
            match cwd {
                Some(d) if !d.contains('\'') => after = PickAfter::Copy(d),
                Some(_) => p.status = Some("that path has a quote I will not paste".into()),
                None => p.status = Some("no working directory known".into()),
            }
            pick_render(p);
        } else if key == "Tab" {
            // The highlighted live row's conversation in place of its
            // pane, and back. A row with no pane shows it anyway.
            p.show_transcript = !p.show_transcript;
            p.status = Some(if p.show_transcript {
                "preview: conversation".into()
            } else {
                "preview: pane".into()
            });
            pick_render(p);
        } else if key == "[" || key == "]" {
            if transcript_shown(p) {
                let step = (p.height as usize).saturating_sub(2).max(2) / 2;
                if let Some(tv) = p.transcript.as_mut() {
                    tv.top = if key == "[" {
                        tv.top.saturating_sub(step)
                    } else {
                        tv.top.saturating_add(step)
                    };
                }
                pick_render(p);
            }
        } else if key == k.rename {
            if let Some(a) = p.selected() {
                p.rename_buf = a.user_name.clone().unwrap_or_default();
                p.renaming = true;
                pick_render(p);
            }
        } else if key == "m" {
            // Message the selected agent: its mailbox holds it until the
            // agent reads it, on this server or the agent's own.
            if p.selected().is_some() {
                p.msg_buf.clear();
                p.composing = true;
                pick_render(p);
            }
        } else if key == k.jump {
            after = jump_after(p, &mut ack);
        } else if key == k.focus || key == "Right" {
            // Right, into the preview: the keyboard goes with it. To the
            // conversation when that is what the preview shows (a finished
            // agent, or Tab on a live one); else to the pane.
            if transcript_shown(p) {
                p.transcript_focus = true;
                ensure_transcript_rendered(p);
                pick_render(p);
            } else {
                ack = focus_preview(p);
            }
        } else if key == "h" || key == "Left" {
            // Left of the list is nothing; the key is spent so that it
            // is never mistaken for text. (`h` used to fold in history.)
        } else if key == k.archive {
            after = archive_after(p);
        } else if key == k.menu {
            if p.selected().is_some() {
                after = PickAfter::Menu;
            }
        } else if key == k.new {
            // Start another agent: the form prefills from the row under
            // the cursor, or from the pressing client's pane when the
            // list is empty.
            after = PickAfter::NewAgent(false);
        } else if key == "f" {
            // Fork the row's agent: the form, prefilled to resume its
            // session as a copy with a name of its own.
            match p.selected() {
                Some(a) if crate::newagent::resumable_id(&a.id).is_some() => {
                    after = PickAfter::NewAgent(true);
                }
                Some(_) => {
                    p.status = Some("nothing to fork: this agent has no session id yet".into());
                    pick_render(p);
                }
                None => {}
            }
        } else if key == k.copy {
            match copy_after(p) {
                Ok(a) => after = a,
                Err(why) => {
                    p.status = Some(why);
                    pick_render(p);
                }
            }
        } else if key == k.attention {
            match attention_after(p) {
                Ok(a) => after = a,
                Err(why) => {
                    p.status = Some(why);
                    pick_render(p);
                }
            }
        } else if key == k.history {
            p.show_history = !p.show_history;
            // Leaving history leaves the archive view too: it lives there.
            if !p.show_history {
                p.archived_only = false;
            }
            after = PickAfter::Reload;
        } else if key == k.archived {
            // The archive as a list of its own. It is a history view, so
            // entering it turns history on; leaving it puts history back
            // the way it was before.
            p.archived_only = !p.archived_only;
            if p.archived_only {
                p.history_before_archive = p.show_history;
                p.show_history = true;
            } else {
                p.show_history = p.history_before_archive;
            }
            p.status = Some(if p.archived_only {
                "archive only: on".into()
            } else {
                "archive only: off".into()
            });
            after = PickAfter::Reload;
        } else if key == "J" {
            mark_and_move(p, 1);
        } else if key == "K" {
            mark_and_move(p, -1);
        } else if key == k.read || key == k.unread {
            // Read and unread by hand: the cursor passing over a row is
            // not reading it, so these are the way to say you have (or
            // have not) dealt with what it stopped for.
            let read = key == k.read;
            let ids: Vec<(String, String)> = action_targets(p)
                .iter()
                .filter(|a| a.live())
                .map(|a| (a.server.clone(), a.id.clone()))
                .collect();
            if ids.is_empty() {
                p.status = Some("no live agent to mark".into());
                pick_render(p);
            } else {
                after = PickAfter::Read(ids, read);
            }
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
        } else if is_up || key == "k" {
            move_sel(p, -1);
        }
        // The cursor landing on a row does not acknowledge it: scrolling
        // past an unread agent is not reading it. Jumping to it, typing
        // into it, or the read key are.
    }
    if let Some((server, id)) = ack {
        ctx.spawn(acknowledge(server, id));
    }
    request_preview(picker);
    match after {
        PickAfter::None => {}
        PickAfter::Type(pane, key) => {
            // Synchronous, like the interrupt: one key into the pane.
            // Then hurry the preview along - the host re-blits it every
            // 500ms, which is fine for watching and too slow for typing.
            match send_key(PaneId(pane), &key) {
                Ok(()) => poke_preview(picker, true),
                Err(e) => {
                    if let Some(p) = picker.borrow_mut().as_mut() {
                        p.preview_focus = false;
                        p.status = Some(format!("typing failed: {}", e.message));
                        pick_render(p);
                    }
                }
            }
        }
        PickAfter::Wheel(pane, key, x, y) => {
            // Into the pane as a mouse key at its cell, then hurry the
            // blit along so copy mode (or the app's scroll) shows now.
            match pane_mouse(PaneId(pane), &key, x, y, client) {
                Ok(()) => poke_preview(picker, false),
                Err(e) => {
                    if let Some(p) = picker.borrow_mut().as_mut() {
                        p.status = Some(format!("wheel: {}", e.message));
                        pick_render(p);
                    }
                }
            }
        }
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
            ctx.spawn(apply_life(Rc::clone(picker), Rc::clone(remotes), ids, life));
        }
        PickAfter::Menu => {
            let picker = Rc::clone(picker);
            ctx.spawn(async move {
                open_menu(picker, client).await;
            });
        }
        PickAfter::NewAgent(fork) => {
            ctx.spawn(crate::newagent::open(Rc::clone(picker), client, fork));
        }
        PickAfter::Read(ids, read) => {
            ctx.spawn(apply_read(Rc::clone(picker), Rc::clone(remotes), ids, read));
        }
        PickAfter::Copy(text) => {
            ctx.spawn(copy_to_clipboard(Rc::clone(picker), text, client));
        }
        PickAfter::Status(ids, status) => {
            ctx.spawn(apply_status(
                Rc::clone(picker),
                Rc::clone(remotes),
                ids,
                status,
            ));
        }
        PickAfter::Reload => {
            let picker = Rc::clone(picker);
            let remotes = Rc::clone(remotes);
            ctx.spawn(async move {
                let req = list_req_of(&picker);
                fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
                reload_picker(picker, remotes, false).await;
            });
        }
        PickAfter::Resize(mode, w, h) => {
            // Resize the float now; remember the choice for next time.
            let _ = mode_resize(mode, w, h);
            ctx.spawn(async move {
                let _ = store::set_setting("pick_w", &w.to_string()).await;
                let _ = store::set_setting("pick_h", &h.to_string()).await;
            });
        }
        PickAfter::Rename(server, id, name) => {
            let picker = Rc::clone(picker);
            let remotes = Rc::clone(remotes);
            ctx.spawn(async move {
                let n = (!name.is_empty()).then_some(name.as_str());
                if server == LOCAL {
                    let _ = store::rename_by_user(&id, n, now_ms() as i64).await;
                } else {
                    let _ = act_remote(&server, &id, "rename", n).await;
                    let req = list_req_of(&picker);
                    fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
                }
                reload_picker(picker, remotes, false).await;
            });
        }
        PickAfter::Interrupt(pane) => {
            // Synchronous: one key into the pane. Nothing is reloaded -
            // the refresh tick repaints the preview, which is where the
            // agent's answer to the interrupt shows up.
            if let Err(e) = send_key(PaneId(pane), "C-c") {
                if let Some(p) = picker.borrow_mut().as_mut() {
                    p.status = Some(format!("interrupt failed: {}", e.message));
                    pick_render(p);
                }
            }
        }
        PickAfter::KillPane(pane) => {
            let picker = Rc::clone(picker);
            let remotes = Rc::clone(remotes);
            ctx.spawn(async move {
                // On a shadow pane this runs on the remote server, which is
                // what kills the real agent. The roster follows on its own:
                // the pane dying retires the row through pane-destroyed.
                let r = run_command(&format!("kill-pane -t %{pane}")).await;
                if let Some(p) = picker.borrow_mut().as_mut() {
                    p.status = Some(match &r {
                        Ok(_) => format!("killed %{pane}"),
                        Err(e) => format!("kill failed: {}", e.message),
                    });
                    pick_render(p);
                }
                reload_picker(picker, remotes, false).await;
            });
        }
        PickAfter::Message(server, id, text) => {
            let picker = Rc::clone(picker);
            let sender = picker
                .borrow()
                .as_ref()
                .and_then(|p| p.current_pane)
                .map(|pn| format!("%{pn}"))
                .unwrap_or_else(|| "picker".into());
            ctx.spawn(async move {
                let ok = message_agent(&server, &id, &sender, &text).await;
                if let Some(p) = picker.borrow_mut().as_mut() {
                    p.status = Some(match ok {
                        Ok(()) => format!("sent to {id}"),
                        Err(e) => format!("send failed: {e}"),
                    });
                    pick_render(p);
                }
            });
        }
    }
}

/// Deliver a message to an agent's mailbox. A local agent's mailbox is on
/// this server; a remote agent's is on its own, reached over the bridge.
/// The box name is the agent's durable id, so the agent reads it with
/// `plugin-command mailbox 'inbox <id>'`.
fn server_of(remotes: &Rc<RefCell<Remotes>>, id: &str) -> Option<String> {
    remotes
        .borrow()
        .servers
        .iter()
        .find(|(_, rows)| rows.agents.iter().any(|a| a.id == id))
        .map(|(name, _)| name.clone())
}

pub async fn message_by_id(
    _picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    id: &str,
    from: &str,
    text: &str,
) {
    // Resolve the target server. An explicit `<id>@<server>` names it (a
    // fresh message to an agent this side has never rostered). A bare id
    // is looked up only in the local rosters this side already has (from
    // servers it linked to); it is never resolved by fetching a roster
    // from an inbound peer. A bare unknown id is refused.
    let (bare, server) = match id.rsplit_once('@') {
        Some((i, s)) => (i.to_string(), Some(s.to_string())),
        None => (id.to_string(), None),
    };
    let server = match server {
        Some(s) => s,
        None => {
            // A local agent id routes to the local mailbox; a remote one
            // must already be in a roster this side pulled. An id in
            // neither is refused, never resolved by asking an inbound peer.
            if store::by_id(&bare).await.ok().flatten().is_some() {
                LOCAL.to_string()
            } else if let Some(s) = server_of(&remotes, &bare) {
                s
            } else {
                let _ = display_message(&format!(
                    "agents: unknown agent {bare}; use {bare}@<server>"
                ));
                return;
            }
        }
    };
    match message_agent(&server, &bare, from, text).await {
        Ok(()) => {
            let _ = display_message(&format!("agents: sent to {bare}@{server}"));
        }
        Err(e) => {
            let _ = display_message(&format!("agents: send failed: {e}"));
        }
    }
}

async fn message_agent(server: &str, id: &str, from: &str, text: &str) -> Result<(), String> {
    let target = if server == LOCAL {
        "mailbox".to_string()
    } else {
        format!("mailbox@{server}")
    };
    let req = serde_json::json!({ "to": id, "from": from, "body": text });
    service::call_json::<_, serde_json::Value>(&target, "deliver", &req)
        .await
        .map(|_| ())
        .map_err(|e| e.message)
}

/// Ask each server's mailbox for its unread counts and fold them into the
/// picker's `unread` map, keyed by (server, agent id). Best effort: a
/// server without a mailbox, or a call that fails, just leaves no badge.
fn fetch_unread(picker: Rc<RefCell<Option<Picker>>>) {
    let (mode, servers) = {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        // The local mailbox always; remote mailboxes only on servers this
        // side linked to (an inbound peer's mailbox is not ours to poll).
        let linked: HashSet<String> = service::servers()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.linked)
            .map(|s| s.name)
            .collect();
        let mut servers: HashSet<String> = p
            .rows
            .iter()
            .map(|a| a.server.clone())
            .filter(|s| linked.contains(s))
            .collect();
        servers.insert(LOCAL.to_string());
        (p.mode, servers)
    };
    for server in servers {
        let picker = Rc::clone(&picker);
        let mode = mode;
        spawn(async move {
            let target = if server == LOCAL {
                "mailbox".to_string()
            } else {
                format!("mailbox@{server}")
            };
            let Ok(boxes) =
                service::call_json::<_, Vec<MailboxCount>>(&target, "boxes", &()).await
            else {
                return;
            };
            let mut b = picker.borrow_mut();
            let Some(p) = b.as_mut() else { return };
            if p.mode.0 != mode.0 {
                return;
            }
            p.unread.retain(|(s, _), _| *s != server);
            for bc in boxes {
                if bc.unread > 0 {
                    p.unread.insert((server.clone(), bc.box_), bc.unread);
                }
            }
            pick_render(p);
        });
    }
}

#[derive(serde::Deserialize)]
struct MailboxCount {
    #[serde(rename = "box")]
    box_: String,
    unread: i64,
    #[allow(dead_code)]
    #[serde(default)]
    total: i64,
}

/// The jump key: to the highlighted row's pane, or the reason it cannot.
/// Jumping acknowledges the agent (`ack`), like landing the cursor on it.
fn jump_after(p: &mut Picker, ack: &mut Option<(String, String)>) -> PickAfter {
    let Some(i) = p.view.get(p.sel).copied() else {
        return PickAfter::None;
    };
    let a = &p.rows[i];
    let live_pane = a.pane.filter(|_| a.live()).map(|x| x as u32);
    match (live_pane, p.local_pane_of(a)) {
        (Some(_), Some(local)) => {
            p.rows[i].acked_ms = Some(p.now_ms as i64);
            *ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
            PickAfter::Jump(local, p.mode)
        }
        (Some(_), None) => {
            p.status = Some(format!("not mirrored here: remote-attach {}", a.server));
            pick_render(p);
            PickAfter::None
        }
        (None, _) => {
            // No pane: bring the agent back. The form opens prefilled to
            // resume it (its transcript, its directory, its session) and
            // is the question - Esc says no. A row with nothing to resume
            // (a provisional id) only says so.
            if crate::newagent::resumable_id(&a.id).is_some() {
                p.status = Some("gone: the form brings it back (Esc: no)".into());
                pick_render(p);
                PickAfter::NewAgent(false)
            } else {
                p.status = Some("no live pane to jump to".into());
                pick_render(p);
                PickAfter::None
            }
        }
    }
}

/// Hand the keyboard to the preview. Only a row with a pane on this
/// server can take it (its own, or the shadow of a remote one); for any
/// other row the status line says why, in the jump key's words. Taking
/// the keyboard acknowledges an unread row, as jumping to it would: you
/// are about to talk to it. Returns the row to acknowledge.
fn focus_preview(p: &mut Picker) -> Option<(String, String)> {
    if live_pane_of_selection(p).is_none() {
        p.status = Some(unreachable_reason(p));
        pick_render(p);
        return None;
    }
    // Whatever was being typed into the picker itself is abandoned; the
    // keyboard cannot be in two places.
    p.filtering = false;
    p.composing = false;
    p.msg_buf.clear();
    p.renaming = false;
    p.rename_buf.clear();
    p.preview_focus = true;
    let mut ack = None;
    if let Some(&i) = p.view.get(p.sel) {
        if p.rows[i].unread() {
            p.rows[i].acked_ms = Some(p.now_ms as i64);
            ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
        }
    }
    pick_render(p);
    ack
}

/// A mouse key at cell (x, y) of the mode screen, 0-based. The screen is
/// the list on the left (its rows from screen row 3, scrolled by `top`),
/// the separator column at `list_w`, the preview to the right. Returns
/// whether the selection moved. A click takes the keyboard to the side it
/// lands on: on the preview it starts typing, on the list it stops.
fn mouse_key(
    p: &mut Picker,
    key: &str,
    x: u32,
    y: u32,
    after: &mut PickAfter,
    ack: &mut Option<(String, String)>,
) -> bool {
    let list_w = p.list_w();
    let (x, y) = (x as usize, y as usize);
    // A modifier prefix does not change where a click lands.
    let base = key.rsplit('-').next().unwrap_or(key);
    let in_list = x < list_w;
    match base {
        "MouseDown1Pane" | "DoubleClick1Pane" => {
            if !in_list {
                if x > list_w {
                    if transcript_shown(p) {
                        p.transcript_focus = true;
                        ensure_transcript_rendered(p);
                        pick_render(p);
                    } else {
                        *ack = focus_preview(p);
                    }
                }
                return false;
            }
            if p.preview_focus {
                p.preview_focus = false;
            }
            p.transcript_focus = false;
            // The search line: a click there focuses the box, as `/`.
            if y == 1 {
                p.filtering = true;
                pick_render(p);
                return false;
            }
            let Some(off) = y.checked_sub(3) else {
                pick_render(p);
                return false;
            };
            if off >= p.list_h() {
                pick_render(p);
                return false;
            }
            let Some(Line::Item(v)) = p.lines.get(p.top + off).cloned() else {
                pick_render(p);
                return false;
            };
            p.sel = v;
            // The user took the cursor: stop pulling it back to the
            // here row.
            p.seek_here = false;
            p.scroll_to_selection();
            if base == "DoubleClick1Pane" {
                *after = jump_after(p, ack);
            }
            pick_render(p);
            true
        }
        "WheelUpPane" | "WheelDownPane" if in_list => {
            // The wheel moves the cursor, so it must take the keyboard
            // back first: a cursor that moves while the preview types
            // would send the rest of the prompt to another agent.
            p.preview_focus = false;
            p.transcript_focus = false;
            move_sel(p, if base == "WheelUpPane" { -1 } else { 1 });
            true
        }
        "WheelUpPane" | "WheelDownPane" if transcript_shown(p) => {
            // Over the conversation: scroll it.
            ensure_transcript_rendered(p);
            scroll_transcript(p, if base == "WheelUpPane" { -3 } else { 3 });
            pick_render(p);
            false
        }
        "WheelUpPane" | "WheelDownPane" if !p.show_info => {
            // Over the live pane: the notch goes to that pane, as if the
            // pointer were over it. Its own bindings decide what a wheel
            // does there - copy mode, or the application's scrolling when
            // it takes the mouse - and the blit shows the result. The
            // picture itself is never scrolled.
            if let (Some(pane), Some(rect)) =
                (live_pane_of_selection(p), preview_rect(p, list_w))
            {
                let rx = (x as u32).saturating_sub(rect.x);
                let ry = (y as u32).saturating_sub(rect.y);
                *after = PickAfter::Wheel(pane, base.to_string(), rx, ry);
            }
            false
        }
        _ => false,
    }
}

/// How soon after a typed key the preview is re-blitted, twice: once for
/// a pane that echoes at once, once more for one that redraws its prompt
/// a beat later (an agent's TUI does). The host's own 500ms refresh
/// carries on regardless; these only bring the next frame forward.
const POKE_MS: [u64; 2] = [40, 160];

/// Bring the preview's next frame forward (see [`POKE_MS`]). Setting the
/// rect again is what redraws it now: the host blits on set, then keeps
/// its own cadence. Fire-and-forget, one task per key: a blit is a grid
/// copy, cheap enough that overlapping pokes cost nothing worth tracking.
fn poke_preview(picker: &Rc<RefCell<Option<Picker>>>, typing: bool) {
    let (mode, rect) = {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        (p.mode, preview_rect(p, p.list_w()))
    };
    let Some(rect) = rect else { return };
    let picker = Rc::clone(picker);
    spawn(async move {
        for ms in POKE_MS {
            if sleep_ms(ms).await.is_err() {
                return;
            }
            // Still the same picker, same pane in the preview (and, for
            // a typed key, still typing into it).
            let same = picker.borrow().as_ref().is_some_and(|p| {
                p.mode.0 == mode.0
                    && (!typing || p.preview_focus)
                    && live_pane_of_selection(p) == Some(rect.pane.0)
            });
            if !same {
                return;
            }
            let _ = mode_preview(mode, Some(&rect));
        }
    });
}

/// Move the selection by `delta` rows, clamped, then scroll and redraw.
fn move_sel(p: &mut Picker, delta: i32) {
    if p.view.is_empty() {
        return;
    }
    let last = (p.view.len() - 1) as i32;
    p.sel = (p.sel as i32 + delta).clamp(0, last) as usize;
    // The user took the cursor: stop pulling it back to the here row.
    p.seek_here = false;
    p.scroll_to_selection();
    pick_render(p);
}

/// Mark the current row into the selection, then move by `delta`. `J`/`K`
/// build a multi-row selection this way: each press ropes in the row under
/// the cursor and steps on, so N presses select N rows and leave the cursor
/// just past them.
fn mark_and_move(p: &mut Picker, delta: i32) {
    if let Some(key) = p.selected().map(Agent::key) {
        p.marked.insert(key);
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

/// The harness's own id for a row, with the roster's `kind:` prefix
/// taken off - what you paste into `claude --resume`, or grep a log for.
/// `None` while the row is still provisional: the roster has seen the
/// pane but no resolver has bound it to a durable id yet, and a made-up
/// `prov-claude-3` on the clipboard is worse than a refusal.
fn durable_id(a: &Agent) -> Option<&str> {
    if a.id.starts_with("prov-") {
        return None;
    }
    Some(a.id.strip_prefix(&format!("{}:", a.kind)).unwrap_or(&a.id))
}

/// The copy key takes the row under the cursor only - a clipboard holds
/// one thing, so a marked selection has no sensible answer here.
fn copy_after(p: &Picker) -> Result<PickAfter, String> {
    let Some(a) = p.selected() else {
        return Ok(PickAfter::None);
    };
    let Some(id) = durable_id(a) else {
        return Err(format!("{} has no id yet", display_name(a)));
    };
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:@+/".contains(c)) {
        // The id reaches the clipboard through a tmux command string,
        // and tmux's single quotes take no escapes, so there is no way
        // to quote a hostile one safely. No id a harness mints looks
        // like this; refuse rather than build the command anyway.
        return Err("that id has characters I will not paste".into());
    }
    Ok(PickAfter::Copy(id.to_string()))
}

/// What a bulk key acts on: the marked rows if there are any, else the
/// row under the cursor.
fn action_targets(p: &Picker) -> Vec<&Agent> {
    let marked: Vec<&Agent> = p
        .view
        .iter()
        .filter_map(|&i| p.rows.get(i))
        .filter(|a| p.marked.contains(&a.key()))
        .collect();
    if marked.is_empty() {
        p.selected().into_iter().collect()
    } else {
        marked
    }
}

/// The attention key moves a row between the top band and `waiting` by
/// hand. It toggles on what the targets already are: anything still in
/// `needs_input` drops to `waiting` (the common direction - the roster
/// flagged it, the user has judged it and wants it out of the way), and
/// a selection that is entirely `waiting` is raised into the attention
/// band (the user knows it needs them even though no hook said so).
///
/// A row that is mid-turn or finished has no say in this: `working` is
/// the pane's own business and a dead agent needs nothing. Rather than
/// silently ignoring the key there, it says why - the band a row sits in
/// is the whole point of the picker, and a key that does nothing on the
/// row under the cursor looks like the picker is wedged.
fn attention_after(p: &Picker) -> Result<PickAfter, String> {
    let targets = action_targets(p);
    if targets.is_empty() {
        return Ok(PickAfter::None);
    }
    let movable: Vec<&Agent> = targets
        .iter()
        .copied()
        .filter(|a| a.live() && matches!(a.status.as_str(), "needs_input" | "waiting"))
        .collect();
    if movable.is_empty() {
        let a = targets[0];
        return Err(if !a.live() {
            "that agent is done".into()
        } else {
            format!("{} is mid-turn; nothing is waiting on you", display_name(a))
        });
    }
    // Any flagged row in the selection means the key clears; only an
    // all-waiting selection raises.
    let to = if movable.iter().any(|a| a.status == "needs_input") {
        "waiting"
    } else {
        "needs_input"
    };
    let ids: Vec<(String, String)> = movable
        .iter()
        .filter(|a| a.status != to)
        .map(|a| (a.server.clone(), a.id.clone()))
        .collect();
    Ok(PickAfter::Status(ids, to.to_string()))
}

/// The archive key toggles: it archives its targets, or un-archives them
/// when they are all already archived (an archived row shows in the
/// history view). Targets are the marked selection, else the highlighted
/// row.
fn archive_after(p: &Picker) -> PickAfter {
    let targets = action_targets(p);
    if targets.is_empty() {
        return PickAfter::None;
    }
    // All targets already archived -> un-archive; otherwise archive.
    let life = if targets.iter().all(|a| a.life == "archived") {
        "active"
    } else {
        "archived"
    };
    let ids: Vec<(String, String)> =
        targets.iter().map(|a| (a.server.clone(), a.id.clone())).collect();
    PickAfter::Life(ids, life.to_string())
}

// ---------------------------------------------------------------------------
// state bands: the roster is grouped by server, then by what needs
// attention, then by recency inside each group.
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

/// Servers sort local first, then by name.
fn server_rank(a: &Agent) -> (bool, &str) {
    (!a.is_local(), a.server.as_str())
}

/// Order rows for a live refresh WITHOUT reshuffling under the cursor.
/// New agents get a rank in ideal (band + unread + recency) order the
/// first time they appear; thereafter rows sort by server, band then that
/// frozen rank. Server and band are primary, so a status change that
/// moves an agent to another band still moves it - only the churn from
/// activity-time bumps is removed.
fn stable_sort(
    order: &mut HashMap<String, u64>,
    order_next: &mut u64,
    rows: &mut Vec<Agent>,
) {
    // Ideal order first, so a batch of new agents is ranked sensibly.
    sort_rows(rows);
    for a in rows.iter() {
        let key = a.key();
        if !order.contains_key(&key) {
            order.insert(key, *order_next);
            *order_next += 1;
        }
    }
    let rank = |a: &Agent| order.get(&a.key()).copied().unwrap_or(u64::MAX);
    rows.sort_by(|a, b| {
        server_rank(a)
            .cmp(&server_rank(b))
            .then(band(a).cmp(&band(b)))
            .then(rank(a).cmp(&rank(b)))
    });
}

fn sort_rows(rows: &mut [Agent]) {
    // 0 sorts before 1: unread first.
    let unread_rank = |a: &Agent| u8::from(!a.unread());
    rows.sort_by(|a, b| {
        server_rank(a)
            .cmp(&server_rank(b))
            .then(band(a).cmp(&band(b)))
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
pub fn display_name(a: &Agent) -> String {
    let user = a.user_name.as_deref().map(unadorned).filter(|s| !s.is_empty());
    let harness = a.name.as_deref().map(unadorned).filter(|s| !s.is_empty());
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

/// A name without the decoration a harness puts in front of it. Claude
/// Code titles its pane "✳ <topic>" - a spinner glyph (✳ ✻ ✶ ✽ ✢ · ∗ ⁕,
/// it rotates) and a space - and the glyph says nothing in a list of
/// names. Any run of leading symbol characters followed by a space goes;
/// a bracket, a quote or a path character is kept, since those can be the
/// name. The stored name stays as written; this is display only.
fn unadorned(s: &str) -> &str {
    let s = s.trim_start();
    let keep = |c: char| {
        c.is_alphanumeric() || "[({<\"'`_~/.$@#-".contains(c) || c.is_whitespace()
    };
    let glyphs = s.chars().take_while(|&c| !keep(c)).count();
    if glyphs == 0 {
        return s;
    }
    let rest: &str = &s[s.char_indices().nth(glyphs).map(|(i, _)| i).unwrap_or(s.len())..];
    // Only a glyph that stands apart from the name is decoration.
    if rest.starts_with(char::is_whitespace) && !rest.trim_start().is_empty() {
        rest.trim_start()
    } else {
        s
    }
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
        "{} {} {} {} {} {} {} {} {} {} {}",
        a.id,
        display_name(a),
        a.kind,
        a.status,
        a.life,
        a.session.as_deref().unwrap_or(""),
        a.window.as_deref().unwrap_or(""),
        a.task.as_deref().unwrap_or(""),
        a.note.as_deref().unwrap_or(""),
        a.reason.as_deref().unwrap_or(""),
        if a.is_local() { "" } else { a.server.as_str() },
    )
}

/// The local pane the selected agent can be acted on through: its own for
/// a local row, its shadow for a mirrored remote one. `None` when the row
/// has no live pane, or is a remote row this server holds no mirror for -
/// both cases [`unreachable_reason`] explains.
fn live_pane_of_selection(p: &Picker) -> Option<u32> {
    let a = p.selected()?;
    a.pane.filter(|_| a.live())?;
    p.local_pane_of(a)
}

/// Why [`live_pane_of_selection`] gave nothing, in the words the jump key
/// already uses for the same two cases.
fn unreachable_reason(p: &Picker) -> String {
    match p.selected() {
        None => "no agent selected".into(),
        Some(a) if a.pane.filter(|_| a.live()).is_none() => {
            "no live pane: this agent is already gone".into()
        }
        Some(a) => format!("not mirrored here: remote-attach {}", a.server),
    }
}

/// Keep the band order of `p.rows`; the filter only includes or excludes.
fn pick_refilter(p: &mut Picker) {
    // The currently-selected agent, resolved against the CURRENT rows.
    // `.get` on both sides: a stale index (rows just replaced under us)
    // must never index out of bounds.
    let keep = p.selected().map(|a| (a.key(), a.server.clone(), a.pane));
    pick_refilter_keep(p, keep, true);
}

/// Rebuild `view`/`sel`/`lines`, restoring the highlight to `keep`'s agent
/// if it survived the filter. Callers that replace `rows` pass the key
/// they captured from the OLD rows, since the internal `view`/`sel` no
/// longer index the new set.
fn pick_refilter_keep(
    p: &mut Picker,
    keep: Option<(String, String, Option<i64>)>,
    reset_scroll: bool,
) {
    let query = parse_query(&p.filter);
    let needle = query.words.clone();
    // Refresh the content-match set when content search is on. The local
    // grep runs in tmux over the live grids (`panes_search`); only the
    // needle and the matches cross the ABI, so it is cheap enough per
    // keystroke. Remote grids are asked through each provider's `search`,
    // whose replies land later (see `remote_search`).
    if p.content_search && !needle.is_empty() {
        // The live local grids, plus (in the archive view) the saved
        // captures of the local archived rows whose pane is gone.
        let local: HashMap<u32, String> = p
            .rows
            .iter()
            .filter(|a| a.live() && a.is_local())
            .filter_map(|a| a.pane.map(|x| (x as u32, a.key())))
            .collect();
        let panes: Vec<PaneId> = local.keys().map(|&x| PaneId(x)).collect();
        let captures: &[(String, String)] =
            if p.archived_only { &p.captures } else { &[] };
        let (mode, grid, text) = run_content_search(&panes, captures, &needle);
        p.content_mode = mode;
        // Keep remote hits for the same query; local ones are fresh.
        if p.content_query != needle {
            p.content_hits.clear();
            p.content_query = needle.clone();
            if p.transcript_query == needle {
                // Otherwise transcript_search below sends the one request
                // that serves both.
                remote_search(p, &needle);
            }
        } else {
            let local_prefix = store::row_key(LOCAL, "");
            p.content_hits.retain(|k, _| !k.starts_with(&local_prefix));
        }
        for (pane, snip) in grid {
            if let Some(key) = local.get(&pane) {
                p.content_hits.insert(key.clone(), snip);
            }
        }
        p.content_hits.extend(text);
    } else {
        p.content_hits = HashMap::new();
        p.content_query.clear();
    }
    // The conversation search: every agent's stored transcript, whether
    // or not its row is in the roster.
    transcript_search(p, &needle);
    pick_reshow(p, keep, reset_scroll);
}

/// Rebuild the visible list from `rows` and the hits in hand, without
/// searching again: the tail of a refilter, and what a late reply (rows
/// for the hits, a remote server's answer) re-runs.
fn pick_reshow(
    p: &mut Picker,
    keep: Option<(String, String, Option<i64>)>,
    reset_scroll: bool,
) {
    let query = parse_query(&p.filter);
    let needle = query.words.clone();
    merge_hit_rows(p);
    p.view = p
        .rows
        .iter()
        .enumerate()
        .filter(|(_, a)| row_shown(p, a, &query))
        .map(|(i, _)| i)
        .collect();
    if !needle.is_empty() {
        // With a query, rows sort by relevance inside their band: a name
        // that starts with the query, then one that contains it, then the
        // conversation hits by score. Stable, so ties keep their order.
        let mut keyed: Vec<(usize, (bool, String), u8, f32)> = p
            .view
            .iter()
            .map(|&i| {
                let a = &p.rows[i];
                let tier = match rank(&haystack(a), &needle) {
                    Some(0) => 2.0e6,
                    Some(1) => 1.0e6,
                    _ => 0.0,
                };
                let hit = p.transcript_hits.get(&a.key()).map(|h| h.score).unwrap_or(0.0);
                (i, (!a.is_local(), a.server.clone()), band(a), tier + hit)
            })
            .collect();
        keyed.sort_by(|x, y| {
            x.1.cmp(&y.1)
                .then(x.2.cmp(&y.2))
                .then(y.3.partial_cmp(&x.3).unwrap_or(std::cmp::Ordering::Equal))
        });
        p.view = keyed.into_iter().map(|k| k.0).collect();
    }
    p.sel = keep
        .and_then(|(key, server, pane)| {
            // Prefer the key; fall back to the pane on the same server,
            // which survives an id migration (prov -> durable) that the
            // key would miss.
            p.view
                .iter()
                .position(|&i| p.rows[i].key() == key)
                .or_else(|| {
                    pane.and_then(|pn| {
                        p.view.iter().position(|&i| {
                            p.rows[i].pane == Some(pn) && p.rows[i].server == server
                        })
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
    p.sync_focus();
}

/// Whether a row is in the view: it belongs to the view's set (every row,
/// or only the archived ones), and the filter matches its metadata, a
/// content hit landed on it, or its conversation matched.
fn row_shown(p: &Picker, a: &Agent, q: &Query) -> bool {
    if p.archived_only && a.life != "archived" {
        return false;
    }
    if !filters_pass(p, q, a) {
        return false;
    }
    let key = a.key();
    rank(&haystack(a), &q.words).is_some()
        || p.content_hits.contains_key(&key)
        || p.transcript_hits.contains_key(&key)
}

// ---------------------------------------------------------------------------
// filter tokens: @server #session ~dir
// ---------------------------------------------------------------------------

/// The sigils that start a filter token in the search box.
pub const SIGILS: [char; 3] = ['@', '#', '~'];

/// How many values the dropdown offers at most.
const COMPLETIONS_MAX: usize = 8;

/// A parsed search box: the free words that search names, tasks and
/// transcripts, and the filter tokens - `@server`, `#session`, `~dir` -
/// that narrow the roster. Several of one kind are alternatives; the
/// kinds combine. A backslash escapes a sigil into an ordinary word:
/// `\@foo` is the word `@foo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub words: String,
    pub servers: Vec<String>,
    pub sessions: Vec<String>,
    pub dirs: Vec<String>,
}

impl Query {
    pub fn has_filters(&self) -> bool {
        !(self.servers.is_empty() && self.sessions.is_empty() && self.dirs.is_empty())
    }

    /// The active tokens, spelled as typed.
    pub fn tokens(&self) -> String {
        let mut t: Vec<String> = Vec::new();
        t.extend(self.servers.iter().map(|v| format!("@{v}")));
        t.extend(self.sessions.iter().map(|v| format!("#{v}")));
        t.extend(self.dirs.iter().map(|v| format!("~{v}")));
        t.join(" ")
    }
}

pub fn parse_query(filter: &str) -> Query {
    let mut q = Query::default();
    let mut words: Vec<&str> = Vec::new();
    for w in filter.split_whitespace() {
        let Some(c) = w.chars().next() else { continue };
        if c == '\\' {
            let rest = &w[1..];
            words.push(if rest.starts_with(SIGILS) { rest } else { w });
        } else if SIGILS.contains(&c) {
            let v = &w[c.len_utf8()..];
            if v.is_empty() {
                // A bare sigil is a token being typed, not a filter yet.
                continue;
            }
            match c {
                '@' => q.servers.push(v.to_string()),
                '#' => q.sessions.push(v.to_string()),
                _ => q.dirs.push(v.to_string()),
            }
        } else {
            words.push(w);
        }
    }
    q.words = words.join(" ");
    q
}

/// `~/...` for a path under the home directory.
fn tilde_of(home: &Option<String>, path: &str) -> String {
    match home {
        Some(h) if path.starts_with(h.as_str()) => format!("~{}", &path[h.len()..]),
        _ => path.to_string(),
    }
}

/// Does the row pass the query's filter tokens? Server and session by
/// prefix, so `@dm` is dmatrix and `#tm` is tmux2; the directory by
/// substring of its `~` form, so `~tmux2` takes both tmux2 trees.
fn filters_pass(p: &Picker, q: &Query, a: &Agent) -> bool {
    fn prefix(vals: &[String], target: &str) -> bool {
        let t = target.to_lowercase();
        vals.is_empty() || vals.iter().any(|v| t.starts_with(&v.to_lowercase()))
    }
    if !prefix(&q.servers, &a.server) {
        return false;
    }
    if !prefix(&q.sessions, a.session.as_deref().unwrap_or("")) {
        return false;
    }
    if !q.dirs.is_empty() {
        let dir = a
            .cwd
            .as_deref()
            .map(|c| tilde_of(&p.home, c))
            .unwrap_or_default()
            .to_lowercase();
        if !q.dirs.iter().any(|v| dir.contains(&v.to_lowercase())) {
            return false;
        }
    }
    true
}

/// The token under the cursor: the search box ends in a word that starts
/// with a sigil. Its sigil and what is typed after it.
fn typing_token(filter: &str) -> Option<(char, &str)> {
    if filter.ends_with(char::is_whitespace) {
        return None;
    }
    let last = filter.split_whitespace().last()?;
    let c = last.chars().next()?;
    if !SIGILS.contains(&c) {
        return None;
    }
    Some((c, &last[c.len_utf8()..]))
}

/// The search text changed under the cursor: refilter, and offer the
/// values that complete a token being typed.
fn filter_edited(p: &mut Picker) {
    p.completion_hidden = false;
    pick_refilter(p);
    update_completions(p);
    pick_render(p);
}

/// Rebuild the dropdown for the token being typed, from the values the
/// roster holds: servers, sessions, or `~` directories, with how many
/// rows have each. Prefix for servers and sessions, substring for
/// directories; most rows first.
fn update_completions(p: &mut Picker) {
    p.completions.clear();
    p.completion_idx = 0;
    if !p.filtering || p.completion_hidden {
        return;
    }
    let Some((sigil, partial)) = typing_token(&p.filter) else { return };
    let partial = partial.to_lowercase();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for a in &p.rows {
        let v = match sigil {
            '@' => Some(a.server.clone()),
            '#' => a.session.clone(),
            // The `~` form without its own tilde: the sigil is one, so
            // the token reads `~/Code/x` and matches the path's tail.
            _ => a.cwd.as_deref().map(|c| {
                let t = tilde_of(&p.home, c);
                t.strip_prefix('~').map(str::to_string).unwrap_or(t)
            }),
        };
        let Some(v) = v.filter(|v| !v.is_empty()) else { continue };
        let lv = v.to_lowercase();
        let ok = if sigil == '~' { lv.contains(&partial) } else { lv.starts_with(&partial) };
        if ok {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list.truncate(COMPLETIONS_MAX);
    // The one value left is what is typed already: nothing to offer.
    if list.len() == 1 && list[0].0.to_lowercase() == partial {
        list.clear();
    }
    p.completions = list;
}

/// Replace the token being typed with the highlighted value, and a space
/// so the next word starts fresh.
fn accept_completion(p: &mut Picker) {
    let Some((v, _)) = p.completions.get(p.completion_idx).cloned() else { return };
    let Some((sigil, plen)) = typing_token(&p.filter).map(|(s, t)| (s, t.len())) else {
        return;
    };
    let cut = p.filter.len() - sigil.len_utf8() - plen;
    p.filter.truncate(cut);
    p.filter.push(sigil);
    p.filter.push_str(&v);
    p.filter.push(' ');
    p.completions.clear();
    pick_refilter(p);
}

/// The token that narrows to the highlighted row: `s` its session, `S`
/// its server, `d` the last folder of its directory.
fn narrow_token(p: &Picker, key: &str) -> Option<String> {
    let a = p.selected()?;
    match key {
        "s" => a.session.as_deref().filter(|s| !s.is_empty()).map(|s| format!("#{s}")),
        "S" => Some(format!("@{}", a.server)),
        _ => a.cwd.as_deref().map(|c| {
            let t = tilde_of(&p.home, c);
            let base = t.trim_end_matches('/').rsplit('/').next().unwrap_or(&t).to_string();
            format!("~{base}")
        }),
    }
}

/// Add the token to the search box, or take it out if it is there.
fn toggle_token(p: &mut Picker, tok: &str) {
    let mut words: Vec<String> = p.filter.split_whitespace().map(str::to_string).collect();
    let before = words.len();
    words.retain(|w| !w.eq_ignore_ascii_case(tok));
    let removed = words.len() != before;
    if !removed {
        words.push(tok.to_string());
    }
    p.filter = words.join(" ");
    if !p.filter.is_empty() {
        p.filter.push(' ');
    }
    p.status = Some(if removed { format!("{tok} off") } else { format!("{tok} on") });
    pick_refilter(p);
    pick_render(p);
}

/// The dropdown: under the search box, aligned with the token being
/// typed, one line per value with its row count; the highlighted one in
/// reverse. Drawn last, over the top of the list.
fn draw_completions(p: &Picker, out: &mut String, list_w: usize) {
    if !p.filtering || p.completions.is_empty() {
        return;
    }
    let Some((sigil, partial)) = typing_token(&p.filter) else { return };
    // "  search " is nine cells; the sigil sits where the token starts.
    let col = 10 + p.filter.chars().count() - 1 - partial.chars().count();
    let wmax = p.completions.iter().map(|(v, _)| v.chars().count()).max().unwrap_or(0);
    let width = (wmax + 8).min(list_w.saturating_sub(col + 1)).max(4);
    for (i, (v, n)) in p.completions.iter().enumerate() {
        let row = 3 + i;
        if row >= p.height as usize - 1 {
            break;
        }
        let line = clip(&format!(" {sigil}{v}  {n}"), width);
        let sgr = if i == p.completion_idx { "7" } else { "48;5;238" };
        out.push_str(&format!("\x1b[{row};{col}H\x1b[{sgr}m{line:<width$}\x1b[0m"));
    }
}

/// The rows the hits brought in follow the roster's own in `rows`, once
/// each and never doubling a row the roster holds; without a query they
/// leave again. Idempotent: the previous merge is cut off first.
fn merge_hit_rows(p: &mut Picker) {
    p.rows.truncate(p.roster_len);
    if p.transcript_query.is_empty() {
        p.hit_rows.clear();
        return;
    }
    let present: HashSet<String> = p.rows.iter().map(Agent::key).collect();
    for a in &p.hit_rows {
        if !present.contains(&a.key()) {
            p.rows.push(a.clone());
        }
    }
}

/// The conversation search for the query. The local index answers inside
/// the keystroke (hits by row key, no snippet yet); the rows the roster
/// lacks and the snippets follow from the store a moment later, and each
/// linked server's provider answers for its own agents (`remote_search`).
/// A query already searched is not searched again.
fn transcript_search(p: &mut Picker, needle: &str) {
    if needle.is_empty() {
        p.transcript_query.clear();
        p.transcript_hits.clear();
        return;
    }
    if p.transcript_query == needle {
        return;
    }
    p.transcript_query = needle.to_string();
    p.transcript_hits.clear();
    p.hit_rows.clear();
    let hits = transcript::search(needle, transcript::HITS_MAX);
    let missing: Vec<String> = hits
        .iter()
        .filter(|h| !p.rows.iter().any(|a| a.is_local() && a.id == h.id))
        .map(|h| h.id.clone())
        .collect();
    let windows: Vec<(String, i64, i64)> = hits
        .iter()
        .map(|h| (h.id.clone(), h.seq, h.seq + transcript::DOC_WINDOW))
        .collect();
    for h in hits {
        p.transcript_hits.insert(
            store::row_key(LOCAL, &h.id),
            TranscriptHit { id: h.id, seq: h.seq, score: h.score, snippet: String::new() },
        );
    }
    if !windows.is_empty() {
        local_hit_details(needle.to_string(), missing, windows);
    }
    remote_search(p, needle);
}

/// Fetch what the local hits still need - the rows the roster did not
/// hold, and one snippet each - and fold them into the picker if it is
/// still on the same query.
fn local_hit_details(needle: String, missing: Vec<String>, windows: Vec<(String, i64, i64)>) {
    spawn(async move {
        let rows = store::by_ids(&missing).await.unwrap_or_default();
        let turns = store::turns_windows(&windows).await.unwrap_or_default();
        let (terms, _) = index::query_terms(&needle);
        PICKER.with(|cell| {
            let Some(picker) = cell.borrow().clone() else { return };
            let mut b = picker.borrow_mut();
            let Some(p) = b.as_mut() else { return };
            if p.transcript_query != needle {
                return;
            }
            for a in rows {
                if !p.hit_rows.iter().any(|r| r.key() == a.key()) {
                    p.hit_rows.push(a);
                }
            }
            for (id, seq, _) in &windows {
                let key = store::row_key(LOCAL, id);
                if let Some(h) = p.transcript_hits.get_mut(&key) {
                    h.snippet = transcript::snippet_for(&turns, id, *seq, &terms);
                }
            }
            let keep = p.selected().map(|a| (a.key(), a.server.clone(), a.pane));
            pick_reshow(p, keep, false);
            pick_render(p);
        });
        // The highlighted row may now be a hit with a turn to open on.
        PICKER.with(|cell| {
            if let Some(picker) = cell.borrow().clone() {
                request_transcript(&picker);
            }
        });
    });
}

/// Ask every linked server's provider to search its agents for `needle`:
/// their live grids (and, in the archive view, their saved captures) for
/// content search, and their stored conversations. The replies fold into
/// `content_hits` and `transcript_hits` when they arrive, with the rows
/// the conversation hits belong to. The picker is found again through
/// the shared cell, so a reply for a closed picker or an outdated query
/// is dropped.
fn remote_search(p: &mut Picker, needle: &str) {
    let archived = p.archived_only;
    let mut servers: HashSet<String> = p
        .rows
        .iter()
        .filter(|a| !a.is_local())
        .map(|a| a.server.clone())
        .collect();
    // A server whose roster is empty here still has conversations.
    for s in service::servers().unwrap_or_default() {
        if !s.local && s.up && s.linked {
            servers.insert(s.name);
        }
    }
    if servers.is_empty() {
        return;
    }
    let mode = p.mode;
    let needle = needle.to_string();
    for server in servers {
        let needle = needle.clone();
        spawn(async move {
            let target = format!("@{server}");
            let req =
                provider::SearchReq { needle: needle.clone(), archived, transcript: true };
            let Ok(reply) =
                service::call_json::<_, provider::SearchReply>(&target, "search", &req).await
            else {
                return;
            };
            PICKER.with(|cell| {
                let Some(picker) = cell.borrow().clone() else { return };
                let mut b = picker.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if p.mode.0 != mode.0 {
                    return;
                }
                let mut changed = false;
                if p.content_search && p.content_query == needle {
                    for (id, snip) in &reply.hits {
                        p.content_hits.insert(store::row_key(&server, id), snip.clone());
                    }
                    if !reply.hits.is_empty() {
                        p.content_mode = provider::reply_mode(&reply);
                    }
                    changed = true;
                }
                if p.transcript_query == needle {
                    for h in reply.transcript {
                        p.transcript_hits.insert(store::row_key(&server, &h.id), h);
                    }
                    for mut a in reply.agents {
                        a.server = server.clone();
                        if !p.hit_rows.iter().any(|r| r.key() == a.key()) {
                            p.hit_rows.push(a);
                        }
                    }
                    changed = true;
                }
                if !changed {
                    return;
                }
                let keep = p.selected().map(|a| (a.key(), a.server.clone(), a.pane));
                // Rebuild the view with the new hits, without re-running
                // the search (the query is unchanged).
                pick_reshow(p, keep, false);
                pick_render(p);
            });
        });
    }
}

thread_local! {
    /// The plugin's picker cell, so a detached task (a remote search
    /// reply, the rows for a conversation hit) can find the picker
    /// without holding a borrow across awaits.
    pub static PICKER: RefCell<Option<Rc<RefCell<Option<Picker>>>>> =
        const { RefCell::new(None) };
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Flatten text that came from outside onto one line. A harness writes
/// its own words into a note (the Claude `Notification` message is a
/// sentence, sometimes two), and a newline or a stray control character
/// inside a row would tear the list apart.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_control() {
            gap = !out.is_empty();
        } else {
            if gap {
                out.push(' ');
                gap = false;
            }
            out.push(c);
        }
    }
    out
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
        // Unread needs-input: the bang on a bright yellow block; read: the
        // bang alone.
        "needs_input" if a.unread() => "\x1b[1;30;103m!\x1b[0m".into(),
        "needs_input" => "\x1b[1;33m!\x1b[0m".into(),
        "working" => "\x1b[32m●\x1b[0m".into(),
        // Unread waiting: a filled badge on a bright cyan block, the one
        // coloured background in the list, so it cannot be missed at a
        // glance. Read waiting: hollow, no block.
        "waiting" if a.unread() => "\x1b[1;30;106m◉\x1b[0m".into(),
        "waiting" => "\x1b[36m◍\x1b[0m".into(),
        "done" => "\x1b[2m·\x1b[0m".into(),
        _ => "?".into(),
    }
}

pub fn pick_render(p: &mut Picker) {
    let w = p.width as usize;
    let h = p.height as usize;
    let list_w = p.list_w();
    let mut out = String::from("\x1b[2J\x1b[H");

    let live = p.rows.iter().filter(|a| a.live()).count();
    let query = parse_query(&p.filter);
    // Narrowed by a filter token, the count says so: "9 of 22 live".
    let live = if query.has_filters() {
        let shown = p.view.iter().filter(|&&i| p.rows[i].live()).count();
        format!("{shown} of {live}")
    } else {
        live.to_string()
    };
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
    let servers_tag = if p.multi {
        let n = p
            .rows
            .iter()
            .map(|a| a.server.as_str())
            .collect::<HashSet<_>>()
            .len();
        format!(", {n} servers")
    } else {
        String::new()
    };
    out.push_str(&format!(
        "\x1b[1;1H\x1b[1m agents\x1b[0m \x1b[2m({live} live{}{unread_tag}{servers_tag}{content_tag}{selected})\x1b[0m",
        if p.archived_only {
            ", archive"
        } else if p.show_history {
            ", +history"
        } else {
            ""
        },
    ));
    if query.has_filters() {
        // The active tokens, at the right edge of the header, so a
        // narrowed list never passes for the whole roster.
        let t = query.tokens();
        let col = list_w.saturating_sub(t.chars().count() + 1).max(1);
        out.push_str(&format!("\x1b[1;{col}H\x1b[33m{}\x1b[0m", clip(&t, list_w.saturating_sub(col))));
    }
    if p.composing {
        // Compose mode takes over the prompt line, with a block cursor.
        let to = p.selected().map(display_name).unwrap_or_default();
        out.push_str(&format!(
            "\x1b[2;1H  \x1b[2mmsg {}\x1b[0m {}\x1b[7m \x1b[0m",
            clip(&to, 16),
            p.msg_buf
        ));
    } else if p.renaming {
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
            "\x1b[2;1H  \x1b[2msearch\x1b[0m \x1b[2m(/ to search)\x1b[0m",
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
        // An empty archive says so; an empty search result does not
        // change its words for the view it ran in.
        let what = if p.archived_only && p.filter.trim().is_empty() {
            "(nothing archived)"
        } else {
            "(no agents)"
        };
        out.push_str(&format!("\x1b[4;1H  \x1b[2m{what}\x1b[0m"));
    } else {
        for (line_i, li) in
            (p.top..(p.top + list_h).min(p.lines.len())).enumerate()
        {
            let row = 4 + line_i;
            match &p.lines[li] {
                Line::Server(server) => {
                    // A server line: bold, with the link state when down,
                    // the reason when its copy is rejected, and a spinner
                    // while its roster is still in flight. A healthy
                    // remote barely flashes; one that keeps the call
                    // hanging says for how long, which is the whole point
                    // (the alternative is a stale group with no reason).
                    let (label, colour) = match (
                        p.down.get(server),
                        p.mismatch.get(server),
                        spin_since(&p.fetching, server, p.now_ms),
                    ) {
                        (Some(since), _, _) => (
                            format!(
                                "{server}  (disconnected {})",
                                fmt_age(p.now_ms.saturating_sub(*since) / 1000)
                            ),
                            "1;31",
                        ),
                        (None, Some(why), _) => (format!("{server}  ({why})"), "1;33"),
                        (None, None, Some(since)) => {
                            let waited = p.now_ms.saturating_sub(since);
                            let frame = SPIN_FRAMES
                                [(p.now_ms / SPIN_MS) as usize % SPIN_FRAMES.len()];
                            if waited >= FETCH_STUCK_MS {
                                (
                                    format!(
                                        "{server}  {frame} (fetching {})",
                                        fmt_age(waited / 1000)
                                    ),
                                    "1;33",
                                )
                            } else {
                                (format!("{server}  {frame}"), "1;34")
                            }
                        }
                        (None, None, None) => (server.clone(), "1;34"),
                    };
                    out.push_str(&format!(
                        "\x1b[{row};1H\x1b[{colour}m▪ {}\x1b[0m",
                        clip(&label, list_w.saturating_sub(3)),
                    ));
                }
                Line::Header(b) => {
                    out.push_str(&format!(
                        "\x1b[{row};1H \x1b[1;2m{}\x1b[0m",
                        clip(band_label(*b), list_w.saturating_sub(2)),
                    ));
                }
                Line::Item(vpos) => {
                    let vpos = *vpos;
                    let a = &p.rows[p.view[vpos]];
                    let cur = vpos == p.sel;
                    let marked = p.marked.contains(&a.key());
                    let marker = if cur { "▸" } else { " " };
                    let age = fmt_age(p.age_ms(a) / 1000);
                    // session + kind + age are fixed columns at the right
                    // edge - the session is where the pane lives, which
                    // the name does not say; the name (with the task, when
                    // there is one) fills all the width that is left.
                    let right = format!(
                        "{:<12} {:<8} {:>6}",
                        clip(a.session.as_deref().unwrap_or(""), 12),
                        clip(&a.kind, 8),
                        age
                    );
                    // prefix = "▸ ● " (marker + badge), gap = 2 before right.
                    let label_w = list_w
                        .saturating_sub(1)
                        .saturating_sub(4 + 2 + right.chars().count())
                        .max(8);
                    let unread = p
                        .unread
                        .get(&(a.server.clone(), a.id.clone()))
                        .copied()
                        .unwrap_or(0);
                    let mut label = if unread > 0 {
                        format!("\u{2709}{unread} {}", display_name(a))
                    } else {
                        display_name(a)
                    };
                    // A content-search hit shows the matching line: it is
                    // why the row is here. Otherwise an archived row (only
                    // in the history views) says so, so the `a` un-archive
                    // is obvious; else the reported task.
                    let key = a.key();
                    let snip = p
                        .content_hits
                        .get(&key)
                        .filter(|_| p.content_search)
                        .or_else(|| {
                            p.transcript_hits
                                .get(&key)
                                .map(|h| &h.snippet)
                                .filter(|s| !s.is_empty())
                        });
                    // The note wins over the task: it is the reason this
                    // row is at the top of the list, and it only exists
                    // while the agent is blocked on the user.
                    let note = a.note.as_deref().filter(|s| !s.is_empty());
                    let task = a.task.as_deref().filter(|s| !s.is_empty());
                    if let Some(sn) = snip {
                        label = format!("{label}  ·  {}", sn.trim());
                    } else if a.life == "archived" {
                        label = format!("{label}  ·  archived");
                    } else if let Some(t) = note.or(task) {
                        label = format!("{label}  ·  {}", one_line(t));
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
                    // A row of a disconnected server is dimmed: what it
                    // shows is what the provider last said.
                    let stale = p.down.contains_key(&a.server);
                    if cur {
                        // The cursor row: reverse video across the width.
                        // Dimmed while the preview has the keyboard, so
                        // the bright thing on screen is where keys go.
                        let sgr = if p.preview_focus { "2;7" } else { "7" };
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[{sgr}m{:<pad$}\x1b[0m",
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
                    } else if stale {
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[2m{}\x1b[0m",
                            strip_sgr(&shown)
                        ));
                    } else if a.unread() {
                        // An unread row's name is bold too: the badge is
                        // one cell, the name is what the eye reads.
                        let bold = shown.replacen(
                            label.as_str(),
                            &format!("\x1b[1m{label}\x1b[0m"),
                            1,
                        );
                        out.push_str(&format!("\x1b[{row};1H{bold}"));
                    } else {
                        out.push_str(&format!("\x1b[{row};1H{shown}"));
                    }
                    // "You are here": the pane the picker was opened from
                    // gets a bright left border, drawn last so it shows over
                    // any row state (cursor, marked, or plain). The pane we
                    // sit in is always local, so a remote row has to be
                    // compared through its shadow: `a.pane` there is an id on
                    // the REMOTE server and would never match (and could
                    // collide with an unrelated local pane's id). `None`
                    // never matches - an unmirrored remote row and a picker
                    // opened from nowhere must not agree.
                    let here = p.is_here(a);
                    if here {
                        let g = if cur { "▸" } else { "▎" };
                        out.push_str(&format!("\x1b[{row};1H\x1b[1;94m{g}\x1b[0m"));
                    }
                }
            }
        }
    }

    // Vertical separator between the list and the preview. It lights up
    // while the preview has the keyboard: the one mark on screen that
    // says which side your keys are going to.
    let sep = if p.preview_focus || p.transcript_focus { "\x1b[1;36m┃" } else { "\x1b[2m│" };
    for r in 1..=h {
        out.push_str(&format!("\x1b[{r};{c}H{sep}\x1b[0m", c = list_w + 1));
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
    let footer = if p.show_help {
        "? back · Esc back".to_string()
    } else if p.show_info {
        format!("i back · {} jump · {} copy id · y copy cwd · Esc back", keyname(&k.jump), keyname(&k.copy))
    } else if p.transcript_focus {
        "j/k scroll · n/N match · g/G top/end · Space/b page · Tab pane · Esc back to list".to_string()
    } else if p.preview_focus {
        // Every key goes to the pane, so the footer can promise only one
        // thing about the keyboard: how to get it back.
        // The prefix route is named too, since it is the one that costs
        // the agent nothing: tmux hands a directional select-pane on the
        // float to the picker (mode-nav), and `prefix h` is select-pane
        // -L in the common config.
        let to = p.selected().map(display_name).unwrap_or_default();
        format!(
            "typing into {} · keys go to its pane · {} back to list (or select-pane -L)",
            clip(&to, 16),
            pretty_key(&k.unfocus)
        )
    } else if p.composing {
        "type a message · Enter send · Esc cancel".to_string()
    } else if p.renaming {
        "type a name · Enter accept · Esc cancel".to_string()
    } else if p.filtering {
        format!("type to search · @server #session ~dir · {ctok} · Esc unfocus")
    } else {
        // Six hints, not twelve. Everything else - archive, rename,
        // copy, the band, interrupt, kill - lives in the action menu,
        // which names each one in full and says which apply to the row
        // under the cursor. A footer that lists every key fits none of
        // them at a usable width.
        format!(
            "j/k move · {} type · {} jump · {} new · {} actions · {} search · {ctok} · {} history · ? help · q/{} close",
            keyname(&k.focus),
            keyname(&k.jump),
            keyname(&k.new),
            keyname(&k.menu),
            keyname(&k.filter),
            keyname(&k.history),
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
    draw_completions(p, &mut out, list_w);

    // The preview: a live blit of the local (or mirrored) pane; else the
    // row's conversation (a finished agent, or a live one with Tab); else
    // the provider's captured text for a remote row with no mirror here.
    let rect = preview_rect(p, list_w);
    if rect.is_none() {
        let x = list_w + 2;
        let pw = w.saturating_sub(list_w + 2);
        let ph = h.saturating_sub(1);
        if p.show_help {
            draw_help(p, &mut out, x, pw, ph);
        } else if p.show_info {
            draw_info(p, &mut out, x, pw, ph);
        } else if transcript_shown(p) {
            draw_transcript(p, &mut out, x, pw, ph);
        } else if let Some(text) = remote_preview_lines(p) {
            for (i, line) in text.iter().rev().take(ph).rev().enumerate() {
                out.push_str(&format!(
                    "\x1b[{};{x}H{}",
                    i + 1,
                    clip(&strip_sgr(line), pw)
                ));
            }
        }
    }

    let _ = mode_write(p.mode, out.as_bytes());
    let _ = mode_preview(p.mode, rect.as_ref());
}

/// Does the preview show the highlighted row's conversation? Only once
/// its turns (or a stand-in capture) have arrived for that row.
fn transcript_shown(p: &Picker) -> bool {
    let Some(a) = p.selected() else { return false };
    p.transcript
        .as_ref()
        .is_some_and(|tv| tv.key == a.key() && (!tv.turns.is_empty() || !tv.capture.is_empty()))
}

/// The info card: everything the roster knows about the highlighted
/// agent, as labelled lines. The row's own facts draw at once; the
/// fetched half (`InfoCard`) fills in when it lands.
/// The quick reference, in the preview column: every key by area, and
/// the search box's syntax. Configured keys are shown as configured.
fn draw_help(p: &Picker, out: &mut String, x: usize, pw: usize, ph: usize) {
    let k = &p.keys;
    let n = |s: &str| keyname(s).to_string();
    let mut lines: Vec<(String, String)> = Vec::new();
    let section = |title: &str, rows: &[(String, &str)], lines: &mut Vec<(String, String)>| {
        if !lines.is_empty() {
            lines.push((String::new(), String::new()));
        }
        lines.push((format!("\x00{title}"), String::new()));
        for (key, what) in rows {
            lines.push((key.clone(), what.to_string()));
        }
    };
    section("moving", &[
        ("j/k ↑/↓".into(), "move the cursor"),
        ("gg / G".into(), "first / last row"),
        (n(&k.jump), "jump to the pane"),
        ("Tab".into(), "conversation / pane in the preview"),
        (format!("{} click", n(&k.focus)), "type into the pane; keys go there"),
        (n(&k.unfocus), "take the keyboard back"),
        ("wheel".into(), "over the preview: scrolls the pane itself"),
    ], &mut lines);
    section("search box", &[
        (n(&k.filter), "focus the box; words match names, tasks, conversations"),
        ("@server".into(), "narrow to a server (prefix)"),
        ("#session".into(), "narrow to a session (prefix)"),
        ("~dir".into(), "narrow to a directory (part of its ~ path)"),
        ("dropdown".into(), "a sigil opens it: Tab/↓ BTab/↑ walk, Enter takes, Esc hides"),
        ("\\@ \\# \\~".into(), "the character as a plain word"),
        ("s / S / d".into(), "narrow to this row's session / server / folder; again widens"),
        (n(&k.content), "also grep the panes' contents"),
        ("C-u".into(), "clear the box"),
        ("Esc Enter".into(), "leave the box, keep the query"),
    ], &mut lines);
    section("rows", &[
        (n(&k.archive), "archive / un-archive"),
        (n(&k.archived), "the archive view"),
        (n(&k.history), "show finished agents too"),
        (n(&k.attention), "move between attention and waiting"),
        (format!("{} / {}", n(&k.read), n(&k.unread)), "mark read / unread"),
        (n(&k.rename), "rename"),
        (n(&k.copy), "copy the agent id"),
        ("m".into(), "message the agent"),
        (n(&k.interrupt), "interrupt (C-c) the agent"),
        (n(&k.kill), "kill its pane (asks)"),
        (n(&k.new), "new agent, prefilled from the row"),
        ("f".into(), "fork this agent (a copy of its session, its own name)"),
        ("J / K".into(), "mark the row and move"),
        (n(&k.menu), "the action menu"),
        ("i".into(), "info card (y copies its cwd)"),
    ], &mut lines);
    section("conversation (Tab)", &[
        ("j/k".into(), "scroll"),
        ("n / N".into(), "next / previous match"),
        ("g / G".into(), "top / end"),
        ("Space / b".into(), "page down / up"),
        ("Esc".into(), "back to the list"),
    ], &mut lines);
    section("picker", &[
        ("+ / -".into(), "resize"),
        ("?".into(), "this card"),
        (format!("q / {}", n(&k.close)), "close (Esc first puts a card or the marks away)"),
    ], &mut lines);
    out.push_str(&format!("\x1b[1;{x}H\x1b[1;36m{}\x1b[0m", clip("quick reference · ? or Esc back", pw)));
    let key_w = 12;
    let body_w = pw.saturating_sub(key_w + 1).max(8);
    let mut row = 2;
    for (key, what) in &lines {
        if row > ph {
            break;
        }
        if let Some(title) = key.strip_prefix('\x00') {
            out.push_str(&format!("\x1b[{row};{x}H\x1b[1;2m{}\x1b[0m", clip(title, pw)));
        } else if !key.is_empty() {
            out.push_str(&format!(
                "\x1b[{row};{x}H\x1b[33m{:<key_w$}\x1b[0m {}",
                clip(key, key_w),
                clip(what, body_w)
            ));
        }
        row += 1;
    }
}

fn draw_info(p: &Picker, out: &mut String, x: usize, pw: usize, ph: usize) {
    let Some(a) = p.selected() else { return };
    let card = p.info.as_ref().filter(|c| c.key == a.key());
    let home = home_dir().ok().filter(|h| !h.is_empty());
    let tilde = |path: &str| -> String {
        match &home {
            Some(h) if path.starts_with(h.as_str()) => format!("~{}", &path[h.len()..]),
            _ => path.to_string(),
        }
    };
    let mut rows: Vec<(&str, String)> = Vec::new();
    // Name: the shown one, and the other party's when both exist.
    let shown = display_name(a);
    let other = match (a.user_name.as_deref(), a.name.as_deref()) {
        (Some(u), Some(h)) if u != h => {
            if shown == u { format!("  (harness: {h})") } else { format!("  (you: {u})") }
        }
        _ => String::new(),
    };
    rows.push(("name", format!("{shown}{other}")));
    let mut harness = a.kind.clone();
    if let Some(v) = a.harness_version.as_deref() {
        harness.push(' ');
        harness.push_str(v);
    }
    if let Some(m) = a.model.as_deref() {
        harness.push_str(" · ");
        harness.push_str(m);
    }
    rows.push(("harness", harness));
    let where_ = if a.live() {
        format!(
            "{}:{} · {} · {}",
            a.session.as_deref().unwrap_or("?"),
            a.window.as_deref().unwrap_or("?"),
            a.pane.map(|n| format!("%{n}")).unwrap_or_else(|| "no pane".into()),
            if a.is_local() { "this server".to_string() } else { a.server.clone() }
        )
    } else {
        format!(
            "ended {} ago ({}) · was {}:{}{}",
            fmt_age(p.now_ms.saturating_sub(a.ended_ms.unwrap_or(0).max(0) as u64) / 1000),
            a.reason.as_deref().unwrap_or("gone"),
            a.session.as_deref().unwrap_or("?"),
            a.window.as_deref().unwrap_or("?"),
            if a.is_local() { String::new() } else { format!(" on {}", a.server) }
        )
    };
    rows.push(("where", where_));
    let cwd = card.and_then(|c| c.live_cwd.clone()).or_else(|| a.cwd.clone());
    rows.push(("cwd", cwd.as_deref().map(tilde).unwrap_or_else(|| "unknown".into())));
    let mut branch = a.git_branch.clone().unwrap_or_else(|| "unknown".into());
    match card.and_then(|c| c.dirty) {
        Some(true) => branch.push_str(" · dirty"),
        Some(false) => branch.push_str(" · clean"),
        None => {}
    }
    rows.push(("branch", branch));
    rows.push((
        "started",
        format!(
            "{} ago · last active {} ago",
            fmt_age(p.now_ms.saturating_sub(a.started().max(0) as u64) / 1000),
            fmt_age(p.age_ms(a) / 1000)
        ),
    ));
    let mut status = a.status.clone();
    if a.life == "archived" {
        status.push_str(" · archived");
    }
    if a.unread() {
        status.push_str(" · unread");
    }
    if let Some(n) = a.note.as_deref().filter(|s| !s.is_empty()) {
        status.push_str(" · ");
        status.push_str(&one_line(n));
    }
    rows.push(("status", status));
    match card.and_then(|c| c.stats.as_ref()) {
        Some(st) => {
            rows.push((
                "turns",
                format!(
                    "{} prompts · {} replies · {} tool calls ({} edits, {} reads, {} commands)",
                    st.prompts, st.replies, st.tools, st.edits, st.reads, st.commands
                ),
            ));
            if !st.files.is_empty() {
                let files: Vec<String> = st
                    .files
                    .iter()
                    .map(|(f, n)| {
                        let short = f.rsplit('/').next().unwrap_or(f);
                        if *n > 1 { format!("{short} ×{n}") } else { short.to_string() }
                    })
                    .collect();
                rows.push(("files", files.join(" · ")));
            }
        }
        None if card.is_none() => rows.push(("turns", "…".into())),
        None => {}
    }
    if let Some(tp) = a.transcript_path.as_deref() {
        rows.push((
            "transcript",
            format!("{} · {} KB read", tilde(tp), a.transcript_cursor.max(0) / 1024),
        ));
    }
    if let Some(id) = durable_id(a) {
        let resume = match a.kind.as_str() {
            "claude" => format!("claude --resume {id}"),
            "codex" => format!("codex resume {id}"),
            _ => id.to_string(),
        };
        rows.push(("resume", resume));
    }
    // Draw: the id on the first line, then the rows, each wrapped to the
    // column with the label's width hanging.
    let mut y = 1usize;
    out.push_str(&format!("\x1b[{y};{x}H\x1b[1;36m{}\x1b[0m", clip(&a.id, pw)));
    y += 2;
    let label_w = 11;
    for (label, value) in rows {
        if y > ph {
            break;
        }
        let body_w = pw.saturating_sub(label_w).max(8);
        for (i, piece) in wrap_cells(&plain_cells(&value, 0), body_w).into_iter().enumerate() {
            if y > ph {
                break;
            }
            let lead = if i == 0 { format!("{label:<label_w$}") } else { " ".repeat(label_w) };
            out.push_str(&format!(
                "\x1b[{y};{x}H\x1b[2m{lead}\x1b[0m{}",
                emit_cells(&piece, false)
            ));
            y += 1;
        }
    }
}

/// Draw the conversation into the preview area: a header line, then the
/// rendered turns from the scroll position.
fn draw_transcript(p: &mut Picker, out: &mut String, x: usize, pw: usize, ph: usize) {
    let live = live_pane_of_selection(p).is_some();
    let focused = p.transcript_focus;
    ensure_transcript_rendered_for(p, pw);
    let Some(tv) = p.transcript.as_mut() else { return };
    let body = ph.saturating_sub(1);
    let max_top = tv.lines.len().saturating_sub(body);
    if tv.top > max_top {
        tv.top = max_top;
    }
    let what = if tv.turns.is_empty() {
        "last screen".to_string()
    } else {
        format!("{} turns", tv.turns.len())
    };
    let matches = match (tv.matches.len(), tv.match_idx) {
        (0, _) => String::new(),
        (n, Some(i)) => format!(" · match {}/{n}", i + 1),
        (n, None) => format!(" · {n} matches"),
    };
    let hint = if focused {
        " · Esc back"
    } else if live {
        " · Tab pane · click or l to scroll"
    } else {
        " · click or l to scroll"
    };
    let header = format!("conversation · {what}{matches}{hint}");
    let sgr = if focused { "\x1b[1;36m" } else { "\x1b[2m" };
    out.push_str(&format!("\x1b[1;{x}H{sgr}{}\x1b[0m", clip(&header, pw)));
    let current = tv.match_idx.and_then(|m| tv.matches.get(m).copied());
    for (i, line) in tv.lines.iter().skip(tv.top).take(body).enumerate() {
        let emitted = emit_cells(line, current == Some(tv.top + i));
        out.push_str(&format!("\x1b[{};{x}H{emitted}", i + 2));
    }
}

/// Render the conversation for the preview's current width, if it is
/// not already.
fn ensure_transcript_rendered(p: &mut Picker) {
    let pw = (p.width as usize).saturating_sub(p.list_w() + 2);
    ensure_transcript_rendered_for(p, pw);
}

fn ensure_transcript_rendered_for(p: &mut Picker, pw: usize) {
    let (terms, _) = index::query_terms(&p.transcript_query);
    if let Some(tv) = p.transcript.as_mut() {
        if tv.width != pw || tv.lines.is_empty() {
            render_transcript(tv, pw, &terms);
        }
    }
}

/// Scroll the conversation by `delta` rendered lines, clamped.
fn scroll_transcript(p: &mut Picker, delta: i64) {
    let body = (p.height as usize).saturating_sub(2).max(1);
    if let Some(tv) = p.transcript.as_mut() {
        let max_top = tv.lines.len().saturating_sub(body);
        let t = (tv.top as i64 + delta).clamp(0, max_top as i64);
        tv.top = t as usize;
    }
}

/// A key while the conversation has the keyboard.
fn transcript_key(p: &mut Picker, key: &str, is_up: bool, is_down: bool) {
    let page = (p.height as usize).saturating_sub(2).max(2) as i64;
    ensure_transcript_rendered(p);
    match key {
        "Escape" | "q" | "h" | "Left" => {
            p.transcript_focus = false;
        }
        "Tab" => {
            // Back to the pane (a live row): the keyboard goes with it
            // to the list, the pane being where typing would go.
            p.show_transcript = false;
            p.transcript_focus = false;
        }
        "j" | "Down" | "C-n" | "C-j" | "Enter" => scroll_transcript(p, 1),
        "k" | "Up" | "C-p" | "C-k" => scroll_transcript(p, -1),
        "Space" | "PageDown" | "C-d" | "]" => scroll_transcript(p, page / 2),
        "b" | "PageUp" | "C-u" | "[" => scroll_transcript(p, -(page / 2)),
        "g" | "Home" => {
            if let Some(tv) = p.transcript.as_mut() {
                tv.top = 0;
            }
        }
        "G" | "End" => {
            if let Some(tv) = p.transcript.as_mut() {
                tv.top = usize::MAX; // clamped when drawn
            }
        }
        "n" | "N" => {
            let forward = key == "n";
            if let Some(tv) = p.transcript.as_mut() {
                if !tv.matches.is_empty() {
                    let n = tv.matches.len();
                    let i = match tv.match_idx {
                        None => {
                            // The first match below the top (or above, for N).
                            if forward {
                                tv.matches.iter().position(|&l| l > tv.top).unwrap_or(0)
                            } else {
                                tv.matches.iter().rposition(|&l| l < tv.top).unwrap_or(n - 1)
                            }
                        }
                        Some(i) if forward => (i + 1) % n,
                        Some(i) => (i + n - 1) % n,
                    };
                    tv.match_idx = Some(i);
                    tv.top = tv.matches[i].saturating_sub(2);
                }
            }
            if p.transcript.as_ref().is_some_and(|tv| tv.matches.is_empty()) {
                p.status = Some("no matches in this conversation".into());
            }
        }
        _ => {
            let _ = (is_up, is_down);
            return;
        }
    }
    pick_render(p);
}

// ---------------------------------------------------------------------------
// the conversation, rendered: light Markdown to styled cells
// ---------------------------------------------------------------------------

const ST_BOLD: u8 = 1;
const ST_ITALIC: u8 = 2;
const ST_CODE: u8 = 4;
const ST_UNDER: u8 = 8;
const ST_DIM: u8 = 16;
const ST_HIT: u8 = 32;
const ST_CYAN: u8 = 64;

/// One cell of a rendered line: a character and its style bits.
type Styled = (char, u8);

/// Lay the turns out for `width`: a prompt with a `❯` in front, in bold;
/// the agent's text with its Markdown rendered (headings, emphasis,
/// inline and fenced code, lists, quotes); a tool line dim; a blank line
/// between turns; the query's terms in reverse video. Remembers which
/// lines hold a match, and opens on the first match when there is one,
/// else on the hit's turn, else at the end.
fn render_transcript(tv: &mut TranscriptView, width: usize, terms: &[String]) {
    tv.width = width;
    tv.lines.clear();
    tv.matches.clear();
    tv.match_idx = None;
    let width = width.max(8);
    let mut open_at: Option<usize> = None;
    if tv.turns.is_empty() {
        for l in &tv.capture {
            tv.lines.push(plain_cells(&clip(&strip_sgr(l), width), 0));
        }
    }
    let mut cells: Vec<Vec<Styled>> = Vec::new();
    let mut prev_kind: Option<&str> = None;
    for t in &tv.turns {
        // A blank line between turns, except between one tool line and
        // the next: a run of tool calls reads as one block.
        if let Some(prev) = prev_kind {
            if !(prev == "tool" && t.kind == "tool") {
                cells.push(Vec::new());
            }
        }
        prev_kind = Some(t.kind.as_str());
        if tv.open_seq >= 0 && open_at.is_none() && t.seq >= tv.open_seq {
            open_at = Some(cells.len());
        }
        match t.kind.as_str() {
            "user" => {
                let body = markdown_lines(&t.text, width.saturating_sub(2), ST_BOLD);
                for (i, l) in body.into_iter().enumerate() {
                    let mut line: Vec<Styled> = if i == 0 {
                        vec![('❯', ST_BOLD | ST_CYAN), (' ', 0)]
                    } else {
                        vec![(' ', 0), (' ', 0)]
                    };
                    line.extend(l);
                    cells.push(line);
                }
            }
            "tool" => {
                for (i, l) in wrap_cells(&plain_cells(&t.text, ST_DIM), width.saturating_sub(4)).into_iter().enumerate() {
                    let mut line: Vec<Styled> = if i == 0 {
                        vec![(' ', 0), (' ', 0), ('⚙', ST_DIM), (' ', 0)]
                    } else {
                        vec![(' ', 0); 4]
                    };
                    line.extend(l);
                    cells.push(line);
                }
            }
            _ => {
                for l in markdown_lines(&t.text, width, 0) {
                    cells.push(l);
                }
            }
        }
    }
    // Highlight, remember the matching lines, emit.
    let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
    for mut line in cells {
        if mark_hits(&mut line, &lower_terms) {
            tv.matches.push(tv.lines.len());
        }
        tv.lines.push(line);
    }
    tv.top = match (tv.matches.first(), open_at) {
        (Some(&m), _) if !terms.is_empty() => {
            tv.match_idx = Some(0);
            m.saturating_sub(2)
        }
        (_, Some(at)) => at,
        _ => usize::MAX, // clamped to the end when drawn
    };
}

/// Plain text as cells, one style throughout.
fn plain_cells(text: &str, style: u8) -> Vec<Styled> {
    text.chars().filter(|c| !c.is_control() || *c == '\n').map(|c| (c, style)).collect()
}

/// A block of Markdown as wrapped, styled lines. Handles what agents
/// actually write: `#` headings, `**bold**`, `*italic*`, `` `code` ``,
/// fenced code blocks, `-`/`*`/`1.` lists, `>` quotes, `---` rules and
/// `[text](url)` links (the text, underlined). `base` is OR'd into every
/// cell (a prompt is bold throughout).
fn markdown_lines(text: &str, width: usize, base: u8) -> Vec<Vec<Styled>> {
    let width = width.max(4);
    let mut out: Vec<Vec<Styled>> = Vec::new();
    let mut in_fence = false;
    let src: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < src.len() {
        let raw = src[i];
        i += 1;
        let line = raw.trim_end();
        // A pipe table: a header row, a separator row of dashes (with
        // optional colons for alignment), then body rows, all with `|`.
        if !in_fence && line.contains('|') && i < src.len() && is_table_separator(src[i]) {
            let sep = src[i];
            i += 1; // the separator
            let mut rows: Vec<&str> = vec![line];
            while i < src.len() && src[i].contains('|') && !src[i].trim().is_empty() {
                rows.push(src[i].trim_end());
                i += 1;
            }
            out.extend(table_lines(&rows, sep, width, base));
            continue;
        }
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            in_fence = !in_fence;
            let lang = rest.trim();
            let mut l: Vec<Styled> = vec![(if in_fence { '┌' } else { '└' }, ST_DIM | base), ('─', ST_DIM | base)];
            if in_fence && !lang.is_empty() {
                l.push((' ', 0));
                l.extend(lang.chars().map(|c| (c, ST_DIM | base)));
            }
            out.push(l);
            continue;
        }
        if in_fence {
            // Code: no inline markup, hard-wrapped, a bar down the side.
            let body: Vec<Styled> = line.chars().filter(|c| !c.is_control()).map(|c| (c, ST_CODE | base)).collect();
            let mut first = true;
            for piece in hard_wrap(&body, width.saturating_sub(2)) {
                let mut l: Vec<Styled> = vec![('│', ST_DIM | base), (' ', 0)];
                if !first {
                    l[0] = (' ', 0);
                }
                first = false;
                l.extend(piece);
                out.push(l);
            }
            if body.is_empty() {
                out.push(vec![('│', ST_DIM | base)]);
            }
            continue;
        }
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if trimmed.is_empty() {
            out.push(Vec::new());
            continue;
        }
        // Horizontal rule.
        if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') {
            out.push(std::iter::repeat(('─', ST_DIM | base)).take(width.min(40)).collect());
            continue;
        }
        // Heading.
        if let Some(rest) = trimmed.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|&c| c == '#').count();
            let body = rest.trim_start_matches('#');
            if body.starts_with(' ') && level <= 6 {
                let style = base | ST_BOLD | if level == 1 { ST_UNDER } else { 0 };
                let cells = inline_cells(body.trim(), style);
                out.extend(wrap_cells(&cells, width));
                continue;
            }
        }
        // Quote.
        if let Some(rest) = trimmed.strip_prefix('>') {
            let cells = inline_cells(rest.trim_start(), base | ST_DIM);
            for piece in wrap_cells(&cells, width.saturating_sub(2)) {
                let mut l: Vec<Styled> = vec![('▎', ST_DIM | base), (' ', 0)];
                l.extend(piece);
                out.push(l);
            }
            continue;
        }
        // List item: a bullet, or a number.
        let (lead, rest): (String, &str) = if let Some(r) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            (format!("{}• ", " ".repeat(indent.min(8))), r)
        } else if let Some(pos) = trimmed.find(". ").filter(|&pos| pos > 0 && pos <= 3 && trimmed[..pos].bytes().all(|b| b.is_ascii_digit())) {
            (format!("{}{} ", " ".repeat(indent.min(8)), &trimmed[..pos + 1]), &trimmed[pos + 2..])
        } else {
            (String::new(), trimmed)
        };
        let cells = inline_cells(rest, base);
        let hang = lead.chars().count();
        for (i, piece) in wrap_cells(&cells, width.saturating_sub(hang)).into_iter().enumerate() {
            let mut l: Vec<Styled> = if i == 0 {
                lead.chars().map(|c| (c, base)).collect()
            } else {
                std::iter::repeat((' ', 0)).take(hang).collect()
            };
            l.extend(piece);
            out.push(l);
        }
    }
    out
}

/// A pipe table's separator row: cells of dashes, each with an optional
/// colon at either end, between pipes.
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('-') || !t.contains('|') {
        return false;
    }
    split_row(t).into_iter().all(|c| {
        let c = c.trim();
        let body = c.trim_start_matches(':').trim_end_matches(':');
        !body.is_empty() && body.chars().all(|ch| ch == '-')
    })
}

/// The cells of a table row: split on `|` (an escaped `\|` stays), with
/// the outer pipes and surrounding spaces dropped.
fn split_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            cur.push('|');
            chars.next();
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A pipe table as lines: the header row bold, a rule under it, the body
/// rows, columns padded to the widest cell and aligned as the separator
/// says. Wider than the preview, the widest columns give way first and
/// their cells are cut with an ellipsis; a table never wraps.
fn table_lines(rows: &[&str], sep: &str, width: usize, base: u8) -> Vec<Vec<Styled>> {
    let aligns: Vec<Align> = split_row(sep)
        .iter()
        .map(|c| {
            let c = c.trim();
            match (c.starts_with(':'), c.ends_with(':')) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect();
    let ncols = aligns.len().max(1);
    let cells: Vec<Vec<Vec<Styled>>> = rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let style = if r == 0 { base | ST_BOLD } else { base };
            let mut cs: Vec<Vec<Styled>> = split_row(row).iter().map(|c| inline_cells(c, style)).collect();
            cs.resize(ncols, Vec::new());
            cs
        })
        .collect();
    let mut widths: Vec<usize> = (0..ncols)
        .map(|c| cells.iter().map(|r| r[c].len()).max().unwrap_or(0).max(1))
        .collect();
    // Fit: 3 cells between columns (" │ "), none outside.
    let fits = |w: &[usize]| w.iter().sum::<usize>() + 3 * (ncols - 1);
    while fits(&widths) > width {
        let (widest, _) = widths.iter().enumerate().max_by_key(|(_, w)| **w).unwrap();
        if widths[widest] <= 3 {
            break;
        }
        widths[widest] -= 1;
    }
    let mut out: Vec<Vec<Styled>> = Vec::new();
    let pad = |cell: &[Styled], w: usize, align: Align| -> Vec<Styled> {
        let mut c: Vec<Styled> = cell.to_vec();
        if c.len() > w {
            c.truncate(w.saturating_sub(1));
            c.push(('…', cell.last().map(|s| s.1).unwrap_or(0)));
        }
        let gap = w.saturating_sub(c.len());
        let (left, right) = match align {
            Align::Left => (0, gap),
            Align::Right => (gap, 0),
            Align::Center => (gap / 2, gap - gap / 2),
        };
        let mut line: Vec<Styled> = std::iter::repeat((' ', 0)).take(left).collect();
        line.extend(c);
        line.extend(std::iter::repeat((' ', 0)).take(right));
        line
    };
    for (r, row) in cells.iter().enumerate() {
        let mut line: Vec<Styled> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            if c > 0 {
                line.extend([(' ', 0), ('│', base | ST_DIM), (' ', 0)]);
            }
            line.extend(pad(cell, widths[c], aligns.get(c).copied().unwrap_or(Align::Left)));
        }
        out.push(line);
        if r == 0 {
            let mut rule: Vec<Styled> = Vec::new();
            for (c, w) in widths.iter().enumerate() {
                if c > 0 {
                    rule.extend([('─', base | ST_DIM), ('┼', base | ST_DIM), ('─', base | ST_DIM)]);
                }
                rule.extend(std::iter::repeat(('─', base | ST_DIM)).take(*w));
            }
            out.push(rule);
        }
    }
    out
}

/// Inline Markdown to cells: `**bold**`, `*italic*` / `_italic_`,
/// `` `code` ``, `[text](url)`. Unmatched markers stay as text.
fn inline_cells(text: &str, base: u8) -> Vec<Styled> {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
    let mut out: Vec<Styled> = Vec::with_capacity(chars.len());
    let mut i = 0;
    let n = chars.len();
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..n.saturating_sub(pat.len() - 1)).find(|&k| chars[k..k + pat.len()] == *pat)
    };
    while i < n {
        let c = chars[i];
        // Inline code: up to the next backtick.
        if c == '`' {
            if let Some(end) = find(i + 1, &['`']) {
                if end > i + 1 {
                    out.extend(chars[i + 1..end].iter().map(|&ch| (ch, base | ST_CODE)));
                    i = end + 1;
                    continue;
                }
            }
        }
        // Bold.
        if c == '*' && i + 1 < n && chars[i + 1] == '*' {
            if let Some(end) = find(i + 2, &['*', '*']) {
                if end > i + 2 {
                    out.extend(inline_cells(&chars[i + 2..end].iter().collect::<String>(), base | ST_BOLD));
                    i = end + 2;
                    continue;
                }
            }
        }
        // Italic: a single marker with a word right after it and a
        // matching one before a non-word, so `2 * 3 * 4` stays as is.
        if (c == '*' || c == '_') && i + 1 < n && !chars[i + 1].is_whitespace() && chars[i + 1] != c {
            if let Some(end) = (i + 2..n).find(|&k| chars[k] == c && !chars[k - 1].is_whitespace()) {
                let after_ok = end + 1 >= n || !chars[end + 1].is_alphanumeric();
                let before_ok = i == 0 || !chars[i - 1].is_alphanumeric() || c == '*';
                if after_ok && before_ok {
                    out.extend(inline_cells(&chars[i + 1..end].iter().collect::<String>(), base | ST_ITALIC));
                    i = end + 1;
                    continue;
                }
            }
        }
        // Link: [text](url) -> text, underlined.
        if c == '[' {
            if let Some(close) = find(i + 1, &[']', '(']) {
                if let Some(end) = find(close + 2, &[')']) {
                    out.extend(inline_cells(&chars[i + 1..close].iter().collect::<String>(), base | ST_UNDER));
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push((c, base));
        i += 1;
    }
    out
}

/// Greedy word wrap over cells; a word wider than the line is split.
fn wrap_cells(cells: &[Styled], width: usize) -> Vec<Vec<Styled>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Styled>> = Vec::new();
    let mut line: Vec<Styled> = Vec::new();
    let mut word: Vec<Styled> = Vec::new();
    let flush_word = |line: &mut Vec<Styled>, word: &mut Vec<Styled>, out: &mut Vec<Vec<Styled>>| {
        if word.is_empty() {
            return;
        }
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            out.push(std::mem::take(line));
        }
        if word.len() > width {
            for piece in hard_wrap(word, width) {
                if !line.is_empty() {
                    out.push(std::mem::take(line));
                }
                *line = piece;
            }
            word.clear();
            return;
        }
        if !line.is_empty() {
            line.push((' ', 0));
        }
        line.append(word);
    };
    for &cell in cells {
        if cell.0 == ' ' {
            flush_word(&mut line, &mut word, &mut out);
        } else {
            word.push(cell);
        }
    }
    flush_word(&mut line, &mut word, &mut out);
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

fn hard_wrap(cells: &[Styled], width: usize) -> Vec<Vec<Styled>> {
    let width = width.max(1);
    if cells.is_empty() {
        return vec![Vec::new()];
    }
    cells.chunks(width).map(|c| c.to_vec()).collect()
}

/// Mark every occurrence of a term in the line (case-insensitively, on
/// the visible text) with the hit bit. Returns whether any was marked.
fn mark_hits(line: &mut [Styled], lower_terms: &[String]) -> bool {
    if lower_terms.is_empty() || line.is_empty() {
        return false;
    }
    let lower: Vec<char> = line.iter().map(|(c, _)| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut any = false;
    for t in lower_terms {
        let tc: Vec<char> = t.chars().collect();
        if tc.is_empty() || tc.len() > lower.len() {
            continue;
        }
        let mut i = 0;
        while i + tc.len() <= lower.len() {
            if lower[i..i + tc.len()] == tc[..] {
                for cell in &mut line[i..i + tc.len()] {
                    cell.1 |= ST_HIT;
                }
                any = true;
                i += tc.len();
            } else {
                i += 1;
            }
        }
    }
    any
}

/// Cells to a terminal line: one SGR per run of equal style. A match
/// is black on yellow, as copy mode's `mode-style` draws one; on the
/// line the cursor is on (`current`) it is bold on bright yellow, so
/// `n`/`N` show where they landed.
fn emit_cells(line: &[Styled], current: bool) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut cur: Option<u8> = None;
    for &(c, st) in line {
        if cur != Some(st) {
            out.push_str("\x1b[0");
            if st & ST_BOLD != 0 {
                out.push_str(";1");
            }
            if st & ST_DIM != 0 && st & ST_HIT == 0 {
                out.push_str(";2");
            }
            if st & ST_ITALIC != 0 {
                out.push_str(";3");
            }
            if st & ST_UNDER != 0 {
                out.push_str(";4");
            }
            if st & ST_HIT != 0 {
                out.push_str(if current { ";1;30;103" } else { ";30;43" });
            } else if st & ST_CODE != 0 {
                out.push_str(";33");
            } else if st & ST_CYAN != 0 {
                out.push_str(";36");
            }
            out.push('m');
            cur = Some(st);
        }
        out.push(c);
    }
    if cur.is_some() {
        out.push_str("\x1b[0m");
    }
    out
}

/// The live pane of the highlighted row (local, or a mirror of a remote
/// one), shown to the right of the list.
fn preview_rect(p: &Picker, list_w: usize) -> Option<PreviewRect> {
    if p.show_info || p.show_help {
        return None;
    }
    let a = p.selected()?;
    let pane = p.local_pane_of(a)?;
    // Tab: the conversation instead of the pane, once it has arrived (a
    // blank preview while it loads would be worse than the pane).
    if p.show_transcript && transcript_shown(p) {
        return None;
    }
    let x = (list_w + 1) as u32;
    let w = (p.width as usize).saturating_sub(list_w + 1) as u32;
    let h = p.height.saturating_sub(1);
    if w == 0 || h == 0 {
        return None;
    }
    Some(PreviewRect { pane: PaneId(pane), x, y: 0, w, h })
}

/// The captured text for the highlighted remote row, when the provider
/// answered for it.
fn remote_preview_lines(p: &Picker) -> Option<&Vec<String>> {
    let a = p.selected()?;
    if a.is_local() {
        return None;
    }
    let (key, lines) = p.remote_capture.as_ref()?;
    if *key != a.key() {
        return None;
    }
    Some(lines)
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

#[cfg(test)]
mod query_tests {
    use super::*;

    #[test]
    fn tokens_and_words() {
        let q = parse_query("dflash @dm #tmux2 ~cfb bench");
        assert_eq!(q.words, "dflash bench");
        assert_eq!(q.servers, vec!["dm"]);
        assert_eq!(q.sessions, vec!["tmux2"]);
        assert_eq!(q.dirs, vec!["cfb"]);
        assert_eq!(q.tokens(), "@dm #tmux2 ~cfb");
        assert!(q.has_filters());
    }

    #[test]
    fn bare_sigil_and_escape() {
        let q = parse_query("@ foo");
        assert_eq!(q.words, "foo");
        assert!(!q.has_filters());
        let q = parse_query("\\@alpha \\x");
        assert_eq!(q.words, "@alpha \\x");
        assert!(!q.has_filters());
    }

    #[test]
    fn ids_found_bottom_first_once() {
        let text = "Message from claude:6a91e2e5-a91c-4084-83c4-cfc84a1e285d via\n\
                    see codex:0123abcd-ef and oldclaude:deadbeef00 and pi:ab\n\
                    again claude:6a91e2e5-a91c-4084-83c4-cfc84a1e285d, then opencode:00ff00ff00";
        let ids = ids_on_screen(text);
        assert_eq!(
            ids,
            vec![
                "opencode:00ff00ff00",
                "claude:6a91e2e5-a91c-4084-83c4-cfc84a1e285d",
                "codex:0123abcd-ef",
            ]
        );
    }

    #[test]
    fn typing() {
        assert_eq!(typing_token("foo #al"), Some(('#', "al")));
        assert_eq!(typing_token("foo #al "), None);
        assert_eq!(typing_token("foo \\#al"), None);
        assert_eq!(typing_token("@"), Some(('@', "")));
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn text(cells: &[Styled]) -> String {
        cells.iter().map(|c| c.0).collect()
    }

    #[test]
    fn inline_markup() {
        let c = inline_cells("say **hi** and *there* with `code` [link](http://x)", 0);
        assert_eq!(text(&c), "say hi and there with code link");
        let bold: String = c.iter().filter(|c| c.1 & ST_BOLD != 0).map(|c| c.0).collect();
        assert_eq!(bold, "hi");
        let italic: String = c.iter().filter(|c| c.1 & ST_ITALIC != 0).map(|c| c.0).collect();
        assert_eq!(italic, "there");
        let code: String = c.iter().filter(|c| c.1 & ST_CODE != 0).map(|c| c.0).collect();
        assert_eq!(code, "code");
        let under: String = c.iter().filter(|c| c.1 & ST_UNDER != 0).map(|c| c.0).collect();
        assert_eq!(under, "link");
        // Arithmetic is not emphasis; an unmatched marker stays.
        assert_eq!(text(&inline_cells("2 * 3 * 4 and a*b", 0)), "2 * 3 * 4 and a*b");
        assert_eq!(text(&inline_cells("lone ` tick", 0)), "lone ` tick");
    }

    #[test]
    fn blocks() {
        let md = "# Title\n\n- one\n- two **b**\n\n```rust\nfn main() {}\n```\n\n> quoted\n\n1. first\n---\nplain para";
        let lines: Vec<String> = markdown_lines(md, 40, 0).iter().map(|l| text(l)).collect();
        assert_eq!(lines[0], "Title");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], "• one");
        assert_eq!(lines[3], "• two b");
        assert_eq!(lines[5], "┌─ rust");
        assert_eq!(lines[6], "│ fn main() {}");
        assert_eq!(lines[7], "└─");
        assert_eq!(lines[9], "▎ quoted");
        assert_eq!(lines[11], "1. first");
        assert!(lines[12].starts_with("────"));
        assert_eq!(lines[13], "plain para");
        let title = &markdown_lines(md, 40, 0)[0];
        assert!(title.iter().all(|c| c.1 & ST_BOLD != 0 && c.1 & ST_UNDER != 0));
    }

    #[test]
    fn tables() {
        let md = "| k | AL | note |\n|---:|:---:|------|\n| 4 | 3.1 | the **baseline** |\n| 16 | 3.9 | wide |";
        let lines: Vec<String> = markdown_lines(md, 60, 0).iter().map(|l| text(l)).collect();
        assert_eq!(lines[0], " k │ AL  │ note        ");
        assert_eq!(lines[1], "───┼─────┼─────────────");
        assert_eq!(lines[2], " 4 │ 3.1 │ the baseline");
        assert_eq!(lines[3], "16 │ 3.9 │ wide        ");
        // The header is bold; a wide table gives up width in its widest column.
        assert!(markdown_lines(md, 60, 0)[0].iter().filter(|c| c.0 != ' ' && c.0 != '│').all(|c| c.1 & ST_BOLD != 0));
        let narrow: Vec<String> = markdown_lines(md, 16, 0).iter().map(|l| text(l)).collect();
        assert!(narrow.iter().all(|l| l.chars().count() <= 16), "{narrow:?}");
        assert!(narrow[2].ends_with('…'));
        // Not a table without a separator row.
        let plain = markdown_lines("a | b\nc | d", 60, 0);
        assert_eq!(text(&plain[0]), "a | b");
    }

    #[test]
    fn wrapping_and_hits() {
        let cells = inline_cells("alpha beta gamma delta", 0);
        let lines = wrap_cells(&cells, 11);
        let t: Vec<String> = lines.iter().map(|l| text(l)).collect();
        assert_eq!(t, vec!["alpha beta", "gamma delta"]);
        let long = inline_cells("abcdefghijkl", 0);
        assert_eq!(wrap_cells(&long, 5).len(), 3);
        let mut line = inline_cells("The DFlash2 bench and dflash2 again", 0);
        assert!(mark_hits(&mut line, &["dflash2".to_string()]));
        let hit: String = line.iter().filter(|c| c.1 & ST_HIT != 0).map(|c| c.0).collect();
        assert_eq!(hit, "DFlash2dflash2");
        assert!(!mark_hits(&mut line, &["zzz".to_string()]));
        // Matches are black on yellow; on the current line, bold on bright yellow.
        let out = emit_cells(&line, false);
        assert!(out.contains("\x1b[0;30;43m") && out.ends_with("\x1b[0m"));
        assert!(emit_cells(&line, true).contains("\x1b[0;1;30;103m"));
        assert_eq!(strip_sgr(&out), "The DFlash2 bench and dflash2 again");
    }
}
