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
use std::rc::Rc;

use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

use crate::provider::{self, mode_label, run_content_search, Snapshot};
use crate::store::{self, Agent, LOCAL};
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
}

/// The remote rosters, by server name. Shared between the plugin (which
/// feeds it from events) and the picker (which reads it).
#[derive(Debug, Default)]
pub struct Remotes {
    pub servers: HashMap<String, RemoteRows>,
    /// Servers whose copy of this plugin this side does not accept, with
    /// the reason to show ("agents 0.2.0 there, 0.1.0 here").
    pub mismatch: HashMap<String, String>,
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

/// Fetch the roster of every connected remote server. `history` asks for
/// the finished rows too.
pub async fn fetch_remotes(remotes: Rc<RefCell<Remotes>>, history: bool) {
    let list = service::servers().unwrap_or_default();
    let mine = list
        .iter()
        .find(|s| s.local)
        .map(|s| s.version.clone())
        .unwrap_or_default();
    for s in list.into_iter().filter(|s| !s.local && s.up) {
        if !s.accepted {
            remotes.borrow_mut().mark_mismatch(
                &s.name,
                format!("agents {} there, {} here; run tmux update", s.version, mine),
            );
            continue;
        }
        let target = format!("@{}", s.name);
        match service::call_json::<_, Snapshot>(
            &target,
            "list",
            &provider::ListReq { history },
        )
        .await
        {
            Ok(snap) => remotes.borrow_mut().apply(&s.name, snap),
            Err(e) => {
                // Not linked for plugins (an old remote, or no provider
                // yet): nothing to show, but nothing to break either.
                if e.code == ErrorCode::Unreachable {
                    remotes.borrow_mut().mark_down(&s.name);
                } else if e.code == ErrorCode::Version {
                    remotes.borrow_mut().mark_mismatch(&s.name, e.message.clone());
                }
            }
        }
    }
}

/// Follow the roster topic of a server, so changes arrive without a poll.
pub fn follow(server: &str) {
    let _ = service::subscribe(&format!("@{server}"), provider::TOPIC);
}

/// Ask a remote provider to act on one of its rows. The reply is a plain
/// "ok", so this is a byte call, not a JSON one.
async fn act_remote(server: &str, id: &str, verb: &str, name: Option<&str>) {
    let target = format!("@{server}");
    let req = provider::ActReq {
        id: id.to_string(),
        verb: verb.to_string(),
        name: name.map(str::to_string),
    };
    let bytes = serde_json::to_vec(&req).unwrap_or_default();
    if let Err(e) = service::call(&target, "act", &bytes).await {
        log(&format!("agents: {verb} on {server}: {}", e.message));
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
    Reload,
    Resize(ModeId, u32, u32),
    /// Rename (server, id) to the name.
    Rename(String, String, String),
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
    /// The matching snippet per (server, pane), from the last content
    /// search. Drives the row's snippet and the OR in the filter.
    pub content_hits: HashMap<(String, u32), String>,
    /// The matcher the last content search actually used (auto-detected,
    /// with a fuzzy fallback). Shown in the footer/header.
    pub content_mode: SearchMode,
    /// The query the remote searches were sent for; a reply for another
    /// query is stale and dropped.
    pub content_query: String,
    pub now_ms: u64,
    pub show_history: bool,
    pub keys: PickKeys,
    pub status: Option<String>,
    /// A `g` was pressed and waits for a second `g` (vim `gg` = go top).
    pub pending_g: bool,
    /// A stable display rank per row key, assigned in the recency order
    /// the FIRST time each agent is seen this session. Live refreshes sort
    /// by server, band then this rank, so an activity-time bump never
    /// reshuffles rows under the cursor.
    pub order: HashMap<String, u64>,
    pub order_next: u64,
    /// The 2s refresh task, cancelled when the picker closes or reopens.
    pub timer: Option<TaskId>,
    /// The pane the picker was opened from. Its row gets a "you are here"
    /// border, so you can spot the agent you are currently sitting on.
    pub current_pane: Option<u32>,
    /// Per server: local clock minus the provider's clock.
    pub skew: HashMap<String, i64>,
    /// Per server: when its link went down (local clock), while it is.
    pub down: HashMap<String, u64>,
    /// Per server: why this side rejects its copy of the plugin.
    pub mismatch: HashMap<String, String>,
    /// (server, remote pane) -> the local shadow pane that mirrors it.
    pub mirrors: HashMap<(String, u32), u32>,
    /// The captured text of the highlighted remote row that has no local
    /// mirror, by row key, once the provider answered.
    pub remote_capture: Option<(String, Vec<String>)>,
    /// Rows come from more than one server: show server headers.
    pub multi: bool,
}

impl Picker {
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
        // A server whose copy this side rejects has no rows; give it a
        // line anyway, so the reason is on screen.
        let mut odd: Vec<&String> = self
            .mismatch
            .keys()
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

    /// The highlighted row.
    fn selected(&self) -> Option<&Agent> {
        self.rows.get(*self.view.get(self.sel)?)
    }

    /// The local pane a row's pane shows in: the pane itself for a local
    /// row, the shadow pane for a mirrored remote row.
    fn local_pane_of(&self, a: &Agent) -> Option<u32> {
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

/// Local rows (from the store) plus every remote row, with the per-server
/// clock skew and link state the picker renders with.
async fn gather_rows(
    remotes: &Rc<RefCell<Remotes>>,
    show_history: bool,
    enrich: bool,
) -> (
    Vec<Agent>,
    HashMap<String, i64>,
    HashMap<String, u64>,
    HashMap<String, String>,
) {
    let mut rows = store::live_agents().await.unwrap_or_default();
    if enrich {
        provider::enrich_live(&mut rows).await;
    }
    if show_history {
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
    (rows, skew, down, r.mismatch.clone())
}

fn is_multi(rows: &[Agent]) -> bool {
    rows.iter().any(|a| !a.is_local())
}

pub async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    cfg: Rc<Config>,
    remotes: Rc<RefCell<Remotes>>,
    client: Option<u64>,
    here: Option<u32>,
) {
    let window = client
        .and_then(|cid| {
            list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
        })
        .and_then(|c| c.session)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window)
        .or_else(|| list_windows().ok().and_then(|w| w.first().map(|x| x.id)));
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
    // Fresh remote rosters first, so the picker opens complete.
    fetch_remotes(Rc::clone(&remotes), false).await;
    let (mut rows, skew, down, mismatch) = gather_rows(&remotes, false, true).await;
    let mut order: HashMap<String, u64> = HashMap::new();
    let mut order_next: u64 = 0;
    stable_sort(&mut order, &mut order_next, &mut rows);
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
    let multi = is_multi(&rows);
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
        content_mode: SearchMode::Plain,
        content_query: String::new(),
        now_ms: now_ms(),
        show_history: false,
        keys: cfg.keys.clone(),
        status: None,
        pending_g: false,
        order,
        order_next,
        timer: None,
        current_pane: here,
        skew,
        down,
        mismatch,
        mirrors: if multi { find_mirrors() } else { HashMap::new() },
        remote_capture: None,
        multi,
    };
    pick_refilter(&mut p);
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
    request_capture(&picker);
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
    let show_history =
        picker.borrow().as_ref().map(|p| p.show_history).unwrap_or(false);
    let (mut rows, skew, down, mismatch) =
        gather_rows(&remotes, show_history, enrich).await;
    let multi = is_multi(&rows);
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
        p.rows = rows;
        p.now_ms = now_ms();
        p.skew = skew;
        p.down = down;
        p.mismatch = mismatch;
        p.mirrors = mirrors;
        p.multi = multi;
        // A refresh keeps the scroll where it is (only filter typing snaps
        // back to the top).
        pick_refilter_keep(p, keep, false);
        pick_render(p);
    }
    drop(b);
    request_capture(&picker);
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
        let (live, history) = {
            let b = picker.borrow();
            match b.as_ref() {
                Some(p) if p.mode.0 == mode.0 => (true, p.show_history),
                _ => (false, false),
            }
        };
        if !live {
            return;
        }
        fetch_remotes(Rc::clone(&remotes), history).await;
        reload_picker(Rc::clone(&picker), Rc::clone(&remotes), true).await;
    }
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
            let _ = store::set_life(id, &life).await;
        } else {
            let verb = if life == "active" { "unarchive" } else { "archive" };
            act_remote(server, id, verb, None).await;
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
    fetch_remotes(Rc::clone(&remotes), false).await;
    reload_picker(picker, remotes, false).await;
}

/// Acknowledge (mark read) a row, wherever it lives.
async fn acknowledge(server: String, id: String) {
    if server == LOCAL {
        let _ = store::acknowledge(&id, now_ms() as i64).await;
    } else {
        act_remote(&server, &id, "ack", None).await;
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
        let sel = p.view.get(p.sel).copied();
        // True when this key moved the selection: drives read-ack.
        let mut moved = false;
        // A pending `g` is consumed by this key; only a second `g`
        // keeps it (see the `g` branch).
        let g_pending = p.pending_g;
        p.pending_g = false;
        // Arrows and their control aliases move the selection in both
        // modes; they are never text.
        let is_down = matches!(key.as_str(), "Down" | "C-n" | "C-j");
        let is_up = matches!(key.as_str(), "Up" | "C-p" | "C-k");
        if p.renaming {
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
            if key == k.close || key == "Enter" {
                p.filtering = false;
                pick_render(p);
            } else if is_down {
                move_sel(p, 1);
                moved = true;
            } else if is_up {
                move_sel(p, -1);
                moved = true;
            } else if key == "BSpace" {
                p.filter.pop();
                pick_refilter(p);
                pick_render(p);
            } else if key == "C-u" {
                p.filter.clear();
                pick_refilter(p);
                pick_render(p);
            } else if key == "Space" {
                p.filter.push(' ');
                pick_refilter(p);
                pick_render(p);
            } else if key == k.content {
                toggle_content(p);
            } else if key.chars().count() == 1
                && !key.chars().next().unwrap().is_control()
            {
                p.filter.push_str(&key);
                pick_refilter(p);
                pick_render(p);
            }
        } else if key == k.close || key == "q" {
            // Esc or q cancels a pending selection first; a second press,
            // with nothing marked, closes the picker.
            if p.marked.is_empty() {
                after = PickAfter::Close(p.mode);
            } else {
                p.marked.clear();
                p.status = Some("selection cleared".into());
                pick_render(p);
            }
        } else if key == k.filter {
            // `/` focuses the search box (so does navigating up).
            p.filtering = true;
            pick_render(p);
        } else if key == "g" {
            // Vim `gg`: the first `g` waits, the second goes to the top.
            if g_pending {
                let n = p.view.len() as i32;
                move_sel(p, -n);
                moved = true;
            } else {
                p.pending_g = true;
            }
        } else if key == "G" {
            // Vim `G`: go to the bottom.
            let n = p.view.len() as i32;
            move_sel(p, n);
            moved = true;
        } else if key == k.content {
            toggle_content(p);
        } else if key == k.rename {
            if let Some(a) = p.selected() {
                p.rename_buf = a.user_name.clone().unwrap_or_default();
                p.renaming = true;
                pick_render(p);
            }
        } else if key == k.jump {
            if let Some(i) = sel {
                let a = &p.rows[i];
                let live_pane = a.pane.filter(|_| a.live()).map(|x| x as u32);
                match (live_pane, p.local_pane_of(a)) {
                    (Some(_), Some(local)) => {
                        // Jumping to the pane acknowledges the agent.
                        p.rows[i].acked_ms = Some(p.now_ms as i64);
                        ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
                        after = PickAfter::Jump(local, p.mode);
                    }
                    (Some(_), None) => {
                        p.status = Some(format!(
                            "not mirrored here: remote-attach {}",
                            a.server
                        ));
                        pick_render(p);
                    }
                    (None, _) => {
                        p.status = Some("no live pane to jump to".into());
                        pick_render(p);
                    }
                }
            }
        } else if key == k.archive {
            after = archive_after(p);
        } else if key == k.history {
            p.show_history = !p.show_history;
            after = PickAfter::Reload;
        } else if key == "J" {
            mark_and_move(p, 1);
            moved = true;
        } else if key == "K" {
            mark_and_move(p, -1);
            moved = true;
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
            moved = true;
        } else if is_up || key == "k" {
            if p.sel == 0 {
                // Already at the top: bring the cursor up into the
                // search box.
                p.filtering = true;
                pick_render(p);
            } else {
                move_sel(p, -1);
                moved = true;
            }
        }
        // Landing the cursor on an unread waiting row acknowledges it
        // (only real navigation acks; opening the picker does not).
        if moved && ack.is_none() {
            if let Some(&i) = p.view.get(p.sel) {
                if p.rows[i].unread() {
                    p.rows[i].acked_ms = Some(p.now_ms as i64);
                    ack = Some((p.rows[i].server.clone(), p.rows[i].id.clone()));
                    pick_render(p);
                }
            }
        }
    }
    if let Some((server, id)) = ack {
        ctx.spawn(acknowledge(server, id));
    }
    request_capture(picker);
    match after {
        PickAfter::None => {}
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
        PickAfter::Reload => {
            let picker = Rc::clone(picker);
            let remotes = Rc::clone(remotes);
            ctx.spawn(async move {
                let history = picker.borrow().as_ref().is_some_and(|p| p.show_history);
                fetch_remotes(Rc::clone(&remotes), history).await;
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
                    act_remote(&server, &id, "rename", n).await;
                    fetch_remotes(Rc::clone(&remotes), false).await;
                }
                reload_picker(picker, remotes, false).await;
            });
        }
    }
}

/// Move the selection by `delta` rows, clamped, then scroll and redraw.
fn move_sel(p: &mut Picker, delta: i32) {
    if p.view.is_empty() {
        return;
    }
    let last = (p.view.len() - 1) as i32;
    p.sel = (p.sel as i32 + delta).clamp(0, last) as usize;
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

/// The archive key toggles: it archives its targets, or un-archives them
/// when they are all already archived (an archived row shows in the
/// history view). Targets are the marked selection, else the highlighted
/// row.
fn archive_after(p: &Picker) -> PickAfter {
    let targets: Vec<&Agent> = {
        let marked: Vec<&Agent> = p
            .view
            .iter()
            .filter_map(|&i| p.rows.get(i))
            .filter(|a| p.marked.contains(&a.key()))
            .collect();
        if !marked.is_empty() {
            marked
        } else {
            p.selected().into_iter().collect()
        }
    };
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
    let user = a.user_name.as_deref().filter(|s| !s.is_empty());
    let harness = a.name.as_deref().filter(|s| !s.is_empty());
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
        "{} {} {} {} {} {} {} {} {}",
        display_name(a),
        a.kind,
        a.status,
        a.life,
        a.session.as_deref().unwrap_or(""),
        a.window.as_deref().unwrap_or(""),
        a.task.as_deref().unwrap_or(""),
        a.reason.as_deref().unwrap_or(""),
        if a.is_local() { "" } else { a.server.as_str() },
    )
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
    let needle = p.filter.trim().to_string();
    // Refresh the content-match set when content search is on. The local
    // grep runs in tmux over the live grids (`panes_search`); only the
    // needle and the matches cross the ABI, so it is cheap enough per
    // keystroke. Remote grids are asked through each provider's `search`,
    // whose replies land later (see `remote_search`).
    if p.content_search && !needle.is_empty() {
        let panes: Vec<PaneId> = p
            .rows
            .iter()
            .filter(|a| a.live() && a.is_local())
            .filter_map(|a| a.pane.map(|x| PaneId(x as u32)))
            .collect();
        let (mode, hits) = run_content_search(&panes, &needle);
        p.content_mode = mode;
        // Keep remote hits for the same query; local ones are fresh.
        if p.content_query != needle {
            p.content_hits.clear();
            p.content_query = needle.clone();
            remote_search(p, &needle);
        } else {
            p.content_hits.retain(|(s, _), _| s != LOCAL);
        }
        for (pane, snip) in hits {
            p.content_hits.insert((LOCAL.to_string(), pane), snip);
        }
    } else {
        p.content_hits = HashMap::new();
        p.content_query.clear();
    }
    p.view = p
        .rows
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            rank(&haystack(a), &needle).is_some()
                || a.pane
                    .map(|pn| p.content_hits.contains_key(&(a.server.clone(), pn as u32)))
                    .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();
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
}

/// Ask every remote server with live rows to grep its panes for `needle`;
/// the replies fold into `content_hits` when they arrive. The picker is
/// found again through the shared cell, so a reply for a closed picker or
/// an outdated query is dropped.
fn remote_search(p: &mut Picker, needle: &str) {
    let servers: HashSet<String> = p
        .rows
        .iter()
        .filter(|a| a.live() && !a.is_local())
        .map(|a| a.server.clone())
        .collect();
    if servers.is_empty() {
        return;
    }
    let mode = p.mode;
    let needle = needle.to_string();
    for server in servers {
        let needle = needle.clone();
        spawn(async move {
            let target = format!("@{server}");
            let req = provider::SearchReq { needle: needle.clone() };
            let Ok(reply) =
                service::call_json::<_, provider::SearchReply>(&target, "search", &req).await
            else {
                return;
            };
            PICKER.with(|cell| {
                let Some(picker) = cell.borrow().clone() else { return };
                let mut b = picker.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if p.mode.0 != mode.0 || p.content_query != needle || !p.content_search {
                    return;
                }
                // Map ids back to panes: hits key on (server, pane).
                let by_id: HashMap<&str, u32> = p
                    .rows
                    .iter()
                    .filter(|a| a.server == server)
                    .filter_map(|a| a.pane.map(|pn| (a.id.as_str(), pn as u32)))
                    .collect();
                for (id, snip) in &reply.hits {
                    if let Some(&pane) = by_id.get(id.as_str()) {
                        p.content_hits.insert((server.clone(), pane), snip.clone());
                    }
                }
                if !reply.hits.is_empty() {
                    p.content_mode = provider::reply_mode(&reply);
                }
                let keep = p.selected().map(|a| (a.key(), a.server.clone(), a.pane));
                // Rebuild the view with the new hits, without re-running
                // the search (the query is unchanged).
                let q = p.content_query.clone();
                p.view = p
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| {
                        rank(&haystack(a), &q).is_some()
                            || a.pane
                                .map(|pn| p.content_hits.contains_key(&(a.server.clone(), pn as u32)))
                                .unwrap_or(false)
                    })
                    .map(|(i, _)| i)
                    .collect();
                p.sel = keep
                    .and_then(|(key, _, _)| p.view.iter().position(|&i| p.rows[i].key() == key))
                    .unwrap_or(0);
                p.rebuild_lines();
                p.scroll_to_selection();
                pick_render(p);
            });
        });
    }
}

thread_local! {
    /// The plugin's picker cell, so a detached task (a remote search
    /// reply) can find the picker without holding a borrow across awaits.
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
        "needs_input" => "\x1b[1;33m!\x1b[0m".into(),
        "working" => "\x1b[32m●\x1b[0m".into(),
        // Unread waiting: bright, bold, filled. Read waiting: hollow.
        "waiting" if a.unread() => "\x1b[1;96m◉\x1b[0m".into(),
        "waiting" => "\x1b[36m◍\x1b[0m".into(),
        "done" => "\x1b[2m·\x1b[0m".into(),
        _ => "?".into(),
    }
}

pub fn pick_render(p: &mut Picker) {
    let w = p.width as usize;
    let h = p.height as usize;
    // 60% of the width for the list, but never let the clamp's min exceed
    // its max: a narrow mode (a split pane) would panic `clamp(30, <30)`
    // and trap the guest. Below ~50 cols give the list almost everything
    // and skip the side preview.
    let list_w = if w <= 50 {
        w.saturating_sub(2).max(1)
    } else {
        (w * 6 / 10).clamp(30, w - 20)
    };
    let mut out = String::from("\x1b[2J\x1b[H");

    let live = p.rows.iter().filter(|a| a.live()).count();
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
        if p.show_history { ", +history" } else { "" },
    ));
    if p.renaming {
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
            "\x1b[2;1H  \x1b[2msearch\x1b[0m \x1b[2m(/ or ↑ to search)\x1b[0m",
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
        out.push_str("\x1b[4;1H  \x1b[2m(no agents)\x1b[0m");
    } else {
        for (line_i, li) in
            (p.top..(p.top + list_h).min(p.lines.len())).enumerate()
        {
            let row = 4 + line_i;
            match &p.lines[li] {
                Line::Server(server) => {
                    // A server line: bold, with the link state when down
                    // and the reason when its copy is rejected.
                    let (label, colour) = match (p.down.get(server), p.mismatch.get(server)) {
                        (Some(since), _) => (
                            format!(
                                "{server}  (disconnected {})",
                                fmt_age(p.now_ms.saturating_sub(*since) / 1000)
                            ),
                            "1;31",
                        ),
                        (None, Some(why)) => (format!("{server}  ({why})"), "1;33"),
                        (None, None) => (server.clone(), "1;34"),
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
                    // kind + age are fixed columns at the right edge; the
                    // name (with the task, when there is one) fills all the
                    // width that is left.
                    let right =
                        format!("{:<8} {:>6}", clip(&a.kind, 8), age);
                    // prefix = "▸ ● " (marker + badge), gap = 2 before right.
                    let label_w = list_w
                        .saturating_sub(1)
                        .saturating_sub(4 + 2 + right.chars().count())
                        .max(8);
                    let mut label = display_name(a);
                    // An archived row (only in the history view) says so,
                    // so the `a` un-archive is obvious. Otherwise a
                    // content-search hit shows the matching line, else the
                    // reported task.
                    let snip = a
                        .pane
                        .and_then(|pn| p.content_hits.get(&(a.server.clone(), pn as u32)))
                        .filter(|_| p.content_search);
                    if a.life == "archived" {
                        label = format!("{label}  ·  archived");
                    } else if let Some(sn) = snip {
                        label = format!("{label}  ·  {}", sn.trim());
                    } else if let Some(t) =
                        a.task.as_deref().filter(|s| !s.is_empty())
                    {
                        label = format!("{label}  ·  {t}");
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
                        out.push_str(&format!(
                            "\x1b[{row};1H\x1b[7m{:<pad$}\x1b[0m",
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
                    } else {
                        out.push_str(&format!("\x1b[{row};1H{shown}"));
                    }
                    // "You are here": the pane the picker was opened from
                    // gets a bright left border, drawn last so it shows over
                    // any row state (cursor, marked, or plain). Only a
                    // local row can be the pane we sit in.
                    let here = a.is_local()
                        && a.live()
                        && a.pane.map(|pn| pn as u32) == p.current_pane;
                    if here {
                        let g = if cur { "▸" } else { "▎" };
                        out.push_str(&format!("\x1b[{row};1H\x1b[1;94m{g}\x1b[0m"));
                    }
                }
            }
        }
    }

    // Vertical separator between the list and the preview.
    for r in 1..=h {
        out.push_str(&format!("\x1b[{r};{c}H\x1b[2m│\x1b[0m", c = list_w + 1));
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
    // The archive key un-archives when the highlighted row is archived.
    let cursor_archived = p.selected().is_some_and(|a| a.life == "archived");
    let arch = if cursor_archived { "unarch" } else { "arch" };
    let footer = if p.renaming {
        "type a name · Enter accept · Esc cancel".to_string()
    } else if p.filtering {
        format!("type to search · {ctok} · Esc unfocus")
    } else {
        format!(
            "j/k move · gg/G ends · {} search · {} jump · {ctok} · {} rename · {} {arch} · +/- size · q/{} close",
            keyname(&k.filter),
            keyname(&k.jump),
            keyname(&k.rename),
            keyname(&k.archive),
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

    // The preview: a live blit of the local (or mirrored) pane, else the
    // provider's captured text for a remote row with no mirror here.
    let rect = preview_rect(p, list_w);
    if rect.is_none() {
        if let Some(text) = remote_preview_lines(p) {
            let x = list_w + 2;
            let pw = w.saturating_sub(list_w + 1);
            let ph = h.saturating_sub(1);
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

/// The live pane of the highlighted row (local, or a mirror of a remote
/// one), shown to the right of the list.
fn preview_rect(p: &Picker, list_w: usize) -> Option<PreviewRect> {
    let a = p.selected()?;
    let pane = p.local_pane_of(a)?;
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
