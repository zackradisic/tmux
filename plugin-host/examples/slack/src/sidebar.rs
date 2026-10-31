//! The sidebar: a listkit picker over the workspace's conversations,
//! grouped the way the user's own Slack sidebar is (its sections, then
//! channels, then DMs), each row carrying its badge, the preview column
//! showing the channel's recent messages. Enter opens the channel view.
//!
//! The preview is the channel view itself, embedded: a `chat::Chat` laid
//! out at the column's width (so nothing is clipped) and drawn by this
//! module after the engine's frame, the way the agents picker draws its
//! transcript. `l`/Right (or a click in the column) hand it the keyboard:
//! the channel view's keys then work on it - j/k, J/K, g/G, y/Y, v, e,
//! m, Enter opens the message's thread in the full view, i/r open the
//! full view composing - and Escape/h/Left give the keyboard back to the
//! list. The separator lights while it has the keyboard.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use listkit::keys::KeyTable;
use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::engine::is_mouse_key;
use listkit::styled::{emit_cells, fit_cells, plain_cells, Styled, ST_BOLD, ST_BRIGHT, ST_CYAN, ST_DIM, ST_INVERT, ST_RED};
use listkit::text::{clip, keyname, menu_item, menu_safe};
use listkit::{Engine, Node, Outcome, Preview, SigilSpec};
use tmux_plugin_sdk::prelude::*;

use crate::chat::After;
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
    /// The cursor's position and the channels just marked read: as those
    /// rows move down the list the cursor stays put, on the row that
    /// takes their place, rather than following them. Dropped once they
    /// are all read, or when the user moves.
    pub hold: Option<(usize, Vec<String>)>,
    /// The highlighted channel's recent messages as an embedded channel
    /// view, laid out at the preview column's width; `None` while the
    /// row is a header or its messages are still loading.
    pub preview: Option<chat::Chat>,
    /// The embedded view has the keyboard.
    pub preview_focus: bool,
}

/// The preview column's geometry for a mode `w` by `h` with the list
/// `list_w` wide: the body's 0-based x, its width, its first row (under
/// the one-line header) and its height (the footer row left free), as
/// listkit's render lays the column out (`x = list_w + 2`, 1-based).
pub(crate) fn preview_geometry(w: usize, h: usize, list_w: usize) -> (usize, usize, usize, usize) {
    let x0 = list_w + 1;
    let pw = w.saturating_sub(list_w + 2);
    let body_y = 1;
    let body_h = h.saturating_sub(2);
    (x0, pw, body_y, body_h)
}

pub fn keys() -> KeyTable {
    Engine::base_keys()
        .with("open_app", "o", "rows", "open the channel in the Slack app")
        .with("mark", "m", "rows", "mark the channel read")
        .with("compose", "i", "rows", "compose a message to the channel")
        .with("agent", "n", "rows", "new agent from the previewed message (or a selection made there)")
        .with("menu", "Space", "rows", "the action menu")
        .with("view", "v", "picker", "sections / unread / all")
        .with("refresh", "R", "picker", "fetch channels, sections, users and badges again")
        .with("fold", "z", "moving", "fold / unfold the section under the cursor")
        .with("fold_all", "Z", "moving", "fold or unfold every section")
        // The engine's `focus` (l) means "type into the pane"; here it
        // hands the keyboard to the preview, so say so on the card.
        .with("focus", "l", "moving", "read the preview: the channel view's keys there (click too), Esc/h back")
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
        s.side = Some(Sidebar {
            engine,
            client,
            pane,
            view,
            previews: HashMap::new(),
            fetching: HashSet::new(),
            hold: None,
            preview: None,
            preview_focus: false,
        });
        rebuild(&mut s);
        render(&mut s);
    }
    ensure_preview(st);
}

pub fn close(st: &Shared) {
    if let Some(old) = st.borrow_mut().side.take() {
        if let Some(c) = old.preview.as_ref() {
            c.forget_images();
        }
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
    render(&mut s);
}

/// Draw the sidebar: the engine's frame, then the embedded channel view
/// in the preview column.
pub fn render(s: &mut State) {
    sync_preview(s);
    let label = s.side.as_ref().and_then(|side| side.preview.as_ref()).map(|c| s.channel_label(&c.channel));
    let topic = s.side.as_ref().and_then(|side| side.preview.as_ref()).map(|c| s.names.with(|n| mrkdwn::inline(&c.channel.topic, ST_DIM, n)));
    let Some(side) = s.side.as_mut() else { return };
    // Refresh (`R`) is on the `?` card and in the menu, not here: the
    // footer is for the keys pressed every time the sidebar is open.
    // The search box line above names `/` itself.
    let k = |action: &str| keyname(side.engine.keys.key_of(action)).to_string();
    // Seven hints at most: the footer is clipped to the list's width,
    // and the rest (o, v, R, [ ]) is on the `?` card and in the menu.
    side.engine.footer = if side.preview_focus {
        "j/k move · Enter thread · y copy · i compose · Space menu · Esc back to list".to_string()
    } else {
        format!(
            "Enter open · {} preview · {} read · {} compose · {} agent · {} menu · ? help · q close",
            k("focus"),
            k("mark"),
            k("compose"),
            k("agent"),
            k("menu"),
        )
    };
    side.engine.separator_lit = side.preview_focus;
    let (mut out, rect) = side.engine.render();
    if !side.engine.show_help {
        if let Some(c) = side.preview.as_ref() {
            draw_preview(c, side.preview_focus, label.unwrap_or_default(), topic.unwrap_or_default(), &mut out, &side.engine);
        }
    }
    let _ = mode_write(side.engine.mode, out.as_bytes());
    let _ = mode_preview(side.engine.mode, rect.as_ref());
}

/// The embedded view in the preview column: a header line (the channel,
/// its topic, lit while it has the keyboard), the body, and on the last
/// row its compose box, status, or - while focused - the hints.
fn draw_preview(c: &chat::Chat, focused: bool, label: String, topic: Vec<Styled>, out: &mut String, engine: &Engine) {
    let (x0, pw, _, _) = preview_geometry(engine.width as usize, engine.height as usize, engine.list_w());
    if pw < 10 {
        return;
    }
    let h = engine.height as usize;
    let mut head = plain_cells(&label, ST_BOLD | if focused { ST_CYAN } else { 0 });
    if !topic.is_empty() {
        head.extend(plain_cells("  ", 0));
        head.extend(topic);
    }
    if !focused && !c.msgs.is_empty() {
        head.extend(plain_cells(" · l to read", ST_DIM));
    }
    out.push_str(&format!("\x1b[1;{}H{}", x0 + 1, emit_cells(&fit_cells(&head, pw), false)));
    chat::paint_body(c, out, x0, c.body_y as usize);
    let foot: Vec<Styled> = if let Some(cp) = &c.compose {
        chat::compose_cells(cp, pw.saturating_sub(1))
    } else if let Some(st) = &c.status {
        plain_cells(&clip(st, pw.saturating_sub(1)), ST_CYAN)
    } else if focused {
        plain_cells(&clip(chat::hints(), pw.saturating_sub(1)), ST_DIM)
    } else {
        Vec::new()
    };
    if h >= 2 {
        out.push_str(&format!("\x1b[{h};{}H{}", x0 + 1, emit_cells(&foot, false)));
    }
}

/// Keep the embedded view in step with the highlighted row and the
/// column's size: a new one when the cursor moved to another channel
/// whose messages are in, the same one laid out again otherwise (its
/// cursor and scroll kept), none on a header or while loading.
fn sync_preview(s: &mut State) {
    let names = Rc::clone(&s.names);
    let tz = s.tz();
    let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
    let now = (now_ms() / 1000) as i64;
    let cache = s.images.clone();
    let Some(side) = s.side.as_ref() else { return };
    let key = side.engine.selected_key().filter(|k| !k.starts_with("sec:")).map(|k| chan_key(&k).to_string());
    let channel = key.as_deref().and_then(|k| s.channel(k).cloned());
    let Some(side) = s.side.as_mut() else { return };
    let (_, pw, body_y, body_h) = preview_geometry(side.engine.width as usize, side.engine.height as usize, side.engine.list_w());
    let msgs = key.as_deref().and_then(|k| side.previews.get(k));
    let (Some(channel), Some(msgs)) = (channel, msgs) else {
        side.preview = None;
        side.preview_focus = false;
        return;
    };
    let c = match side.preview.as_mut().filter(|c| c.channel.id == channel.id) {
        Some(c) => {
            // The same channel: new messages (or a refetch) replace the
            // list; following the bottom moves the cursor to the newest.
            // The row's read mark is live here (m moves the rule), unlike
            // the float's, which keeps the mark it opened with.
            let changed = c.msgs.len() != msgs.len() || c.msgs.last().map(|m| &m.ts) != msgs.last().map(|m| &m.ts) || c.msgs.first().map(|m| &m.ts) != msgs.first().map(|m| &m.ts);
            if changed {
                c.msgs = msgs.clone();
                if c.follow || c.cursor_idx().is_none() {
                    let last = c.msgs.len().saturating_sub(1);
                    c.set_cursor_idx(last);
                }
            }
            c.last_read = channel.last_read.clone();
            c.channel = channel;
            c.width = pw as u32;
            c.height = body_h as u32;
            c.body_y = body_y as u32;
            c
        }
        None => {
            side.preview_focus = false;
            // Another channel: the terminal forgets the old one's pictures.
            if let Some(old) = side.preview.take() {
                old.forget_images();
            }
            let mut c = chat::embedded(side.engine.mode, channel, side.client, side.pane, msgs.clone(), pw as u32, body_h as u32, body_y as u32);
            let (on, aspect, root) = chat::picture_setup(side.client, side.pane);
            c.images_on = on;
            c.cell_aspect = aspect;
            c.data_root = root;
            side.preview.insert(c)
        }
    };
    // Pictures as in the float: laid out from the cache, placed by
    // paint_body through the sidebar's mode, the missing ones fetched.
    let pics = chat::Pictures { sizes: &cache.ready, cell_aspect: c.cell_aspect };
    let on = c.images_on;
    chat::layout_chat(c, &names, tz, &me, now, on.then_some(&pics));
    if !c.follow {
        c.reveal_cursor();
    }
    let wanted = c.wanted_images(&cache);
    if !wanted.is_empty() {
        crate::images::request(s, wanted);
    }
}

/// A picture arrived on disk (images.rs): the embedded preview lays out
/// again so its rows appear, and the column is redrawn.
pub fn on_image(st: &Shared) {
    let mut s = st.borrow_mut();
    if s.side.as_ref().is_none_or(|p| p.preview.is_none()) {
        return;
    }
    sync_preview(&mut s);
    render(&mut s);
}

// ---------------------------------------------------------------------------
// nodes
// ---------------------------------------------------------------------------

/// The key prefix of a row in the Unread section, which repeats a
/// channel that has a row in its own section too.
const UNREAD_PREFIX: &str = "u:";

/// The channel id behind a row key, whichever section the row is in.
fn chan_key(key: &str) -> &str {
    key.strip_prefix(UNREAD_PREFIX).unwrap_or(key)
}

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

/// The engine's own preview of a channel row: only the loading line,
/// while the messages are on their way. Once they are in, the column is
/// drawn by `render` as an embedded channel view (`Preview::None` keeps
/// the engine's hands off it).
fn preview_for(side: &Sidebar, c: &Channel) -> Preview {
    match side.previews.get(&c.id) {
        Some(_) => Preview::None,
        None => Preview::TextTail(vec![plain_cells("loading…", ST_DIM)]),
    }
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

fn channel_node(s: &State, side: &Sidebar, c: &Channel, depth: u8, now: u64) -> Node {
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
    n.preview = preview_for(side, c);
    n
}

fn build_nodes(s: &State) -> Vec<Node> {
    let Some(side) = s.side.as_ref() else { return Vec::new() };
    let now = now_ms();
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
            nodes.push(channel_node(s, side, c, 1, now));
        }
    };
    match side.view {
        View::Unread => {
            let rows: Vec<&Channel> = chans.iter().copied().filter(|c| (c.has_unreads || c.mentions > 0) && !c.is_muted).collect();
            for c in rows {
                nodes.push(channel_node(s, side, c, 0, now));
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
            // Everything with something new first, DMs before channels,
            // each in rank order. The rows stay in their own sections
            // below as well, so these carry a key prefix of their own.
            let fresh = |c: &&Channel| (c.has_unreads || c.mentions > 0) && !c.is_muted;
            let unread: Vec<&Channel> = chans
                .iter()
                .copied()
                .filter(|c| c.kind.is_dm() && fresh(c))
                .chain(chans.iter().copied().filter(|c| !c.kind.is_dm() && fresh(c)))
                .collect();
            if !unread.is_empty() {
                nodes.push(group_node("sec:unread", "Unread", unread.len(), 0));
                for c in &unread {
                    let mut n = channel_node(s, side, c, 1, now);
                    n.key = format!("{UNREAD_PREFIX}{}", c.id);
                    nodes.push(n);
                }
            }
            let base = nodes.len();
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
                nodes.insert(base, group_node("sec:starred", "Starred", starred.len(), starred.iter().filter(|c| c.has_unreads).count()));
                let mut i = base + 1;
                for c in &starred {
                    nodes.insert(i, channel_node(s, side, c, 1, now));
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
    let held = s.side.as_ref().and_then(|p| p.hold.clone());
    let held_done = held.as_ref().is_some_and(|(_, ids)| ids.iter().all(|id| s.channel(id).is_none_or(|c| !c.has_unreads)));
    if let Some(side) = s.side.as_mut() {
        side.engine.header_tag = format!("{tag} · by {}", view.label());
        side.engine.empty_text = empty;
        side.engine.set_nodes(nodes);
        if let Some((pos, ids)) = held {
            // set_nodes followed the marked row by key; put the cursor
            // back where it was.
            if side.engine.selected_key().is_some_and(|k| ids.iter().any(|id| id == chan_key(&k))) {
                side.engine.select_pos(pos);
            }
            if held_done {
                side.hold = None;
            }
        }
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
        let key = chan_key(&key).to_string();
        if key.starts_with("sec:") || side.previews.contains_key(&key) || side.fetching.contains(&key) {
            return;
        }
        side.fetching.insert(key.clone());
        (ident, key)
    };
    fetch_preview(st, id, key);
}

/// Fetch `key`'s recent messages into `previews` (the caller has put the
/// key in `fetching`) and redraw. The embedded view's "refetching…"
/// status goes once they are in.
fn fetch_preview(st: &Shared, id: crate::store::Identity, key: String) {
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
        side.previews.insert(key.clone(), fetched);
        if let Some(c) = side.preview.as_mut().filter(|c| c.channel.id == key) {
            if c.status.as_deref() == Some("refetching…") {
                c.status = None;
            }
        }
        rebuild(&mut s);
        render(&mut s);
    });
}

/// A message arrived on the feed: into the channel's preview, if the
/// sidebar holds one (the caller redraws).
pub fn on_arrived(st: &Shared, m: &Message) {
    if m.is_reply() {
        return;
    }
    let mut s = st.borrow_mut();
    let Some(side) = s.side.as_mut() else { return };
    if let Some(msgs) = side.previews.get_mut(&m.channel) {
        chat::merge(msgs, vec![m.clone()]);
    }
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
        let sel = side.engine.selected_key().and_then(|key| s.channel(chan_key(&key)).cloned());
        let row = sel.is_some();
        let unread = sel.as_ref().is_some_and(|c| c.has_unreads || c.mentions > 0);
        let title = match &sel {
            Some(c) => menu_safe(&s.channel_label(c), 30),
            None => NAME.to_string(),
        };
        // The preview has the keyboard: the channel view's menu, whose
        // items come back through `menu-key` to the preview as well.
        if let (true, Some(c)) = (side.preview_focus, side.preview.as_ref()) {
            let mut items = chat::menu_items(c);
            // Its "back to sidebar" is this menu's "back to the list".
            items = items.replace("'back to sidebar'", "'back to the list'");
            (title, items, side.client)
        } else {
            let readable = side.preview.as_ref().is_some_and(|c| !c.msgs.is_empty());
            let mut items = String::new();
            items.push_str(&menu_item(NAME, "open", &k("activate"), row));
            items.push_str(&menu_item(NAME, "read the preview", &k("focus"), row && readable));
            items.push_str(&menu_item(NAME, "mark read", &k("mark"), row && unread));
            items.push_str(&menu_item(NAME, "compose", &k("compose"), row));
            items.push_str(&menu_item(NAME, "new agent from the preview", &k("agent"), row && readable));
            items.push_str(&menu_item(NAME, "open in the Slack app", &k("open_app"), row));
            items.push_str(" ''");
            items.push_str(&menu_item(NAME, &format!("view: {}", side.view.next().label()), &k("view"), true));
            items.push_str(&menu_item(NAME, "fold the section", &k("fold"), side.view != View::Unread));
            items.push_str(&menu_item(NAME, "refresh", &k("refresh"), true));
            items.push_str(" ''");
            items.push_str(&menu_item(NAME, "help", "?", true));
            items.push_str(&menu_item(NAME, "close", "q", true));
            (title, items, side.client)
        }
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
    // The `--`: a dimmed item is spelled `-label`, and first in line
    // tmux would read it as a flag.
    let cmd = format!("display-menu{target} -T ' {title} ' --{items}");
    let st2 = Rc::clone(st);
    spawn(async move {
        if let Err(e) = run_command(&cmd).await {
            let mut s = st2.borrow_mut();
            if let Some(side) = s.side.as_mut() {
                side.engine.status = Some(format!("menu failed: {}", e.message));
            }
            render(&mut s);
        }
    });
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// A key arriving by another road than the mode (the Space menu's
/// `menu-key`): handled exactly as a press, with no mouse position.
pub fn replay_key(st: &Shared, key: &str) {
    press(st, key, None);
}

/// What the embedded view made of a key it took.
#[derive(Debug, PartialEq)]
enum Took {
    /// The channel view's own answer, to carry out against the preview.
    Chat(After),
    /// Open the full channel view on the previewed channel: a thread
    /// (Enter), composing (i, r).
    Open(Option<String>, bool),
}

/// Can the preview take the keyboard: a channel row whose messages are
/// in (a header or a loading row has nothing to read).
fn can_focus(side: &Sidebar) -> bool {
    side.preview.as_ref().is_some_and(|c| !c.msgs.is_empty()) && !side.engine.selected().is_some_and(|n| n.is_group())
}

/// The embedded view's turn at a key, before the engine's: `None` when
/// the key is the engine's after all. Unfocused, it takes `l`/Right (the
/// keyboard goes to the preview), `[`/`]` and the wheel over its column
/// (scroll it) and a click there (the keyboard goes with it, the cursor
/// to the clicked message). Focused, it takes every key: Escape, h and
/// Left hand the keyboard back, i and r open the full view composing,
/// the rest is the channel view's own keymap (`chat::handle_key`). A
/// click on the list takes the keyboard back and is the engine's.
fn preview_key(side: &mut Sidebar, key: &str, mouse: Option<(u32, u32)>) -> Option<Took> {
    if side.engine.show_help || side.engine.prompt().is_some() || side.engine.filtering {
        return None;
    }
    let list_w = side.engine.list_w();
    if is_mouse_key(key) {
        let (x, _) = mouse?;
        let base = key.rsplit('-').next().unwrap_or(key);
        // x == list_w is the separator (a drag of it is the engine's).
        if (x as usize) <= list_w {
            if side.preview_focus && matches!(base, "MouseDown1Pane" | "DoubleClick1Pane") {
                side.preview_focus = false;
            }
            return None;
        }
        let c = side.preview.as_mut()?;
        return match base {
            "MouseDown1Pane" | "DoubleClick1Pane" => {
                if !c.msgs.is_empty() {
                    side.preview_focus = true;
                }
                Some(Took::Chat(chat::handle_key(c, key, mouse)))
            }
            "WheelUpPane" | "WheelDownPane" => Some(Took::Chat(chat::handle_key(c, key, mouse))),
            _ => None,
        };
    }
    if !side.preview_focus {
        if (key == side.engine.keys.key_of("focus") || key == "Right") && can_focus(side) {
            side.preview_focus = true;
            return Some(Took::Chat(After::None));
        }
        if key == "[" || key == "]" {
            let c = side.preview.as_mut()?;
            let step = (c.body_h() / 2).max(1) as isize;
            c.scroll(if key == "[" { -step } else { step });
            return Some(Took::Chat(After::None));
        }
        // n on a row is the preview's n: the message under its cursor
        // (or a selection made there) goes to a new agent.
        if key == side.engine.keys.key_of("agent") && can_focus(side) {
            let c = side.preview.as_mut()?;
            return Some(Took::Chat(chat::handle_key(c, "n", None)));
        }
        return None;
    }
    let Some(c) = side.preview.as_mut() else {
        side.preview_focus = false;
        return None;
    };
    if c.compose.is_none() {
        match key {
            // The float's size is the engine's whoever has the keyboard.
            "+" | "=" | "-" => return None,
            // Escape clears a selection first, as in the view itself.
            "Escape" if c.anchor.is_some() => {}
            "Escape" | "h" | "Left" => {
                side.preview_focus = false;
                c.status = None;
                return Some(Took::Chat(After::None));
            }
            "i" => return Some(Took::Open(None, true)),
            "r" => {
                let Some(i) = c.cursor_idx() else { return Some(Took::Chat(After::None)) };
                let m = &c.msgs[i];
                let root = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
                return Some(Took::Open(Some(root), true));
            }
            "[" | "]" => {
                let step = (c.body_h() / 2).max(1) as isize;
                c.scroll(if key == "[" { -step } else { step });
                return Some(Took::Chat(After::None));
            }
            _ => {}
        }
    }
    Some(Took::Chat(chat::handle_key(c, key, mouse)))
}

/// A key (or a mouse event at a cell) on the sidebar: the embedded
/// view's first, then the engine's.
fn press(st: &Shared, key: &str, mouse: Option<(u32, u32)>) {
    let step = {
        let mut s = st.borrow_mut();
        let Some(side) = s.side.as_mut() else { return };
        side.hold = None;
        // A drag of the separator is the engine's wherever it lands.
        let drag = match mouse {
            Some((x, _)) if is_mouse_key(key) => side.engine.split_drag(key, x),
            _ => None,
        };
        match drag {
            Some(o) => Err(o),
            None => match preview_key(side, key, mouse) {
                Some(took) => Ok(took),
                None => Err(side.engine.handle_key(key, mouse)),
            },
        }
    };
    match step {
        Ok(took) => preview_after(st, took),
        Err(outcome) => execute(st, outcome),
    }
}

/// The channel under preview and the sidebar's client and pane.
fn preview_ctx(st: &Shared) -> Option<(Channel, Option<u64>, Option<u32>)> {
    let s = st.borrow();
    let side = s.side.as_ref()?;
    Some((side.preview.as_ref()?.channel.clone(), side.client, side.pane))
}

/// Put `status` on the embedded view, if it still shows `channel`, and
/// redraw.
fn preview_status(st: &Shared, channel: &str, status: String) {
    let mut s = st.borrow_mut();
    if let Some(c) = s.side.as_mut().and_then(|side| side.preview.as_mut()).filter(|c| c.channel.id == channel) {
        c.status = Some(status);
    }
    render(&mut s);
}

/// Carry out what the embedded view asked for. The effects are the
/// channel view's (`chat::run` does them for the float), against the
/// preview's messages and this sidebar's client; a change of
/// conversation closes the sidebar and opens the full view.
fn preview_after(st: &Shared, took: Took) {
    let Some((channel, client, pane)) = preview_ctx(st) else {
        let mut s = st.borrow_mut();
        render(&mut s);
        return;
    };
    let after = match took {
        Took::Open(thread, compose) => {
            close(st);
            chat::open(st, channel, thread, client, pane, compose);
            return;
        }
        Took::Chat(a) => a,
    };
    match after {
        After::None => {}
        After::Close => {
            close(st);
            return;
        }
        After::Back | After::Sidebar => {
            let mut s = st.borrow_mut();
            if let Some(side) = s.side.as_mut() {
                side.preview_focus = false;
                if let Some(c) = side.preview.as_mut() {
                    c.status = None;
                }
            }
        }
        After::Switch(thread, _) => {
            close(st);
            chat::open(st, channel, thread, client, pane, false);
            return;
        }
        After::Menu => open_menu(st),
        After::Post(text, thread) => {
            // Not reachable from the keymap (i and r open the full view),
            // but a paste into a reaction box is not a post either; keep
            // the view honest should it ever ask.
            let st2 = Rc::clone(st);
            let cid = channel.id.clone();
            spawn(async move {
                if let Err(e) = crate::post_message(&st2, &cid, &text, thread.as_deref()).await {
                    preview_status(&st2, &cid, format!("post failed: {e}"));
                }
            });
        }
        After::React(ts, name) => {
            let Some(id) = st.borrow().ident.clone() else { return };
            let st2 = Rc::clone(st);
            let cid = channel.id.clone();
            spawn(async move {
                let r = api::react(&id, &cid, &ts, &name).await;
                let status = match &r {
                    Ok(()) => format!(":{name}: added"),
                    Err(e) => format!("react: {e}"),
                };
                if r.is_ok() {
                    // Into the fetched messages, which the view is laid
                    // out from on every redraw.
                    let mut s = st2.borrow_mut();
                    let Some(side) = s.side.as_mut() else { return };
                    for list in side.previews.get_mut(&cid).into_iter().chain(side.preview.as_mut().filter(|c| c.channel.id == cid).map(|c| &mut c.msgs)) {
                        if let Some(m) = list.iter_mut().find(|m| m.ts == ts) {
                            match m.reactions.iter_mut().find(|(n, _)| *n == name) {
                                Some((_, n)) => *n += 1,
                                None => m.reactions.push((name.clone(), 1)),
                            }
                        }
                    }
                }
                preview_status(&st2, &cid, status);
            });
        }
        After::OpenApp(ts) => {
            let team = st.borrow().ident.as_ref().map(|i| i.team_id.clone()).unwrap_or_default();
            let mut url = format!("slack://channel?team={team}&id={}", channel.id);
            if !ts.is_empty() {
                url.push_str(&format!("&message={ts}"));
            }
            spawn(async move {
                let _ = run_job(&format!("command -v open >/dev/null 2>&1 && open {u} || xdg-open {u}", u = crate::http::sq(&url)), None).await;
            });
        }
        After::Permalink(ts) => {
            let Some(id) = st.borrow().ident.clone() else { return };
            let st2 = Rc::clone(st);
            let cid = channel.id.clone();
            spawn(async move {
                let r = api::permalink(&id, &cid, &ts).await;
                let status = match &r {
                    Ok(_) => "permalink copied".to_string(),
                    Err(e) => format!("permalink: {e}"),
                };
                if let Ok(url) = &r {
                    let _ = run_command(&format!("set-buffer -- '{}'", url.replace('\'', ""))).await;
                }
                preview_status(&st2, &cid, status);
            });
        }
        // n from the preview: the same hand-off as the float's, from the
        // embedded view's messages; the form is a float of its own.
        After::Agent(idxs) => {
            let req = {
                let s = st.borrow();
                let Some(c) = s.side.as_ref().and_then(|side| side.preview.as_ref()) else { return };
                let first_ts = idxs.iter().filter_map(|&i| c.msgs.get(i)).map(|m| m.ts.clone()).min().unwrap_or_default();
                crate::handoff::Request {
                    st: Rc::clone(st),
                    label: s.channel_label(&c.channel),
                    channel: s.channel_label(&c.channel).trim_start_matches(['#', '@']).to_string(),
                    thread: false,
                    first_ts,
                    transcript: chat::transcript(&c.msgs, &idxs, s.tz(), &s.names),
                    count: idxs.len(),
                    client,
                    pane,
                    tz: s.tz(),
                }
            };
            spawn(crate::handoff::open(req));
        }
        After::Copy(idxs) => {
            let text = {
                let s = st.borrow();
                let Some(c) = s.side.as_ref().and_then(|side| side.preview.as_ref()) else { return };
                chat::transcript(&c.msgs, &idxs, s.tz(), &s.names)
            };
            let n = idxs.len();
            let st2 = Rc::clone(st);
            let cid = channel.id.clone();
            spawn(async move {
                let status = match chat::copy_text(&text, client).await {
                    Ok(()) => format!("copied {n} message{}", if n == 1 { "" } else { "s" }),
                    Err(e) => format!("copy: {e}"),
                };
                preview_status(&st2, &cid, status);
            });
        }
        After::Mark => {
            // No hold here: the reader is in this channel, so the cursor
            // follows its row down the list and the preview stays put.
            let st2 = Rc::clone(st);
            let cid = channel.id.clone();
            spawn(async move {
                crate::mark_read(&st2, &cid).await;
            });
        }
        After::Refetch => {
            let id = {
                let mut s = st.borrow_mut();
                let Some(id) = s.ident.clone() else { return };
                let Some(side) = s.side.as_mut() else { return };
                if !side.fetching.insert(channel.id.clone()) {
                    return;
                }
                id
            };
            fetch_preview(st, id, channel.id.clone());
        }
        After::Older => {
            let mut s = st.borrow_mut();
            if let Some(c) = s.side.as_mut().and_then(|side| side.preview.as_mut()) {
                c.status = Some("older history is in the channel view (Enter opens it)".into());
            }
        }
    }
    let mut s = st.borrow_mut();
    render(&mut s);
    drop(s);
    ensure_preview(st);
}

pub fn on_mode_event(st: &Shared, name: &str, event: &Event) {
    match name {
        "mode-key" => {
            let key = event.get_str("key").unwrap_or("").to_string();
            let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
                (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
                _ => None,
            };
            press(st, &key, mouse);
        }
        "mode-nav" => {
            // select-pane -L/-R/-U/-D on the float: right hands the
            // keyboard to the preview, left takes it back, up and down
            // page it while it has the keyboard; else the engine's.
            let dir = event.get_str("dir").unwrap_or("").to_string();
            let outcome = {
                let mut s = st.borrow_mut();
                let Some(side) = s.side.as_mut() else { return };
                side.hold = None;
                let taken = match dir.as_str() {
                    "right" if can_focus(side) => {
                        side.preview_focus = true;
                        true
                    }
                    "left" if side.preview_focus => {
                        side.preview_focus = false;
                        true
                    }
                    "up" | "down" if side.preview_focus => {
                        if let Some(c) = side.preview.as_mut() {
                            let page = c.body_h() as isize;
                            c.scroll(if dir == "up" { -page } else { page });
                        }
                        true
                    }
                    _ => false,
                };
                if taken {
                    render(&mut s);
                    return;
                }
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
                // Into the embedded view's box while it has the
                // keyboard and one is open (a reaction name).
                if side.preview_focus {
                    if let Some(cp) = side.preview.as_mut().and_then(|c| c.compose.as_mut()) {
                        cp.text.push_str(&text);
                        render(&mut s);
                        return;
                    }
                }
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
            render(&mut s);
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
                render(&mut s);
                After::None
            }
            Outcome::Close => After::Close,
            Outcome::Resize(w, h) => {
                let _ = mode_resize(side.engine.mode, w, h);
                let _ = set_option("@slack-width", &w.to_string());
                let _ = set_option("@slack-height", &h.to_string());
                After::None
            }
            Outcome::Activate(key) => match s.channel(chan_key(&key)).cloned() {
                Some(c) => After::OpenChat(c, false),
                None => return,
            },
            Outcome::Action(action, targets) => {
                let chans: Vec<Channel> = targets.iter().filter_map(|k| s.channel(chan_key(k)).cloned()).collect();
                match action {
                    "compose" => match chans.into_iter().next() {
                        Some(c) => After::OpenChat(c, true),
                        None => return,
                    },
                    "open_app" => After::OpenApp(chans),
                    // The preview took n when it had something; here it
                    // has not loaded (or the row is a header).
                    "agent" => {
                        if let Some(side) = s.side.as_mut() {
                            side.engine.status = Some("the preview has nothing to hand off yet".into());
                        }
                        render(&mut s);
                        After::None
                    }
                    "mark" => {
                        let ids: Vec<String> = chans.into_iter().map(|c| c.id).collect();
                        if let Some(side) = s.side.as_mut() {
                            side.engine.clear_marks();
                            side.hold = Some((side.engine.sel(), ids.clone()));
                        }
                        After::Mark(ids)
                    }
                    "view" => {
                        let v = s.side.as_ref().map(|p| p.view).unwrap_or(View::Sections).next();
                        let _ = set_option("@slack-view", v.label());
                        if let Some(side) = s.side.as_mut() {
                            side.view = v;
                            side.engine.reset_scroll();
                        }
                        rebuild(&mut s);
                        render(&mut s);
                        After::None
                    }
                    "refresh" => {
                        if let Some(side) = s.side.as_mut() {
                            side.engine.status = Some("refreshing…".into());
                        }
                        render(&mut s);
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
                render(&mut s);
                drop(s);
                ensure_preview(&st2);
            });
        }
    }
    ensure_preview(st);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: &str) -> Channel {
        Channel {
            id: id.into(),
            name: "general".into(),
            kind: Kind::Public,
            user_id: None,
            topic: String::new(),
            purpose: String::new(),
            is_member: true,
            is_muted: false,
            is_starred: false,
            is_archived: false,
            is_open: true,
            last_read: None,
            latest: None,
            has_unreads: false,
            mentions: 0,
            updated_ms: 0,
        }
    }

    fn msg(ts: &str, text: &str) -> Message {
        Message { channel: "C1".into(), ts: ts.into(), user: Some("U2".into()), text: text.into(), ..Default::default() }
    }

    /// A sidebar of one section and one channel row, the cursor on the
    /// row (or on the header), the row's messages as an embedded view
    /// laid out one line per message.
    fn side(msgs: Vec<Message>, on_header: bool) -> Sidebar {
        let mut engine = Engine::new(ModeId(1), 144, 40, "t", keys(), sigils());
        engine.set_nodes(vec![group_node("sec:work", "Work", 1, 0), Node::item("C1", 1)]);
        if on_header {
            // The cursor rests on a header only when it is folded.
            engine.set_expanded("sec:work", false);
            engine.select_pos(0);
            assert!(engine.selected().is_some_and(|n| n.is_group()));
        } else {
            engine.select_key("C1");
        }
        let (_, pw, body_y, body_h) = preview_geometry(144, 40, engine.list_w());
        let mut side = Sidebar {
            engine,
            client: None,
            pane: None,
            view: View::Sections,
            previews: HashMap::new(),
            fetching: HashSet::new(),
            hold: None,
            preview: None,
            preview_focus: false,
        };
        side.previews.insert("C1".into(), msgs.clone());
        let mut c = chat::embedded(ModeId(1), channel("C1"), None, None, msgs, pw as u32, body_h as u32, body_y as u32);
        c.lines = c.msgs.iter().enumerate().map(|(i, m)| chat::Line { cells: plain_cells(&m.text, 0), msg: Some(i), image: None }).collect();
        side.preview = Some(c);
        side
    }

    fn three() -> Vec<Message> {
        vec![msg("1.000100", "one"), msg("2.000100", "two"), msg("3.000100", "three")]
    }

    #[test]
    fn geometry_fills_the_column_right_of_the_separator() {
        // listkit draws the separator at 1-based column list_w + 1 and
        // its own text previews from list_w + 2: the body starts there
        // (0-based x0 = list_w + 1) and runs to the right edge.
        let (x0, pw, body_y, body_h) = preview_geometry(144, 40, 86);
        assert_eq!((x0, pw), (87, 56));
        assert_eq!(x0 + pw, 143, "the last column stays blank, as under listkit's own text previews");
        assert_eq!((body_y, body_h), (1, 38), "a header row above, the foot row below");
        assert_eq!(preview_geometry(40, 10, 38).1, 0, "no column: nothing to draw");
    }

    #[test]
    fn l_hands_the_keyboard_to_a_row_with_messages() {
        let mut s = side(Vec::new(), false);
        assert_eq!(preview_key(&mut s, "l", None), None, "nothing to read: the engine's key");
        assert!(!s.preview_focus);
        let mut s = side(three(), true);
        assert_eq!(preview_key(&mut s, "l", None), None, "on a header l is the engine's (down a level)");
        let mut s = side(three(), false);
        assert_eq!(preview_key(&mut s, "Right", None), Some(Took::Chat(After::None)));
        assert!(s.preview_focus);
        assert_eq!(s.preview.as_ref().unwrap().cursor_idx(), Some(2), "the cursor opens on the newest message");
    }

    #[test]
    fn focused_keys_are_the_channel_views_and_escape_comes_back() {
        let mut s = side(three(), false);
        s.preview_focus = true;
        assert_eq!(preview_key(&mut s, "k", None), Some(Took::Chat(After::None)));
        assert_eq!(s.preview.as_ref().unwrap().cursor_idx(), Some(1));
        assert_eq!(preview_key(&mut s, "Enter", None), Some(Took::Chat(After::Switch(Some("2.000100".into()), true))));
        assert_eq!(preview_key(&mut s, "r", None), Some(Took::Open(Some("2.000100".into()), true)));
        assert_eq!(preview_key(&mut s, "i", None), Some(Took::Open(None, true)));
        assert_eq!(preview_key(&mut s, "y", None), Some(Took::Chat(After::Copy(vec![1]))));
        assert_eq!(preview_key(&mut s, "q", None), Some(Took::Chat(After::Close)), "q closes the sidebar from the preview too");
        assert_eq!(preview_key(&mut s, "-", None), None, "the size is the engine's");
        assert!(s.preview_focus);
        assert_eq!(preview_key(&mut s, "Escape", None), Some(Took::Chat(After::None)));
        assert!(!s.preview_focus);
        assert_eq!(preview_key(&mut s, "j", None), None, "back on the list, j is the engine's");
        s.preview_focus = true;
        preview_key(&mut s, "h", None);
        assert!(!s.preview_focus, "h gives the keyboard back as well");
    }

    #[test]
    fn escape_clears_a_selection_before_leaving() {
        let mut s = side(three(), false);
        s.preview_focus = true;
        preview_key(&mut s, "v", None);
        assert!(s.preview.as_ref().unwrap().anchor.is_some());
        preview_key(&mut s, "Escape", None);
        assert!(s.preview.as_ref().unwrap().anchor.is_none());
        assert!(s.preview_focus, "the first Escape only cleared the selection");
        preview_key(&mut s, "Escape", None);
        assert!(!s.preview_focus);
    }

    #[test]
    fn the_mouse_takes_the_keyboard_to_the_side_it_lands_on() {
        let mut s = side(three(), false);
        let list_w = s.engine.list_w() as u32;
        // The wheel over the column scrolls it without taking the keyboard.
        assert_eq!(preview_key(&mut s, "WheelUpPane", Some((list_w + 5, 3))), Some(Took::Chat(After::None)));
        assert!(!s.preview_focus);
        // A click there: the keyboard, and the cursor on the clicked line.
        assert_eq!(preview_key(&mut s, "MouseDown1Pane", Some((list_w + 5, 1))), Some(Took::Chat(After::None)));
        assert!(s.preview_focus);
        assert_eq!(s.preview.as_ref().unwrap().cursor_idx(), Some(0), "body row 1 is the first line");
        // A click on the list (or the separator) is the engine's and
        // takes the keyboard back.
        assert_eq!(preview_key(&mut s, "MouseDown1Pane", Some((2, 5))), None);
        assert!(!s.preview_focus);
        assert_eq!(preview_key(&mut s, "WheelDownPane", Some((list_w, 5))), None, "the separator column is the engine's");
        // A mouse key typed by name has no cell: the engine's.
        assert_eq!(preview_key(&mut s, "MouseDown1Pane", None), None);
    }

    #[test]
    fn the_engine_keeps_its_keys_while_a_card_or_the_box_is_up() {
        let mut s = side(three(), false);
        s.engine.show_help = true;
        assert_eq!(preview_key(&mut s, "l", None), None);
        s.engine.show_help = false;
        s.engine.filtering = true;
        assert_eq!(preview_key(&mut s, "l", None), None, "l types into the search box");
    }
}
