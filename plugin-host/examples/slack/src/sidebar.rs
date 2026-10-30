//! The sidebar: a listkit picker over the workspace's conversations,
//! grouped the way the user's own Slack sidebar is (its sections, then
//! channels, then DMs), each row carrying its badge, the preview column
//! showing the channel's recent messages. Enter opens the channel view.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use listkit::keys::KeyTable;
use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::styled::{plain_cells, wrap_cells, Styled, ST_BOLD, ST_BRIGHT, ST_CYAN, ST_DIM, ST_INVERT, ST_RED};
use listkit::text::{keyname, menu_item, menu_safe};
use listkit::{Engine, Node, Outcome, Preview, SigilSpec};
use tmux_plugin_sdk::prelude::*;

use crate::store::{Channel, Kind, Message};
use crate::{api, chat, mrkdwn, timefmt, Shared, State, NAME};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Sections,
    Unread,
    All,
}

impl View {
    fn next(self) -> View {
        match self {
            View::Sections => View::Unread,
            View::Unread => View::All,
            View::All => View::Sections,
        }
    }
    fn label(self) -> &'static str {
        match self {
            View::Sections => "sections",
            View::Unread => "unread",
            View::All => "all",
        }
    }
    fn parse(s: &str) -> View {
        match s.trim() {
            "unread" => View::Unread,
            "all" => View::All,
            _ => View::Sections,
        }
    }
}

pub struct Sidebar {
    pub engine: Engine,
    pub client: Option<u64>,
    pub pane: Option<u32>,
    pub view: View,
    /// Recent messages per channel, for the preview column.
    pub previews: HashMap<String, Vec<Message>>,
    pub fetching: HashSet<String>,
}

pub fn keys() -> KeyTable {
    Engine::base_keys()
        .with("open_app", "o", "rows", "open the channel in the Slack app")
        .with("mark", "m", "rows", "mark the channel read")
        .with("compose", "n", "rows", "compose a message to the channel")
        .with("menu", "Space", "rows", "the action menu")
        .with("view", "v", "picker", "sections / unread / all")
        .with("refresh", "R", "picker", "fetch channels, sections, users and badges again")
        .with("fold", "z", "moving", "fold / unfold the section under the cursor")
        .with("fold_all", "Z", "moving", "fold or unfold every section")
        .note("rows", "Enter", "open the channel view")
}

pub fn sigils() -> Vec<SigilSpec> {
    vec![
        SigilSpec::new('#', "channel", false, Some("c"), "narrow to a channel (prefix)"),
        SigilSpec::new('@', "user", false, Some("u"), "narrow to a DM with a user (prefix)"),
        SigilSpec::new('!', "state", false, None, "narrow to a state: unread, mention, dm, muted, starred"),
    ]
}

fn size_box() -> SizeBox {
    SizeBox { min_w: 70, min_h: 14, max_w: 200, max_h: 60, fill_tenths: 9 }
}

fn opt_u32(name: &str) -> Option<u32> {
    get_option(name).ok().and_then(|v| v.trim().parse::<u32>().ok()).filter(|v| *v > 0)
}

pub fn open(st: &Shared, client: Option<u64>, pane: Option<u32>) {
    close(st);
    let Some(window) = listkit::window_for(client, pane) else {
        let _ = display_message("slack: no window to open the sidebar in");
        return;
    };
    let size = size_box();
    let (ww, wh) = resolve_window(WindowId(window)).map(|w| (w.width, w.height)).unwrap_or((size.max_w, size.max_h));
    let (mut width, mut height) = default_size(ww, wh, &size);
    if let Some(n) = opt_u32("@slack-width") {
        width = clamp_dim(n, size.min_w, ww);
    }
    if let Some(n) = opt_u32("@slack-height") {
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
            let _ = display_message(&format!("slack: open: {}", e.message));
            return;
        }
    };
    let keys = st.borrow().keys.clone();
    let mut engine = Engine::new(mode, width, height, NAME, keys, sigils());
    engine.size = size;
    engine.spacers = true;
    let view = View::parse(&get_option("@slack-view").unwrap_or_default());
    {
        let mut s = st.borrow_mut();
        s.side = Some(Sidebar { engine, client, pane, view, previews: HashMap::new(), fetching: HashSet::new() });
        rebuild(&mut s);
        if let Some(side) = s.side.as_mut() {
            render(side);
        }
    }
    ensure_preview(st);
}

pub fn close(st: &Shared) {
    if let Some(old) = st.borrow_mut().side.take() {
        let _ = mode_close(old.engine.mode);
    }
}

/// Redraw an open sidebar from the current state.
pub fn refresh(st: &Shared) {
    let mut s = st.borrow_mut();
    if s.side.is_none() {
        return;
    }
    rebuild(&mut s);
    if let Some(side) = s.side.as_mut() {
        render(side);
    }
}

pub fn render(side: &mut Sidebar) {
    // Refresh (`R`) is on the `?` card and in the menu, not here: the
    // footer is for the keys pressed every time the sidebar is open.
    // The search box line above names `/` itself.
    let k = |action: &str| keyname(side.engine.keys.key_of(action)).to_string();
    side.engine.footer = format!(
        "Enter open · {} read · {} compose · {} app · {} view · {} menu · ? help · q close",
        k("mark"),
        k("compose"),
        k("open_app"),
        k("view"),
        k("menu"),
    );
    let (out, rect) = side.engine.render();
    let _ = mode_write(side.engine.mode, out.as_bytes());
    let _ = mode_preview(side.engine.mode, rect.as_ref());
}

// ---------------------------------------------------------------------------
// nodes
// ---------------------------------------------------------------------------

/// Is the channel one the sidebar shows at all?
fn shown(c: &Channel) -> bool {
    !c.is_archived && c.is_member && (!c.kind.is_dm() || c.is_open)
}

/// Rows with something to say first: mentions, then unread, then the
/// rest by recency; muted ones last.
fn rank(c: &Channel) -> (u8, std::cmp::Reverse<String>, String) {
    let band = if c.is_muted {
        3
    } else if c.mentions > 0 {
        0
    } else if c.has_unreads {
        1
    } else {
        2
    };
    (band, std::cmp::Reverse(c.latest.clone().unwrap_or_default()), c.name.to_lowercase())
}

/// The badge: the channel's state as one glyph, the agents picker's
/// way. A mention is the one coloured block in the list (`!`, red,
/// inverted), so it cannot be missed at a glance; unread is a bright
/// cyan dot; read and muted a dim one.
fn badge_cells(c: &Channel) -> Styled {
    if c.is_muted {
        ('·', ST_DIM)
    } else if c.mentions > 0 {
        ('!', ST_BOLD | ST_RED | ST_BRIGHT | ST_INVERT)
    } else if c.has_unreads {
        ('●', ST_BOLD | ST_BRIGHT | ST_CYAN)
    } else {
        ('·', ST_DIM)
    }
}

/// One channel as a row: the badge, then the glyph and the name (bold
/// while unread, red when mentioned, with the mention count after it;
/// dim when read or muted), the star when starred.
fn row_left(s: &State, c: &Channel) -> Vec<Styled> {
    let label = s.channel_label(c);
    let (glyph, rest) = label.split_at(1);
    let glyph = match c.kind {
        Kind::Private => "*",
        _ => glyph,
    };
    let style = if c.is_muted {
        ST_DIM
    } else if c.mentions > 0 {
        ST_BOLD | ST_RED
    } else if c.has_unreads {
        ST_BOLD
    } else {
        ST_DIM
    };
    let mut cells: Vec<Styled> = vec![badge_cells(c), (' ', 0)];
    cells.extend(plain_cells(glyph, ST_DIM | (style & ST_RED)));
    cells.extend(plain_cells(rest, style));
    if c.mentions > 0 && !c.is_muted {
        cells.extend(plain_cells(&format!("  @{}", c.mentions.min(99)), ST_DIM | ST_RED));
    }
    if c.is_starred {
        cells.extend(plain_cells(" ★", ST_DIM));
    }
    cells
}

/// The right column: how long ago the channel last spoke.
fn row_right(c: &Channel, now: u64) -> Vec<Styled> {
    let age = c.latest.as_deref().map(|t| timefmt::age(t, now)).unwrap_or_default();
    plain_cells(&format!("{age:>4}"), ST_DIM)
}

fn preview_for(s: &State, side: &Sidebar, c: &Channel, width: usize) -> Preview {
    let mut lines: Vec<Vec<Styled>> = Vec::new();
    if !c.topic.is_empty() {
        lines.push(s.names.with(|n| mrkdwn::inline(&c.topic, ST_DIM, n)));
        lines.push(Vec::new());
    }
    match side.previews.get(&c.id) {
        Some(msgs) if msgs.is_empty() => lines.push(plain_cells("(no messages)", ST_DIM)),
        Some(msgs) => {
            let tz = s.tz();
            let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
            for m in msgs.iter().rev().take(25).collect::<Vec<_>>().into_iter().rev() {
                let who = m.user.as_deref().map(|u| s.user_label(u)).or_else(|| m.username.clone()).unwrap_or_else(|| "?".into());
                let mut head = plain_cells(&who, ST_BOLD | if m.user.as_deref() == Some(me.as_str()) { listkit::styled::ST_GREEN } else { ST_CYAN });
                head.extend(plain_cells(&format!("  {}", timefmt::clock(&m.ts, tz)), ST_DIM));
                if m.reply_count > 0 {
                    head.extend(plain_cells(&format!("  ↳{}", m.reply_count), ST_DIM));
                }
                lines.push(head);
                let body = s.names.with(|n| mrkdwn::render(&m.text, width.saturating_sub(2).max(10), 0, n));
                for mut l in body {
                    let mut row = plain_cells("  ", 0);
                    row.append(&mut l);
                    lines.push(row);
                }
                // Attachments under the text, as the channel view draws
                // them: a bot's post is often nothing but its card.
                for a in &m.attachments {
                    for raw in a.lines() {
                        let text = s.names.with(|n| mrkdwn::inline(raw, ST_DIM, n));
                        for mut w in wrap_cells(&text, width.saturating_sub(4).max(10)) {
                            let mut row = plain_cells("  │ ", ST_DIM);
                            row.append(&mut w);
                            lines.push(row);
                        }
                    }
                }
                for f in &m.files {
                    lines.push(plain_cells(&format!("  📎 {f}"), ST_DIM));
                }
                lines.push(Vec::new());
            }
            // The preview opens at its end: no blank last line there.
            if lines.last().is_some_and(|l| l.is_empty()) {
                lines.pop();
            }
        }
        None => lines.push(plain_cells("loading…", ST_DIM)),
    }
    Preview::TextTail(lines)
}

fn group_node(key: &str, name: &str, count: usize, unread: usize) -> Node {
    let mut n = Node::group(key, 0, true, false);
    n.left = plain_cells(name, ST_BOLD);
    n.left.extend(plain_cells(
        &if unread > 0 { format!("  {count} · {unread} unread") } else { format!("  {count}") },
        ST_DIM,
    ));
    n.header_glyph = Some('▪');
    n
}

fn channel_node(s: &State, side: &Sidebar, c: &Channel, depth: u8, now: u64, preview_w: usize) -> Node {
    let mut n = Node::item(c.id.clone(), depth);
    n.left = row_left(s, c);
    n.right = row_right(c, now);
    let label = s.channel_label(c);
    n.haystack = format!("{label} {} {}", c.topic, c.purpose);
    if c.kind.is_dm() {
        n.tokens.push(('@', label.trim_start_matches('@').to_string()));
        n.tokens.push(('!', "dm".into()));
    } else {
        n.tokens.push(('#', c.name.clone()));
    }
    if c.has_unreads {
        n.tokens.push(('!', "unread".into()));
    }
    if c.mentions > 0 {
        n.tokens.push(('!', "mention".into()));
    }
    if c.is_muted {
        n.tokens.push(('!', "muted".into()));
    }
    if c.is_starred {
        n.tokens.push(('!', "starred".into()));
    }
    n.dim = c.is_muted;
    n.preview = preview_for(s, side, c, preview_w);
    n
}

fn build_nodes(s: &State) -> Vec<Node> {
    let Some(side) = s.side.as_ref() else { return Vec::new() };
    let now = now_ms();
    let preview_w = (side.engine.width as usize).saturating_sub(side.engine.list_w() + 3).max(20);
    let mut chans: Vec<&Channel> = s.channels.iter().filter(|c| shown(c)).collect();
    chans.sort_by_key(|c| rank(c));
    let mut nodes = Vec::new();
    let group = |nodes: &mut Vec<Node>, key: &str, name: &str, rows: &[&Channel]| {
        if rows.is_empty() {
            return;
        }
        let unread = rows.iter().filter(|c| c.has_unreads && !c.is_muted).count();
        nodes.push(group_node(key, name, rows.len(), unread));
        for c in rows {
            nodes.push(channel_node(s, side, c, 1, now, preview_w));
        }
    };
    match side.view {
        View::Unread => {
            let rows: Vec<&Channel> = chans.iter().copied().filter(|c| (c.has_unreads || c.mentions > 0) && !c.is_muted).collect();
            for c in rows {
                nodes.push(channel_node(s, side, c, 0, now, preview_w));
            }
        }
        View::All => {
            let mut ch: Vec<&Channel> = chans.iter().copied().filter(|c| !c.kind.is_dm()).collect();
            ch.sort_by_key(|c| c.name.to_lowercase());
            let mut dm: Vec<&Channel> = chans.iter().copied().filter(|c| c.kind.is_dm()).collect();
            dm.sort_by_key(|c| s.channel_label(c).to_lowercase());
            group(&mut nodes, "sec:channels", "Channels", &ch);
            group(&mut nodes, "sec:dms", "Direct messages", &dm);
        }
        View::Sections => {
            let mut placed: HashSet<&str> = HashSet::new();
            let mut channels_name = "Channels".to_string();
            let mut dms_name = "Direct messages".to_string();
            let mut starred: Vec<&Channel> = chans.iter().copied().filter(|c| c.is_starred).collect();
            let mut starred_listed = false;
            for sec in &s.sections {
                match sec.kind.as_str() {
                    "channels" if sec.channel_ids.is_empty() => {
                        if !sec.name.is_empty() {
                            channels_name = sec.name.clone();
                        }
                        continue;
                    }
                    "direct_messages" if sec.channel_ids.is_empty() => {
                        if !sec.name.is_empty() {
                            dms_name = sec.name.clone();
                        }
                        continue;
                    }
                    "stars" | "starred" => {
                        starred_listed = true;
                        for id in &sec.channel_ids {
                            if let Some(c) = chans.iter().find(|c| &c.id == id) {
                                if !starred.iter().any(|x| x.id == c.id) {
                                    starred.push(c);
                                }
                            }
                        }
                        starred.sort_by_key(|c| rank(c));
                        for c in &starred {
                            placed.insert(c.id.as_str());
                        }
                        group(&mut nodes, &format!("sec:{}", sec.id), if sec.name.is_empty() { "Starred" } else { &sec.name }, &starred);
                        continue;
                    }
                    _ => {}
                }
                let mut rows: Vec<&Channel> = sec
                    .channel_ids
                    .iter()
                    .filter_map(|id| chans.iter().copied().find(|c| &c.id == id))
                    .filter(|c| !placed.contains(c.id.as_str()))
                    .collect();
                rows.sort_by_key(|c| rank(c));
                for c in &rows {
                    placed.insert(c.id.as_str());
                }
                group(&mut nodes, &format!("sec:{}", sec.id), &sec.name, &rows);
            }
            if !starred_listed && !starred.is_empty() {
                for c in &starred {
                    placed.insert(c.id.as_str());
                }
                nodes.insert(0, group_node("sec:starred", "Starred", starred.len(), starred.iter().filter(|c| c.has_unreads).count()));
                let mut i = 1;
                for c in &starred {
                    nodes.insert(i, channel_node(s, side, c, 1, now, preview_w));
                    i += 1;
                }
            }
            let ch: Vec<&Channel> = chans.iter().copied().filter(|c| !c.kind.is_dm() && !placed.contains(c.id.as_str())).collect();
            let dm: Vec<&Channel> = chans.iter().copied().filter(|c| c.kind.is_dm() && !placed.contains(c.id.as_str())).collect();
            group(&mut nodes, "sec:channels", &channels_name, &ch);
            group(&mut nodes, "sec:dms", &dms_name, &dm);
        }
    }
    nodes
}

pub fn rebuild(s: &mut State) {
    let nodes = build_nodes(s);
    let unread = s.channels.iter().filter(|c| c.has_unreads && !c.is_muted && shown(c)).count();
    let mentions: i64 = s.channels.iter().filter(|c| !c.is_muted).map(|c| c.mentions).sum();
    let tag = {
        let mut parts = Vec::new();
        if let Some(id) = &s.ident {
            parts.push(id.team_name.clone());
        }
        parts.push(match (unread, mentions) {
            (0, 0) => "all read".to_string(),
            (u, 0) => format!("{u} unread"),
            (u, m) => format!("{u} unread, {m} mentions"),
        });
        if s.threads_unread {
            parts.push("threads".into());
        }
        if s.syncing {
            parts.push("syncing…".into());
        }
        parts.join(" · ")
    };
    let empty = match (&s.auth_error, s.channels.is_empty()) {
        (Some(e), _) => format!("no Slack session: {e}"),
        (None, true) => "(syncing the workspace…)".to_string(),
        (None, false) => match s.side.as_ref().map(|p| p.view) {
            Some(View::Unread) => "(nothing unread)".to_string(),
            _ => "(no channels)".to_string(),
        },
    };
    let view = s.side.as_ref().map(|p| p.view).unwrap_or(View::Sections);
    if let Some(side) = s.side.as_mut() {
        side.engine.header_tag = format!("{tag} · by {}", view.label());
        side.engine.empty_text = empty;
        side.engine.set_nodes(nodes);
    }
}

/// Fetch recent messages for the highlighted channel if the preview has
/// none yet.
fn ensure_preview(st: &Shared) {
    let (id, key) = {
        let mut s = st.borrow_mut();
        let Some(ident) = s.ident.clone() else { return };
        let Some(side) = s.side.as_mut() else { return };
        let Some(key) = side.engine.selected_key() else { return };
        if key.starts_with("sec:") || side.previews.contains_key(&key) || side.fetching.contains(&key) {
            return;
        }
        side.fetching.insert(key.clone());
        (ident, key)
    };
    let st2 = Rc::clone(st);
    spawn(async move {
        let fetched = match api::history(&id, &key, None, None, 25).await {
            Ok((mut msgs, _)) => {
                msgs.reverse();
                crate::absorb(&st2, &key, &msgs).await;
                msgs
            }
            Err(e) => {
                log(&format!("slack: preview {key}: {e}"));
                // What the cache has, then.
                crate::store::messages(&key, 25).await
            }
        };
        let mut s = st2.borrow_mut();
        let Some(side) = s.side.as_mut() else { return };
        side.fetching.remove(&key);
        side.previews.insert(key, fetched);
        rebuild(&mut s);
        if let Some(side) = s.side.as_mut() {
            render(side);
        }
    });
}

// ---------------------------------------------------------------------------
// the action menu
// ---------------------------------------------------------------------------

/// The Space menu: every sidebar action for the highlighted row, the
/// ones that do not apply dimmed, as a tmux `display-menu` on the client
/// that pressed the key. The items do nothing themselves: each hands its
/// key back through `plugin-command slack 'menu-key <key>'`, which
/// `replay_key` presses into the engine, so the menu is a view of the
/// keymap and cannot disagree with it.
fn open_menu(st: &Shared) {
    let (title, items, client) = {
        let s = st.borrow();
        let Some(side) = s.side.as_ref() else { return };
        let k = |action: &str| side.engine.keys.key_of(action).to_string();
        let sel = side.engine.selected_key().and_then(|key| s.channel(&key).cloned());
        let row = sel.is_some();
        let unread = sel.as_ref().is_some_and(|c| c.has_unreads || c.mentions > 0);
        let title = match &sel {
            Some(c) => menu_safe(&s.channel_label(c), 30),
            None => NAME.to_string(),
        };
        let mut items = String::new();
        items.push_str(&menu_item(NAME, "open", &k("activate"), row));
        items.push_str(&menu_item(NAME, "mark read", &k("mark"), row && unread));
        items.push_str(&menu_item(NAME, "compose", &k("compose"), row));
        items.push_str(&menu_item(NAME, "open in the Slack app", &k("open_app"), row));
        items.push_str(" ''");
        items.push_str(&menu_item(NAME, &format!("view: {}", side.view.next().label()), &k("view"), true));
        items.push_str(&menu_item(NAME, "fold the section", &k("fold"), side.view != View::Unread));
        items.push_str(&menu_item(NAME, "refresh", &k("refresh"), true));
        items.push_str(" ''");
        items.push_str(&menu_item(NAME, "help", "?", true));
        items.push_str(&menu_item(NAME, "close", "q", true));
        (title, items, side.client)
    };
    // The client that pressed the key, by name; without one tmux picks
    // "the current client", which is right only while one is attached.
    let target = client
        .and_then(|cid| list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid))
        .map(|c| c.name)
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)))
        .map(|n| format!(" -c '{n}'"))
        .unwrap_or_default();
    // No -x/-y: the menu centres on the client's window, where the
    // sidebar floats.
    let cmd = format!("display-menu{target} -T ' {title} '{items}");
    let st2 = Rc::clone(st);
    spawn(async move {
        if let Err(e) = run_command(&cmd).await {
            let mut s = st2.borrow_mut();
            if let Some(side) = s.side.as_mut() {
                side.engine.status = Some(format!("menu failed: {}", e.message));
                render(side);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// A key arriving by another road than the mode (the Space menu's
/// `menu-key`): handled exactly as a press, with no mouse position.
pub fn replay_key(st: &Shared, key: &str) {
    let outcome = {
        let mut s = st.borrow_mut();
        let Some(side) = s.side.as_mut() else { return };
        side.engine.handle_key(key, None)
    };
    execute(st, outcome);
}

pub fn on_mode_event(st: &Shared, name: &str, event: &Event) {
    match name {
        "mode-key" => {
            let key = event.get_str("key").unwrap_or("").to_string();
            let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
                (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
                _ => None,
            };
            let outcome = {
                let mut s = st.borrow_mut();
                let Some(side) = s.side.as_mut() else { return };
                side.engine.handle_key(&key, mouse)
            };
            execute(st, outcome);
        }
        "mode-nav" => {
            let dir = event.get_str("dir").unwrap_or("").to_string();
            let outcome = {
                let mut s = st.borrow_mut();
                let Some(side) = s.side.as_mut() else { return };
                side.engine.handle_nav(&dir)
            };
            execute(st, outcome);
        }
        "mode-paste" => {
            let Some(text) = event.get_str("text").map(str::to_string) else { return };
            let outcome = {
                let mut s = st.borrow_mut();
                let Some(side) = s.side.as_mut() else { return };
                side.engine.handle_paste(&text)
            };
            execute(st, outcome);
        }
        "mode-resize" => {
            let mut s = st.borrow_mut();
            let (w, h) = {
                let Some(side) = s.side.as_ref() else { return };
                (
                    event.get_i64("width").map(|v| v as u32).unwrap_or(side.engine.width),
                    event.get_i64("height").map(|v| v as u32).unwrap_or(side.engine.height),
                )
            };
            if let Some(side) = s.side.as_mut() {
                side.engine.resize(w, h);
            }
            rebuild(&mut s);
            if let Some(side) = s.side.as_mut() {
                render(side);
            }
        }
        "mode-closed" => {
            st.borrow_mut().side = None;
        }
        _ => {}
    }
}

fn execute(st: &Shared, outcome: Outcome) {
    enum After {
        None,
        OpenChat(Channel, bool),
        OpenApp(Vec<Channel>),
        Mark(Vec<String>),
        Refresh,
        Menu,
        Close,
    }
    let after = {
        let mut s = st.borrow_mut();
        let Some(side) = s.side.as_mut() else { return };
        match outcome {
            Outcome::Nothing => return,
            Outcome::Redraw | Outcome::FilterChanged | Outcome::Split(_) | Outcome::Expanded(..) => {
                render(side);
                After::None
            }
            Outcome::Close => After::Close,
            Outcome::Resize(w, h) => {
                let _ = mode_resize(side.engine.mode, w, h);
                let _ = set_option("@slack-width", &w.to_string());
                let _ = set_option("@slack-height", &h.to_string());
                After::None
            }
            Outcome::Activate(key) => match s.channel(&key).cloned() {
                Some(c) => After::OpenChat(c, false),
                None => return,
            },
            Outcome::Action(action, targets) => {
                let chans: Vec<Channel> = targets.iter().filter_map(|k| s.channel(k).cloned()).collect();
                match action {
                    "compose" => match chans.into_iter().next() {
                        Some(c) => After::OpenChat(c, true),
                        None => return,
                    },
                    "open_app" => After::OpenApp(chans),
                    "mark" => {
                        if let Some(side) = s.side.as_mut() {
                            side.engine.clear_marks();
                        }
                        After::Mark(chans.into_iter().map(|c| c.id).collect())
                    }
                    "view" => {
                        let v = s.side.as_ref().map(|p| p.view).unwrap_or(View::Sections).next();
                        let _ = set_option("@slack-view", v.label());
                        if let Some(side) = s.side.as_mut() {
                            side.view = v;
                            side.engine.reset_scroll();
                        }
                        rebuild(&mut s);
                        if let Some(side) = s.side.as_mut() {
                            render(side);
                        }
                        After::None
                    }
                    "refresh" => {
                        if let Some(side) = s.side.as_mut() {
                            side.engine.status = Some("refreshing…".into());
                            render(side);
                        }
                        After::Refresh
                    }
                    "menu" => After::Menu,
                    _ => return,
                }
            }
            Outcome::Key(_) | Outcome::Prompt(..) | Outcome::PreviewKey(..) | Outcome::PreviewWheel(..) | Outcome::PreviewHeaderClick(..) => {
                return;
            }
        }
    };
    match after {
        After::None => {}
        After::Close => close(st),
        After::OpenChat(c, compose) => {
            let (client, pane) = st.borrow().side.as_ref().map(|p| (p.client, p.pane)).unwrap_or((None, None));
            close(st);
            chat::open(st, c, None, client, pane, compose);
            return;
        }
        After::OpenApp(chans) => {
            let team = st.borrow().ident.as_ref().map(|i| i.team_id.clone()).unwrap_or_default();
            for c in chans {
                let url = format!("slack://channel?team={team}&id={}", c.id);
                spawn(async move {
                    let _ = run_job(&format!("command -v open >/dev/null 2>&1 && open {u} || xdg-open {u}", u = crate::http::sq(&url)), None).await;
                });
            }
        }
        After::Mark(ids) => {
            for id in ids {
                let st2 = Rc::clone(st);
                spawn(async move {
                    crate::mark_read(&st2, &id).await;
                });
            }
        }
        After::Menu => open_menu(st),
        After::Refresh => {
            let st2 = Rc::clone(st);
            spawn(async move {
                let r = crate::full_sync(&st2).await;
                let mut s = st2.borrow_mut();
                if let Some(side) = s.side.as_mut() {
                    side.engine.status = r.err().map(|e| format!("refresh: {e}"));
                    side.previews.clear();
                }
                rebuild(&mut s);
                if let Some(side) = s.side.as_mut() {
                    render(side);
                }
                drop(s);
                ensure_preview(&st2);
            });
        }
    }
    ensure_preview(st);
}
