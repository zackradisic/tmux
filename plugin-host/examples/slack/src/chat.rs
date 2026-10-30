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
//! Borrow discipline, as in sidebar.rs: decide under the `State`
//! borrow, drop it, then do the async work or call into lib.rs.

use std::rc::Rc;

use listkit::lines::{clamp_dim, default_size, SizeBox};
use listkit::styled::{emit_cells, fit_cells, plain_cells, wrap_cells, Styled, ST_BOLD, ST_BRIGHT, ST_CYAN, ST_DIM, ST_GREEN, ST_INVERT, ST_RED};
use listkit::text::clip;
use tmux_plugin_sdk::prelude::*;

use crate::store::{Channel, Message};
use crate::{api, emoji, mrkdwn, sidebar, store, timefmt, NameMap, Shared, State, NAME};

/// Two posts by the same author this close together share one header.
const GROUP_SECS: i64 = 5 * 60;
/// `mark_read` goes to Slack at most this often while the view follows.
const MARK_DEBOUNCE_MS: u64 = 2000;
/// A page of history, for the first fetch of a thin cache and for `L`.
const PAGE: u32 = 100;
/// The cache has to hold at least this many timeline messages before a
/// fetch asks only for what came after it.
const THIN_CACHE: usize = 20;

/// One display line: its cells (without the gutter) and the message it
/// belongs to, if any (separators belong to none).
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub cells: Vec<Styled>,
    pub msg: Option<usize>,
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
    /// The thread was opened with `t` from the timeline: q goes back.
    pub from_timeline: bool,
    pub client: Option<u64>,
    pub pane: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub msgs: Vec<Message>,
    pub lines: Vec<Line>,
    /// The first display line on screen.
    pub top: usize,
    /// The cursor message, by ts: stable across a relayout or a prepend.
    pub cursor: Option<String>,
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
}

impl Chat {
    fn body_h(&self) -> usize {
        (self.height as usize).saturating_sub(4).max(1)
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.body_h())
    }

    fn cursor_idx(&self) -> Option<usize> {
        let ts = self.cursor.as_deref()?;
        self.msgs.iter().position(|m| m.ts == ts)
    }

    fn set_cursor_idx(&mut self, i: usize) {
        self.cursor = self.msgs.get(i).map(|m| m.ts.clone());
    }

    /// Scroll so the cursor message is on screen; a message taller than
    /// the body shows from its first line.
    fn reveal_cursor(&mut self) {
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
    fn scroll(&mut self, delta: isize) {
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
/// the messages are a thread (no thread markers then).
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
        out.push(Line { cells, msg: None });
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
            out.push(Line { cells, msg: Some(i) });
        }
        let indent = || plain_cells("  ", 0);
        if m.deleted {
            let mut cells = indent();
            cells.extend(plain_cells("(deleted)", ST_DIM));
            out.push(Line { cells, msg: Some(i) });
        } else if !m.text.trim().is_empty() {
            let body = names.with(|n| mrkdwn::render(&m.text, body_w, 0, n));
            for mut l in body {
                let mut cells = indent();
                cells.append(&mut l);
                out.push(Line { cells, msg: Some(i) });
            }
        }
        for a in &m.attachments {
            for raw in a.lines() {
                let text = names.with(|n| mrkdwn::inline(raw, ST_DIM, n));
                for mut w in wrap_cells(&text, body_w.saturating_sub(2).max(2)) {
                    let mut cells = indent();
                    cells.extend(plain_cells("│ ", ST_DIM));
                    cells.append(&mut w);
                    out.push(Line { cells, msg: Some(i) });
                }
            }
        }
        for f in &m.files {
            let mut cells = indent();
            cells.extend(plain_cells(&format!("📎 {f}"), ST_DIM));
            out.push(Line { cells, msg: Some(i) });
        }
        if !m.reactions.is_empty() {
            let text = m.reactions.iter().map(|(n, c)| format!(":{n}: {c}")).collect::<Vec<_>>().join(" · ");
            let mut cells = indent();
            emoji::push_text(&mut cells, &text, ST_DIM);
            out.push(Line { cells, msg: Some(i) });
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
            out.push(Line { cells, msg: Some(i) });
        }
        prev = Some(m);
    }
    out
}

/// The first and last display line of message `i`.
pub fn lines_of(lines: &[Line], i: usize) -> Option<(usize, usize)> {
    let first = lines.iter().position(|l| l.msg == Some(i))?;
    let last = lines.iter().rposition(|l| l.msg == Some(i))?;
    Some((first, last))
}

/// Lay the open conversation out again from the current state.
fn relayout(s: &mut State) {
    let names = Rc::clone(&s.names);
    let tz = s.tz();
    let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
    let now = (now_ms() / 1000) as i64;
    let Some(c) = s.chat.as_mut() else { return };
    c.lines = layout(&c.msgs, c.width as usize, tz, &me, c.last_read.as_deref(), &names, now, c.thread.is_some());
    if c.follow {
        c.top = c.max_top();
    } else {
        c.top = c.top.min(c.max_top());
    }
}

// ---------------------------------------------------------------------------
// paint
// ---------------------------------------------------------------------------

fn hints() -> &'static str {
    "j/k move · i compose · r reply · t thread · e react · o app · y link · m read · L older · b sidebar · ? help · q close"
}

fn rule(width: usize) -> String {
    "─".repeat(width)
}

/// The compose box: `› text` with a reverse-video cursor cell; the tail
/// of a long text, so the cursor stays on screen.
fn compose_cells(c: &Compose, width: usize) -> Vec<Styled> {
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
    // The body.
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
        out.push_str(&format!("\x1b[3;3H\x1b[2m{text}\x1b[0m"));
    }
    for (k, line) in c.lines.iter().skip(c.top).take(body_h).enumerate() {
        let row = 3 + k;
        let on_cursor = cursor.is_some() && line.msg == cursor;
        let mut cells: Vec<Styled> = vec![if on_cursor { ('▎', ST_CYAN | ST_BRIGHT) } else { (' ', 0) }];
        cells.extend(fit_cells(&line.cells, w.saturating_sub(1)));
        out.push_str(&format!("\x1b[{row};1H{}", emit_cells(&cells, false)));
    }
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
            msgs: Vec::new(),
            lines: Vec::new(),
            top: 0,
            cursor: None,
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
        let _ = mode_close(c.mode);
    }
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
fn merge(msgs: &mut Vec<Message>, new: Vec<Message>) -> usize {
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
            if c.follow {
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
enum After {
    None,
    Close,
    Back,
    Sidebar,
    Switch(Option<String>, bool),
    Post(String, Option<String>),
    React(String, String),
    OpenApp(String),
    Permalink(String),
    Mark,
    Refetch,
    Older,
}

/// A key arriving by another road than the mode (the Space menu's
/// `menu-key`): handled exactly as a press, with no mouse position.
pub fn replay_key(st: &Shared, key: &str) {
    let after = {
        let mut s = st.borrow_mut();
        if s.chat.is_none() {
            return;
        }
        let after = handle_key(&mut s, key, None);
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
                if s.chat.is_none() {
                    return;
                }
                let after = handle_key(&mut s, &key, mouse);
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

fn handle_key(s: &mut State, key: &str, mouse: Option<(u32, u32)>) -> After {
    let Some(c) = s.chat.as_mut() else { return After::None };
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
        "i" | "Enter" => {
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
        "t" => {
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
        "y" => {
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
        "q" | "Escape" => {
            if c.thread.is_some() && c.from_timeline {
                After::Back
            } else {
                After::Close
            }
        }
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
                    // Rows are 1-based on screen, the mouse 0-based: the
                    // body starts at y == 2.
                    if y >= 2 && (y as usize) < 2 + c.body_h() {
                        let line = c.top + (y as usize - 2);
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

/// The session, channel and generation of the open view, for an API
/// call made on its behalf.
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
        let lines = layout(&msgs, 60, 0, "U1", None, &names(), NOON + 3600, false);
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
        let lines = layout(&msgs, 60, 0, "U1", None, &names(), midnight + 3600, false);
        let t: Vec<String> = lines.iter().map(|l| cells_text(&l.cells).trim().to_string()).collect();
        assert_eq!(t, vec!["── Yesterday ──", "zack  23:59", "a", "── Today ──", "zack  00:01", "b"]);
    }

    #[test]
    fn new_messages_line() {
        let msgs = vec![msg(NOON, "U1", "old"), msg(NOON + 10, "U2", "new"), msg(NOON + 20, "U2", "newer")];
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667205.000000"), &names(), NOON + 100, false);
        let t = texts(&lines);
        assert_eq!(t[3].trim(), "── new messages ──");
        assert!(lines[3].cells.iter().any(|c| c.1 & ST_RED != 0));
        assert_eq!(t.iter().filter(|l| l.contains("new messages")).count(), 1, "drawn once");
        // Read before everything: our own first message is not news, so
        // the line still sits between it and the others'.
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667100.000000"), &names(), NOON + 100, false);
        assert_eq!(texts(&lines)[3].trim(), "── new messages ──");
        // Nothing older shown at all: no line, nothing to be new against.
        let others = vec![msg(NOON + 10, "U2", "new"), msg(NOON + 20, "U2", "newer")];
        let lines = layout(&others, 60, 0, "U1", Some("1698667100.000000"), &names(), NOON + 100, false);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
        // Everything read: no line either.
        let lines = layout(&msgs, 60, 0, "U1", Some("1698667220.000100"), &names(), NOON + 100, false);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
        // Only our own post is newer than the mark: nothing is new.
        let mine = vec![msg(NOON, "U2", "theirs"), msg(NOON + 10, "U1", "ours")];
        let lines = layout(&mine, 60, 0, "U1", Some("1698667205.000000"), &names(), NOON + 100, false);
        assert!(!texts(&lines).iter().any(|l| l.contains("new messages")));
    }

    #[test]
    fn cursor_to_lines() {
        let mut long = msg(NOON, "U1", "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda");
        long.reactions = vec![("+1".into(), 3), ("tada".into(), 1)];
        long.reply_count = 3;
        long.latest_reply = Some(format!("{}.000000", NOON + 1800));
        let mut second = msg(NOON + 1000, "U2", "hi");
        second.files = vec!["notes.txt".into()];
        second.attachments = vec!["Card title\ncard text".into()];
        second.edited = true;
        let msgs = vec![long, second];
        let lines = layout(&msgs, 24, 0, "U1", None, &names(), NOON + 3600, false);
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
        let thread = layout(&msgs, 24, 0, "U1", None, &names(), NOON + 3600, true);
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
