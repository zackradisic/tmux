//! The view half: the picker. It merges the local provider's roster (read
//! straight from this server's store) with the rosters of the providers on
//! linked servers (fetched through services and kept in [`Remotes`]), and
//! shows them grouped by server, then by state band.
//!
//! The list itself - cursor, marks, the search box and its dropdown, the
//! frame, the preview column - is `listkit::Engine`. The rows are handed
//! to it as nodes on every refresh (a server and a band are groups, an
//! agent is an item), and every key the engine does not own comes back
//! as an `Outcome` that this module turns into an agent action. What is
//! agent-specific sits on top: the bands, the unread badges, the
//! conversation in the preview with its own focus and matches, the info
//! card, the rename and message prompts, the content search.
//!
//! Rows carry their server; ids are unique per server only, so marks and
//! ranks key on [`Agent::key`]. Ages use each provider's clock: the
//! snapshot's `now_ms` gives the skew to correct by. A remote row's pane
//! is a pane on the other machine: Enter jumps to its local shadow when a
//! `remote-attach` link mirrors it, and the preview shows the shadow grid
//! live, else the provider's captured text.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use listkit::engine::Outcome;
use listkit::keys::KeyTable;
use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::remotes::{spin_since, FETCH_STALE_MS, FETCH_STUCK_MS, SPIN_FRAMES, SPIN_GRACE_MS, SPIN_MS};
use listkit::styled::*;
use listkit::text::{clip, fmt_age, keyname, menu_item, menu_safe, one_line, pretty_key, strip_sgr, tilde_of};
use listkit::{Engine, Node, Preview, SigilSpec};
use tmux_plugin_sdk::prelude::*;

use crate::index;
use crate::provider::{self, mode_label, run_content_search, ListReq, Snapshot, StatsReq, TurnsReq};
use crate::store::{self, Agent, Stats, TurnRow, LOCAL};
use crate::transcript::{self, TranscriptHit};
use crate::{Config, PickKeys, HISTORY_MAX};

/// The picker opens at a fraction of the window, clamped to this box. A
/// manual resize (+/-) is remembered and overrides the default.
const SIZE: SizeBox = SizeBox { min_w: 72, min_h: 16, max_w: 180, max_h: 54, fill_tenths: 9 };
/// While the picker stays open, re-read the harness session files (and
/// the remote rosters) on this cadence.
const REFRESH_MS: u64 = 2000;
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

/// Prompt tags: what Enter in the search line means.
const TAG_RENAME: u32 = 1;
const TAG_MESSAGE: u32 = 2;

// ---------------------------------------------------------------------------
// remote rosters
// ---------------------------------------------------------------------------

/// The remote rosters: what each linked server's provider last reported,
/// as [`listkit::remotes::Remotes`] over agent rows.
pub type Remotes = listkit::remotes::Remotes<Agent>;

/// Take a provider's snapshot for a server. Its rows carry the server
/// name from here on: ids are unique per server only.
pub fn apply_snapshot(remotes: &mut Remotes, server: &str, snap: Snapshot) {
    let rows = snap
        .agents
        .into_iter()
        .map(|mut a| {
            a.server = server.to_string();
            a
        })
        .collect();
    remotes.apply(server, rows, snap.now_ms);
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
    let queue = remotes.borrow_mut().claim(&list, min_age_ms, |s| {
        format!("agents {} there, {} here; run tmux update", s.version, mine)
    });
    // Even with nothing to start, a fetch from another task may still be
    // outstanding and want a spinner; the task exits on its own when
    // none is.
    start_spinner(&picker, &remotes);
    if queue.is_empty() {
        return;
    }
    let landed_picker = Rc::clone(&picker);
    let landed_remotes = Rc::clone(&remotes);
    listkit::remotes::fetch_all(
        Rc::clone(&remotes),
        queue,
        MAX_INFLIGHT,
        move |server| {
            Box::pin(async move {
                let target = format!("@{server}");
                let snap = service::call_json::<_, Snapshot>(&target, "list", &req).await?;
                let rows = snap
                    .agents
                    .into_iter()
                    .map(|mut a| {
                        a.server = server.clone();
                        a
                    })
                    .collect();
                Ok((rows, snap.now_ms))
            })
        },
        move |_server| {
            let picker = Rc::clone(&landed_picker);
            let remotes = Rc::clone(&landed_remotes);
            Box::pin(async move { refresh_if_open(&picker, &remotes).await })
        },
    )
    .await;
}

/// Turn the per-server spinner while any fetch is outstanding. Only the
/// frame and the server headers move, so each tick rebuilds the rows
/// already in hand: no DB read, no file scan, and no new search.
fn start_spinner(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    if picker.borrow().is_none() {
        return;
    }
    let picker = Rc::clone(picker);
    listkit::remotes::start_spinner(remotes, move |fetching, now| {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return false };
        p.now_ms = now;
        p.fetching = fetching;
        p.multi = is_multi(&p.rows, &p.fetching, now);
        let keep = p.keep();
        pick_reshow(p, keep, false);
        pick_render(p);
        true
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
    listkit::any_client()
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
    /// Paste this text into the local pane the preview shows.
    Paste(u32, String),
    /// Open the new-agent form over the picker, prefilled from the
    /// highlighted row (see `newagent`); `true` opens it as a fork.
    NewAgent(bool),
    /// Mark these (server, id) rows read (true) or unread (false).
    Read(Vec<(String, String)>, bool),
    /// Close, and open the sessions chooser on this local pane (the
    /// highlighted row's, when it has one here).
    Sessions(Option<u32>),
}

pub struct Picker {
    /// The list: cursor, marks, search box, frame, preview column.
    pub engine: Engine,
    pub rows: Vec<Agent>,
    /// Row key -> index in `rows`, rebuilt with the nodes.
    by_key: HashMap<String, usize>,
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
    /// The new-agent form's launchers (name, shell line): the configured
    /// ones, then the detected harness commands, see `Config::launchers`.
    pub launchers: Vec<(String, String)>,
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

/// The filter tokens the search box takes: `@server`, `#session`, `~dir`
/// (the directory by substring of its `~` form), and the keys that put
/// the highlighted row's own value in the box.
fn sigils() -> Vec<SigilSpec> {
    vec![
        SigilSpec::new('@', "server", false, Some("S"), "narrow to a server (prefix)"),
        SigilSpec::new('#', "session", false, Some("s"), "narrow to a session (prefix)"),
        SigilSpec::new('~', "dir", true, Some("d"), "narrow to a directory (part of its ~ path)"),
    ]
}

/// The key table, from the configured keys. The engine looks up
/// `activate`, `focus`, `unfocus`, `filter` and `close` itself; the rest
/// come back as actions this module runs.
fn key_table(k: &PickKeys) -> KeyTable {
    let mut t = KeyTable::new()
        .with("activate", "Enter", "moving", "jump to the pane")
        .with("transcript", "Tab", "moving", "conversation / pane in the preview")
        .with("focus", "l", "moving", "type into the pane; keys go there (a click too)")
        .with("unfocus", "C-]", "moving", "take the keyboard back")
        .note("moving", "j/k ↑/↓", "move the cursor")
        .note("moving", "gg / G", "first / last row")
        .note("moving", "J / K", "mark the row and move")
        .note("moving", "wheel", "over the preview: scrolls the pane itself")
        .with("filter", "/", "search box", "focus the box; words match names, tasks, conversations")
        .with("content", "C-f", "search box", "also grep the panes' contents")
        .note("search box", "dropdown", "a sigil opens it: Tab/↓ BTab/↑ walk, Enter takes, Esc hides")
        .note("search box", "C-u", "clear the box")
        .note("search box", "Esc Enter", "leave the box, keep the query")
        .with("archive", "a", "rows", "archive / un-archive")
        .with("archived", "A", "rows", "the archive view")
        .with("history", ".", "rows", "show finished agents too")
        .with("attention", "w", "rows", "move between attention and waiting")
        .with("read", "r", "rows", "mark read")
        .with("unread", "u", "rows", "mark unread")
        .with("rename", "R", "rows", "rename")
        .with("copy", "c", "rows", "copy the agent id")
        .with("message", "m", "rows", "message the agent")
        .with("interrupt", "x", "rows", "interrupt (C-c) the agent")
        .with("kill", "X", "rows", "kill its pane (asks)")
        .with("new", "n", "rows", "new agent, prefilled from the row")
        .with("fork", "f", "rows", "fork this agent (a copy of its session, its own name)")
        .with("menu", "Space", "rows", "the action menu")
        .with("info", "i", "rows", "info card (y copies its cwd)")
        .with("copy_cwd", "y", "rows", "copy the info card's directory")
        .with("sessions", "t", "rows", "the sessions chooser, on this row's pane")
        .note("conversation (Tab)", "j/k", "scroll")
        .note("conversation (Tab)", "n / N", "next / previous match")
        .note("conversation (Tab)", "g / G", "top / end")
        .note("conversation (Tab)", "Space / b", "page down / up")
        .note("conversation (Tab)", "Esc", "back to the list")
        .with("close", "Escape", "picker", "close (Esc first puts a card or the marks away)")
        .note("picker", "+ / -", "resize")
        .note("picker", "?", "this card");
    for (action, key) in [
        ("activate", &k.jump),
        ("filter", &k.filter),
        ("archive", &k.archive),
        ("attention", &k.attention),
        ("copy", &k.copy),
        ("menu", &k.menu),
        ("history", &k.history),
        ("archived", &k.archived),
        ("close", &k.close),
        ("content", &k.content),
        ("rename", &k.rename),
        ("interrupt", &k.interrupt),
        ("kill", &k.kill),
        ("focus", &k.focus),
        ("unfocus", &k.unfocus),
        ("new", &k.new),
        ("read", &k.read),
        ("unread", &k.unread),
        ("sessions", &k.sessions),
    ] {
        t.bind(action, Some(key));
    }
    t
}

impl Picker {
    fn key(&self, action: &str) -> &str {
        self.engine.keys.key_of(action)
    }

    fn status(&mut self, s: impl Into<String>) {
        self.engine.status = Some(s.into());
    }

    /// The preview cannot keep the keyboard without a pane to type into:
    /// the agent may have finished, or a refresh replaced the rows and
    /// the highlighted one is a remote row with no mirror here.
    fn sync_focus(&mut self) {
        if self.engine.preview_focus && live_pane_of_selection(self).is_none() {
            self.engine.preview_focus = false;
        }
    }

    /// What to ask the store and every provider for beyond the live rows.
    pub fn list_req(&self) -> ListReq {
        ListReq { history: self.show_history, archived: self.archived_only }
    }

    /// The highlighted row, as (key, server, pane): what a rebuild keeps
    /// the cursor on. The pane is the fallback for an id migration (prov
    /// -> durable), which changes the key but never the pane.
    fn keep(&self) -> Option<(String, String, Option<i64>)> {
        self.selected().map(|a| (a.key(), a.server.clone(), a.pane))
    }

    /// Put the cursor on the row for the pane the picker was opened from,
    /// if that row is on screen and the cursor has not been moved since
    /// open. Returns whether it landed.
    fn select_here(&mut self) -> bool {
        if !self.seek_here || (self.current_pane.is_none() && self.here_id.is_none()) {
            return false;
        }
        let key = self
            .engine
            .visible()
            .find(|n| self.by_key.get(&n.key).is_some_and(|&i| self.is_here(&self.rows[i])))
            .map(|n| n.key.clone());
        let Some(key) = key else { return false };
        self.engine.select_key(&key);
        self.seek_here = false;
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
        let key = self.engine.selected_key()?;
        self.rows.get(*self.by_key.get(&key)?)
    }

    fn selected_index(&self) -> Option<usize> {
        let key = self.engine.selected_key()?;
        self.by_key.get(&key).copied()
    }

    /// The rows a bulk key acts on: the marked ones, else the highlighted.
    fn targets(&self) -> Vec<&Agent> {
        self.engine
            .targets()
            .iter()
            .filter_map(|k| self.by_key.get(k))
            .filter_map(|&i| self.rows.get(i))
            .collect()
    }

    /// The local pane a row's pane shows in: the pane itself for a local
    /// row, the shadow pane for a mirrored remote one.
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

    /// The preview column's width.
    fn preview_w(&self) -> usize {
        (self.engine.width as usize).saturating_sub(self.engine.list_w() + 2)
    }
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
    Gathered {
        rows,
        captures,
        skew: r.skews(),
        down: r.downs(),
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
    let window = listkit::window_for(client, here);
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
        .unwrap_or((SIZE.max_w, SIZE.max_h));
    let (mut width, mut height) = default_size(ww, wh, &SIZE);
    if let Ok(Some(v)) = store::get_setting("pick_w").await {
        if let Ok(n) = v.parse::<u32>() {
            width = clamp_dim(n, SIZE.min_w, ww);
        }
    }
    if let Ok(Some(v)) = store::get_setting("pick_h").await {
        if let Ok(n) = v.parse::<u32>() {
            height = clamp_dim(n, SIZE.min_h, wh);
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
    let mut engine = Engine::new(mode, width, height, "agents", key_table(&cfg.keys), sigils());
    engine.size = SIZE;
    let mut p = Picker {
        engine,
        rows,
        by_key: HashMap::new(),
        content_search: false,
        content_hits: HashMap::new(),
        captures,
        content_mode: SearchMode::Plain,
        content_query: String::new(),
        now_ms: now_ms(),
        show_history: req.history,
        archived_only,
        history_before_archive: false,
        launchers: cfg.launchers.clone(),
        pending_kill: None,
        order,
        order_next,
        timer: None,
        current_pane: here,
        here_id,
        home: home_dir().ok().filter(|h| !h.is_empty()),
        seek_here: here.is_some() || seek_id.is_some(),
        skew,
        down,
        mismatch,
        unread: HashMap::new(),
        fetching,
        mirrors: if multi { find_mirrors() } else { HashMap::new() },
        remote_capture: None,
        multi,
        transcript_query: String::new(),
        transcript_hits: HashMap::new(),
        hit_rows: Vec::new(),
        roster_len: 0,
        transcript: None,
        show_transcript: false,
        transcript_focus: false,
        show_info: false,
        info: None,
    };
    if let Some(id) = missing_id {
        p.status(format!("no agent {id} on any server"));
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
        // before we swap them in, so the highlight follows the agent.
        let keep = p.keep();
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
                Some(p) if p.engine.mode.0 == mode.0 => (true, p.list_req()),
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

/// Whatever the highlighted row needs in the preview that is not a live
/// blit: its conversation, a remote provider's captured text, the info
/// card's fetched half.
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
        (key, a.server.clone(), a.id.clone(), a.is_local(), local_pane, a.cwd.clone(), p.engine.mode)
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
        if p.engine.mode.0 != mode.0 {
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
        (key, a.server.clone(), a.id.clone(), a.is_local(), open_seq, p.engine.mode)
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
        if p.engine.mode.0 != mode.0 || p.selected().map(|a| a.key()) != Some(key.clone()) {
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

/// A remote row without a local mirror shows the provider's captured text
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
        (key, a.server.clone(), a.id.clone(), p.engine.mode)
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
        if p.engine.mode.0 != mode.0 {
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
            p.status(if ids.len() == 1 {
                verb.to_string()
            } else {
                format!("{} {verb}", ids.len())
            });
            // The bulk action consumed the selection.
            p.engine.clear_marks();
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
            p.status(match (done, refused) {
                (1, None) => verb.to_string(),
                (n, None) => format!("{n} {verb}"),
                (0, Some(why)) => why,
                (n, Some(why)) => format!("{n} {verb}; {why}"),
            });
            p.engine.clear_marks();
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
            p.status(match (moved, refused) {
                (1, None) => verb.to_string(),
                (n, None) => format!("{n} {verb}"),
                (0, Some(why)) => why,
                (n, Some(why)) => format!("{n} {verb}; {why}"),
            });
            p.engine.clear_marks();
        }
    }
    let req = list_req_of(&picker);
    fetch_remotes_now(Rc::clone(&picker), Rc::clone(&remotes), req).await;
    reload_picker(picker, remotes, false).await;
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
            let k = |action: &str| p.key(action).to_string();
            let live = live_pane_of_selection(p).is_some();
            let flagged = a.status == "needs_input";
            let movable =
                a.live() && matches!(a.status.as_str(), "needs_input" | "waiting");
            let mut items = String::new();
            items.push_str(&menu_item("agents", "jump to pane", &k("activate"), live));
            items.push_str(&menu_item("agents", "type into pane", &k("focus"), live));
            items.push_str(&menu_item("agents", "new agent here", &k("new"), true));
            items.push_str(&menu_item("agents", "fork this agent", &k("fork"), true));
            items.push_str(&menu_item("agents", "message", &k("message"), true));
            let stopped =
                a.live() && matches!(a.status.as_str(), "needs_input" | "waiting");
            items.push_str(&menu_item("agents", "mark read", &k("read"), stopped && a.unread()));
            items.push_str(&menu_item("agents", "mark unread", &k("unread"), stopped && !a.unread()));
            items.push_str(&menu_item("agents", "sessions chooser here", &k("sessions"), true));
            items.push_str(&menu_item("agents", "copy id", &k("copy"), durable_id(a).is_some()));
            items.push_str(&menu_item("agents", "info", &k("info"), true));
            items.push_str(&menu_item("agents", "help", "?", true));
            items.push_str(&menu_item("agents", "rename", &k("rename"), true));
            items.push_str(" ''");
            let band = if flagged { "move to waiting" } else { "flag: needs input" };
            items.push_str(&menu_item("agents", band, &k("attention"), movable));
            let arch = if a.life == "archived" { "un-archive" } else { "archive" };
            items.push_str(&menu_item("agents", arch, &k("archive"), true));
            items.push_str(" ''");
            items.push_str(&menu_item("agents", "interrupt (C-c)", &k("interrupt"), live));
            items.push_str(&menu_item("agents", "kill pane", &k("kill"), live));
            // The one view toggle in a menu of row actions: a key that
            // is not in the footer has to be findable somewhere.
            items.push_str(" ''");
            let hist = if p.show_history {
                "hide finished (history)"
            } else {
                "show finished (history)"
            };
            items.push_str(&menu_item("agents", hist, &k("history"), true));
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
            p.status(format!("menu failed: {}", e.message));
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
        p.status(msg);
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
        if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
            return;
        }
        if busy.get() {
            return;
        }
        p.engine.status = None;
        match dir.as_str() {
            "left" => {
                p.engine.preview_focus = false;
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
                let typing = p.engine.preview_focus;
                p.engine.move_sel(if dir == "up" { -1 } else { 1 });
                p.seek_here = false;
                // The keyboard stays with the preview, which now shows
                // another agent - unless that row has no pane to type
                // into, in which case the focus drops and the footer says
                // so.
                p.sync_focus();
                let _ = typing;
                pick_render(p);
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
    let mode_id = picker.borrow().as_ref().map(|p| p.engine.mode.0 as i64);
    // No picker: the menu outlived it (its window went away, or someone
    // ran the command by hand). Nothing to act on.
    if mode_id.is_none() {
        return;
    }
    dispatch_key(picker, busy, remotes, ctx, mode_id, key, mouse, client);
}

/// Text pasted into the picker (a bracketed paste, or `paste-buffer` on
/// the float). While the preview has the keyboard it goes to the agent's
/// pane, as typed keys do; otherwise into the search box (or an open
/// prompt), which takes the focus, so a pasted agent id or filter token
/// lands where it works.
pub fn on_mode_paste(picker: &Rc<RefCell<Option<Picker>>>, event: &Event) {
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
        return;
    }
    let Some(text) = event.get_str("text") else { return };
    let text = text.to_string();
    if p.engine.preview_focus {
        if let Some(pane) = live_pane_of_selection(p) {
            let _ = send_text(PaneId(pane), &text);
        }
        return;
    }
    match p.engine.handle_paste(&text) {
        Outcome::FilterChanged => {
            pick_refilter(p);
            pick_render(p);
        }
        Outcome::Redraw => pick_render(p),
        _ => {}
    }
}

/// The agent ids written on a screen: `kind:hex-or-dash`, as the mailbox
/// prints them ("Message from claude:… via the tmux2 mailbox") and as
/// the picker copies them. Nearest the bottom first, each once.
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
/// or without a modifier prefix.
fn is_mouse_key(key: &str) -> bool {
    listkit::engine::is_mouse_key(key)
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
    // A row to acknowledge (mark read) after the borrow drops: the user
    // jumped to it or started typing into it.
    let mut ack: Option<(String, String)> = None;
    {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if mode_id != Some(p.engine.mode.0 as i64) {
            return;
        }
        if busy.get() {
            return;
        }
        p.engine.status = None;
        // Any key that is not the kill key again cancels a pending kill.
        let kill_pending = p.pending_kill.take();
        let is_down = matches!(key.as_str(), "Down" | "C-n" | "C-j");
        let is_up = matches!(key.as_str(), "Up" | "C-p" | "C-k");
        let outcome = if is_mouse_key(&key) {
            // The mouse means the same thing whatever has the keyboard: a
            // click lands where it lands. Without a cell (a mouse key
            // typed by name) there is nowhere for it to land.
            match mouse {
                None => Outcome::Nothing,
                Some((x, _y)) => {
                    let list_w = p.engine.list_w();
                    let base = key.rsplit('-').next().unwrap_or(&key).to_string();
                    let on_preview = x as usize > list_w;
                    if on_preview && transcript_drawn(p) {
                        // Over the conversation: a click takes the keyboard
                        // to it, the wheel scrolls it.
                        match base.as_str() {
                            "MouseDown1Pane" | "DoubleClick1Pane" => {
                                p.transcript_focus = true;
                                ensure_transcript_rendered(p);
                                Outcome::Redraw
                            }
                            "WheelUpPane" | "WheelDownPane" => {
                                ensure_transcript_rendered(p);
                                scroll_transcript(p, if base == "WheelUpPane" { -3 } else { 3 });
                                Outcome::Redraw
                            }
                            _ => Outcome::Nothing,
                        }
                    } else if on_preview && matches!(base.as_str(), "MouseDown1Pane" | "DoubleClick1Pane") && !p.show_info && !p.engine.show_help {
                        // A click on the pane: start typing into it.
                        ack = focus_preview(p);
                        Outcome::Redraw
                    } else {
                        let o = p.engine.handle_key(&key, mouse);
                        if x as usize <= list_w && matches!(o, Outcome::Redraw | Outcome::Activate(_)) {
                            // The user took the cursor: stop pulling it
                            // back to the here row, and the conversation
                            // loses the keyboard.
                            p.seek_here = false;
                            p.transcript_focus = false;
                            p.sync_focus();
                        }
                        o
                    }
                }
            }
        } else if p.transcript_focus {
            // The conversation has the keyboard: scroll it, step through
            // its matches, or hand the keyboard back.
            transcript_key(p, &key, is_up, is_down);
            Outcome::Nothing
        } else if p.engine.preview_focus || p.engine.prompt().is_some() || p.engine.filtering {
            p.engine.handle_key(&key, mouse)
        } else if key == p.key("focus") || key == "Right" {
            // Right, into the preview: the keyboard goes with it. To the
            // conversation when that is what the preview shows (a finished
            // agent, or Tab on a live one); else to the pane.
            if transcript_shown(p) {
                p.transcript_focus = true;
                ensure_transcript_rendered(p);
            } else {
                ack = focus_preview(p);
            }
            Outcome::Redraw
        } else if (key == p.key("close") || key == "q") && p.show_info && !p.engine.show_help {
            // Esc puts the info card away first.
            p.show_info = false;
            Outcome::Redraw
        } else {
            let o = p.engine.handle_key(&key, mouse);
            if matches!(o, Outcome::Redraw) && (is_up || is_down || matches!(key.as_str(), "j" | "k" | "g" | "G" | "J" | "K")) {
                p.seek_here = false;
            }
            o
        };
        match outcome {
            Outcome::Nothing => {}
            Outcome::Redraw | Outcome::Expanded(..) | Outcome::PreviewHeaderClick(..) => pick_render(p),
            Outcome::FilterChanged => {
                pick_refilter(p);
                pick_render(p);
            }
            Outcome::Close => after = PickAfter::Close(p.engine.mode),
            Outcome::Activate(_) => after = jump_after(p, &mut ack),
            Outcome::Resize(w, h) => {
                p.engine.resize(w, h);
                pick_render(p);
                after = PickAfter::Resize(p.engine.mode, w, h);
            }
            Outcome::PreviewKey(pane, k) => {
                after = match k.strip_prefix("\u{0}paste:") {
                    Some(text) => PickAfter::Paste(pane.0, text.to_string()),
                    None => PickAfter::Type(pane.0, k),
                };
            }
            Outcome::PreviewWheel(pane, k, x, y) => after = PickAfter::Wheel(pane.0, k, x, y),
            Outcome::Prompt(tag, text) => {
                if let Some(a) = p.selected() {
                    let (server, id) = (a.server.clone(), a.id.clone());
                    match tag {
                        TAG_RENAME => after = PickAfter::Rename(server, id, text),
                        TAG_MESSAGE if !text.is_empty() => after = PickAfter::Message(server, id, text),
                        _ => {}
                    }
                }
                pick_render(p);
            }
            Outcome::Key(k) => match k.as_str() {
                "[" | "]" if transcript_shown(p) => {
                    let step = (p.engine.height as usize).saturating_sub(2).max(2) as i64 / 2;
                    ensure_transcript_rendered(p);
                    scroll_transcript(p, if k == "[" { -step } else { step });
                    pick_render(p);
                }
                "WheelUpPane" | "WheelDownPane" if transcript_shown(p) => {
                    ensure_transcript_rendered(p);
                    scroll_transcript(p, if k == "WheelUpPane" { -3 } else { 3 });
                    pick_render(p);
                }
                _ => {}
            },
            Outcome::Action(name, _) => match name {
                "archive" => after = archive_after(p),
                "attention" => match attention_after(p) {
                    Ok(a) => after = a,
                    Err(why) => {
                        p.status(why);
                        pick_render(p);
                    }
                },
                "copy" => match copy_after(p) {
                    Ok(a) => after = a,
                    Err(why) => {
                        p.status(why);
                        pick_render(p);
                    }
                },
                "menu" => {
                    if p.selected().is_some() {
                        after = PickAfter::Menu;
                    }
                }
                "history" => {
                    p.show_history = !p.show_history;
                    // Leaving history leaves the archive view too: it
                    // lives there.
                    if !p.show_history {
                        p.archived_only = false;
                    }
                    after = PickAfter::Reload;
                }
                "archived" => {
                    // The archive as a list of its own. It is a history
                    // view, so entering it turns history on; leaving it
                    // puts history back the way it was before.
                    p.archived_only = !p.archived_only;
                    if p.archived_only {
                        p.history_before_archive = p.show_history;
                        p.show_history = true;
                    } else {
                        p.show_history = p.history_before_archive;
                    }
                    p.status(if p.archived_only { "archive only: on" } else { "archive only: off" });
                    after = PickAfter::Reload;
                }
                "content" => toggle_content(p),
                "rename" => {
                    if let Some(a) = p.selected() {
                        let init = a.user_name.clone().unwrap_or_default();
                        p.engine.open_prompt("rename", &init, TAG_RENAME);
                        pick_render(p);
                    }
                }
                "message" => {
                    // Message the selected agent: its mailbox holds it
                    // until the agent reads it, on this server or the
                    // agent's own.
                    if let Some(a) = p.selected() {
                        let to = clip(&display_name(a), 16);
                        p.engine.open_prompt(&format!("msg {to}"), "", TAG_MESSAGE);
                        pick_render(p);
                    }
                }
                "interrupt" => {
                    // Interrupt, do not kill: send C-c and let the agent
                    // decide what that means. Claude with work in flight
                    // answers with its own "are you sure?", and the live
                    // preview shows it, so the second press is an
                    // informed one rather than a guess.
                    match live_pane_of_selection(p) {
                        Some(pane) => {
                            after = PickAfter::Interrupt(pane);
                            p.status(format!("interrupt sent to %{pane}"));
                        }
                        None => {
                            let why = unreachable_reason(p);
                            p.status(why);
                        }
                    }
                    pick_render(p);
                }
                "kill" => {
                    // Killing the pane takes the agent's process and its
                    // scrollback with it, so it asks first. The second
                    // press must be on the same pane the first one named.
                    match live_pane_of_selection(p) {
                        Some(pane) => {
                            if kill_pending == Some(pane) {
                                after = PickAfter::KillPane(pane);
                                p.status(format!("killing %{pane}"));
                            } else {
                                p.pending_kill = Some(pane);
                                let k = pretty_key(p.key("kill"));
                                p.status(format!("kill %{pane}? {k} again to confirm"));
                            }
                        }
                        None => {
                            let why = unreachable_reason(p);
                            p.status(why);
                        }
                    }
                    pick_render(p);
                }
                "new" => {
                    // Start another agent: the form prefills from the row
                    // under the cursor, or from the pressing client's pane
                    // when the list is empty.
                    after = PickAfter::NewAgent(false);
                }
                "fork" => {
                    // Fork the row's agent: the form, prefilled to resume
                    // its session as a copy with a name of its own.
                    match p.selected() {
                        Some(a) if crate::newagent::resumable_id(&a.id).is_some() => {
                            after = PickAfter::NewAgent(true);
                        }
                        Some(_) => {
                            p.status("nothing to fork: this agent has no session id yet");
                            pick_render(p);
                        }
                        None => {}
                    }
                }
                "info" => {
                    // The info card in place of the preview, and back.
                    p.show_info = !p.show_info;
                    p.engine.show_help = false;
                    pick_render(p);
                }
                "copy_cwd" => {
                    if p.show_info {
                        // Copy the working directory, the way c copies the id.
                        let cwd = p
                            .info
                            .as_ref()
                            .and_then(|c| c.live_cwd.clone())
                            .or_else(|| p.selected().and_then(|a| a.cwd.clone()));
                        match cwd {
                            Some(d) if !d.contains('\'') => after = PickAfter::Copy(d),
                            Some(_) => p.status("that path has a quote I will not paste"),
                            None => p.status("no working directory known"),
                        }
                        pick_render(p);
                    }
                }
                "transcript" => {
                    // The highlighted live row's conversation in place of
                    // its pane, and back. A row with no pane shows it anyway.
                    p.show_transcript = !p.show_transcript;
                    p.status(if p.show_transcript { "preview: conversation" } else { "preview: pane" });
                    pick_render(p);
                }
                "read" | "unread" => {
                    // Read and unread by hand: the cursor passing over a
                    // row is not reading it, so these are the way to say
                    // you have (or have not) dealt with what it stopped for.
                    let read = name == "read";
                    let ids: Vec<(String, String)> = p
                        .targets()
                        .iter()
                        .filter(|a| a.live())
                        .map(|a| (a.server.clone(), a.id.clone()))
                        .collect();
                    if ids.is_empty() {
                        p.status("no live agent to mark");
                        pick_render(p);
                    } else {
                        after = PickAfter::Read(ids, read);
                    }
                }
                "sessions" => {
                    // The sessions chooser, on the pane under the cursor:
                    // the same tree seen by session rather than by agent.
                    after = PickAfter::Sessions(live_pane_of_selection(p));
                }
                _ => {}
            },
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
                        p.engine.preview_focus = false;
                        p.status(format!("typing failed: {}", e.message));
                        pick_render(p);
                    }
                }
            }
        }
        PickAfter::Paste(pane, text) => {
            if let Err(e) = send_text(PaneId(pane), &text) {
                if let Some(p) = picker.borrow_mut().as_mut() {
                    p.status(format!("paste failed: {}", e.message));
                    pick_render(p);
                }
            } else {
                poke_preview(picker, true);
            }
        }
        PickAfter::Wheel(pane, key, x, y) => {
            // Into the pane as a mouse key at its cell, then hurry the
            // blit along so copy mode (or the app's scroll) shows now.
            match pane_mouse(PaneId(pane), &key, x, y, client) {
                Ok(()) => poke_preview(picker, false),
                Err(e) => {
                    if let Some(p) = picker.borrow_mut().as_mut() {
                        p.status(format!("wheel: {}", e.message));
                        pick_render(p);
                    }
                }
            }
        }
        PickAfter::Close(mode) => {
            let _ = mode_close(mode);
        }
        PickAfter::Sessions(pane) => {
            // Close first: the chooser floats over the same window, and
            // its `t` comes back here the same way.
            if let Some(old) = picker.borrow_mut().take() {
                if let Some(t) = old.timer {
                    cancel(t);
                }
                let _ = mode_close(old.engine.mode);
            }
            ctx.spawn(async move {
                let target = pane.map(|p| format!("-t '%{p}' ")).unwrap_or_default();
                let _ = run_command(&format!("plugin-command {target}sessions 'pick pane'")).await;
            });
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
                    p.status(format!("interrupt failed: {}", e.message));
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
                    p.status(match &r {
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
                    p.status(match ok {
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
        .find(|(_, rows)| rows.rows.iter().any(|a| a.id == id))
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
        (p.engine.mode, servers)
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
            if p.engine.mode.0 != mode.0 {
                return;
            }
            p.unread.retain(|(s, _), _| *s != server);
            for bc in boxes {
                if bc.unread > 0 {
                    p.unread.insert((server.clone(), bc.box_), bc.unread);
                }
            }
            let keep = p.keep();
            pick_reshow(p, keep, false);
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
/// Jumping acknowledges the agent (`ack`), like typing into it.
fn jump_after(p: &mut Picker, ack: &mut Option<(String, String)>) -> PickAfter {
    let Some(i) = p.selected_index() else {
        return PickAfter::None;
    };
    let a = &p.rows[i];
    let live_pane = a.pane.filter(|_| a.live()).map(|x| x as u32);
    match (live_pane, p.local_pane_of(a)) {
        (Some(_), Some(local)) => {
            p.rows[i].acked_ms = Some(p.now_ms as i64);
            *ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
            PickAfter::Jump(local, p.engine.mode)
        }
        (Some(_), None) => {
            let server = a.server.clone();
            p.status(format!("not mirrored here: remote-attach {server}"));
            pick_render(p);
            PickAfter::None
        }
        (None, _) => {
            // No pane: bring the agent back. The form opens prefilled to
            // resume it (its transcript, its directory, its session) and
            // is the question - Esc says no. A row with nothing to resume
            // (a provisional id) only says so.
            if crate::newagent::resumable_id(&a.id).is_some() {
                p.status("gone: the form brings it back (Esc: no)");
                pick_render(p);
                PickAfter::NewAgent(false)
            } else {
                p.status("no live pane to jump to");
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
        let why = unreachable_reason(p);
        p.status(why);
        pick_render(p);
        return None;
    }
    // Whatever was being typed into the picker itself is abandoned; the
    // keyboard cannot be in two places.
    p.engine.filtering = false;
    p.engine.close_prompt();
    p.engine.preview_focus = true;
    p.transcript_focus = false;
    let mut ack = None;
    if let Some(i) = p.selected_index() {
        if p.rows[i].unread() {
            p.rows[i].acked_ms = Some(p.now_ms as i64);
            ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
        }
    }
    pick_render(p);
    ack
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
        (p.engine.mode, p.engine.preview_rect())
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
                p.engine.mode.0 == mode.0
                    && (!typing || p.engine.preview_focus)
                    && live_pane_of_selection(p) == Some(rect.pane.0)
            });
            if !same {
                return;
            }
            let _ = mode_preview(mode, Some(&rect));
        }
    });
}

/// Flip content search on or off, then re-filter. Turning it off drops
/// the cached snippets. Turning it on makes the next `pick_refilter` grep
/// the live panes.
fn toggle_content(p: &mut Picker) {
    p.content_search = !p.content_search;
    if !p.content_search {
        p.content_hits.clear();
    }
    p.status(if p.content_search { "search pane contents: on" } else { "search pane contents: off" });
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
    let targets = p.targets();
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
    let targets = p.targets();
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

// ---------------------------------------------------------------------------
// the rows as nodes
// ---------------------------------------------------------------------------

/// Keep the band order of `p.rows`; the filter only includes or excludes.
fn pick_refilter(p: &mut Picker) {
    let keep = p.keep();
    pick_refilter_keep(p, keep, true);
}

/// Run the searches the query needs, then rebuild the list, restoring the
/// highlight to `keep`'s agent if it survived. Callers that replace
/// `rows` pass the key they captured from the OLD rows.
fn pick_refilter_keep(
    p: &mut Picker,
    keep: Option<(String, String, Option<i64>)>,
    reset_scroll: bool,
) {
    let query = p.engine.query();
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

/// Rebuild the list from `rows` and the hits in hand, without searching
/// again: the tail of a refilter, and what a late reply (rows for the
/// hits, a remote server's answer, the mailbox counts) re-runs.
fn pick_reshow(
    p: &mut Picker,
    keep: Option<(String, String, Option<i64>)>,
    reset_scroll: bool,
) {
    merge_hit_rows(p);
    let nodes = build_nodes(p);
    p.engine.set_nodes(nodes);
    // The engine kept the highlight by key; fall back to the pane on the
    // same server, which survives an id migration (prov -> durable) that
    // the key would miss.
    if let Some((key, server, pane)) = keep {
        if p.engine.selected_key().as_deref() != Some(key.as_str()) {
            if let Some(pn) = pane {
                let fallback = p.rows.iter().find(|a| a.pane == Some(pn) && a.server == server).map(Agent::key);
                if let Some(k) = fallback {
                    p.engine.select_key(&k);
                }
            }
        }
    }
    // Filter typing snaps to the top; a refresh keeps the scroll where it
    // is (then just nudges to keep the selection visible).
    if reset_scroll {
        p.engine.reset_scroll();
    }
    p.sync_focus();
}

/// The rows as the engine's nodes: a server header (when rows come from
/// more than one server), a band header under it, then the rows of that
/// band, in `rows` order - or by relevance inside their band while a
/// query is typed. Headers are groups the cursor cannot land on; the
/// engine drops a header whose rows the filter took.
fn build_nodes(p: &mut Picker) -> Vec<Node> {
    p.by_key = p.rows.iter().enumerate().map(|(i, a)| (a.key(), i)).collect();
    let query = p.engine.query();
    let needle = query.words.clone();
    let mut idx: Vec<usize> = (0..p.rows.len())
        .filter(|&i| !p.archived_only || p.rows[i].life == "archived")
        .collect();
    if !needle.is_empty() {
        // With a query, rows sort by relevance inside their band: a name
        // that starts with the query, then one that contains it, then the
        // conversation hits by score. Stable, so ties keep their order.
        let mut keyed: Vec<(usize, (bool, String), u8, f32)> = idx
            .iter()
            .map(|&i| {
                let a = &p.rows[i];
                let tier = match listkit::query::rank(&haystack(a), &needle) {
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
        idx = keyed.into_iter().map(|k| k.0).collect();
    }
    let mut nodes: Vec<Node> = Vec::new();
    let mut prev_server: Option<String> = None;
    let mut prev_band: Option<u8> = None;
    let item_depth = if p.multi { 2 } else { 1 };
    for i in idx {
        let a = &p.rows[i];
        if p.multi && prev_server.as_deref() != Some(a.server.as_str()) {
            nodes.push(server_node(p, &a.server));
            prev_server = Some(a.server.clone());
            prev_band = None;
        }
        let b = band(a);
        if prev_band != Some(b) {
            let mut n = Node::group(format!("band\u{1}{}\u{1}{b}", a.server), item_depth - 1, true, false);
            n.left = plain_cells(band_label(b), ST_DIM | ST_BOLD);
            n.header_glyph = None;
            n.indent = Some(1);
            n.tokens = vec![('@', a.server.clone())];
            nodes.push(n);
            prev_band = Some(b);
        }
        nodes.push(row_node(p, a, item_depth));
    }
    // A server whose copy this side rejects has no rows, and nor does
    // one being fetched for the first time; give both a line anyway, so
    // the reason (or the spinner) is on screen. A fetch still inside its
    // grace is not one of them: a line that appears and vanishes within
    // 500ms is worse than no line.
    let now = p.now_ms;
    let mut odd: Vec<&String> = p
        .mismatch
        .keys()
        .chain(
            p.fetching
                .iter()
                .filter(|(_, t)| now.saturating_sub(**t) >= SPIN_GRACE_MS)
                .map(|(k, _)| k),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|s| !p.rows.iter().any(|a| a.server == **s))
        .collect();
    odd.sort();
    for s in odd {
        nodes.push(server_node(p, s));
    }
    nodes
}

/// A server header: bold, with the link state when down, the reason when
/// its copy is rejected, and a spinner while its roster is still in
/// flight. A healthy remote barely flashes; one that keeps the call
/// hanging says for how long, which is the whole point (the alternative
/// is a stale group with no reason).
fn server_node(p: &Picker, server: &str) -> Node {
    let (label, style) = match (
        p.down.get(server),
        p.mismatch.get(server),
        spin_since(&p.fetching, server, p.now_ms),
    ) {
        (Some(since), _, _) => (
            format!("{server}  (disconnected {})", fmt_age(p.now_ms.saturating_sub(*since) / 1000)),
            ST_RED,
        ),
        (None, Some(why), _) => (format!("{server}  ({why})"), ST_CODE),
        (None, None, Some(since)) => {
            let waited = p.now_ms.saturating_sub(since);
            let frame = SPIN_FRAMES[(p.now_ms / SPIN_MS) as usize % SPIN_FRAMES.len()];
            if waited >= FETCH_STUCK_MS {
                (format!("{server}  {frame} (fetching {})", fmt_age(waited / 1000)), ST_CODE)
            } else {
                (format!("{server}  {frame}"), ST_CYAN)
            }
        }
        (None, None, None) => (server.to_string(), ST_CYAN),
    };
    let mut n = Node::group(format!("srv\u{1}{server}"), 0, true, false);
    n.left = plain_cells(&label, style);
    n.haystack = server.to_string();
    n.tokens = vec![('@', server.to_string())];
    n
}

/// The badge: the agent's state as one coloured glyph. Magenta for an
/// archived agent, dim for a finished one; an unread stopped agent gets
/// a block behind its glyph, the one coloured background in the list,
/// so it cannot be missed at a glance.
fn badge_cells(a: &Agent) -> Styled {
    if a.life == "archived" {
        return ('◆', ST_MAGENTA);
    }
    if !a.live() {
        return ('·', ST_DIM);
    }
    match a.status.as_str() {
        "needs_input" if a.unread() => ('!', ST_HIT),
        "needs_input" => ('!', ST_BOLD | ST_CODE),
        "working" => ('●', ST_GREEN),
        "waiting" if a.unread() => ('◉', ST_BOLD | ST_CYAN | ST_BRIGHT | ST_INVERT),
        "waiting" => ('◍', ST_CYAN),
        "done" => ('·', ST_DIM),
        _ => ('?', 0),
    }
}

/// One agent as a row: the badge, the unread count, the name (bold while
/// unread), then the reason the row is here - a content or conversation
/// hit's line, "archived", or the note or task - dim; the session, kind
/// and age at the right.
fn row_node(p: &Picker, a: &Agent, depth: u8) -> Node {
    let key = a.key();
    let mut left: Vec<Styled> = vec![badge_cells(a), (' ', 0)];
    let unread = p
        .unread
        .get(&(a.server.clone(), a.id.clone()))
        .copied()
        .unwrap_or(0);
    let name = if unread > 0 {
        format!("\u{2709}{unread} {}", display_name(a))
    } else {
        display_name(a)
    };
    // An unread row's name is bold too: the badge is one cell, the name
    // is what the eye reads.
    left.extend(plain_cells(&name, if a.unread() { ST_BOLD } else { 0 }));
    // A content-search hit shows the matching line: it is why the row is
    // here. Otherwise an archived row (only in the history views) says
    // so, so the `a` un-archive is obvious; else the note (the reason
    // the row is at the top, only while the agent is blocked on the
    // user) or the task.
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
    let note = a.note.as_deref().filter(|s| !s.is_empty());
    let task = a.task.as_deref().filter(|s| !s.is_empty());
    let tail = if let Some(sn) = snip {
        Some(sn.trim().to_string())
    } else if a.life == "archived" {
        Some("archived".to_string())
    } else {
        note.or(task).map(one_line)
    };
    if let Some(t) = tail {
        left.extend(plain_cells(&format!("  ·  {t}"), ST_DIM));
    }
    // session + kind + age are fixed columns at the right edge - the
    // session is where the pane lives, which the name does not say.
    let right = format!(
        "{:<12} {:<8} {:>6}",
        clip(a.session.as_deref().unwrap_or(""), 12),
        clip(&a.kind, 8),
        fmt_age(p.age_ms(a) / 1000)
    );
    let mut n = Node::item(key.clone(), depth);
    n.indent = Some(0);
    n.left = left;
    n.right = plain_cells(&right, 0);
    n.haystack = haystack(a);
    n.tokens = vec![('@', a.server.clone())];
    if let Some(s) = a.session.as_deref().filter(|s| !s.is_empty()) {
        n.tokens.push(('#', s.to_string()));
    }
    if let Some(c) = a.cwd.as_deref() {
        n.tokens.push(('~', tilde_of(&p.home, c)));
    }
    n.force_match = p.content_hits.contains_key(&key) || p.transcript_hits.contains_key(&key);
    // A row of a disconnected server is dimmed: what it shows is what
    // the provider last said.
    n.dim = p.down.contains_key(&a.server);
    n.here = p.is_here(a);
    n
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
            let keep = p.keep();
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
    let mode = p.engine.mode;
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
                if p.engine.mode.0 != mode.0 {
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
                // Rebuild the view with the new hits, without re-running
                // the search (the query is unchanged).
                let keep = p.keep();
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

// ---------------------------------------------------------------------------
// render
// ---------------------------------------------------------------------------

/// Is the conversation what the preview shows for the highlighted row:
/// it has arrived, and the row has no live pane here (or asked for it
/// with Tab), and no card or typing is over it.
fn transcript_drawn(p: &Picker) -> bool {
    if p.show_info || p.engine.show_help || p.engine.preview_focus {
        return false;
    }
    if !transcript_shown(p) {
        return false;
    }
    p.show_transcript || p.selected().and_then(|a| p.local_pane_of(a)).is_none()
}

/// What the preview column shows for the highlighted row, set on its
/// node: the info card; the conversation (drawn by this module, so the
/// node says `None`); the live pane; a remote row's captured text.
fn refresh_preview(p: &mut Picker) {
    let Some(a) = p.selected() else { return };
    let key = a.key();
    let pw = p.preview_w();
    let preview = if p.show_info {
        Preview::Text(info_lines(p, a, pw))
    } else if transcript_drawn(p) {
        Preview::None
    } else if let Some(pane) = p.local_pane_of(a) {
        Preview::Pane(PaneId(pane))
    } else if let Some(lines) = remote_preview_lines(p) {
        let ph = (p.engine.height as usize).saturating_sub(1);
        Preview::Text(
            lines
                .iter()
                .rev()
                .take(ph)
                .rev()
                .map(|l| plain_cells(&strip_sgr(l), 0))
                .collect(),
        )
    } else {
        Preview::None
    };
    p.engine.set_preview(&key, preview);
}

pub fn pick_render(p: &mut Picker) {
    refresh_preview(p);
    let w = p.engine.width as usize;
    let h = p.engine.height as usize;
    let list_w = p.engine.list_w();

    // The header: how many live, narrowed how, which view.
    let live_total = p.rows.iter().filter(|a| a.live()).count();
    let query = p.engine.query();
    // Narrowed by a filter token, the count says so: "9 of 22 live".
    let live = if query.has_filters() {
        let shown = p
            .engine
            .visible_matched()
            .filter_map(|n| p.by_key.get(&n.key))
            .filter(|&&i| p.rows[i].live())
            .count();
        format!("{shown} of {live_total}")
    } else {
        live_total.to_string()
    };
    let selected = if p.engine.marked().is_empty() {
        String::new()
    } else {
        format!(", {} selected", p.engine.marked().len())
    };
    let content_tag = if p.content_search {
        format!(", {}", mode_label(p.content_mode))
    } else {
        String::new()
    };
    let unread = p.rows.iter().filter(|a| a.unread()).count();
    let unread_tag = if unread > 0 { format!(", {unread} unread") } else { String::new() };
    let servers_tag = if p.multi {
        let n = p.rows.iter().map(|a| a.server.as_str()).collect::<HashSet<_>>().len();
        format!(", {n} servers")
    } else {
        String::new()
    };
    let view_tag = if p.archived_only {
        ", archive"
    } else if p.show_history {
        ", +history"
    } else {
        ""
    };
    p.engine.header_tag = format!("({live} live{view_tag}{unread_tag}{servers_tag}{content_tag}{selected})");
    p.engine.empty_text = if p.archived_only && p.engine.filter.trim().is_empty() {
        "(nothing archived)".into()
    } else {
        "(no agents)".into()
    };
    p.engine.separator_lit = p.transcript_focus;

    // The footer. The engine writes its own for the search box, a
    // prompt, typing into the pane and the help card; the list's and the
    // conversation's are this module's. Six hints, not twelve: everything
    // else lives in the action menu, which names each one in full.
    let k = |action: &str| keyname(p.engine.keys.key_of(action)).to_string();
    let ctok = if p.content_search {
        format!("{} {}", pretty_key(p.engine.keys.key_of("content")), mode_label(p.content_mode))
    } else {
        format!("{} contents", pretty_key(p.engine.keys.key_of("content")))
    };
    let footer = if p.show_info {
        format!("{} back · {} jump · {} copy id · {} copy cwd · Esc back", k("info"), k("activate"), k("copy"), k("copy_cwd"))
    } else if p.transcript_focus {
        "j/k scroll · n/N match · g/G top/end · Space/b page · Tab pane · Esc back to list".to_string()
    } else {
        format!(
            "j/k move · {} type · {} jump · {} new · {} actions · {} search · {ctok} · {} history · ? help · q/{} close",
            k("focus"),
            k("activate"),
            k("new"),
            k("menu"),
            k("filter"),
            k("history"),
            k("close"),
        )
    };
    // Light up the content-search hotkey while it is on.
    p.engine.footer = if p.content_search {
        footer.replacen(&ctok, &format!("\x1b[0;7m{ctok}\x1b[0;2m"), 1)
    } else {
        footer
    };

    let (mut out, rect) = p.engine.render();
    if transcript_drawn(p) {
        let x = list_w + 2;
        let pw = w.saturating_sub(list_w + 2);
        let ph = h.saturating_sub(1);
        draw_transcript(p, &mut out, x, pw, ph);
    }
    let _ = mode_write(p.engine.mode, out.as_bytes());
    let _ = mode_preview(p.engine.mode, rect.as_ref());
}

/// Does the highlighted row have a conversation to show? Only once its
/// turns (or a stand-in capture) have arrived for that row.
fn transcript_shown(p: &Picker) -> bool {
    let Some(a) = p.selected() else { return false };
    p.transcript
        .as_ref()
        .is_some_and(|tv| tv.key == a.key() && (!tv.turns.is_empty() || !tv.capture.is_empty()))
}

/// The info card: everything the roster knows about the highlighted
/// agent, as labelled lines. The row's own facts draw at once; the
/// fetched half (`InfoCard`) fills in when it lands.
fn info_lines(p: &Picker, a: &Agent, pw: usize) -> Vec<Vec<Styled>> {
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
    // The id on the first line, then the rows, each wrapped to the
    // column with the label's width hanging.
    let mut out: Vec<Vec<Styled>> = vec![plain_cells(&clip(&a.id, pw), ST_BOLD | ST_CYAN), Vec::new()];
    let label_w = 11;
    let body_w = pw.saturating_sub(label_w).max(8);
    for (label, value) in rows {
        for (i, piece) in wrap_cells(&plain_cells(&value, 0), body_w).into_iter().enumerate() {
            let lead = if i == 0 { format!("{label:<label_w$}") } else { " ".repeat(label_w) };
            let mut line = plain_cells(&lead, ST_DIM);
            line.extend(piece);
            out.push(line);
        }
    }
    out
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
    let pw = p.preview_w();
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
    let body = (p.engine.height as usize).saturating_sub(2).max(1);
    if let Some(tv) = p.transcript.as_mut() {
        let max_top = tv.lines.len().saturating_sub(body);
        let t = (tv.top as i64 + delta).clamp(0, max_top as i64);
        tv.top = t as usize;
    }
}

/// A key while the conversation has the keyboard.
fn transcript_key(p: &mut Picker, key: &str, is_up: bool, is_down: bool) {
    let page = (p.engine.height as usize).saturating_sub(2).max(2) as i64;
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
                p.status("no matches in this conversation");
            }
        }
        _ => {
            let _ = (is_up, is_down);
            return;
        }
    }
    pick_render(p);
}

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

#[cfg(test)]
mod query_tests {
    use super::*;

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
}
