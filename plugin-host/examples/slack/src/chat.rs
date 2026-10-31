//! The channel view: one conversation in a float - a channel or DM
//! timeline, or a single thread - read from the cache first, then kept
//! current by a poll while it is open, with a one-line compose box at
//! the bottom.
//!
//! Messages are multi-line, so the view lays them out itself into
//! display lines (`layout`) and scrolls over lines, not messages; the
//! cursor is a message, drawn as a bar in the left gutter on each of
//! its lines. `layout` takes plain inputs so the tests can run it
//! without a mode.
//!
//! Pictures: a message's image files, and the pictures its attachments
//! carry (a Giphy post, an unfurl), are fetched and converted by
//! images.rs, laid out as rows under their `📎` or caption line and
//! drawn as kitty graphics placeholders - `U+10EEEE` cells carrying the
//! image id and the cell's row and column - over one `t=f` transmit per
//! image and block size (listkit::image). Text to tmux, the picture to
//! Ghostty or kitty, a blank block anywhere else; `@slack-images off`
//! turns them off.
//!
//! Borrow discipline, as in sidebar.rs: decide under the `State`
//! borrow, drop it, then do the async work or call into lib.rs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use listkit::image::{kitty_delete, kitty_transmit_file, placeholder_row, preview_fit, MAX_BLOCK};
use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::styled::{emit_cells, fit_cells, plain_cells, wrap_cells, Styled, ST_BOLD, ST_BRIGHT, ST_CYAN, ST_DIM, ST_GREEN, ST_INVERT, ST_ORANGE, ST_RED};
use listkit::text::{clip, menu_item, menu_safe};
use tmux_plugin_sdk::prelude::*;

use crate::store::{Channel, FileRef, Message};
use crate::{api, emoji, images, mrkdwn, sidebar, store, timefmt, NameMap, Shared, State, NAME};

/// Two posts by the same author this close together share one header.
const GROUP_SECS: i64 = 5 * 60;
/// `mark_read` goes to Slack at most this often while the view follows.
const MARK_DEBOUNCE_MS: u64 = 2000;
/// The cell aspect a view assumes before it has asked the window for
/// its own (an embedded view never does; it shows no pictures).
const DEFAULT_CELL_ASPECT: f64 = 2.0;
/// A page of history, for the first fetch of a thin cache and for `L`.
const PAGE: u32 = 100;
/// The cache has to hold at least this many timeline messages before a
/// fetch asks only for what came after it.
const THIN_CACHE: usize = 20;
/// A picture's block is at most this tall...
const IMAGE_ROWS: u32 = 12;
/// ...and this wide (the diacritics number no more cells than that).
const IMAGE_COLS: u32 = 80;
/// The first kitty image id this view hands out; the scp preview takes
/// its ids from the clock, so a fixed range keeps clear of it.
const IMAGE_ID_BASE: u32 = 0x53_0000;

/// One display line: its cells (without the gutter) and the message it
/// belongs to, if any (separators belong to none). A picture's row has
/// `image` set and is drawn as placeholders after its `cells` (the
/// indent).
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub cells: Vec<Styled>,
    pub msg: Option<usize>,
    pub image: Option<ImageRow>,
}

/// Row `row` of the `cols`×`rows` block showing file `file`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageRow {
    pub file: String,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
}

/// What `layout` knows about pictures: the converted ones by file id
/// with their pixel size, and the cell shape (height over width) the
/// blocks are fitted with.
pub struct Pictures<'a> {
    pub sizes: &'a HashMap<String, (u32, u32)>,
    pub cell_aspect: f64,
}

/// A picture handed to the terminal: its kitty id and the block it was
/// placed for. Another block size means another transmit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub id: u32,
    pub cols: u32,
    pub rows: u32,
}

thread_local! {
    static IMAGE_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(IMAGE_ID_BASE) };
}

/// The next kitty image id: never 0, never reused in this process, and
/// inside the 24 bits a placeholder's foreground colour carries.
fn next_image_id() -> u32 {
    IMAGE_ID.with(|g| {
        let id = (g.get() & 0x00ff_ffff).max(1);
        g.set(id + 1);
        id
    })
}

/// What Enter does with the typed text.
#[derive(Clone, Debug, PartialEq)]
pub enum ComposeKind {
    /// Post to the conversation, into a thread when `Some`.
    Message(Option<String>),
    /// Add the named emoji to the message with this ts.
    Reaction(String),
}

pub struct Compose {
    pub kind: ComposeKind,
    pub text: String,
}

pub struct Chat {
    pub mode: ModeId,
    pub channel: Channel,
    /// The root ts when the view shows a thread.
    pub thread: Option<String>,
    /// The thread was opened with Enter from the timeline rather than
    /// by `open <chan> <ts>`. Escape goes back to the timeline either
    /// way; this only records how we got here.
    pub from_timeline: bool,
    pub client: Option<u64>,
    pub pane: Option<u32>,
    pub width: u32,
    pub height: u32,
    /// Rows the frame takes out of `height`: the title, its rule, the
    /// foot's rule and the foot in the float (4); none when the body is
    /// drawn into another view's column.
    pub chrome: u32,
    /// The 0-based screen row of the body's first line (2 under the
    /// float's title and rule).
    pub body_y: u32,
    pub msgs: Vec<Message>,
    pub lines: Vec<Line>,
    /// The first display line on screen.
    pub top: usize,
    /// The cursor message, by ts: stable across a relayout or a prepend.
    pub cursor: Option<String>,
    /// The other end of a selection (`v`), by ts: the messages from it
    /// to the cursor are what `y` copies.
    pub anchor: Option<String>,
    /// Stick to the bottom as messages arrive.
    pub follow: bool,
    /// Slack has older history than what is shown (`L` fetches it).
    pub has_more: bool,
    /// The channel's read mark when the view opened: where the
    /// `new messages` line goes.
    pub last_read: Option<String>,
    pub compose: Option<Compose>,
    pub status: Option<String>,
    /// Bumped whenever the conversation changes, so a fetch that
    /// started for the previous one is dropped on return.
    pub gen: u64,
    pub poll: Option<TaskId>,
    pub fetching: bool,
    pub marked_ms: u64,
    /// The cache was consulted (so an empty view says "no messages",
    /// not "loading").
    pub loaded: bool,
    /// Pictures are drawn (`@slack-images`, on unless set off).
    pub images_on: bool,
    /// Cell height over cell width, for the pictures' blocks.
    pub cell_aspect: f64,
    /// The plugin's data directory, where the pictures are.
    pub data_root: String,
    /// The pictures the terminal holds for this view, by file id. A
    /// cell, not a field the paint borrows mutably: `paint` reads the
    /// state and this is the one thing it records.
    pub placed: RefCell<HashMap<String, Placed>>,
}

impl Chat {
    /// The image files of the messages near the viewport (a page above
    /// and below) that `cache` has not seen: what to fetch now.
    pub(crate) fn wanted_images(&self, cache: &images::Cache) -> Vec<FileRef> {
        if !self.images_on {
            return Vec::new();
        }
        let h = self.body_h();
        let from = self.top.saturating_sub(h);
        let to = self.top + 2 * h;
        let mut out: Vec<FileRef> = Vec::new();
        let mut seen: Option<usize> = None;
        for l in self.lines.iter().take(to).skip(from) {
            let Some(i) = l.msg else { continue };
            if seen == Some(i) {
                continue;
            }
            seen = Some(i);
            for f in self.msgs[i].files.iter().chain(&self.msgs[i].pictures) {
                if f.is_image() && !cache.ready.contains_key(&f.id) && !cache.asked.contains(&f.id) && !out.iter().any(|o| o.id == f.id) {
                    out.push(f.clone());
                }
            }
        }
        out
    }

    /// Tell the terminal to forget every picture placed for this view
    /// (through the mode, so before it closes).
    pub(crate) fn forget_images(&self) {
        let seqs: String = self.placed.borrow_mut().drain().map(|(_, p)| kitty_delete(p.id)).collect();
        if !seqs.is_empty() {
            let _ = mode_write(self.mode, seqs.as_bytes());
        }
    }

    pub(crate) fn body_h(&self) -> usize {
        (self.height as usize).saturating_sub(self.chrome as usize).max(1)
    }

    /// The messages a copy acts on: from the anchor to the cursor while
    /// a selection is open, else the cursor message alone.
    fn selection(&self) -> Vec<usize> {
        let Some(cur) = self.cursor_idx() else { return Vec::new() };
        let anchor = self.anchor.as_deref().and_then(|a| self.msgs.iter().position(|m| m.ts == a));
        match anchor {
            Some(a) => (a.min(cur)..=a.max(cur)).collect(),
            None => vec![cur],
        }
    }

    /// Message `i` is inside the open selection.
    fn in_selection(&self, i: usize) -> bool {
        let (Some(cur), Some(a)) = (self.cursor_idx(), self.anchor.as_deref()) else { return false };
        let Some(a) = self.msgs.iter().position(|m| m.ts == a) else { return false };
        (a.min(cur)..=a.max(cur)).contains(&i)
    }

    pub(crate) fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.body_h())
    }

    pub(crate) fn cursor_idx(&self) -> Option<usize> {
        let ts = self.cursor.as_deref()?;
        self.msgs.iter().position(|m| m.ts == ts)
    }

    pub(crate) fn set_cursor_idx(&mut self, i: usize) {
        self.cursor = self.msgs.get(i).map(|m| m.ts.clone());
    }

    /// Scroll so the cursor message is on screen; a message taller than
    /// the body shows from its first line.
    pub(crate) fn reveal_cursor(&mut self) {
        let Some(i) = self.cursor_idx() else { return };
        let Some((first, last)) = lines_of(&self.lines, i) else { return };
        let h = self.body_h();
        if first < self.top {
            self.top = first;
        } else if last >= self.top + h {
            self.top = (last + 1 - h).min(first);
        }
        self.top = self.top.min(self.max_top());
    }

    /// Scroll by `delta` lines; moving up leaves the bottom, reaching
    /// it sticks there again.
    pub(crate) fn scroll(&mut self, delta: isize) {
        let was = self.top;
        let top = (self.top as isize + delta).max(0) as usize;
        self.top = top.min(self.max_top());
        if self.top < was {
            self.follow = false;
        } else if self.top == self.max_top() && delta > 0 {
            self.follow = true;
        }
    }

    /// The first message with a line at or below `top`: the cursor
    /// after a page or a click.
    fn msg_at_top(&self) -> Option<usize> {
        self.lines.iter().skip(self.top).find_map(|l| l.msg)
    }
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

/// The display lines of a conversation, oldest at the top.
///
/// `width` is the whole float; the lines are laid out for `width - 1`
/// cells (the gutter is drawn by `paint`). `me` is the user's own id,
/// `last_read` the channel's read mark when the view opened, `now_secs`
/// the clock for the day labels and thread ages, `in_thread` whether
/// the messages are a thread (no thread markers then). `pics` is what
/// is known about pictures; `None` draws none (the `📎` lines stay).
#[allow(clippy::too_many_arguments)]
pub fn layout(
    msgs: &[Message],
    width: usize,
    tz: i64,
    me: &str,
    last_read: Option<&str>,
    names: &NameMap,
    now_secs: i64,
    in_thread: bool,
    pics: Option<&Pictures>,
) -> Vec<Line> {
    let cw = width.saturating_sub(1).max(8);
    let body_w = width.saturating_sub(4).max(4);
    let today = timefmt::local(now_secs, tz).0;
    let mut out: Vec<Line> = Vec::new();
    let sep = |out: &mut Vec<Line>, label: &str, style: u16| {
        let text = format!("── {label} ──");
        let pad = cw.saturating_sub(text.chars().count()) / 2;
        let mut cells: Vec<Styled> = plain_cells(&" ".repeat(pad), 0);
        cells.extend(plain_cells(&text, style));
        out.push(Line { cells, msg: None, image: None });
    };
    let author = |m: &Message| m.user.clone().or_else(|| m.username.clone()).unwrap_or_default();
    // The read mark goes before the first message newer than it that is
    // not ours (what we said is not news), and only when something
    // older is on screen to be "new" against.
    let new_at = last_read
        .and_then(|lr| msgs.iter().position(|m| m.ts.as_str() > lr && m.user.as_deref() != Some(me)))
        .filter(|i| *i > 0);
    let mut prev_day: Option<i64> = None;
    let mut prev: Option<&Message> = None;
    for (i, m) in msgs.iter().enumerate() {
        let secs = timefmt::secs(&m.ts);
        let day = timefmt::local(secs, tz).0;
        let mut separated = false;
        if prev_day != Some(day) {
            sep(&mut out, &timefmt::day_label(day, today), ST_DIM);
            separated = true;
            prev_day = Some(day);
        }
        if new_at == Some(i) {
            sep(&mut out, "new messages", ST_RED);
            separated = true;
        }
        let grouped = !separated
            && prev.is_some_and(|p| author(p) == author(m) && secs - timefmt::secs(&p.ts) < GROUP_SECS);
        if !grouped {
            let who = m
                .user
                .as_deref()
                .map(|u| names.users.get(u).cloned().unwrap_or_else(|| u.to_string()))
                .or_else(|| m.username.clone())
                .unwrap_or_else(|| "?".into());
            let colour = if m.user.as_deref() == Some(me) { ST_GREEN } else { ST_CYAN };
            let mut cells = plain_cells(&who, ST_BOLD | colour);
            cells.extend(plain_cells(&format!("  {}", timefmt::clock(&m.ts, tz)), ST_DIM));
            if m.edited {
                cells.extend(plain_cells(" (edited)", ST_DIM));
            }
            out.push(Line { cells, msg: Some(i), image: None });
        }
        let indent = || plain_cells("  ", 0);
        if m.deleted {
            let mut cells = indent();
            cells.extend(plain_cells("(deleted)", ST_DIM));
            out.push(Line { cells, msg: Some(i), image: None });
        } else if !m.text.trim().is_empty() {
            let body = names.with(|n| mrkdwn::render(&m.text, body_w, 0, n));
            for mut l in body {
                let mut cells = indent();
                cells.append(&mut l);
                out.push(Line { cells, msg: Some(i), image: None });
            }
        }
        for a in &m.attachments {
            for raw in a.lines() {
                let text = names.with(|n| mrkdwn::inline(raw, ST_DIM, n));
                for mut w in wrap_cells(&text, body_w.saturating_sub(2).max(2)) {
                    let mut cells = indent();
                    cells.extend(plain_cells("│ ", ST_DIM));
                    cells.append(&mut w);
                    out.push(Line { cells, msg: Some(i), image: None });
                }
            }
        }
        // An attachment's picture (a Giphy post, an unfurl): its caption
        // is among the attachment lines, the block goes under them.
        for p in &m.pictures {
            if let Some((cols, rows)) = pics.and_then(|px| image_block(px, p, body_w)) {
                for row in 0..rows {
                    out.push(Line { cells: indent(), msg: Some(i), image: Some(ImageRow { file: p.id.clone(), row, cols, rows }) });
                }
            }
        }
        for f in &m.files {
            let mut cells = indent();
            cells.extend(plain_cells(&format!("📎 {}", f.name), ST_DIM));
            out.push(Line { cells, msg: Some(i), image: None });
            // The picture, once it is on disk: a block under the line.
            if let Some((cols, rows)) = pics.and_then(|p| image_block(p, f, body_w)) {
                for row in 0..rows {
                    out.push(Line { cells: indent(), msg: Some(i), image: Some(ImageRow { file: f.id.clone(), row, cols, rows }) });
                }
            }
        }
        if !m.reactions.is_empty() {
            let text = m.reactions.iter().map(|(n, c)| format!(":{n}: {c}")).collect::<Vec<_>>().join(" · ");
            let mut cells = indent();
            emoji::push_text(&mut cells, &text, ST_DIM);
            out.push(Line { cells, msg: Some(i), image: None });
        }
        if !in_thread && m.reply_count > 0 {
            let when = m.latest_reply.as_deref().unwrap_or(&m.ts);
            let age = listkit::text::fmt_age(now_secs.saturating_sub(timefmt::secs(when)).max(0) as u64);
            let text = format!(
                "↳ {} {} · {age}",
                m.reply_count,
                if m.reply_count == 1 { "reply" } else { "replies" }
            );
            let mut cells = indent();
            cells.extend(plain_cells(&text, ST_CYAN));
            out.push(Line { cells, msg: Some(i), image: None });
        }
        prev = Some(m);
    }
    out
}

/// The block for file `f`'s picture in a body `body_w` cells wide, when
/// the picture is there: at most [`IMAGE_ROWS`] tall and [`IMAGE_COLS`]
/// wide, the image's shape within that.
fn image_block(pics: &Pictures, f: &FileRef, body_w: usize) -> Option<(u32, u32)> {
    if !f.is_image() {
        return None;
    }
    let &(w, h) = pics.sizes.get(&f.id)?;
    let max_cols = (body_w as u32).min(IMAGE_COLS).min(MAX_BLOCK);
    Some(preview_fit(w, h, pics.cell_aspect, max_cols, IMAGE_ROWS))
}

/// The first and last display line of message `i`.
pub fn lines_of(lines: &[Line], i: usize) -> Option<(usize, usize)> {
    let first = lines.iter().position(|l| l.msg == Some(i))?;
    let last = lines.iter().rposition(|l| l.msg == Some(i))?;
    Some((first, last))
}

/// Messages `idxs` as text for the clipboard, one per line with its
/// author and stamp, continuation lines indented:
///
/// ```text
/// Tejas (2026-10-30 14:02): first line
///   second line
///   📎 screenshot.png
/// zack (2026-10-30 14:05): reply
/// ```
pub fn transcript(msgs: &[Message], idxs: &[usize], tz: i64, names: &NameMap) -> String {
    let mut out = String::new();
    for &i in idxs {
        let Some(m) = msgs.get(i) else { continue };
        let who = m
            .user
            .as_deref()
            .map(|u| names.users.get(u).cloned().unwrap_or_else(|| u.to_string()))
            .or_else(|| m.username.clone())
            .unwrap_or_else(|| "?".into());
        let mut lines: Vec<String> = Vec::new();
        if m.deleted {
            lines.push("(deleted)".into());
        } else if !m.text.trim().is_empty() {
            lines.extend(names.with(|n| mrkdwn::plain_lines(&m.text, n)));
        }
        for a in &m.attachments {
            for raw in a.lines() {
                let t = names.with(|n| mrkdwn::plain(raw, n));
                if !t.is_empty() {
                    lines.push(format!("│ {t}"));
                }
            }
        }
        for f in &m.files {
            lines.push(format!("📎 {}", f.name));
        }
        let head = format!("{who} ({}):", timefmt::stamp(&m.ts, tz));
        let mut it = lines.into_iter();
        match it.next() {
            Some(first) => out.push_str(&format!("{head} {first}\n")),
            None => out.push_str(&format!("{head}\n")),
        }
        for l in it {
            out.push_str(&format!("  {l}\n"));
        }
    }
    out
}

/// Lay the open conversation out again from the current state.
fn relayout(s: &mut State) {
    let names = Rc::clone(&s.names);
    let tz = s.tz();
    let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
    let now = (now_ms() / 1000) as i64;
    let State { chat, images, .. } = s;
    let Some(c) = chat.as_mut() else { return };
    let pics = Pictures { sizes: &images.ready, cell_aspect: c.cell_aspect };
    let on = c.images_on;
    layout_chat(c, &names, tz, &me, now, on.then_some(&pics));
    let wanted = c.wanted_images(images);
    if !wanted.is_empty() {
        images::request(s, wanted);
    }
}

/// Lay one view out again at its width: the float's, or a view embedded
/// in another's column (the sidebar's preview, which passes no
/// pictures). Following the bottom keeps it there; otherwise a reader
/// scrolled up keeps the message under the top line where it is,
/// however many picture rows arrive above it.
pub(crate) fn layout_chat(c: &mut Chat, names: &NameMap, tz: i64, me: &str, now: i64, pics: Option<&Pictures>) {
    let anchor = (!c.follow)
        .then(|| c.lines.iter().skip(c.top).find_map(|l| l.msg))
        .flatten()
        .and_then(|i| Some((c.msgs.get(i)?.ts.clone(), c.top as isize - lines_of(&c.lines, i)?.0 as isize)));
    c.lines = layout(&c.msgs, c.width as usize, tz, me, c.last_read.as_deref(), names, now, c.thread.is_some(), pics);
    if c.follow {
        c.top = c.max_top();
    } else {
        if let Some((ts, off)) = anchor {
            if let Some(first) = c.msgs.iter().position(|m| m.ts == ts).and_then(|i| lines_of(&c.lines, i)).map(|(f, _)| f) {
                c.top = (first as isize + off).max(0) as usize;
            }
        }
        c.top = c.top.min(c.max_top());
    }
}

// ---------------------------------------------------------------------------
// paint
// ---------------------------------------------------------------------------

pub(crate) fn hints() -> &'static str {
    "j/k move · Enter thread · i compose · r reply · e react · y copy · v select · n agent · m read · L older · Space menu · Esc back · ? help · q close"
}

fn rule(width: usize) -> String {
    "─".repeat(width)
}

/// The compose box: `› text` with a reverse-video cursor cell; the tail
/// of a long text, so the cursor stays on screen.
pub(crate) fn compose_cells(c: &Compose, width: usize) -> Vec<Styled> {
    let prompt = match &c.kind {
        ComposeKind::Message(Some(_)) => "reply › ",
        ComposeKind::Message(None) => "› ",
        ComposeKind::Reaction(_) => "react :",
    };
    let mut cells = plain_cells(prompt, ST_DIM);
    let avail = width.saturating_sub(cells.len() + 1);
    let shown: Vec<char> = c.text.chars().map(|ch| if ch == '\n' { '↵' } else { ch }).collect();
    let skip = shown.len().saturating_sub(avail);
    for ch in &shown[skip..] {
        cells.push((*ch, 0));
    }
    cells.push((' ', ST_INVERT));
    cells
}

/// Draw the whole screen.
fn paint(s: &State) {
    let Some(c) = s.chat.as_ref() else { return };
    let w = c.width as usize;
    let h = c.height as usize;
    let mut out = String::from("\x1b[2J\x1b[H");
    // Row 1: the title.
    let mut title = plain_cells(&format!(" {}", s.channel_label(&c.channel)), ST_BOLD);
    if c.thread.is_some() {
        title.extend(plain_cells(" · thread", ST_BOLD | ST_DIM));
    }
    if !c.channel.topic.is_empty() {
        title.extend(plain_cells("  ", 0));
        title.extend(s.names.with(|n| mrkdwn::inline(&c.channel.topic, ST_DIM, n)));
    }
    out.push_str(&format!("\x1b[1;1H{}", emit_cells(&fit_cells(&title, w), false)));
    // Row 2: a rule, with the older-history hint when there is any.
    out.push_str(&format!("\x1b[2;1H\x1b[2m{}\x1b[0m", rule(w)));
    if c.top > 0 || c.has_more {
        let hint = if c.has_more && c.thread.is_none() { " ↑ older (L) " } else { " ↑ older " };
        let n = hint.chars().count();
        if w > n + 2 {
            out.push_str(&format!("\x1b[2;{}H\x1b[2m{hint}\x1b[0m", w - n));
        }
    }
    paint_body(c, &mut out, 0, c.body_y as usize);
    // The foot: a rule, then the compose box, a status, or the hints.
    if h >= 2 {
        out.push_str(&format!("\x1b[{};1H\x1b[2m{}\x1b[0m", h - 1, rule(w)));
    }
    let foot: Vec<Styled> = if let Some(cp) = &c.compose {
        compose_cells(cp, w.saturating_sub(1))
    } else if let Some(st) = &c.status {
        plain_cells(&clip(st, w.saturating_sub(2)), ST_CYAN)
    } else {
        plain_cells(&clip(hints(), w.saturating_sub(2)), ST_DIM)
    };
    out.push_str(&format!("\x1b[{h};2H{}", emit_cells(&foot, false)));
    let _ = mode_write(c.mode, out.as_bytes());
}

/// Draw the body - the message lines from `top`, each behind its gutter
/// cell - with its top-left at 0-based column `x0`, row `y0`, `width`
/// cells wide and `body_h` rows tall. The frame around it is the
/// caller's (the float's title and foot, or another view's column).
pub(crate) fn paint_body(c: &Chat, out: &mut String, x0: usize, y0: usize) {
    let w = c.width as usize;
    let body_h = c.body_h();
    let cursor = c.cursor_idx();
    if c.lines.is_empty() {
        let text = if !c.loaded {
            "loading…"
        } else if c.thread.is_some() {
            "(no replies)"
        } else {
            "(no messages)"
        };
        out.push_str(&format!("\x1b[{};{}H\x1b[2m{text}\x1b[0m", y0 + 1, x0 + 3));
    }
    for (k, line) in c.lines.iter().skip(c.top).take(body_h).enumerate() {
        let row = y0 + 1 + k;
        let on_cursor = cursor.is_some() && line.msg == cursor;
        let selected = line.msg.is_some_and(|i| c.in_selection(i));
        let gutter = if on_cursor {
            ('▎', ST_CYAN | ST_BRIGHT)
        } else if selected {
            ('▎', ST_ORANGE)
        } else {
            (' ', 0)
        };
        let mut cells: Vec<Styled> = vec![gutter];
        if let Some(img) = &line.image {
            // The indent as text, then the placeholders: the terminal
            // draws row `img.row` of the picture over them.
            cells.extend(fit_cells(&line.cells, w.saturating_sub(1)));
            let cols = img.cols.min(w.saturating_sub(cells.len() + 1) as u32);
            let id = place_image(c, img, out);
            out.push_str(&format!("\x1b[{row};{}H{}{}", x0 + 1, emit_cells(&cells, false), placeholder_row(id, img.row, cols)));
            let used = cells.len() + cols as usize;
            if used < w {
                out.push_str(&" ".repeat(w - used));
            }
            continue;
        }
        cells.extend(fit_cells(&line.cells, w.saturating_sub(1)));
        out.push_str(&format!("\x1b[{row};{}H{}", x0 + 1, emit_cells(&cells, false)));
    }
}

/// The kitty id showing `img`'s file at its block size: the one already
/// placed, else a transmit (by path: the terminal reads the PNG itself)
/// written into `out` ahead of the row, the old placement of another
/// size deleted first. Scrolling, then, re-sends only text.
fn place_image(c: &Chat, img: &ImageRow, out: &mut String) -> u32 {
    let mut placed = c.placed.borrow_mut();
    if let Some(p) = placed.get(&img.file) {
        if p.cols == img.cols && p.rows == img.rows {
            return p.id;
        }
        out.push_str(&kitty_delete(p.id));
    }
    let id = next_image_id();
    let path = format!("{}/{}", c.data_root, images::png_name(&img.file));
    out.push_str(&kitty_transmit_file(id, &path, img.cols, img.rows));
    placed.insert(img.file.clone(), Placed { id, cols: img.cols, rows: img.rows });
    id
}

fn relayout_paint(s: &mut State) {
    relayout(s);
    paint(s);
}

// ---------------------------------------------------------------------------
// open / close
// ---------------------------------------------------------------------------

fn size_box() -> SizeBox {
    SizeBox { min_w: 60, min_h: 12, max_w: 160, max_h: 50, fill_tenths: 8 }
}

fn opt_u32(name: &str) -> Option<u32> {
    get_option(name).ok().and_then(|v| v.trim().parse::<u32>().ok()).filter(|v| *v > 0)
}

/// An on/off option: `off`, `0`, `no` or `false` turn it off, anything
/// else (unset included) is `default`.
pub(crate) fn opt_flag(name: &str, default: bool) -> bool {
    match get_option(name).ok().map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "off" | "0" | "no" | "false") => false,
        Some(v) if matches!(v.as_str(), "on" | "1" | "yes" | "true") => true,
        _ => default,
    }
}

thread_local! {
    static GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn next_gen() -> u64 {
    GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    })
}

pub fn open(st: &Shared, ch: Channel, thread: Option<String>, client: Option<u64>, pane: Option<u32>, compose: bool) {
    close(st);
    let Some(window) = listkit::window_for(client, pane) else {
        let _ = display_message("slack: no window to open the channel in");
        return;
    };
    let size = size_box();
    let (ww, wh) = resolve_window(WindowId(window)).map(|w| (w.width, w.height)).unwrap_or((size.max_w, size.max_h));
    let (mut width, mut height) = default_size(ww, wh, &size);
    if let Some(n) = opt_u32("@slack-chat-width") {
        width = clamp_dim(n, size.min_w, ww);
    }
    if let Some(n) = opt_u32("@slack-chat-height") {
        height = clamp_dim(n, size.min_h, wh);
    }
    let label = st.borrow().channel_label(&ch);
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
        title: Some(format!("{NAME} {label}")),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("slack: open: {}", e.message));
            return;
        }
    };
    let gen = next_gen();
    let compose = compose.then(|| Compose { kind: ComposeKind::Message(thread.clone()), text: String::new() });
    let images_on = opt_flag("@slack-images", true);
    let cell_aspect = listkit::image::cell_aspect(window);
    let data_root = fs_root().unwrap_or_default();
    {
        let mut s = st.borrow_mut();
        // The freshest channel row we have: the badge may have moved
        // since the caller's copy was taken.
        let channel = s.channel(&ch.id).cloned().unwrap_or(ch);
        let last_read = channel.last_read.clone();
        s.chat = Some(Chat {
            mode,
            channel,
            thread,
            from_timeline: false,
            client,
            pane,
            width,
            height,
            chrome: 4,
            body_y: 2,
            msgs: Vec::new(),
            lines: Vec::new(),
            top: 0,
            cursor: None,
            anchor: None,
            follow: true,
            has_more: false,
            last_read,
            compose,
            status: None,
            gen,
            poll: None,
            fetching: false,
            marked_ms: 0,
            loaded: false,
            images_on,
            cell_aspect,
            data_root,
            placed: RefCell::new(HashMap::new()),
        });
        paint(&s);
    }
    start_session(st, gen);
}

pub fn close(st: &Shared) {
    if let Some(c) = st.borrow_mut().chat.take() {
        if let Some(t) = c.poll {
            cancel(t);
        }
        c.forget_images();
        let _ = mode_close(c.mode);
    }
}

/// A picture arrived on disk (images.rs): lay out again, so its rows
/// appear under its `📎` line, and draw them.
pub fn on_image(st: &Shared) {
    let mut s = st.borrow_mut();
    if s.chat.is_none() {
        return;
    }
    relayout_paint(&mut s);
}

/// What a view needs to show pictures: whether they are on, the cell
/// aspect of the window it draws into, and where the files are.
pub(crate) fn picture_setup(client: Option<u64>, pane: Option<u32>) -> (bool, f64, String) {
    let on = opt_flag("@slack-images", true);
    let aspect = listkit::window_for(client, pane).map(listkit::image::cell_aspect).unwrap_or(DEFAULT_CELL_ASPECT);
    (on, aspect, fs_root().unwrap_or_default())
}

/// A timeline view drawn inside another view's column (the sidebar's
/// preview): no frame of its own, no session or poll - the owner feeds
/// it messages and calls `layout_chat`. `width` and `height` are the
/// body's; `body_y` its first screen row (0-based). It opens at its
/// end with the cursor on the newest message.
#[allow(clippy::too_many_arguments)]
pub(crate) fn embedded(mode: ModeId, channel: Channel, client: Option<u64>, pane: Option<u32>, msgs: Vec<Message>, width: u32, height: u32, body_y: u32) -> Chat {
    let last_read = channel.last_read.clone();
    let mut c = Chat {
        mode,
        channel,
        thread: None,
        from_timeline: false,
        client,
        pane,
        width,
        height,
        chrome: 0,
        body_y,
        msgs,
        lines: Vec::new(),
        top: 0,
        cursor: None,
        anchor: None,
        follow: true,
        has_more: false,
        last_read,
        compose: None,
        status: None,
        gen: 0,
        poll: None,
        fetching: false,
        marked_ms: 0,
        loaded: true,
        images_on: false,
        cell_aspect: DEFAULT_CELL_ASPECT,
        data_root: String::new(),
        placed: RefCell::new(HashMap::new()),
    };
    let last = c.msgs.len().saturating_sub(1);
    c.set_cursor_idx(last);
    c
}

/// Repaint if open (something the view shows changed).
pub fn redraw(st: &Shared) {
    let mut s = st.borrow_mut();
    if s.chat.is_none() {
        return;
    }
    // The channel row may have moved (topic, read mark).
    if let Some(fresh) = s.chat.as_ref().and_then(|c| s.channel(&c.channel.id).cloned()) {
        if let Some(c) = s.chat.as_mut() {
            c.channel = fresh;
        }
    }
    relayout_paint(&mut s);
}

/// The names map was rebuilt: lay out again with the new names.
pub fn refresh_names(st: &Shared) {
    let mut s = st.borrow_mut();
    if s.chat.is_none() {
        return;
    }
    relayout_paint(&mut s);
}

/// A message we posted (lib.rs): show it at once if it belongs here.
pub fn on_posted(st: &Shared, m: Message) {
    let mut s = st.borrow_mut();
    let Some(c) = s.chat.as_mut() else { return };
    if m.channel != c.channel.id {
        return;
    }
    let belongs = match &c.thread {
        Some(root) => m.ts == *root || m.thread_ts.as_deref() == Some(root.as_str()),
        None => !m.is_reply(),
    };
    if belongs {
        merge(&mut c.msgs, vec![m]);
        c.follow = true;
        let last = c.msgs.len().saturating_sub(1);
        c.set_cursor_idx(last);
    } else if c.thread.is_none() {
        // A reply posted from the timeline: the root grows a marker.
        if let Some(root) = m.thread_ts.as_deref().and_then(|t| c.msgs.iter_mut().find(|x| x.ts == t)) {
            root.reply_count += 1;
            root.latest_reply = Some(m.ts.clone());
        }
    } else {
        return;
    }
    if c.status.as_deref() == Some("posting…") {
        c.status = None;
    }
    relayout_paint(&mut s);
}

/// Does `m` belong in the open conversation?
fn belongs(c: &Chat, m: &Message) -> bool {
    if m.channel != c.channel.id {
        return false;
    }
    match &c.thread {
        Some(root) => m.ts == *root || m.thread_ts.as_deref() == Some(root.as_str()),
        None => !m.is_reply(),
    }
}

/// A message that arrived on the feed: shown at once when it belongs
/// here. Following the bottom keeps following; a reader scrolled up is
/// told, not moved.
pub fn on_arrived(st: &Shared, m: Message) {
    let mut s = st.borrow_mut();
    let Some(c) = s.chat.as_mut() else { return };
    if belongs(c, &m) {
        if merge(&mut c.msgs, vec![m]) > 0 {
            if c.follow {
                let last = c.msgs.len().saturating_sub(1);
                c.set_cursor_idx(last);
            } else {
                c.status = Some("new messages below (G)".into());
            }
        }
    } else if c.thread.is_none() && m.channel == c.channel.id {
        if let Some(root) = m.thread_ts.as_deref().and_then(|t| c.msgs.iter_mut().find(|x| x.ts == t)) {
            root.reply_count += 1;
            root.latest_reply = Some(m.ts.clone());
        }
    } else {
        return;
    }
    relayout_paint(&mut s);
}

/// A message changed (an edit, a reaction, a reply count): the shown
/// copy is replaced.
pub fn on_changed(st: &Shared, m: Message) {
    let mut s = st.borrow_mut();
    let Some(c) = s.chat.as_mut() else { return };
    if !belongs(c, &m) || !c.msgs.iter().any(|x| x.ts == m.ts) {
        return;
    }
    merge(&mut c.msgs, vec![m]);
    relayout_paint(&mut s);
}

/// A message was deleted.
pub fn on_deleted(st: &Shared, channel: &str, ts: &str) {
    let mut s = st.borrow_mut();
    let Some(c) = s.chat.as_mut() else { return };
    if c.channel.id != channel {
        return;
    }
    let before = c.msgs.len();
    c.msgs.retain(|x| x.ts != ts);
    if c.msgs.len() == before {
        return;
    }
    if c.cursor.as_deref() == Some(ts) {
        c.cursor = None;
    }
    relayout_paint(&mut s);
}

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

/// Upsert `new` into `msgs` by ts, keeping ts order.
pub(crate) fn merge(msgs: &mut Vec<Message>, new: Vec<Message>) -> usize {
    let mut added = 0;
    for m in new {
        match msgs.iter_mut().find(|x| x.ts == m.ts) {
            Some(slot) => *slot = m,
            None => {
                msgs.push(m);
                added += 1;
            }
        }
    }
    if added > 0 {
        msgs.sort_by(|a, b| a.ts.cmp(&b.ts));
    }
    added
}

/// Is the view still showing generation `gen`?
fn live(st: &Shared, gen: u64) -> bool {
    st.borrow().chat.as_ref().is_some_and(|c| c.gen == gen)
}

/// Cancel the running session and start one for the current
/// conversation: cache, first fetch, then the poll.
fn start_session(st: &Shared, gen: u64) {
    let old = st.borrow_mut().chat.as_mut().and_then(|c| c.poll.take());
    if let Some(t) = old {
        cancel(t);
    }
    let st2 = Rc::clone(st);
    let task = spawn(async move {
        session(st2, gen).await;
    });
    if let Some(c) = st.borrow_mut().chat.as_mut() {
        if c.gen == gen {
            c.poll = Some(task);
        }
    }
}

async fn session(st: Shared, gen: u64) {
    let Some((channel, thread)) = st.borrow().chat.as_ref().filter(|c| c.gen == gen).map(|c| (c.channel.id.clone(), c.thread.clone())) else {
        return;
    };
    let cached = match &thread {
        Some(root) => store::thread(&channel, root).await,
        None => store::messages(&channel, 200).await,
    };
    {
        let mut s = st.borrow_mut();
        let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
        c.msgs = cached;
        c.loaded = true;
        let last = c.msgs.len().saturating_sub(1);
        c.set_cursor_idx(last);
        relayout_paint(&mut s);
    }
    fetch(&st, gen).await;
    loop {
        let ms = st.borrow().cfg.open_poll_ms;
        if sleep_ms(ms).await.is_err() || !live(&st, gen) {
            return;
        }
        fetch(&st, gen).await;
    }
}

/// Fetch what is newer than the view has (a thread: all of it) and show
/// it. Overlapping fetches of one view are skipped, not queued.
async fn fetch(st: &Shared, gen: u64) {
    let (id, channel, thread, newest, count) = {
        let mut s = st.borrow_mut();
        let Some(id) = s.ident.clone() else { return };
        let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
        if c.fetching {
            return;
        }
        c.fetching = true;
        (id, c.channel.id.clone(), c.thread.clone(), c.msgs.last().map(|m| m.ts.clone()), c.msgs.len())
    };
    let r: Result<(Vec<Message>, Option<bool>), String> = match &thread {
        Some(root) => api::replies(&id, &channel, root).await.map(|m| (m, None)),
        None => {
            // A thin cache gets a whole page; a full one only the tail.
            let oldest = if count < THIN_CACHE { None } else { newest.as_deref() };
            api::history(&id, &channel, oldest, None, PAGE).await.map(|(mut m, more)| {
                m.reverse();
                (m, if oldest.is_none() { Some(more) } else { None })
            })
        }
    };
    if !live(st, gen) {
        if let Some(c) = st.borrow_mut().chat.as_mut() {
            c.fetching = false;
        }
        return;
    }
    let follow = match r {
        Ok((msgs, more)) => {
            crate::absorb(st, &channel, &msgs).await;
            let mut s = st.borrow_mut();
            let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
            c.fetching = false;
            let added = if thread.is_some() {
                let before = c.msgs.len();
                c.msgs = msgs;
                c.msgs.len().saturating_sub(before)
            } else {
                merge(&mut c.msgs, msgs.into_iter().filter(|m| !m.is_reply()).collect())
            };
            if let Some(more) = more {
                c.has_more = more;
            }
            // Following the bottom, the cursor goes to what just arrived;
            // a poll that brought nothing leaves it where the user put it
            // (a selection in progress, say).
            if c.follow && added > 0 {
                let last = c.msgs.len().saturating_sub(1);
                c.set_cursor_idx(last);
            } else if added > 0 {
                c.status = Some(format!("{added} new below"));
            }
            let follow = c.follow && !c.msgs.is_empty();
            relayout_paint(&mut s);
            follow
        }
        Err(e) => {
            let mut s = st.borrow_mut();
            let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
            c.fetching = false;
            c.status = Some(format!("fetch: {e}"));
            paint(&s);
            false
        }
    };
    if follow {
        maybe_mark_read(st);
    }
}

/// `mark_read` for the open channel, at most once per debounce window.
fn maybe_mark_read(st: &Shared) {
    let channel = {
        let mut s = st.borrow_mut();
        let Some(c) = s.chat.as_mut() else { return };
        let now = now_ms();
        if now.saturating_sub(c.marked_ms) < MARK_DEBOUNCE_MS {
            return;
        }
        c.marked_ms = now;
        c.channel.id.clone()
    };
    let st2 = Rc::clone(st);
    spawn(async move {
        crate::mark_read(&st2, &channel).await;
    });
}

/// `L`: the page before the oldest message shown, keeping the line
/// under the top of the screen where it is.
fn load_older(st: &Shared) {
    let (id, channel, oldest, gen) = {
        let mut s = st.borrow_mut();
        let Some(id) = s.ident.clone() else { return };
        let Some(c) = s.chat.as_mut() else { return };
        if c.thread.is_some() {
            c.status = Some("a thread is fetched whole".into());
            paint(&s);
            return;
        }
        if c.fetching {
            return;
        }
        let Some(oldest) = c.msgs.first().map(|m| m.ts.clone()) else { return };
        c.fetching = true;
        c.status = Some("loading older…".into());
        let out = (id, c.channel.id.clone(), oldest, c.gen);
        paint(&s);
        out
    };
    let st2 = Rc::clone(st);
    spawn(async move {
        let r = api::history(&id, &channel, None, Some(&oldest), PAGE).await;
        if !live(&st2, gen) {
            return;
        }
        match r {
            Ok((mut msgs, more)) => {
                msgs.reverse();
                crate::absorb(&st2, &channel, &msgs).await;
                let mut s = st2.borrow_mut();
                let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
                c.fetching = false;
                // Anchor: the message at the top line and how far into it.
                let anchor = c.lines.iter().skip(c.top).find_map(|l| l.msg).and_then(|i| {
                    let (first, _) = lines_of(&c.lines, i)?;
                    Some((c.msgs[i].ts.clone(), c.top.saturating_sub(first)))
                });
                let added = merge(&mut c.msgs, msgs.into_iter().filter(|m| !m.is_reply()).collect());
                c.has_more = more;
                c.follow = false;
                c.status = Some(if added == 0 { "no older messages".into() } else { format!("{added} older loaded") });
                relayout(&mut s);
                if let Some(c) = s.chat.as_mut() {
                    if let Some((ts, off)) = anchor {
                        if let Some(i) = c.msgs.iter().position(|m| m.ts == ts) {
                            if let Some((first, _)) = lines_of(&c.lines, i) {
                                c.top = (first + off).min(c.max_top());
                            }
                        }
                    }
                }
                paint(&s);
            }
            Err(e) => {
                let mut s = st2.borrow_mut();
                let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
                c.fetching = false;
                c.status = Some(format!("older: {e}"));
                paint(&s);
            }
        }
    });
}

/// Switch the open view to another conversation of the same channel (a
/// thread, or back to the timeline) without reopening the float.
fn switch(st: &Shared, thread: Option<String>, from_timeline: bool) {
    let gen = next_gen();
    {
        let mut s = st.borrow_mut();
        let Some(c) = s.chat.as_mut() else { return };
        c.thread = thread;
        c.from_timeline = from_timeline;
        c.gen = gen;
        c.msgs.clear();
        c.lines.clear();
        c.top = 0;
        c.cursor = None;
        c.follow = true;
        c.has_more = false;
        c.compose = None;
        c.status = None;
        c.fetching = false;
        c.loaded = false;
        paint(&s);
    }
    start_session(st, gen);
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// What a key asks for once the borrow is dropped.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum After {
    None,
    Close,
    /// Back from a thread to its channel's timeline.
    Back,
    Sidebar,
    Switch(Option<String>, bool),
    /// The Space menu.
    Menu,
    Post(String, Option<String>),
    React(String, String),
    OpenApp(String),
    Permalink(String),
    /// Copy these messages as text.
    Copy(Vec<usize>),
    /// Start an agent with these messages as its first prompt (the
    /// hand-off form, handoff.rs).
    Agent(Vec<usize>),
    Mark,
    Refetch,
    Older,
}

/// A key arriving by another road than the mode (the Space menu's
/// `menu-key`): handled exactly as a press, with no mouse position.
pub fn replay_key(st: &Shared, key: &str) {
    let after = {
        let mut s = st.borrow_mut();
        let Some(c) = s.chat.as_mut() else { return };
        let after = handle_key(c, key, None);
        paint(&s);
        after
    };
    run(st, after);
}

pub fn on_mode_event(st: &Shared, name: &str, event: &Event) {
    match name {
        "mode-key" => {
            let key = event.get_str("key").unwrap_or("").to_string();
            let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
                (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
                _ => None,
            };
            let after = {
                let mut s = st.borrow_mut();
                let Some(c) = s.chat.as_mut() else { return };
                let after = handle_key(c, &key, mouse);
                paint(&s);
                after
            };
            run(st, after);
        }
        "mode-nav" => {
            let dir = event.get_str("dir").unwrap_or("").to_string();
            let mut s = st.borrow_mut();
            let Some(c) = s.chat.as_mut() else { return };
            match dir.as_str() {
                "up" => c.scroll(-(c.body_h() as isize)),
                "down" => c.scroll(c.body_h() as isize),
                _ => return,
            }
            paint(&s);
        }
        "mode-paste" => {
            let Some(text) = event.get_str("text").map(str::to_string) else { return };
            let mut s = st.borrow_mut();
            let Some(c) = s.chat.as_mut() else { return };
            match c.compose.as_mut() {
                Some(cp) => cp.text.push_str(&text),
                // A paste outside the box starts composing with it.
                None => {
                    c.compose = Some(Compose { kind: ComposeKind::Message(c.thread.clone()), text });
                }
            }
            paint(&s);
        }
        "mode-resize" => {
            let mut s = st.borrow_mut();
            let Some(c) = s.chat.as_mut() else { return };
            if let Some(w) = event.get_i64("width") {
                c.width = (w.max(1)) as u32;
            }
            if let Some(h) = event.get_i64("height") {
                c.height = (h.max(1)) as u32;
            }
            relayout(&mut s);
            if let Some(c) = s.chat.as_mut() {
                c.reveal_cursor();
            }
            paint(&s);
        }
        "mode-closed" => {
            if let Some(c) = st.borrow_mut().chat.take() {
                if let Some(t) = c.poll {
                    cancel(t);
                }
            }
        }
        _ => {}
    }
}

/// A key while composing: typing, editing, send or cancel.
fn compose_key(c: &mut Chat, key: &str) -> After {
    let Some(cp) = c.compose.as_mut() else { return After::None };
    match key {
        "Enter" => {
            let text = std::mem::take(&mut cp.text);
            let kind = cp.kind.clone();
            c.compose = None;
            match kind {
                ComposeKind::Message(thread) => {
                    if text.trim().is_empty() {
                        After::None
                    } else {
                        c.status = Some("posting…".into());
                        After::Post(text, thread)
                    }
                }
                ComposeKind::Reaction(ts) => {
                    let name = text.trim().trim_matches(':').to_string();
                    if name.is_empty() {
                        After::None
                    } else {
                        c.status = Some(format!("reacting :{name}:…"));
                        After::React(ts, name)
                    }
                }
            }
        }
        "Escape" => {
            c.compose = None;
            After::None
        }
        "BSpace" | "C-h" => {
            cp.text.pop();
            After::None
        }
        "C-u" => {
            cp.text.clear();
            After::None
        }
        "C-w" => {
            let trimmed = cp.text.trim_end().len();
            cp.text.truncate(trimmed);
            while cp.text.chars().last().is_some_and(|ch| !ch.is_whitespace()) {
                cp.text.pop();
            }
            After::None
        }
        "Space" => {
            cp.text.push(' ');
            After::None
        }
        "Tab" => {
            cp.text.push('\t');
            After::None
        }
        _ => {
            let mut it = key.chars();
            if let (Some(ch), None) = (it.next(), it.next()) {
                if !ch.is_control() {
                    cp.text.push(ch);
                }
            }
            After::None
        }
    }
}

/// The keymap. Enter opens the thread of the message under the cursor
/// (any message, replies or not), Escape steps back one level (thread
/// -> timeline -> sidebar), q closes outright, Space opens the menu,
/// which comes back here through `replay_key`.
pub(crate) fn handle_key(c: &mut Chat, key: &str, mouse: Option<(u32, u32)>) -> After {
    if c.compose.is_some() {
        return compose_key(c, key);
    }
    // A status answers the last action; the next key dismisses it.
    if key != "?" {
        c.status = None;
    }
    let base = key.rsplit('-').next().unwrap_or(key);
    let body_h = c.body_h() as isize;
    match key {
        "j" | "Down" => {
            let n = c.msgs.len();
            if n > 0 {
                let i = c.cursor_idx().map(|i| (i + 1).min(n - 1)).unwrap_or(0);
                c.set_cursor_idx(i);
                c.reveal_cursor();
                if i == n - 1 && c.top == c.max_top() {
                    c.follow = true;
                }
            }
            After::None
        }
        "k" | "Up" => {
            let n = c.msgs.len();
            if n > 0 {
                let i = c.cursor_idx().map(|i| i.saturating_sub(1)).unwrap_or(0);
                c.set_cursor_idx(i);
                let was = c.top;
                c.reveal_cursor();
                if c.top < was {
                    c.follow = false;
                }
            }
            After::None
        }
        "J" | "NPage" => {
            c.scroll(body_h);
            if let Some(i) = c.msg_at_top() {
                c.set_cursor_idx(i);
            }
            After::None
        }
        "K" | "PPage" => {
            c.scroll(-body_h);
            if let Some(i) = c.msg_at_top() {
                c.set_cursor_idx(i);
            }
            After::None
        }
        "g" => {
            c.top = 0;
            c.follow = false;
            c.set_cursor_idx(0);
            After::None
        }
        "G" => {
            c.follow = true;
            c.top = c.max_top();
            let last = c.msgs.len().saturating_sub(1);
            c.set_cursor_idx(last);
            After::None
        }
        "i" => {
            c.compose = Some(Compose { kind: ComposeKind::Message(c.thread.clone()), text: String::new() });
            After::None
        }
        "r" => {
            let Some(i) = c.cursor_idx() else { return After::None };
            let m = &c.msgs[i];
            let root = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
            c.compose = Some(Compose { kind: ComposeKind::Message(Some(root)), text: String::new() });
            After::None
        }
        // Enter opens the thread of any message, with or without replies
        // (Enter then i is "reply in this thread"); t is its alias.
        "Enter" | "t" => {
            if c.thread.is_some() {
                c.status = Some("already in the thread".into());
                return After::None;
            }
            let Some(i) = c.cursor_idx() else { return After::None };
            let m = &c.msgs[i];
            let root = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
            After::Switch(Some(root), true)
        }
        "e" => {
            let Some(i) = c.cursor_idx() else { return After::None };
            let ts = c.msgs[i].ts.clone();
            c.compose = Some(Compose { kind: ComposeKind::Reaction(ts), text: String::new() });
            After::None
        }
        "o" => {
            let ts = c.cursor_idx().map(|i| c.msgs[i].ts.clone()).unwrap_or_default();
            After::OpenApp(ts)
        }
        // v opens a selection at the cursor; j/k extend it, y copies it
        // (or the cursor message alone), Y the cursor message's link.
        "v" => {
            if c.anchor.take().is_some() {
                c.status = Some("selection cleared".into());
            } else if let Some(i) = c.cursor_idx() {
                c.anchor = Some(c.msgs[i].ts.clone());
                c.status = Some("selecting · j/k extend · y copy · n agent · v cancel".into());
            }
            After::None
        }
        "y" => {
            let sel = c.selection();
            if sel.is_empty() {
                return After::None;
            }
            c.anchor = None;
            c.status = Some("copying…".into());
            After::Copy(sel)
        }
        // n hands the same messages to a new agent as its first prompt.
        "n" => {
            let sel = c.selection();
            if sel.is_empty() {
                return After::None;
            }
            c.anchor = None;
            c.status = Some("new agent…".into());
            After::Agent(sel)
        }
        "Y" => {
            let Some(i) = c.cursor_idx() else { return After::None };
            c.status = Some("fetching the permalink…".into());
            After::Permalink(c.msgs[i].ts.clone())
        }
        "m" => {
            c.status = Some("marked read".into());
            After::Mark
        }
        "R" => {
            c.status = Some("refetching…".into());
            After::Refetch
        }
        "L" => After::Older,
        "b" => After::Sidebar,
        // Escape steps back one level: a thread (however it was opened)
        // to its channel's timeline, the timeline to the sidebar.
        "Escape" => {
            if c.anchor.take().is_some() {
                c.status = Some("selection cleared".into());
                After::None
            } else if c.thread.is_some() {
                After::Back
            } else {
                After::Sidebar
            }
        }
        "q" => After::Close,
        "Space" => After::Menu,
        "?" => {
            c.status = Some(hints().to_string());
            After::None
        }
        _ => match base {
            "WheelUpPane" => {
                c.scroll(-3);
                After::None
            }
            "WheelDownPane" => {
                c.scroll(3);
                After::None
            }
            "MouseDown1Pane" | "DoubleClick1Pane" => {
                if let Some((_, y)) = mouse {
                    // The mouse row is 0-based, like `body_y`.
                    let y0 = c.body_y as usize;
                    if (y as usize) >= y0 && (y as usize) < y0 + c.body_h() {
                        let line = c.top + (y as usize - y0);
                        if let Some(i) = c.lines.get(line).and_then(|l| l.msg) {
                            c.set_cursor_idx(i);
                        }
                    }
                }
                After::None
            }
            _ => After::None,
        },
    }
}

/// The Space menu's items for the view as it stands: every key of the
/// keymap, the ones that do not apply dimmed. Each item hands its key
/// back through `plugin-command slack 'menu-key <key>'`, so the menu is
/// a mirror of the keymap with no logic of its own.
pub(crate) fn menu_items(c: &Chat) -> String {
    let on_msg = c.cursor_idx().is_some();
    let in_thread = c.thread.is_some();
    let mut items = String::new();
    items.push_str(&menu_item(NAME, "open thread", "Enter", on_msg && !in_thread));
    items.push_str(&menu_item(NAME, "compose", "i", true));
    items.push_str(&menu_item(NAME, "reply in thread", "r", on_msg));
    items.push_str(&menu_item(NAME, "react", "e", on_msg));
    items.push_str(&menu_item(NAME, if c.anchor.is_some() { "copy the selection" } else { "copy message" }, "y", on_msg));
    items.push_str(&menu_item(NAME, "copy permalink", "Y", on_msg));
    items.push_str(&menu_item(NAME, if c.anchor.is_some() { "cancel selection" } else { "select from here" }, "v", on_msg));
    items.push_str(&menu_item(NAME, if c.anchor.is_some() { "new agent from the selection" } else { "new agent from message" }, "n", on_msg));
    items.push_str(&menu_item(NAME, "open in Slack app", "o", true));
    items.push_str(" ''");
    items.push_str(&menu_item(NAME, "mark read", "m", true));
    items.push_str(&menu_item(NAME, "load older", "L", !in_thread));
    items.push_str(" ''");
    let back = if in_thread { "back to channel" } else { "back to sidebar" };
    items.push_str(&menu_item(NAME, back, "Escape", true));
    items.push_str(&menu_item(NAME, "help", "?", true));
    items.push_str(&menu_item(NAME, "close", "q", true));
    items
}

/// The action menu (Space): a `display-menu` on the client that pressed
/// the key, titled with the channel, one item per key.
async fn open_menu(st: Shared) {
    let Some((title, items, client)) = ({
        let s = st.borrow();
        s.chat.as_ref().map(|c| (menu_safe(&s.channel_label(&c.channel), 30), menu_items(c), c.client))
    }) else {
        return;
    };
    let target = client
        .and_then(|cid| list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid))
        .map(|c| c.name)
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)))
        .map(|n| format!(" -c '{n}'"))
        .unwrap_or_default();
    // No -x/-y: display-menu then centres on the client's window, where
    // the view floats. The `--` matters: a dimmed item is spelled
    // `-label`, and when it comes first tmux would read it as a flag.
    let cmd = format!("display-menu{target} -T ' {title} ' --{items}");
    if let Err(e) = run_command(&cmd).await {
        let mut s = st.borrow_mut();
        if let Some(c) = s.chat.as_mut() {
            c.status = Some(format!("menu failed: {}", e.message));
        }
        paint(&s);
    }
}

/// The session, channel and generation of the open view, for an API
/// call made on its behalf.
/// Put `text` on the clipboard of the client that pressed the key: a
/// tmux paste buffer and, over OSC 52, the terminal's own clipboard
/// (`load-buffer -w`). The text goes through a file under the plugin's
/// data directory, since a tmux command line cannot carry newlines.
pub(crate) async fn copy_text(text: &str, client: Option<u64>) -> Result<(), String> {
    let root = fs_root().map_err(|e| e.message.clone())?;
    fs_write("copy.txt", text.as_bytes().to_vec(), false).await.map_err(|e| e.message.clone())?;
    let path = crate::http::sq(&format!("{root}/copy.txt"));
    let name = client
        .and_then(|cid| list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid))
        .map(|c| c.name)
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)));
    if let Some(n) = name {
        if run_command(&format!("load-buffer -w -t '{n}' {path}")).await.is_ok() {
            return Ok(());
        }
    }
    run_command(&format!("load-buffer -w {path}")).await.map(|_| ()).map_err(|e| e.message.clone())
}

/// A line for the view's status (another module reporting back, the
/// hand-off form mostly), drawn at once if the view is open.
pub fn note(st: &Shared, text: String) {
    let mut s = st.borrow_mut();
    let Some(c) = s.chat.as_mut() else { return };
    c.status = Some(text);
    paint(&s);
}

fn view_ctx(st: &Shared) -> Option<(crate::store::Identity, String, u64)> {
    let s = st.borrow();
    let id = s.ident.clone()?;
    let c = s.chat.as_ref()?;
    Some((id, c.channel.id.clone(), c.gen))
}

fn run(st: &Shared, after: After) {
    match after {
        After::None => {}
        After::Close => close(st),
        After::Back => switch(st, None, false),
        After::Sidebar => {
            let (client, pane) = st.borrow().chat.as_ref().map(|c| (c.client, c.pane)).unwrap_or((None, None));
            close(st);
            sidebar::open(st, client, pane);
        }
        After::Switch(thread, from_timeline) => switch(st, thread, from_timeline),
        After::Menu => {
            let st2 = Rc::clone(st);
            spawn(async move {
                open_menu(st2).await;
            });
        }
        After::Post(text, thread) => {
            let Some(channel) = st.borrow().chat.as_ref().map(|c| c.channel.id.clone()) else { return };
            let st2 = Rc::clone(st);
            spawn(async move {
                match crate::post_message(&st2, &channel, &text, thread.as_deref()).await {
                    // on_posted already drew the message.
                    Ok(_) => {}
                    Err(e) => {
                        let mut s = st2.borrow_mut();
                        if let Some(c) = s.chat.as_mut().filter(|c| c.channel.id == channel) {
                            c.status = Some(format!("post failed: {e}"));
                            // Give the text back rather than lose it.
                            c.compose = Some(Compose { kind: ComposeKind::Message(thread.clone()), text: text.clone() });
                        }
                        paint(&s);
                    }
                }
            });
        }
        After::React(ts, name) => {
            let Some((id, channel, gen)) = view_ctx(st) else { return };
            let st2 = Rc::clone(st);
            spawn(async move {
                let r = api::react(&id, &channel, &ts, &name).await;
                let mut s = st2.borrow_mut();
                let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) else { return };
                match r {
                    Ok(()) => {
                        if let Some(m) = c.msgs.iter_mut().find(|m| m.ts == ts) {
                            match m.reactions.iter_mut().find(|(n, _)| *n == name) {
                                Some((_, n)) => *n += 1,
                                None => m.reactions.push((name.clone(), 1)),
                            }
                        }
                        c.status = Some(format!(":{name}: added"));
                        relayout_paint(&mut s);
                    }
                    Err(e) => {
                        c.status = Some(format!("react: {e}"));
                        paint(&s);
                    }
                }
            });
        }
        After::OpenApp(ts) => {
            let (team, channel) = {
                let s = st.borrow();
                let team = s.ident.as_ref().map(|i| i.team_id.clone()).unwrap_or_default();
                let Some(c) = s.chat.as_ref() else { return };
                (team, c.channel.id.clone())
            };
            let mut url = format!("slack://channel?team={team}&id={channel}");
            if !ts.is_empty() {
                url.push_str(&format!("&message={ts}"));
            }
            spawn(async move {
                let _ = run_job(&format!("command -v open >/dev/null 2>&1 && open {u} || xdg-open {u}", u = crate::http::sq(&url)), None).await;
            });
        }
        After::Permalink(ts) => {
            let Some((id, channel, gen)) = view_ctx(st) else { return };
            let st2 = Rc::clone(st);
            spawn(async move {
                let r = api::permalink(&id, &channel, &ts).await;
                let status = match &r {
                    Ok(_) => "permalink copied".to_string(),
                    Err(e) => format!("permalink: {e}"),
                };
                if let Ok(url) = &r {
                    let _ = run_command(&format!("set-buffer -- '{}'", url.replace('\'', ""))).await;
                }
                let mut s = st2.borrow_mut();
                if let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) {
                    c.status = Some(status);
                    paint(&s);
                }
            });
        }
        After::Copy(idxs) => {
            let (text, client, gen) = {
                let s = st.borrow();
                let Some(c) = s.chat.as_ref() else { return };
                (transcript(&c.msgs, &idxs, s.tz(), &s.names), c.client, c.gen)
            };
            let n = idxs.len();
            let st2 = Rc::clone(st);
            spawn(async move {
                let status = match copy_text(&text, client).await {
                    Ok(()) => format!("copied {n} message{}", if n == 1 { "" } else { "s" }),
                    Err(e) => format!("copy: {e}"),
                };
                let mut s = st2.borrow_mut();
                if let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) {
                    c.status = Some(status);
                    paint(&s);
                }
            });
        }
        After::Agent(idxs) => {
            // The transcript `y` would have copied, with where it came
            // from; the form takes it from here (handoff.rs). The view
            // stays open underneath: the form is a float of its own.
            let req = {
                let s = st.borrow();
                let Some(c) = s.chat.as_ref() else { return };
                let first_ts = idxs.iter().filter_map(|&i| c.msgs.get(i)).map(|m| m.ts.clone()).min().unwrap_or_default();
                crate::handoff::Request {
                    st: Rc::clone(st),
                    label: s.channel_label(&c.channel),
                    channel: s.channel_label(&c.channel).trim_start_matches(['#', '@']).to_string(),
                    thread: c.thread.is_some(),
                    first_ts,
                    transcript: transcript(&c.msgs, &idxs, s.tz(), &s.names),
                    count: idxs.len(),
                    client: c.client,
                    pane: c.pane,
                    tz: s.tz(),
                }
            };
            spawn(crate::handoff::open(req));
        }
        After::Mark => {
            let channel = {
                let mut s = st.borrow_mut();
                let Some(c) = s.chat.as_mut() else { return };
                c.marked_ms = now_ms();
                c.channel.id.clone()
            };
            let st2 = Rc::clone(st);
            spawn(async move {
                crate::mark_read(&st2, &channel).await;
            });
        }
        After::Refetch => {
            let Some(gen) = st.borrow().chat.as_ref().map(|c| c.gen) else { return };
            let st2 = Rc::clone(st);
            spawn(async move {
                fetch(&st2, gen).await;
                let mut s = st2.borrow_mut();
                if let Some(c) = s.chat.as_mut().filter(|c| c.gen == gen) {
                    if c.status.as_deref() == Some("refetching…") {
                        c.status = None;
                    }
                    paint(&s);
                }
            });
        }
        After::Older => load_older(st),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use listkit::styled::cells_text;

    const DAY: i64 = 86_400;
    /// 2023-10-30 12:00 UTC, a Monday.
    const NOON: i64 = 1_698_667_200;

    fn msg(ts: i64, user: &str, text: &str) -> Message {
        Message { channel: "C1".into(), ts: format!("{ts}.000100"), user: Some(user.into()), text: text.into(), ..Default::default() }
    }

    fn names() -> NameMap {
        let mut n = NameMap::default();
        n.users.insert("U1".into(), "zack".into());
        n.users.insert("U2".into(), "tejas".into());
        n
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| cells_text(&l.cells).trim_end().to_string()).collect()
    }

    #[test]
    fn group_headers_and_day_separators() {
        let msgs = vec![
            msg(NOON - DAY, "U1", "yesterday"),
            msg(NOON, "U1", "one"),
            msg(NOON + 60, "U1", "two"),       // same author, a minute on: no header
            msg(NOON + 60 + 400, "U1", "three"), // six minutes on: a header again
            msg(NOON + 500, "U2", "four"),       // another author: a header
        ];
        let lines = layout(&msgs, 60, 0, "U1", None, &names(), NOON + 3600, false, None);
        let t = texts(&lines);
        assert_eq!(t[0].trim(), "── Yesterday ──");
        assert_eq!(t[1], "zack  12:00");
        assert_eq!(t[2], "  yesterday");
        assert_eq!(t[3].trim(), "── Today ──");
        assert_eq!(t[4], "zack  12:00");
        assert_eq!(t[5], "  one");
        assert_eq!(t[6], "  two", "a post a minute later shares the header");
        assert_eq!(t[7], "zack  12:07");
        assert_eq!(t[8], "  three");
        assert_eq!(t[9], "tejas  12:08");
        assert_eq!(t[10], "  four");
        assert_eq!(lines.len(), 11);
        // Own name green, the other cyan; separators belong to no message.
        assert!(lines[1].cells[0].1 & ST_GREEN != 0);
        assert!(lines[9].cells[0].1 & ST_CYAN != 0);
        assert_eq!(lines[0].msg, None);
        assert_eq!(lines[3].msg, None);
        assert_eq!(lines[6].msg, Some(2));
    }

    #[test]
    fn day_change_forces_a_header() {
        // Same author within five minutes but across midnight: the day
        // separator breaks the group.
        let midnight = (NOON / DAY + 1) * DAY;
        let msgs = vec![msg(midnight - 60, "U1", "a"), msg(midnight + 60, "U1", "b")];
        let lines = layout(&msgs, 60, 0, "U1", None, &names(), midnight + 3600, false, None);
        let t: Vec<String> = lines.iter().map(|l| cells_text(&l.cells).trim().to_string()).collect();
        assert_eq!(t, vec!["── Yesterday ──", "zack  23:59", "a", "── Today ──", "zack  00:01", "b"]);
    }

    #[test]
    fn new_messages_line() {
        let msgs = vec![msg(NOON, "U1", "old"), msg(NOON + 10, "U2", "new"), msg(NOON + 20, "U2", "newer")];
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667205.000000"), &names(), NOON + 100, false, None);
        let t = texts(&lines);
        assert_eq!(t[3].trim(), "── new messages ──");
        assert!(lines[3].cells.iter().any(|c| c.1 & ST_RED != 0));
        assert_eq!(t.iter().filter(|l| l.contains("new messages")).count(), 1, "drawn once");
        // Read before everything: our own first message is not news, so
        // the line still sits between it and the others'.
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667100.000000"), &names(), NOON + 100, false, None);
        assert_eq!(texts(&lines)[3].trim(), "── new messages ──");
        // Nothing older shown at all: no line, nothing to be new against.
        let others = vec![msg(NOON + 10, "U2", "new"), msg(NOON + 20, "U2", "newer")];
        let lines = layout(&others, 60, 0, "U1", Some("1698667100.000000"), &names(), NOON + 100, false, None);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
        // Everything read: no line either.
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667220.000100"), &names(), NOON + 100, false, None);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
        // Only our own post is newer than the mark: nothing is new.
        let mine = vec![msg(NOON, "U2", "theirs"), msg(NOON + 10, "U1", "ours")];
        let lines = layout(&mine, 60, 0, "U1", Some("1698667205.000000"), &names(), NOON + 100, false, None);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
    }

    #[test]
    fn cursor_to_lines() {
        let mut long = msg(NOON, "U1", "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda");
        long.reactions = vec![("+1".into(), 3), ("tada".into(), 1)];
        long.reply_count = 3;
        long.latest_reply = Some(format!("{}.000000", NOON + 1800));
        let mut second = msg(NOON + 1000, "U2", "hi");
        second.files = vec![FileRef::named("notes.txt")];
        second.attachments = vec!["Card title\ncard text".into()];
        second.edited = true;
        let msgs = vec![long, second];
        let lines = layout(&msgs, 24, 0, "U1", None, &names(), NOON + 3600, false, None);
        let t = texts(&lines);
        // Today, header, wrapped body (width 20 -> several lines), reactions, thread marker.
        assert_eq!(lines[0].msg, None);
        let (f0, l0) = lines_of(&lines, 0).unwrap();
        assert_eq!(f0, 1);
        assert_eq!(t[l0], "  ↳ 3 replies · 30m");
        assert_eq!(t[l0 - 1], "  \u{1F44D} 3 · \u{1F389} 1");
        assert!(lines[f0 + 1..l0 - 1].iter().all(|l| l.cells.len() <= 23), "{t:?}");
        let (f1, l1) = lines_of(&lines, 1).unwrap();
        assert_eq!(f1, l0 + 1);
        assert_eq!(t[f1], "tejas  12:16 (edited)");
        assert_eq!(t[f1 + 1], "  hi");
        assert_eq!(t[f1 + 2], "  │ Card title");
        assert_eq!(t[f1 + 3], "  │ card text");
        assert_eq!(t[l1], "  📎 notes.txt");
        assert_eq!(l1, lines.len() - 1);
        // In a thread the root shows no marker.
        let thread = layout(&msgs, 24, 0, "U1", None, &names(), NOON + 3600, true, None);
        assert!(!texts(&thread).iter().any(|l| l.contains("replies")));
    }

    #[test]
    fn merge_keeps_order_and_replaces() {
        let mut msgs = vec![msg(NOON, "U1", "a"), msg(NOON + 20, "U1", "c")];
        let mut edited = msg(NOON, "U1", "a2");
        edited.edited = true;
        let added = merge(&mut msgs, vec![msg(NOON + 10, "U2", "b"), edited]);
        assert_eq!(added, 1);
        let t: Vec<&str> = msgs.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(t, vec!["a2", "b", "c"]);
        assert!(msgs[0].edited);
    }

    /// A timeline view of two messages with the cursor on the last.
    fn chat() -> Chat {
        let channel = Channel {
            id: "C1".into(),
            name: "general".into(),
            kind: crate::store::Kind::Public,
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
        };
        let msgs = vec![msg(NOON, "U2", "root"), msg(NOON + 10, "U1", "second")];
        let mut c = Chat {
            mode: ModeId(1),
            channel,
            thread: None,
            from_timeline: false,
            client: None,
            pane: None,
            width: 80,
            height: 20,
            chrome: 4,
            body_y: 2,
            msgs,
            lines: Vec::new(),
            top: 0,
            cursor: None,
            anchor: None,
            follow: true,
            has_more: false,
            last_read: None,
            compose: None,
            status: None,
            gen: 1,
            poll: None,
            fetching: false,
            marked_ms: 0,
            loaded: true,
            images_on: true,
            cell_aspect: 2.0,
            data_root: "/d".into(),
            placed: RefCell::new(HashMap::new()),
        };
        c.set_cursor_idx(1);
        c
    }

    #[test]
    fn picture_rows_follow_the_clip_line() {
        let mut m = msg(NOON, "U2", "look");
        m.files.push(FileRef {
            id: "F1".into(),
            name: "wide.png".into(),
            mimetype: "image/png".into(),
            size: Some((800, 100)),
            thumb: Some("https://files.slack.com/t/F1/thumb_480.png".into()),
            url: None,
        });
        m.files.push(FileRef::named("notes.txt"));
        let msgs = vec![m, msg(NOON + 10, "U1", "ok")];
        // Nothing known about the picture: the clip line alone.
        let plain = layout(&msgs, 80, 0, "U1", None, &names(), NOON + 100, false, None);
        assert_eq!(texts(&plain)[3], "  📎 wide.png");
        assert!(plain.iter().all(|l| l.image.is_none()));
        // On disk at 800x100: a block of 5 rows (76 cells wide at aspect
        // 2, which the 76-cell body allows) under the line, each row
        // tagged with the file, before the next file's line.
        let mut sizes = HashMap::new();
        sizes.insert("F1".to_string(), (800u32, 100u32));
        let pics = Pictures { sizes: &sizes, cell_aspect: 2.0 };
        let lines = layout(&msgs, 80, 0, "U1", None, &names(), NOON + 100, false, Some(&pics));
        let t = texts(&lines);
        assert_eq!(t[3], "  📎 wide.png");
        let rows: Vec<&ImageRow> = lines.iter().filter_map(|l| l.image.as_ref()).collect();
        assert_eq!(rows.len(), 5, "{t:?}");
        assert_eq!(rows[0], &ImageRow { file: "F1".into(), row: 0, cols: 76, rows: 5 });
        assert_eq!(rows[4].row, 4);
        assert_eq!(lines[4].image.as_ref().map(|r| r.row), Some(0));
        assert_eq!(lines[4].msg, Some(0), "a picture row belongs to its message");
        assert_eq!(t[9], "  📎 notes.txt");
        assert_eq!(lines.len(), plain.len() + 5);
        // A narrow view clamps the block to its body.
        let narrow = layout(&msgs, 40, 0, "U1", None, &names(), NOON + 100, false, Some(&pics));
        let r = narrow.iter().find_map(|l| l.image.as_ref()).unwrap();
        assert_eq!((r.cols, r.rows), (36, 2));
        // A tall one takes every row and few columns.
        sizes.insert("F1".to_string(), (100, 800));
        let pics = Pictures { sizes: &sizes, cell_aspect: 2.0 };
        let tall = layout(&msgs, 80, 0, "U1", None, &names(), NOON + 100, false, Some(&pics));
        let r = tall.iter().find_map(|l| l.image.as_ref()).unwrap();
        assert_eq!((r.cols, r.rows), (3, 12));
        // The transcript never carries picture rows.
        assert_eq!(transcript(&msgs, &[0], 0, &names()), "tejas (2023-10-30 12:00): look\n  📎 wide.png\n  📎 notes.txt\n");
    }

    #[test]
    fn pictures_are_placed_once_per_block_size() {
        let mut c = chat();
        let mut m = msg(NOON + 5, "U2", "");
        m.files.push(FileRef { id: "F1".into(), name: "a.png".into(), mimetype: "image/png".into(), size: None, thumb: Some("u".into()), url: None });
        c.msgs.insert(1, m);
        let mut sizes = HashMap::new();
        sizes.insert("F1".to_string(), (800u32, 100u32));
        let pics = Pictures { sizes: &sizes, cell_aspect: 2.0 };
        c.lines = layout(&c.msgs, c.width as usize, 0, "U1", None, &names(), NOON + 100, false, Some(&pics));
        let mut out = String::new();
        paint_body(&c, &mut out, 0, 2);
        // One transmit by path, then placeholder rows carrying id 0x530000.
        assert_eq!(out.matches("a=T,t=f,f=100,i=").count(), 1, "{out:?}");
        assert!(out.contains("\x1bPtmux;\x1b\x1b_Ga=T,t=f,f=100,i="), "{out:?}");
        let ph = "\u{10EEEE}";
        assert!(out.matches(ph).count() >= 5 * 70, "{}", out.matches(ph).count());
        let id = c.placed.borrow().get("F1").map(|p| p.id).unwrap();
        assert!(id >= IMAGE_ID_BASE);
        assert!(out.contains(&format!("\x1b[38;2;{};{};{}m", (id >> 16) & 255, (id >> 8) & 255, id & 255)));
        // The next paint sends text only.
        let mut again = String::new();
        paint_body(&c, &mut again, 0, 2);
        assert!(!again.contains("_Ga=T"), "{again:?}");
        assert!(again.contains(ph));
        // A narrower view: the old placement is deleted, a new one sent.
        c.width = 40;
        c.lines = layout(&c.msgs, 40, 0, "U1", None, &names(), NOON + 100, false, Some(&pics));
        let mut resized = String::new();
        paint_body(&c, &mut resized, 0, 2);
        assert!(resized.contains(&format!("a=d,d=I,i={id}")), "{resized:?}");
        assert_eq!(resized.matches("a=T,t=f").count(), 1);
        assert_ne!(c.placed.borrow().get("F1").map(|p| p.id), Some(id));
        // Nothing wanted from the cache once the picture is known.
        let mut cache = images::Cache::default();
        assert_eq!(c.wanted_images(&cache).len(), 1);
        cache.asked.insert("F1".into());
        assert!(c.wanted_images(&cache).is_empty());
        c.images_on = false;
        cache.asked.clear();
        assert!(c.wanted_images(&cache).is_empty(), "off: nothing is fetched");
    }

    fn in_thread(from_timeline: bool) -> Chat {
        let mut c = chat();
        c.thread = Some(c.msgs[0].ts.clone());
        c.from_timeline = from_timeline;
        c
    }

    #[test]
    fn y_copies_the_cursor_or_the_selection() {
        let mut c = chat();
        assert_eq!(handle_key(&mut c, "y", None), After::Copy(vec![1]));
        assert_eq!(handle_key(&mut c, "v", None), After::None);
        assert!(c.anchor.is_some() && c.in_selection(1) && !c.in_selection(0));
        handle_key(&mut c, "k", None);
        assert!(c.in_selection(0) && c.in_selection(1));
        assert_eq!(handle_key(&mut c, "y", None), After::Copy(vec![0, 1]));
        assert!(c.anchor.is_none(), "y closes the selection");
        let ts = c.msgs[0].ts.clone();
        assert_eq!(handle_key(&mut c, "Y", None), After::Permalink(ts));
        // n hands the cursor message, or the selection, to a new agent
        // and closes the selection like y does.
        assert_eq!(handle_key(&mut c, "n", None), After::Agent(vec![0]));
        handle_key(&mut c, "v", None);
        assert!(c.status.as_deref().is_some_and(|s| s.contains("n agent")), "{:?}", c.status);
        handle_key(&mut c, "j", None);
        assert_eq!(handle_key(&mut c, "n", None), After::Agent(vec![0, 1]));
        assert!(c.anchor.is_none(), "n closes the selection");
        assert_eq!(c.status.as_deref(), Some("new agent…"));
        // Escape clears a selection before it steps back.
        handle_key(&mut c, "v", None);
        assert_eq!(handle_key(&mut c, "Escape", None), After::None);
        assert!(c.anchor.is_none());
        assert_eq!(handle_key(&mut c, "Escape", None), After::Sidebar);
    }

    #[test]
    fn transcript_lines() {
        let mut m = msg(NOON, "U2", "hey <@U1> *look*\n```\nlet x = 1;\n```");
        m.files.push(FileRef::named("shot.png"));
        let msgs = vec![m, msg(NOON + 60, "U1", "ok")];
        let t = transcript(&msgs, &[0, 1], 0, &names());
        assert_eq!(t, "tejas (2023-10-30 12:00): hey @zack look\n  let x = 1;\n  📎 shot.png\nzack (2023-10-30 12:01): ok\n");
    }

    #[test]
    fn enter_opens_the_thread_and_i_composes() {
        let mut c = chat();
        let second = c.msgs[1].ts.clone();
        // Enter on a message without replies still opens its thread.
        assert_eq!(handle_key(&mut c, "Enter", None), After::Switch(Some(second.clone()), true));
        assert!(c.compose.is_none(), "Enter no longer composes");
        // t is the alias.
        assert_eq!(handle_key(&mut c, "t", None), After::Switch(Some(second), true));
        // A reply opens its root's thread.
        c.msgs[1].thread_ts = Some(c.msgs[0].ts.clone());
        assert_eq!(handle_key(&mut c, "Enter", None), After::Switch(Some(c.msgs[0].ts.clone()), true));
        // Only i composes, to the open conversation.
        assert_eq!(handle_key(&mut c, "i", None), After::None);
        assert_eq!(c.compose.as_ref().map(|cp| cp.kind.clone()), Some(ComposeKind::Message(None)));
        // Inside the thread Enter has nothing to open; i replies there.
        let mut t = in_thread(true);
        assert_eq!(handle_key(&mut t, "Enter", None), After::None);
        assert_eq!(t.status.as_deref(), Some("already in the thread"));
        handle_key(&mut t, "i", None);
        assert_eq!(t.compose.as_ref().map(|cp| cp.kind.clone()), Some(ComposeKind::Message(t.thread.clone())));
    }

    #[test]
    fn escape_steps_back_and_q_closes() {
        // Timeline: Escape (and b) to the sidebar, q closes.
        let mut c = chat();
        assert_eq!(handle_key(&mut c, "Escape", None), After::Sidebar);
        assert_eq!(handle_key(&mut c, "b", None), After::Sidebar);
        assert_eq!(handle_key(&mut c, "q", None), After::Close);
        // A thread: Escape back to the timeline however it was opened.
        assert_eq!(handle_key(&mut in_thread(true), "Escape", None), After::Back);
        assert_eq!(handle_key(&mut in_thread(false), "Escape", None), After::Back);
        assert_eq!(handle_key(&mut in_thread(false), "q", None), After::Close);
    }

    #[test]
    fn space_is_the_menu_outside_the_compose_box() {
        let mut c = chat();
        assert_eq!(handle_key(&mut c, "Space", None), After::Menu);
        // Composing: Space types, Enter sends, Escape drops the text.
        handle_key(&mut c, "i", None);
        handle_key(&mut c, "h", None);
        assert_eq!(handle_key(&mut c, "Space", None), After::None);
        handle_key(&mut c, "i", None);
        assert_eq!(c.compose.as_ref().map(|cp| cp.text.as_str()), Some("h i"));
        assert_eq!(handle_key(&mut c, "Enter", None), After::Post("h i".into(), None));
        assert!(c.compose.is_none());
        handle_key(&mut c, "i", None);
        handle_key(&mut c, "x", None);
        assert_eq!(handle_key(&mut c, "Escape", None), After::None);
        assert!(c.compose.is_none(), "Escape drops the text");
    }

    #[test]
    fn menu_mirrors_the_keymap() {
        let items = menu_items(&chat());
        assert!(items.contains(" 'open thread' 'Enter' \"plugin-command slack 'menu-key Enter'\""), "{items}");
        assert!(items.contains(" 'compose' 'i' \"plugin-command slack 'menu-key i'\""), "{items}");
        assert!(items.contains(" 'back to sidebar' 'Escape' "), "{items}");
        assert!(items.contains(" 'load older' 'L' "), "{items}");
        assert!(items.contains(" 'close' 'q' "), "{items}");
        assert!(items.contains(" 'new agent from message' 'n' \"plugin-command slack 'menu-key n'\""), "{items}");
        let mut sel = chat();
        sel.anchor = Some(sel.msgs[0].ts.clone());
        assert!(menu_items(&sel).contains(" 'new agent from the selection' 'n' "), "{}", menu_items(&sel));
        assert!(!items.contains("'-"), "everything applies on a message in a timeline: {items}");
        // In a thread: no thread to open, nothing older to load, back
        // goes to the channel.
        let items = menu_items(&in_thread(false));
        assert!(items.contains(" '-open thread' 'Enter' "), "{items}");
        assert!(items.contains(" '-load older' 'L' "), "{items}");
        assert!(items.contains(" 'back to channel' 'Escape' "), "{items}");
        assert!(items.contains(" 'reply in thread' 'r' "), "{items}");
        // No cursor (an empty view): the per-message actions are dimmed.
        let mut c = chat();
        c.msgs.clear();
        c.cursor = None;
        let items = menu_items(&c);
        for label in ["open thread", "reply in thread", "react", "copy permalink", "new agent from message"] {
            assert!(items.contains(&format!(" '-{label}' ")), "{label}: {items}");
        }
        assert!(items.contains(" 'compose' 'i' "), "{items}");
    }

    #[test]
    fn compose_box_shows_tail_with_cursor() {
        let cp = Compose { kind: ComposeKind::Message(None), text: "hello\nworld".into() };
        let cells = compose_cells(&cp, 40);
        assert_eq!(cells_text(&cells), "› hello↵world ");
        assert!(cells.last().unwrap().1 & ST_INVERT != 0);
        let cp = Compose { kind: ComposeKind::Reaction("1.2".into()), text: "abcdefghijklmnop".into() };
        let cells = compose_cells(&cp, 12);
        assert_eq!(cells.len(), 12);
        assert!(cells_text(&cells).ends_with("nop "));
    }
}
