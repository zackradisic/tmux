//! The view half: the chooser. It merges this server's tree with the
//! trees of the providers on linked servers, folds a shadow session into
//! the remote session it mirrors, badges the rows that hold an agent
//! (asked of the agents plugin on each server), and hands the list to
//! `listkit::Engine`. What a key does beyond the list is here: switch,
//! kill, rename, zoom, attach a remote session, drop a link, hand off to
//! session_creator, flip to the agents picker.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use listkit::engine::Outcome;
use listkit::keys::KeyTable;
use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::remotes::{spin_since, FETCH_STUCK_MS, SPIN_FRAMES, SPIN_MS};
use listkit::styled::{plain_cells, Styled, ST_BOLD, ST_CODE, ST_CYAN, ST_DIM, ST_ORANGE, ST_RED};
use listkit::text::{clip, fmt_age, menu_item, menu_safe, tilde_of};
use listkit::{Engine, Node, Preview, SigilSpec};
use serde::Deserialize;
use tmux_plugin_sdk::prelude::*;

use crate::layout;
use crate::provider;
use crate::tree::{self, id_of, Formats, Pn, Sess, Tree, Win, LOCAL};

pub const NAME: &str = "sessions";
/// While the picker stays open, re-read the local tree, the agent
/// badges and the remote trees on this cadence.
const REFRESH_MS: u64 = 2000;
const FETCH_DEBOUNCE_MS: u64 = 300;
const MAX_INFLIGHT: usize = 4;
/// The map of a window's panes above the preview: its height, plus one
/// blank line.
const STRIP_H: usize = 5;

pub type Remotes = listkit::remotes::Remotes<Tree>;

/// The expand/collapse toggles, remembered per entry mode across reopens.
pub type Memo = HashMap<char, HashMap<String, bool>>;

/// The agents plugin's `list` reply, the fields the badges need.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentSnap {
    agents: Vec<AgentRow>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AgentRow {
    pane: Option<i64>,
    status: String,
    shell: bool,
    ended_ms: Option<i64>,
    life: String,
    name: Option<String>,
    name_ms: Option<i64>,
    user_name: Option<String>,
    user_name_ms: Option<i64>,
}

/// What the rows show for a pane that holds an agent.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentInfo {
    pub status: String,
    /// Working only because a background shell still runs (orange).
    pub shell: bool,
    /// The name the agents picker shows: the user's, or the harness's,
    /// whichever was set last.
    pub name: String,
}

/// A title's decoration stripped: the glyph a harness puts in front
/// (`✳ task`), as the agents picker does.
fn unadorned(s: &str) -> &str {
    let s = s.trim_start();
    let keep = |c: char| c.is_alphanumeric() || "[({<\"'`_~/.$@#-".contains(c) || c.is_whitespace();
    let glyphs = s.chars().take_while(|&c| !keep(c)).count();
    if glyphs == 0 {
        return s;
    }
    let rest: &str = &s[s.char_indices().nth(glyphs).map(|(i, _)| i).unwrap_or(s.len())..];
    if rest.starts_with(char::is_whitespace) && !rest.trim_start().is_empty() {
        rest.trim_start()
    } else {
        s
    }
}

impl AgentRow {
    fn display_name(&self) -> String {
        let user = self.user_name.as_deref().map(unadorned).filter(|s| !s.is_empty());
        let harness = self.name.as_deref().map(unadorned).filter(|s| !s.is_empty());
        match (user, harness) {
            (Some(u), Some(h)) => {
                if self.user_name_ms.unwrap_or(0) >= self.name_ms.unwrap_or(0) { u } else { h }
            }
            (Some(u), None) => u,
            (None, Some(h)) => h,
            (None, None) => "",
        }
        .to_string()
    }
}

/// What a row stands for, keyed by its node key.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub kind: Kind,
    pub server: String,
    /// Local ids (a shadow's for a mirrored remote row).
    pub session: Option<u32>,
    pub window: Option<u32>,
    pub pane: Option<u32>,
    pub name: String,
    /// For a linked or unlinked remote session: the host and the
    /// session's name there.
    pub host: String,
    pub remote_name: String,
    /// The panes a window (or a session's current window) can preview:
    /// local ids, in layout order, with their indexes and the layout.
    pub panes: Vec<(u32, String)>,
    pub layout: String,
    /// The layout's pane ids (the owning server's) with their indexes,
    /// for the map above the preview.
    pub labels: Vec<(u32, String)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Kind {
    #[default]
    Server,
    Session,
    Window,
    Pane,
    /// A session on a remote server with no shadow here.
    Unlinked,
    /// A plugin on a linked server asking to call back here (a pending
    /// peer grant); `name` is the plugin, `host` the server.
    Peer,
}

pub struct Picker {
    pub engine: Engine,
    pub timer: Option<TaskId>,
    pub client: Option<u64>,
    pub client_name: Option<String>,
    /// The pane the picker was opened from ("you are here").
    pub here: Option<u32>,
    /// `s` (sessions folded) or `w` (windows shown).
    pub entry: char,
    pub local: Tree,
    /// (server, pane id on that server) -> the agent there.
    pub agents: HashMap<(String, u32), AgentInfo>,
    pub targets: HashMap<String, Target>,
    /// Which of a window's panes the preview shows, by node key.
    pub cycle: HashMap<String, usize>,
    pub pending_kill: Option<String>,
    /// The cursor has yet to land: on the pane the command targeted
    /// (`pick pane`, what the agents picker's flip runs) when
    /// `seek_pane`, else on the client's session.
    pub seek: bool,
    pub seek_pane: bool,
    pub now_ms: u64,
    pub down: HashMap<String, u64>,
    pub mismatch: HashMap<String, String>,
    pub fetching: HashMap<String, u64>,
}

/// Prompt tags.
const PROMPT_RENAME: u32 = 1;

pub fn keys() -> KeyTable {
    Engine::base_keys()
        .with("kill", "x", "rows", "kill the session / window / pane (asks)")
        .with("rename", "r", "rows", "rename the session or window")
        .with("zoom", "m", "rows", "maximise (zoom) the pane")
        .with("new", "S", "rows", "new session here (session_creator)")
        .with("worktree", "W", "rows", "new worktree session here (session_creator)")
        .with("menu", "Space", "rows", "the action menu")
        .with("drop", "d", "links", "drop the link to this host")
        .with("reconnect", "R", "links", "reconnect the link now")
        .with("allow", "a", "links", "allow a plugin asking to call back here")
        .with("deny", "D", "links", "deny it")
        .with("fold", "z", "moving", "fold / unfold the group under the cursor")
        .with("fold_all", "Z", "moving", "fold or unfold every group at this level")
        // `t` both ways: the agents picker's `t` lands here.
        .with("agents", "t", "picker", "the agents picker, on this row's pane")
        .note("rows", "Tab / BTab", "which pane the preview shows (a window row)")
        .note("links", "Enter", "on a session not linked here: remote-attach it")
}

pub fn sigils() -> Vec<SigilSpec> {
    vec![
        SigilSpec::new('@', "server", false, Some("S"), "narrow to a server (prefix)"),
        SigilSpec::new('#', "session", false, Some("s"), "narrow to a session (prefix)"),
        SigilSpec::new(':', "command", false, Some("c"), "narrow to a command (prefix)"),
        SigilSpec::new('~', "dir", true, None, "narrow to a directory (part of its ~ path)"),
        SigilSpec::new('!', "status", false, None, "narrow to an agent status: working, shell, waiting, needs_input"),
    ]
}

// ---------------------------------------------------------------------------
// open, refresh, render
// ---------------------------------------------------------------------------

pub async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    remotes: Rc<RefCell<Remotes>>,
    memo: Rc<RefCell<Memo>>,
    keys: KeyTable,
    client: Option<u64>,
    here: Option<u32>,
    entry: char,
    seek_pane: bool,
) {
    let Some(window) = listkit::window_for(client, here) else {
        let _ = display_message("sessions: no window to open the picker");
        return;
    };
    let size = SizeBox::default();
    let (ww, wh) = resolve_window(WindowId(window)).map(|wi| (wi.width, wi.height)).unwrap_or((size.max_w, size.max_h));
    let (mut width, mut height) = default_size(ww, wh, &size);
    if let Some(n) = tree::read_opt("@sessions-width").and_then(|v| v.parse::<u32>().ok()) {
        width = clamp_dim(n, size.min_w, ww);
    }
    if let Some(n) = tree::read_opt("@sessions-height").and_then(|v| v.parse::<u32>().ok()) {
        height = clamp_dim(n, size.min_h, wh);
    }
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
        title: Some(NAME.into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("sessions: open: {}", e.message));
            return;
        }
    };
    let client_name = client.and_then(|cid| {
        list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid).map(|c| c.name)
    });
    let mut engine = Engine::new(mode, width, height, NAME, keys, sigils());
    engine.size = size;
    engine.empty_text = "(no sessions)".into();
    engine.restore_expanded(memo.borrow().get(&entry).cloned().unwrap_or_default());
    let local = tree::snapshot(&Formats::load());
    let p = Picker {
        engine,
        timer: None,
        client,
        client_name,
        here,
        entry,
        local,
        agents: HashMap::new(),
        targets: HashMap::new(),
        cycle: HashMap::new(),
        pending_kill: None,
        seek: true,
        seek_pane,
        now_ms: now_ms(),
        down: HashMap::new(),
        mismatch: HashMap::new(),
        fetching: HashMap::new(),
    };
    {
        let mut b = picker.borrow_mut();
        *b = Some(p);
        let p = b.as_mut().unwrap();
        rebuild(p, &remotes.borrow());
        render(p);
    }
    // The timer: local tree, badges and remote trees on a cadence.
    let t = {
        let picker = Rc::clone(&picker);
        let remotes = Rc::clone(&remotes);
        spawn(async move {
            loop {
                if sleep_ms(REFRESH_MS).await.is_err() {
                    return;
                }
                if picker.borrow().is_none() {
                    return;
                }
                refresh_local(&picker, &remotes).await;
                fetch_remotes(Rc::clone(&picker), Rc::clone(&remotes), FETCH_DEBOUNCE_MS).await;
            }
        })
    };
    if let Some(p) = picker.borrow_mut().as_mut() {
        p.timer = Some(t);
    }
    fetch_agents(Rc::clone(&picker), Rc::clone(&remotes)).await;
    fetch_remotes(picker, remotes, 0).await;
}

pub fn close(picker: &Rc<RefCell<Option<Picker>>>) {
    if let Some(old) = picker.borrow_mut().take() {
        if let Some(t) = old.timer {
            cancel(t);
        }
        let _ = mode_close(old.engine.mode);
    }
}

pub fn render(p: &mut Picker) {
    p.engine.footer = format!(
        "j/k move · h/l {}/{} fold · Enter switch · {} kill · {} rename · {} actions · {} agents · / search · ? help · q close",
        p.engine.keys.key_of("fold"),
        p.engine.keys.key_of("fold_all"),
        p.engine.keys.key_of("kill"),
        p.engine.keys.key_of("rename"),
        p.engine.keys.key_of("menu"),
        p.engine.keys.key_of("agents"),
    );
    let (out, rect) = p.engine.render();
    let _ = mode_write(p.engine.mode, out.as_bytes());
    let _ = mode_preview(p.engine.mode, rect.as_ref());
}

/// Re-read the local tree and redraw, when the picker is open.
pub async fn refresh_local(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    if picker.borrow().is_none() {
        return;
    }
    let local = tree::snapshot(&Formats::load());
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    p.local = local;
    rebuild(p, &remotes.borrow());
    render(p);
}

/// Redraw from what is in hand (a remote tree landed, a link changed).
pub fn refresh_if_open(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    rebuild(p, &remotes.borrow());
    render(p);
}

/// Follow a server's `changed` topic.
pub fn follow(server: &str) {
    let _ = service::subscribe(&format!("@{server}"), provider::TOPIC);
}

/// Fetch the tree of every linked server, at most [`MAX_INFLIGHT`] at a
/// time, repainting as each lands.
pub async fn fetch_remotes(picker: Rc<RefCell<Option<Picker>>>, remotes: Rc<RefCell<Remotes>>, min_age_ms: u64) {
    let list = service::servers().unwrap_or_default();
    let mine = list.iter().find(|s| s.local).map(|s| s.version.clone()).unwrap_or_default();
    let queue = remotes.borrow_mut().claim(&list, min_age_ms, |s| {
        format!("sessions {} there, {} here; run tmux update", s.version, mine)
    });
    start_spinner(&picker, &remotes);
    if queue.is_empty() {
        return;
    }
    let lp = Rc::clone(&picker);
    let lr = Rc::clone(&remotes);
    listkit::remotes::fetch_all(
        Rc::clone(&remotes),
        queue,
        MAX_INFLIGHT,
        move |server| {
            Box::pin(async move {
                let mut t: Tree = service::call_json(&format!("@{server}"), "tree", &serde_json::json!({})).await?;
                t.server = server.clone();
                let now = t.now_ms;
                Ok((vec![t], now))
            })
        },
        move |_server| {
            let picker = Rc::clone(&lp);
            let remotes = Rc::clone(&lr);
            Box::pin(async move { refresh_if_open(&picker, &remotes) })
        },
    )
    .await;
    fetch_agents(picker, remotes).await;
}

fn start_spinner(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>) {
    if picker.borrow().is_none() {
        return;
    }
    let picker = Rc::clone(picker);
    let r2 = Rc::clone(remotes);
    listkit::remotes::start_spinner(remotes, move |fetching, now| {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return false };
        p.now_ms = now;
        p.fetching = fetching;
        rebuild(p, &r2.borrow());
        render(p);
        true
    });
}

/// Ask the agents plugin on every server which panes hold an agent, and
/// badge the rows. No agents plugin, or an old one: no badges.
pub async fn fetch_agents(picker: Rc<RefCell<Option<Picker>>>, remotes: Rc<RefCell<Remotes>>) {
    let servers: Vec<String> = {
        let r = remotes.borrow();
        let mut v: Vec<String> = r.servers.keys().filter(|s| !r.downs().contains_key(*s)).cloned().collect();
        v.sort();
        v.insert(0, LOCAL.to_string());
        v
    };
    let req = serde_json::json!({ "history": false, "archived": false });
    let mut map: HashMap<(String, u32), AgentInfo> = HashMap::new();
    for server in servers {
        let target = if server == LOCAL { "agents".to_string() } else { format!("agents@{server}") };
        let Ok(snap) = service::call_json::<_, AgentSnap>(&target, "list", &req).await else { continue };
        for a in snap.agents {
            let Some(pane) = a.pane else { continue };
            if a.ended_ms.is_some() || a.life == "archived" {
                continue;
            }
            let info = AgentInfo { status: a.status.clone(), shell: a.shell, name: a.display_name() };
            map.insert((server.clone(), pane as u32), info);
        }
    }
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    if p.agents != map {
        p.agents = map;
        rebuild(p, &remotes.borrow());
        render(p);
    }
}

// ---------------------------------------------------------------------------
// the tree as nodes
// ---------------------------------------------------------------------------

/// Worst first. `shell` is the agents picker's working-with-a-flag: the
/// turn ended but a background shell still runs, so it ranks under a
/// turn in progress and over an agent that waits on the user.
fn status_rank(s: &str) -> u8 {
    match s {
        "needs_input" => 4,
        "working" => 3,
        "shell" => 2,
        "waiting" => 1,
        _ => 0,
    }
}

fn badge(status: Option<&str>) -> Vec<Styled> {
    match status {
        Some("needs_input") => vec![('◉', ST_BOLD | ST_CODE), (' ', 0)],
        Some("working") => vec![('●', ST_CYAN), (' ', 0)],
        Some("shell") => vec![('●', ST_ORANGE), (' ', 0)],
        Some("waiting") => vec![('◍', ST_DIM), (' ', 0)],
        _ => Vec::new(),
    }
}

fn worst<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    if status_rank(b.unwrap_or("")) > status_rank(a.unwrap_or("")) { b } else { a }
}

/// Where a pane's agent is keyed: by the pane's id on the server that
/// runs it (a shadow pane through its remote id).
fn pane_agent<'a>(p: &'a Picker, server: &str, pn: &Pn) -> Option<&'a AgentInfo> {
    let id = if pn.remote_id.is_empty() { Some(pn.id) } else { id_of(&pn.remote_id) };
    p.agents.get(&(server.to_string(), id?))
}

fn pane_status<'a>(p: &'a Picker, server: &str, pn: &Pn) -> Option<&'a str> {
    pane_agent(p, server, pn).map(|a| if a.shell { "shell" } else { a.status.as_str() })
}

/// The agent's name for a row: a pane's own agent, or the one agent of a
/// one-pane window. The row then reads as the agents picker does.
fn agent_name_for_window<'a>(p: &'a Picker, server: &str, w: &Win) -> Option<&'a str> {
    if w.panes.len() != 1 {
        return None;
    }
    pane_agent(p, server, &w.panes[0]).map(|a| a.name.as_str()).filter(|n| !n.is_empty())
}

fn win_status<'a>(p: &'a Picker, server: &str, w: &Win) -> Option<&'a str> {
    w.panes.iter().fold(None, |acc, pn| worst(acc, pane_status(p, server, pn)))
}

fn sess_status<'a>(p: &'a Picker, server: &str, s: &Sess) -> Option<&'a str> {
    s.windows.iter().fold(None, |acc, w| worst(acc, win_status(p, server, w)))
}

/// One session (and its windows and panes) as nodes. `local` is the
/// local object that actions reach: the session itself, or the shadow
/// of a remote session; `None` for a remote session with no shadow.
#[allow(clippy::too_many_arguments)]
fn session_nodes(
    p: &Picker,
    out: &mut Vec<Node>,
    targets: &mut HashMap<String, Target>,
    server: &str,
    host: &str,
    depth: u8,
    s: &Sess,
    local: Option<&Sess>,
    dim: bool,
    home: &Option<String>,
) {
    let linked = local.is_some();
    let key_id = if linked || server == LOCAL { s.id } else { s.id };
    let skey = if linked || server == LOCAL {
        format!("{server}\u{1}${key_id}")
    } else {
        format!("{server}\u{1}~${key_id}")
    };
    // remote pane id -> local shadow pane id, for a mirrored session.
    let mut mirror: HashMap<u32, u32> = HashMap::new();
    if let Some(l) = local {
        if server != LOCAL {
            for w in &l.windows {
                for pn in &w.panes {
                    if let Some(rid) = id_of(&pn.remote_id) {
                        mirror.insert(rid, pn.id);
                    }
                }
            }
        }
    }
    let local_pane = |pid: u32| -> Option<u32> {
        if server == LOCAL {
            Some(pid)
        } else {
            mirror.get(&pid).copied()
        }
    };
    let status = sess_status(p, server, s);
    let mut left = badge(status);
    let text = if s.text.is_empty() { s.name.clone() } else { s.text.clone() };
    left.extend(plain_cells(&text, if linked || server == LOCAL { 0 } else { ST_DIM }));
    let mut right: Vec<Styled> = Vec::new();
    if let Some(l) = local.filter(|l| !l.remote_host.is_empty()) {
        let (g, st) = match l.remote_state.as_str() {
            "connected" => ('⇄', ST_DIM),
            "connecting" => ('…', ST_CODE),
            _ => ('⚠', ST_RED | ST_BOLD),
        };
        right.push((g, st));
        right.push((' ', 0));
    } else if !linked && server != LOCAL {
        right.extend(plain_cells("not linked ", ST_DIM));
    }
    if s.last_attached > 0 {
        let age = (p.now_ms / 1000).saturating_sub(s.last_attached as u64);
        right.extend(plain_cells(&fmt_age(age), ST_DIM));
    }
    let cur_win = s.windows.iter().find(|w| Some(w.id) == s.current_window).or(s.windows.first());
    let mut node = Node::group(skey.clone(), depth, p.entry == 'w', true);
    node.left = left;
    node.right = right;
    node.haystack = format!("{} {}", s.name, text);
    node.tokens = vec![('@', host.to_string()), ('#', s.name.clone())];
    if let Some(st) = status {
        node.tokens.push(('!', st.to_string()));
    }
    node.dim = dim;
    let here_sess = p.here.is_some()
        && local.map_or(false, |l| l.windows.iter().any(|w| w.panes.iter().any(|pn| Some(pn.id) == p.here)))
        && server == LOCAL;
    node.here = here_sess;
    let mut t = Target {
        kind: if linked || server == LOCAL { Kind::Session } else { Kind::Unlinked },
        server: server.to_string(),
        session: local.map(|l| l.id),
        window: None,
        pane: None,
        name: s.name.clone(),
        host: host.to_string(),
        remote_name: s.name.clone(),
        panes: Vec::new(),
        layout: String::new(),
        labels: Vec::new(),
    };
    if let Some(w) = cur_win {
        t.window = local.and_then(|l| l.windows.iter().find(|lw| lw.index == w.index).map(|lw| lw.id)).or(if server == LOCAL { Some(w.id) } else { None });
        t.pane = w.active_pane.and_then(local_pane);
        t.panes = w.panes.iter().filter_map(|pn| local_pane(pn.id).map(|lp| (lp, pn.index.to_string()))).collect();
        t.layout = w.layout.clone();
        t.labels = w.panes.iter().map(|pn| (pn.id, pn.index.to_string())).collect();
    }
    node.preview = preview_for(p, &t, &skey, cur_win, local.map(|l| l.remote_error.as_str()).unwrap_or(""), s);
    targets.insert(skey.clone(), t);
    out.push(node);

    for w in &s.windows {
        if w.hidden {
            continue;
        }
        let wkey = format!("{server}\u{1}@{}", w.id);
        let lw = local.and_then(|l| l.windows.iter().find(|lw| lw.index == w.index));
        let status = win_status(p, server, w);
        let mut left = badge(status);
        let text = if w.text.is_empty() { format!("{}: {}", w.index, w.name) } else { w.text.clone() };
        // A one-pane window that holds an agent is that agent: name it
        // as the agents picker does, with the window's own text after.
        if let Some(name) = agent_name_for_window(p, server, w) {
            left.extend(plain_cells(&format!("{}: {name}", w.index), 0));
            let rest = text.strip_prefix(&format!("{}: ", w.index)).unwrap_or(&text);
            left.extend(plain_cells(&format!("  ·  {rest}"), ST_DIM));
        } else {
            left.extend(plain_cells(&text, 0));
        }
        let mut right: Vec<Styled> = Vec::new();
        if w.zoomed {
            right.push(('Z', ST_BOLD));
        }
        if w.activity {
            right.push(('!', ST_CODE));
        }
        let mut node = Node::group(wkey.clone(), depth + 1, false, true);
        node.left = left;
        node.right = right;
        node.haystack = format!("{} {} {} {}", w.index, w.name, text, agent_name_for_window(p, server, w).unwrap_or(""));
        let cmd = w.panes.iter().find(|pn| pn.active).map(|pn| pn.cmd.clone()).unwrap_or_default();
        node.tokens = vec![(':', cmd)];
        if let Some(st) = status {
            node.tokens.push(('!', st.to_string()));
        }
        node.dim = dim;
        node.here = server == LOCAL && p.here.is_some() && w.panes.iter().any(|pn| Some(pn.id) == p.here);
        let t = Target {
            kind: Kind::Window,
            server: server.to_string(),
            session: local.map(|l| l.id),
            window: lw.map(|lw| lw.id).or(if server == LOCAL { Some(w.id) } else { None }),
            pane: w.active_pane.and_then(local_pane),
            name: w.name.clone(),
            host: host.to_string(),
            remote_name: s.name.clone(),
            panes: w.panes.iter().filter_map(|pn| local_pane(pn.id).map(|lp| (lp, pn.index.to_string()))).collect(),
            layout: w.layout.clone(),
            labels: w.panes.iter().map(|pn| (pn.id, pn.index.to_string())).collect(),
        };
        node.preview = preview_for(p, &t, &wkey, Some(w), "", s);
        targets.insert(wkey.clone(), t);
        out.push(node);

        for pn in &w.panes {
            if pn.hidden {
                continue;
            }
            let pkey = format!("{server}\u{1}%{}", pn.id);
            let agent = pane_agent(p, server, pn);
            let status = agent.map(|a| a.status.as_str());
            let mut left = badge(status);
            let text = if pn.text.is_empty() { format!("{}: {}", pn.index, pn.cmd) } else { pn.text.clone() };
            // A pane with an agent reads as the agents picker names it.
            match agent.map(|a| a.name.as_str()).filter(|n| !n.is_empty()) {
                Some(name) => {
                    left.extend(plain_cells(&format!("{}: {name}", pn.index), 0));
                    let rest = text.strip_prefix(&format!("{}: ", pn.index)).unwrap_or(&text);
                    left.extend(plain_cells(&format!("  ·  {rest}"), ST_DIM));
                }
                None => left.extend(plain_cells(&text, 0)),
            }
            let mut right: Vec<Styled> = Vec::new();
            if pn.dead {
                right.extend(plain_cells("dead ", ST_RED));
            }
            if pn.floating {
                right.extend(plain_cells("float ", ST_DIM));
            }
            if pn.active {
                right.push(('*', ST_DIM));
            }
            let mut node = Node::item(pkey.clone(), depth + 2);
            node.left = left;
            node.right = right;
            node.haystack = format!(
                "{} {} {} {} {}",
                pn.index,
                pn.cmd,
                tilde_of(home, &pn.path),
                pn.title,
                agent.map(|a| a.name.as_str()).unwrap_or("")
            );
            node.tokens = vec![(':', pn.cmd.clone()), ('~', tilde_of(home, &pn.path))];
            if let Some(st) = status {
                node.tokens.push(('!', st.to_string()));
            }
            node.dim = dim;
            node.here = server == LOCAL && Some(pn.id) == p.here;
            let lp = local_pane(pn.id);
            node.preview = match lp {
                Some(id) => Preview::Pane(PaneId(id)),
                None => Preview::Text(vec![plain_cells("not mirrored here", ST_DIM)]),
            };
            targets.insert(
                pkey.clone(),
                Target {
                    kind: Kind::Pane,
                    server: server.to_string(),
                    session: local.map(|l| l.id),
                    window: lw.map(|lw| lw.id).or(if server == LOCAL { Some(w.id) } else { None }),
                    pane: lp,
                    name: pn.cmd.clone(),
                    host: host.to_string(),
                    remote_name: s.name.clone(),
                    panes: Vec::new(),
                    layout: String::new(),
                    labels: Vec::new(),
                },
            );
            out.push(node);
        }
    }
}

/// The preview of a session or window row: its (chosen) pane under a
/// map of the window's panes; or text when nothing here mirrors it.
fn preview_for(p: &Picker, t: &Target, key: &str, w: Option<&Win>, error: &str, s: &Sess) -> Preview {
    let Some(w) = w else {
        return Preview::Text(vec![plain_cells("(no windows)", ST_DIM)]);
    };
    if t.panes.is_empty() {
        let mut lines: Vec<Vec<Styled>> = Vec::new();
        if t.kind == Kind::Unlinked {
            lines.push(plain_cells(&format!("{} on {}: not linked here", s.name, t.host), ST_BOLD));
            lines.push(plain_cells(&format!("Enter: remote-attach -t '{}' {}", s.name, t.host), ST_DIM));
        } else if !error.is_empty() {
            lines.push(plain_cells(&format!("disconnected: {error}"), ST_RED));
        } else {
            lines.push(plain_cells("not mirrored here", ST_DIM));
        }
        lines.push(Vec::new());
        for win in &s.windows {
            let text = if win.text.is_empty() { format!("{}: {}", win.index, win.name) } else { win.text.clone() };
            lines.push(plain_cells(&text, 0));
        }
        return Preview::Text(lines);
    }
    let n = t.panes.len();
    let which = p.cycle.get(key).copied().unwrap_or_else(|| {
        t.panes.iter().position(|(lp, _)| Some(*lp) == t.pane).unwrap_or(0)
    }) % n;
    let (pane, _) = t.panes[which];
    if n == 1 {
        return Preview::Pane(PaneId(pane));
    }
    let labels: HashMap<u32, String> = w.panes.iter().map(|pn| (pn.id, pn.index.to_string())).collect();
    // The strip is drawn in the layout's pane ids (the owning server's);
    // the chosen pane is known by its index there.
    let hl = w.panes.iter().find(|pn| pn.index.to_string() == t.panes[which].1).map(|pn| pn.id);
    let (sw, sh) = strip_size(p);
    match layout::strip(&w.layout, sw, sh, &labels, hl) {
        Some(mut lines) => {
            // The caption says what the boxes are and how to pick one.
            let label = &t.panes[which].1;
            lines.push(plain_cells(
                &format!("panes of this window, {} shown · Tab/BTab or a click picks another · l opens them as rows", label),
                ST_DIM,
            ));
            Preview::PaneBelow(PaneId(pane), lines)
        }
        None => Preview::Pane(PaneId(pane)),
    }
}

/// The map's size: the preview's width (capped) by the strip's rows.
fn strip_size(p: &Picker) -> (usize, usize) {
    let pw = (p.engine.width as usize).saturating_sub(p.engine.list_w() + 2).max(4);
    (pw.min(60), STRIP_H - 1)
}

/// A click on the map above the preview: the box under it becomes the
/// previewed pane. Returns whether anything changed.
fn pick_pane_at(p: &mut Picker, x: u32, y: u32) -> bool {
    let Some(key) = p.engine.selected_key() else { return false };
    let Some(t) = p.targets.get(&key) else { return false };
    if t.panes.len() < 2 || t.layout.is_empty() {
        return false;
    }
    let labels: HashMap<u32, String> = t.labels.iter().cloned().collect();
    let (sw, sh) = strip_size(p);
    let Some(id) = layout::pane_at(&t.layout, sw, sh, &labels, x as usize, y as usize) else { return false };
    let Some(label) = labels.get(&id) else { return false };
    let Some(which) = t.panes.iter().position(|(_, l)| l == label) else { return false };
    p.cycle.insert(key, which);
    p.engine.status = Some(format!("preview: pane {label}"));
    true
}

/// Rebuild the engine's nodes from the local tree, the remote trees and
/// the badges.
pub fn rebuild(p: &mut Picker, remotes: &Remotes) {
    p.now_ms = now_ms();
    p.down = remotes.downs();
    p.mismatch = remotes.mismatch.clone();
    let home = home_dir().ok();
    let mut nodes: Vec<Node> = Vec::new();
    let mut targets: HashMap<String, Target> = HashMap::new();

    // Servers: local, then every host a shadow names or a provider
    // answered for, by name.
    let mut hosts: Vec<String> = remotes.servers.keys().cloned().collect();
    for s in &p.local.sessions {
        if !s.remote_host.is_empty() && !hosts.contains(&s.remote_host) {
            hosts.push(s.remote_host.clone());
        }
    }
    for h in p.mismatch.keys().chain(p.fetching.keys()) {
        if !hosts.contains(h) {
            hosts.push(h.clone());
        }
    }
    hosts.sort();
    hosts.dedup();
    let multi = !hosts.is_empty();
    let depth = if multi { 1 } else { 0 };

    // The client's session, first.
    let cur_session = p.client.and_then(|cid| list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)?.session);
    let order = |a: &Sess, b: &Sess| {
        let ca = Some(a.id) == cur_session;
        let cb = Some(b.id) == cur_session;
        cb.cmp(&ca).then(b.last_attached.cmp(&a.last_attached)).then(a.name.cmp(&b.name))
    };

    // The peer grant table, for the rows that ask. Built without the
    // `peers` feature (for a host older than the import) there are none.
    #[cfg(feature = "peers")]
    let grants: Vec<PeerGrant> = service::peers().unwrap_or_default();
    #[cfg(not(feature = "peers"))]
    let grants: Vec<PeerGrant> = Vec::new();

    let local_host = p.local.host.clone();
    if multi {
        let mut n = Node::group(format!("srv\u{1}{LOCAL}"), 0, true, false);
        n.left = plain_cells(if local_host.is_empty() { "local" } else { &local_host }, ST_CYAN);
        n.tokens = vec![('@', LOCAL.into()), ('@', local_host.clone())];
        n.haystack = format!("local {local_host}");
        targets.insert(n.key.clone(), Target { kind: Kind::Server, server: LOCAL.into(), host: local_host.clone(), ..Default::default() });
        nodes.push(n);
    }
    let mut locals: Vec<&Sess> = p.local.sessions.iter().filter(|s| s.remote_host.is_empty() && !s.hidden).collect();
    locals.sort_by(|a, b| order(a, b));
    let local_tree = p.local.clone();
    for s in locals {
        session_nodes(p, &mut nodes, &mut targets, LOCAL, &local_host, depth, s, Some(s), false, &home);
    }

    for host in &hosts {
        let tree = remotes.servers.get(host).and_then(|r| r.rows.first());
        let down = p.down.get(host).copied();
        let shadows: Vec<&Sess> = local_tree.sessions.iter().filter(|s| s.remote_host == *host).collect();
        // The header, with the link state.
        let (label, st) = match (down, p.mismatch.get(host), spin_since(&p.fetching, host, p.now_ms)) {
            (Some(since), _, _) => (format!("{host}  (disconnected {})", fmt_age(p.now_ms.saturating_sub(since) / 1000)), ST_RED),
            (None, Some(why), _) => (format!("{host}  ({why})"), ST_CODE),
            (None, None, Some(since)) => {
                let waited = p.now_ms.saturating_sub(since);
                let frame = SPIN_FRAMES[(p.now_ms / SPIN_MS) as usize % SPIN_FRAMES.len()];
                if waited >= FETCH_STUCK_MS {
                    (format!("{host}  {frame} (fetching {})", fmt_age(waited / 1000)), ST_CODE)
                } else {
                    (format!("{host}  {frame}"), ST_CYAN)
                }
            }
            _ => {
                let disc = shadows.iter().filter(|s| s.remote_state == "disconnected").count();
                if disc > 0 && disc == shadows.len() && tree.is_none() {
                    let why = shadows.iter().map(|s| s.remote_error.as_str()).find(|e| !e.is_empty()).unwrap_or("");
                    (format!("{host}  (disconnected{})", if why.is_empty() { String::new() } else { format!(": {}", clip(why, 40)) }), ST_RED)
                } else {
                    (host.clone(), ST_CYAN)
                }
            }
        };
        let mut n = Node::group(format!("srv\u{1}{host}"), 0, true, false);
        n.left = plain_cells(&label, st);
        n.tokens = vec![('@', host.clone())];
        n.haystack = host.clone();
        let mut info: Vec<Vec<Styled>> = vec![plain_cells(host, ST_BOLD)];
        if let Some(t) = tree {
            info.push(plain_cells(&format!("{} sessions, host {}", t.sessions.len(), t.host), 0));
        }
        for s in &shadows {
            info.push(plain_cells(&format!("{}: {}{}", s.name, s.remote_state, if s.remote_error.is_empty() { String::new() } else { format!(" ({})", s.remote_error) }), ST_DIM));
        }
        let asks: Vec<&PeerGrant> = grants.iter().filter(|g| g.server == *host && g.state == "pending").collect();
        let allowed: Vec<&str> = grants.iter().filter(|g| g.server == *host && g.state == "allow").map(|g| g.plugin.as_str()).collect();
        if !allowed.is_empty() {
            info.push(plain_cells(&format!("may call back here: {}", allowed.join(", ")), ST_DIM));
        }
        n.preview = Preview::Text(info);
        targets.insert(n.key.clone(), Target { kind: Kind::Server, server: host.clone(), host: host.clone(), ..Default::default() });
        nodes.push(n);

        // A plugin there asking to call this server back: one row per
        // request, allow or deny with a key.
        for g in asks {
            let key = format!("srv\u{1}{host}\u{1}peer\u{1}{}", g.plugin);
            let mut n = Node::item(key.clone(), depth);
            n.left = vec![('⚿', ST_CODE | ST_BOLD), (' ', 0)];
            n.left.extend(plain_cells(&format!("{} asks to call back here", g.plugin), 0));
            n.right = plain_cells("pending", ST_CODE);
            n.haystack = format!("{} peer pending", g.plugin);
            n.tokens = vec![('@', host.clone())];
            n.preview = Preview::Text(vec![
                plain_cells(&format!("{} on {} wants to call its {} here", g.plugin, host, g.plugin), ST_BOLD),
                Vec::new(),
                plain_cells(&format!("{}: allow   {}: deny", p.engine.keys.key_of("allow"), p.engine.keys.key_of("deny")), 0),
                plain_cells(&format!("(plugin-peers allow {} {})", host, g.plugin), ST_DIM),
            ]);
            targets.insert(key, Target { kind: Kind::Peer, server: host.clone(), host: host.clone(), name: g.plugin.clone(), ..Default::default() });
            nodes.push(n);
        }

        match tree {
            Some(t) => {
                // The remote's sessions, each folded with its shadow.
                let mut sessions: Vec<&Sess> = t.sessions.iter().filter(|s| !s.hidden).collect();
                sessions.sort_by(|a, b| b.last_attached.cmp(&a.last_attached).then(a.name.cmp(&b.name)));
                for s in sessions {
                    let shadow = shadows.iter().copied().find(|sh| id_of(&sh.remote_id) == Some(s.id));
                    let dim = down.is_some() || shadow.map_or(false, |sh| sh.remote_state != "connected");
                    session_nodes(p, &mut nodes, &mut targets, host, host, depth, s, shadow, dim, &home);
                }
                // A shadow whose session the remote no longer lists.
                for sh in &shadows {
                    if t.sessions.iter().any(|s| Some(s.id) == id_of(&sh.remote_id)) {
                        continue;
                    }
                    session_nodes(p, &mut nodes, &mut targets, host, host, depth, sh, Some(sh), true, &home);
                }
            }
            None => {
                // No provider answered (yet, or ever): the shadows alone,
                // from local data.
                for sh in &shadows {
                    let dim = sh.remote_state != "connected";
                    session_nodes(p, &mut nodes, &mut targets, host, host, depth, sh, Some(sh), dim, &home);
                }
            }
        }
    }

    let n_sessions = targets.values().filter(|t| matches!(t.kind, Kind::Session | Kind::Unlinked)).count();
    let n_agents = p.agents.len();
    let mut tag = format!("({n_sessions} sessions");
    if multi {
        tag.push_str(&format!(", {} servers", hosts.len() + 1));
    }
    if n_agents > 0 {
        tag.push_str(&format!(", {n_agents} agents"));
    }
    if !p.engine.marked().is_empty() {
        tag.push_str(&format!(", {} selected", p.engine.marked().len()));
    }
    tag.push(')');
    p.engine.header_tag = tag;
    p.targets = targets;
    p.engine.set_nodes(nodes);
    if p.seek {
        // Land on the pane the command targeted (the agents picker's
        // flip names the agent's pane), else on the client's session
        // (its current window in `w`).
        let here_key = p.here.filter(|_| p.seek_pane).and_then(|pane| {
            p.targets.iter().find(|(_, t)| t.kind == Kind::Pane && t.pane == Some(pane)).map(|(k, _)| k.clone())
        });
        if let Some(k) = here_key {
            if p.engine.reveal_key(&k) {
                p.seek = false;
            }
        }
        let want = cur_session.filter(|_| p.seek).and_then(|sid| {
            p.targets.iter().find(|(_, t)| {
                t.session == Some(sid)
                    && if p.entry == 'w' { t.kind == Kind::Window && t.window == p.local.sessions.iter().find(|s| s.id == sid).and_then(|s| s.current_window) } else { t.kind == Kind::Session }
            }).map(|(k, _)| k.clone())
        });
        if let Some(k) = want {
            if p.engine.reveal_key(&k) {
                p.seek = false;
            }
        } else {
            p.seek = false;
        }
    }
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

pub fn on_mode_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    remotes: &Rc<RefCell<Remotes>>,
    memo: &Rc<RefCell<Memo>>,
    busy: &Rc<Cell<bool>>,
    event: &Event,
) {
    let mode_id = event.get_i64("mode");
    let key = event.get_str("key").unwrap_or("").to_string();
    let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
        (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
        _ => None,
    };
    dispatch_key(picker, remotes, memo, busy, mode_id, &key, mouse);
}

pub fn on_menu_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    remotes: &Rc<RefCell<Remotes>>,
    memo: &Rc<RefCell<Memo>>,
    busy: &Rc<Cell<bool>>,
    key: &str,
    mouse: Option<(u32, u32)>,
) {
    let mode_id = picker.borrow().as_ref().map(|p| p.engine.mode.0 as i64);
    if mode_id.is_none() {
        return;
    }
    dispatch_key(picker, remotes, memo, busy, mode_id, key, mouse);
}

pub fn on_mode_nav(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>, memo: &Rc<RefCell<Memo>>, busy: &Rc<Cell<bool>>, event: &Event) {
    let dir = event.get_str("dir").unwrap_or("").to_string();
    let outcome = {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if event.get_i64("mode") != Some(p.engine.mode.0 as i64) || busy.get() {
            return;
        }
        p.engine.handle_nav(&dir)
    };
    execute(picker, remotes, memo, busy, outcome);
}

pub fn on_mode_paste(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>, memo: &Rc<RefCell<Memo>>, busy: &Rc<Cell<bool>>, event: &Event) {
    let outcome = {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
            return;
        }
        let Some(text) = event.get_str("text") else { return };
        p.engine.handle_paste(text)
    };
    execute(picker, remotes, memo, busy, outcome);
}

pub fn on_mode_resize(picker: &Rc<RefCell<Option<Picker>>>, event: &Event) {
    let mut b = picker.borrow_mut();
    let Some(p) = b.as_mut() else { return };
    if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
        return;
    }
    let w = event.get_i64("width").map(|v| v as u32).unwrap_or(p.engine.width);
    let h = event.get_i64("height").map(|v| v as u32).unwrap_or(p.engine.height);
    p.engine.resize(w, h);
    render(p);
}

pub fn on_mode_closed(picker: &Rc<RefCell<Option<Picker>>>, memo: &Rc<RefCell<Memo>>, event: &Event) {
    let mut b = picker.borrow_mut();
    if b.as_ref().is_some_and(|p| event.get_i64("mode") == Some(p.engine.mode.0 as i64)) {
        if let Some(p) = b.as_ref() {
            if let Some(t) = p.timer {
                cancel(t);
            }
            memo.borrow_mut().insert(p.entry, p.engine.expanded_overrides().clone());
        }
        *b = None;
    }
}

fn dispatch_key(
    picker: &Rc<RefCell<Option<Picker>>>,
    remotes: &Rc<RefCell<Remotes>>,
    memo: &Rc<RefCell<Memo>>,
    busy: &Rc<Cell<bool>>,
    mode_id: Option<i64>,
    key: &str,
    mouse: Option<(u32, u32)>,
) {
    let outcome = {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        if mode_id != Some(p.engine.mode.0 as i64) || busy.get() {
            return;
        }
        // Any key but the kill key again cancels a pending kill.
        let pending = p.pending_kill.take();
        let o = p.engine.handle_key(key, mouse);
        if let Outcome::Action("kill", _) = &o {
            p.pending_kill = pending;
        }
        o
    };
    execute(picker, remotes, memo, busy, outcome);
}

/// What the picker does with an outcome. Decided under the borrow,
/// run after it: a tmux command is async and must not hold the picker.
fn execute(picker: &Rc<RefCell<Option<Picker>>>, remotes: &Rc<RefCell<Remotes>>, memo: &Rc<RefCell<Memo>>, busy: &Rc<Cell<bool>>, outcome: Outcome) {
    // A command to run after the borrow, and whether to close first.
    let mut run: Option<String> = None;
    let mut close_first = false;
    let mut after_run_refresh = false;
    {
        let mut b = picker.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        match outcome {
            Outcome::Nothing => return,
            Outcome::Redraw | Outcome::FilterChanged => {
                render(p);
                return;
            }
            Outcome::Close => {
                close_first = true;
            }
            Outcome::Resize(w, h) => {
                let _ = mode_resize(p.engine.mode, w, h);
                let _ = set_option("@sessions-width", &w.to_string());
                let _ = set_option("@sessions-height", &h.to_string());
                return;
            }
            Outcome::Expanded(key, open) => {
                memo.borrow_mut().entry(p.entry).or_default().insert(key, open);
                render(p);
                return;
            }
            Outcome::PreviewKey(pane, key) => {
                if let Some(text) = key.strip_prefix("\u{0}paste:") {
                    let _ = send_text(pane, text);
                } else {
                    let _ = send_key(pane, &key);
                }
                poke_preview(picker, p);
                return;
            }
            Outcome::PreviewWheel(pane, key, x, y) => {
                let _ = pane_mouse(pane, &key, x, y, p.client);
                poke_preview(picker, p);
                return;
            }
            Outcome::PreviewHeaderClick(x, y) => {
                if pick_pane_at(p, x, y) {
                    rebuild(p, &remotes.borrow());
                }
                render(p);
                return;
            }
            Outcome::Activate(key) => {
                let Some(t) = p.targets.get(&key).cloned() else { return };
                match t.kind {
                    Kind::Unlinked => {
                        run = Some(format!("remote-attach -t '{}' '{}'", safe(&t.remote_name), safe(&t.host)));
                        p.engine.status = Some(format!("linking {} on {}…", t.remote_name, t.host));
                        after_run_refresh = true;
                        render(p);
                    }
                    Kind::Server => return,
                    _ => {
                        let Some(cmd) = switch_cmd(p, &t) else {
                            p.engine.status = Some("nothing here to switch to".into());
                            render(p);
                            return;
                        };
                        run = Some(cmd);
                        close_first = true;
                    }
                }
            }
            Outcome::Prompt(PROMPT_RENAME, text) => {
                let Some(t) = p.engine.selected_key().and_then(|k| p.targets.get(&k).cloned()) else { return };
                if text.is_empty() || text.contains('\'') {
                    p.engine.status = Some("no name, or a quote I will not paste".into());
                    render(p);
                    return;
                }
                run = match (t.kind, t.session, t.window) {
                    (Kind::Session, Some(s), _) => Some(format!("rename-session -t '${s}' '{text}'")),
                    (Kind::Window, _, Some(w)) => Some(format!("rename-window -t '@{w}' '{text}'")),
                    _ => None,
                };
                if run.is_none() {
                    p.engine.status = Some("only a session or a window here can be renamed".into());
                }
                after_run_refresh = true;
                render(p);
            }
            Outcome::Prompt(_, _) => return,
            Outcome::Key(k) => {
                if k == "Tab" || k == "BTab" {
                    if cycle_preview(p, if k == "Tab" { 1 } else { -1 }) {
                        rebuild(p, &remotes.borrow());
                    }
                    render(p);
                    return;
                }
                // A user key: `@sessions-key-<K>` is a tmux command
                // expanded against the highlighted row's own object.
                let Some(cmd) = tree::read_opt(&format!("@sessions-key-{k}")) else {
                    return;
                };
                let Some(t) = p.engine.selected_key().and_then(|k| p.targets.get(&k).cloned()) else { return };
                let target = match (t.kind, t.pane, t.window, t.session) {
                    (Kind::Pane, Some(id), _, _) => Some(OptionTarget::Pane(PaneId(id))),
                    (Kind::Window, _, Some(id), _) => Some(OptionTarget::Window(WindowId(id))),
                    (Kind::Session, _, _, Some(id)) => Some(OptionTarget::Session(SessionId(id))),
                    _ => None,
                };
                let Some(target) = target else {
                    p.engine.status = Some("that row has no local object to run a command on".into());
                    render(p);
                    return;
                };
                match format_expand(target, &cmd) {
                    Ok(c) => {
                        p.engine.status = Some(format!("ran {}", clip(&c, 40)));
                        run = Some(c);
                        after_run_refresh = true;
                    }
                    Err(e) => p.engine.status = Some(format!("@sessions-key-{k}: {}", e.message)),
                }
                render(p);
            }
            Outcome::Action(name, keys) => {
                let Some(key) = keys.first().cloned() else { return };
                let Some(t) = p.targets.get(&key).cloned() else { return };
                match name {
                    "kill" => {
                        if keys.len() > 1 {
                            p.engine.status = Some("kill takes one row at a time".into());
                            render(p);
                            return;
                        }
                        let Some(cmd) = kill_cmd(p, &t) else {
                            render(p);
                            return;
                        };
                        if p.pending_kill.as_deref() == Some(&key) {
                            p.pending_kill = None;
                            p.engine.status = Some(format!("killing {}", t.name));
                            run = Some(cmd);
                            after_run_refresh = true;
                        } else {
                            p.pending_kill = Some(key.clone());
                            let what = match t.kind {
                                Kind::Session if !t.host.is_empty() && t.server != LOCAL => "session (drops its link)",
                                Kind::Session => "session",
                                Kind::Window => "window",
                                Kind::Pane => "pane",
                                _ => "row",
                            };
                            p.engine.status = Some(format!("kill {what} {}? {} again to confirm", t.name, p.engine.keys.key_of("kill")));
                        }
                        render(p);
                    }
                    "rename" => {
                        if matches!(t.kind, Kind::Session | Kind::Window) && t.session.is_some() {
                            p.engine.open_prompt("rename", &t.name, PROMPT_RENAME);
                        } else {
                            p.engine.status = Some("only a session or a window here can be renamed".into());
                        }
                        render(p);
                    }
                    "zoom" => match (t.kind, t.pane) {
                        (Kind::Pane, Some(id)) => {
                            run = Some(format!("resize-pane -Z -t '%{id}'"));
                            after_run_refresh = true;
                        }
                        _ => {
                            p.engine.status = Some("zoom takes a pane row".into());
                            render(p);
                        }
                    },
                    "new" | "worktree" => {
                        let Some(pane) = t.pane else {
                            p.engine.status = Some("no local pane to start from here".into());
                            render(p);
                            return;
                        };
                        let what = if name == "new" { "new" } else { "worktree" };
                        run = Some(format!("plugin-command -t '%{pane}' session_creator {what}"));
                        close_first = true;
                    }
                    "menu" => {
                        run = Some(menu_cmd(p, &t));
                    }
                    "drop" | "reconnect" => {
                        if t.host.is_empty() || t.server == LOCAL {
                            p.engine.status = Some("not a remote row".into());
                            render(p);
                            return;
                        }
                        // On a session row, that session's link alone; on
                        // the server header, every link to the host.
                        let which = if t.kind == Kind::Server || t.remote_name.is_empty() {
                            String::new()
                        } else {
                            format!("-t '{}' ", safe(&t.remote_name))
                        };
                        let flag = if name == "drop" { "-k" } else { "-R" };
                        run = Some(format!("remote-attach {flag} {which}'{}'", safe(&t.host)));
                        p.engine.status = Some(if name == "drop" {
                            format!("dropping the link to {}", t.host)
                        } else {
                            format!("reconnecting {}", t.host)
                        });
                        after_run_refresh = true;
                        render(p);
                    }
                    "agents" => {
                        let target = t.pane.map(|id| format!("-t '%{id}' ")).unwrap_or_default();
                        run = Some(format!("plugin-command {target}agents pick"));
                        close_first = true;
                    }
                    "allow" | "deny" => {
                        if t.kind != Kind::Peer {
                            p.engine.status = Some("not a peer request row".into());
                            render(p);
                            return;
                        }
                        run = Some(format!("plugin-peers {name} '{}' '{}'", safe(&t.host), safe(&t.name)));
                        p.engine.status = Some(format!("{name}: {} on {}", t.name, t.host));
                        after_run_refresh = true;
                        render(p);
                    }
                    _ => return,
                }
            }
        }
    }
    if close_first {
        close(picker);
    }
    let Some(cmd) = run else { return };
    let picker = Rc::clone(picker);
    let remotes = Rc::clone(remotes);
    let busy = Rc::clone(busy);
    let _ = memo;
    spawn(async move {
        busy.set(true);
        let r = run_command(&cmd).await;
        busy.set(false);
        if let Err(e) = r {
            log(&format!("sessions: {cmd}: {}", e.message));
            if let Some(p) = picker.borrow_mut().as_mut() {
                p.engine.status = Some(format!("failed: {}", e.message));
                render(p);
            }
        }
        if after_run_refresh {
            // Give tmux a beat to apply it, then re-read.
            let _ = sleep_ms(80).await;
            refresh_local(&picker, &remotes).await;
        }
    });
}

/// Which of a window's panes the preview shows: the next (or previous)
/// one. Returns whether it changed (the caller rebuilds the nodes).
fn cycle_preview(p: &mut Picker, delta: i32) -> bool {
    let Some(key) = p.engine.selected_key() else { return false };
    let Some(t) = p.targets.get(&key) else { return false };
    if t.panes.len() < 2 {
        p.engine.status = Some("one pane here".into());
        return false;
    }
    let n = t.panes.len() as i32;
    let cur = p.cycle.get(&key).copied().unwrap_or_else(|| t.panes.iter().position(|(lp, _)| Some(*lp) == t.pane).unwrap_or(0)) as i32;
    let next = ((cur + delta) % n + n) as usize % t.panes.len();
    let label = t.panes[next].1.clone();
    p.cycle.insert(key, next);
    p.engine.status = Some(format!("preview: pane {label}"));
    true
}

const POKE_MS: [u64; 2] = [40, 160];

/// Bring the preview's next frame forward after a typed key.
fn poke_preview(picker: &Rc<RefCell<Option<Picker>>>, p: &Picker) {
    let mode = p.engine.mode;
    let Some(rect) = p.engine.preview_rect() else { return };
    let picker = Rc::clone(picker);
    spawn(async move {
        for ms in POKE_MS {
            if sleep_ms(ms).await.is_err() {
                return;
            }
            let same = picker.borrow().as_ref().is_some_and(|p| p.engine.mode.0 == mode.0 && p.engine.preview_pane() == Some(rect.pane));
            if !same {
                return;
            }
            let _ = mode_preview(mode, Some(&rect));
        }
    });
}

/// A value safe inside single quotes in a tmux command: quotes out.
fn safe(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\'' | '\\' | ';' | '#')).collect()
}

/// `switch-client` to the row: a pane target selects session, window
/// and pane at once; a session row goes to its current window's pane.
fn switch_cmd(p: &Picker, t: &Target) -> Option<String> {
    let target = match (t.pane, t.window, t.session) {
        (Some(id), _, _) => format!("%{id}"),
        (None, Some(id), _) => format!("@{id}"),
        (None, None, Some(id)) => format!("${id}"),
        _ => return None,
    };
    let client = p.client_name.as_deref().map(|c| format!("-c '{}' ", safe(c))).unwrap_or_default();
    Some(format!("switch-client {client}-t '{target}'"))
}

/// The kill for a row, or `None` with the reason in the status: the
/// attached shadow session (killing it drops the link, and under
/// `detach-on-destroy` the client would exit with it) is switched away
/// from first, and refused when there is nowhere to go.
fn kill_cmd(p: &mut Picker, t: &Target) -> Option<String> {
    match (t.kind, t.pane, t.window, t.session) {
        (Kind::Pane, Some(id), _, _) => Some(format!("kill-pane -t '%{id}'")),
        (Kind::Window, _, Some(id), _) => Some(format!("kill-window -t '@{id}'")),
        (Kind::Session, _, _, Some(id)) => {
            let shadow = t.server != LOCAL;
            let client_here = p.client.and_then(|cid| list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)?.session) == Some(id);
            if shadow && client_here {
                let other = p.local.sessions.iter().find(|s| s.id != id && s.remote_host.is_empty()).map(|s| s.id);
                let Some(other) = other else {
                    p.engine.status = Some(format!("no other local session to switch to first; use {} to drop the link", p.engine.keys.key_of("drop")));
                    return None;
                };
                let client = p.client_name.as_deref().map(|c| format!("-c '{}' ", safe(c))).unwrap_or_default();
                return Some(format!("switch-client {client}-t '${other}' ; kill-session -t '${id}'"));
            }
            Some(format!("kill-session -t '${id}'"))
        }
        (Kind::Unlinked, ..) => {
            p.engine.status = Some("not linked here: nothing local to kill".into());
            None
        }
        _ => {
            p.engine.status = Some("nothing to kill here".into());
            None
        }
    }
}

/// The action menu for the row: every bound action, dimmed when it does
/// not apply, each item handing its key back through `menu-key`.
fn menu_cmd(p: &Picker, t: &Target) -> String {
    let k = &p.engine.keys;
    let remote = !t.host.is_empty() && t.server != LOCAL;
    let mut items = String::new();
    items.push_str(&menu_item(NAME, "switch", k.key_of("activate"), t.kind != Kind::Server));
    items.push_str(&menu_item(NAME, "rename", k.key_of("rename"), matches!(t.kind, Kind::Session | Kind::Window) && t.session.is_some()));
    items.push_str(&menu_item(NAME, "kill", k.key_of("kill"), matches!(t.kind, Kind::Session | Kind::Window | Kind::Pane)));
    items.push_str(&menu_item(NAME, "zoom pane", k.key_of("zoom"), t.kind == Kind::Pane));
    items.push_str(&menu_item(NAME, "new session here", k.key_of("new"), t.pane.is_some()));
    items.push_str(&menu_item(NAME, "new worktree session here", k.key_of("worktree"), t.pane.is_some()));
    items.push_str(&menu_item(NAME, "agents picker on this pane", k.key_of("agents"), true));
    items.push_str(" '' ");
    items.push_str(&menu_item(NAME, "drop the link", k.key_of("drop"), remote));
    items.push_str(&menu_item(NAME, "reconnect the link", k.key_of("reconnect"), remote));
    items.push_str(&menu_item(NAME, "allow the plugin to call back", k.key_of("allow"), t.kind == Kind::Peer));
    items.push_str(&menu_item(NAME, "deny it", k.key_of("deny"), t.kind == Kind::Peer));
    let client = p.client_name.as_deref().map(|c| format!("-c '{}' ", safe(c))).unwrap_or_default();
    format!("display-menu {client}-T ' {} ' -x C -y C{items}", menu_safe(&t.name, 24))
}
