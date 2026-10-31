//! vimkit: a modal text editor widget for plugin modes. An [`Editor`]
//! holds a small document, takes tmux key names one at a time, and
//! renders itself into styled cells at any width and height, so a plugin
//! that wants "a vim-flavoured box" (the slack compose box, a prompt
//! field) embeds one instead of hand-rolling a line editor.
//!
//! The API, in short:
//!
//! - [`Editor::new`], [`Editor::with_text`], [`Editor::text`],
//!   [`Editor::set_text`], [`Editor::clear`], [`Editor::is_empty`],
//!   [`Editor::mode`], [`Editor::insert`] (start in insert mode).
//! - [`Editor::key`] takes a key as a plugin's `mode-key` event names it
//!   (`"a"`, `"Space"`, `"Enter"`, `"Escape"`, `"BSpace"`, `"C-w"`,
//!   `"Up"`, `"C-v"`...) and answers with an [`Event`]: what the owner
//!   must do beyond redrawing - submit the text (`Enter`), close the box
//!   (`Escape` in normal mode), put a yank on the clipboard.
//! - [`Editor::paste`] inserts text as is, newlines kept, in any mode.
//! - [`Editor::render`] gives `height` rows of [`Styled`] cells, the long
//!   lines soft-wrapped at `width`, the cursor an inverted cell, the
//!   visual selection inverted and dim, scrolled so the cursor stays on
//!   screen. [`Editor::rows_needed`] says how many rows the whole text
//!   takes at a width, for a box that grows with its text.
//! - [`Editor::status`] is the mode tag vim would show at the bottom:
//!   `-- INSERT --`, `-- VISUAL --`, `-- VISUAL LINE --`,
//!   `-- VISUAL BLOCK --`, `/pattern` while a search is typed, the
//!   pending count and operator (`3d`) in normal mode, nothing otherwise.
//!   The owner decides where it goes.
//!
//! What it does: normal, insert, visual, visual-line and visual-block
//! modes; `h j k l` (and the arrows), `w b e` / `W B E`, `0 ^ $`, `{ }`,
//! `gg G`, `f F t T`, counts; `x X D C s S dd dw cw ... yy p P o O a A i I
//! r J ~ u C-r`; visual `y d x c o`; visual-block `y d c I A` (the typed
//! text repeated on every line of the block); `/` and `?` search (plain
//! substring; case-insensitive while the pattern has no capital) with
//! `n` and `N`. In insert mode: typing, `BSpace`/`C-h`, `DC`, `C-w`,
//! `C-u`, the arrows, `Home`/`End`, `C-j` for a newline (tmux cannot tell
//! Shift-Enter from Enter, so Enter is left to the owner as
//! [`Event::Submit`]), `Escape` back to normal mode.
//!
//! What it deliberately is not: there is one unnamed register and no
//! named ones, no macros, no `.` repeat, no ex commands, no text
//! objects (`iw`, `i(`), no marks, no indent operators, no regular
//! expressions in the search, no auto-indent. Undo is whole-document
//! snapshots, one per normal-mode change and one per insert session.

use std::cell::Cell;

use listkit::styled::{glyph_cells, ST_DIM, ST_INVERT};
pub use listkit::styled::Styled;

#[cfg(test)]
mod tests;

/// The editor's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
    VisualBlock,
    /// A `/` or `?` pattern is being typed.
    Search,
}

/// What a key asks of the owner, beyond redrawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The key meant nothing here.
    None,
    /// The text or the cursor changed.
    Redraw,
    /// Enter: the owner takes [`Editor::text`] (and usually clears).
    Submit,
    /// Escape in normal mode: the owner closes the box.
    Leave,
    /// Text was yanked (`y`, `yy`, visual `y`): the owner may put it on
    /// the clipboard.
    Yank(String),
}

type Pos = (usize, usize);

#[derive(Clone, Debug, PartialEq)]
enum Reg {
    Chars(Vec<char>),
    Lines(Vec<Vec<char>>),
    Block(Vec<Vec<char>>),
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<Vec<char>>,
    cursor: Pos,
}

/// A visual-block `I`/`A`/`c` in progress: what is typed on the first
/// row is repeated on the others when insert mode ends.
struct BlockInsert {
    rows: (usize, usize),
    col: usize,
    /// Pad short lines out to `col` (`A`) rather than skip them (`I`).
    pad: bool,
    /// Lines shorter than this are left alone: `I` skips a line that
    /// does not reach the block, `c` only one that never held it.
    min_len: usize,
    typed: Vec<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordFwd(bool),
    WordBack(bool),
    WordEnd(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    ParaFwd,
    ParaBack,
    Top,
    Bottom,
    /// `f`/`F`/`t`/`T` and the character.
    Find(char, char),
}

#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Incomplete,
    Invalid,
    Motion(usize, Motion),
    /// `d`/`c`/`y` with a motion; the count is the product of both.
    Op(usize, char, Motion),
    /// `dd` `cc` `yy`.
    OpLine(usize, char),
    Replace(usize, char),
    Simple(usize, char),
}

pub struct Editor {
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    mode: Mode,
    /// The column `j`/`k` aim for while passing shorter lines.
    want_col: Option<usize>,
    /// Count and operator keys waiting for the rest of the command.
    pending: Vec<char>,
    /// The other end of the visual selection.
    anchor: Pos,
    reg: Option<Reg>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The pattern being typed, and its direction.
    search: Vec<char>,
    search_fwd: bool,
    last_search: Option<(Vec<char>, bool)>,
    block_insert: Option<BlockInsert>,
    /// The first display row on screen, kept across renders so the
    /// viewport only moves when the cursor leaves it.
    top: Cell<usize>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// character classes and helpers
// ---------------------------------------------------------------------------

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// 0 blank, 1 word, 2 punctuation (everything is a word for `W`).
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || is_word(c) {
        1
    } else {
        2
    }
}

fn is_blank_line(l: &[char]) -> bool {
    l.iter().all(|c| c.is_whitespace())
}

fn first_non_blank(l: &[char]) -> usize {
    l.iter().position(|c| !c.is_whitespace()).unwrap_or(0)
}

fn to_lines(text: &str) -> Vec<Vec<char>> {
    let mut lines: Vec<Vec<char>> = text.split('\n').map(|l| l.chars().collect()).collect();
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

/// The character a key types, if it is one.
fn key_char(key: &str) -> Option<char> {
    match key {
        "Space" => Some(' '),
        "Tab" => Some('\t'),
        _ => {
            let mut it = key.chars();
            match (it.next(), it.next()) {
                (Some(c), None) if !c.is_control() => Some(c),
                _ => None,
            }
        }
    }
}

/// A named key as the normal-mode character that does the same.
fn key_alias(key: &str) -> Option<char> {
    Some(match key {
        "Left" | "BSpace" | "C-h" => 'h',
        "Right" | "Space" => 'l',
        "Up" => 'k',
        "Down" => 'j',
        "Home" => '0',
        "End" => '$',
        "DC" => 'x',
        _ => return None,
    })
}

fn motion_of(c: char) -> Option<Motion> {
    Some(match c {
        'h' => Motion::Left,
        'l' => Motion::Right,
        'k' => Motion::Up,
        'j' => Motion::Down,
        'w' => Motion::WordFwd(false),
        'W' => Motion::WordFwd(true),
        'b' => Motion::WordBack(false),
        'B' => Motion::WordBack(true),
        'e' => Motion::WordEnd(false),
        'E' => Motion::WordEnd(true),
        '0' => Motion::LineStart,
        '^' => Motion::FirstNonBlank,
        '$' => Motion::LineEnd,
        '{' => Motion::ParaBack,
        '}' => Motion::ParaFwd,
        'G' => Motion::Bottom,
        _ => return None,
    })
}

/// Leading digits as a count (a `0` only continues one), and what
/// follows them. `None` when the count is still being typed.
fn take_count(cmd: &[char]) -> (Option<usize>, &[char]) {
    let mut i = 0;
    while i < cmd.len() && cmd[i].is_ascii_digit() && !(i == 0 && cmd[i] == '0') {
        i += 1;
    }
    let n = if i == 0 { None } else { Some(cmd[..i].iter().fold(0usize, |a, d| a.saturating_mul(10).saturating_add(d.to_digit(10).unwrap_or(0) as usize))) };
    (n, &cmd[i..])
}

/// A motion with its own count: `3w`, `gg`, `f(`, or `Incomplete`.
fn parse_motion(rest: &[char]) -> Result<(usize, Motion), Parsed> {
    let (n, rest) = take_count(rest);
    let n = n.unwrap_or(1);
    let Some(&c) = rest.first() else { return Err(Parsed::Incomplete) };
    match c {
        'g' => match rest.get(1) {
            None => Err(Parsed::Incomplete),
            Some('g') => Ok((n, Motion::Top)),
            Some(_) => Err(Parsed::Invalid),
        },
        'f' | 'F' | 't' | 'T' => match rest.get(1) {
            None => Err(Parsed::Incomplete),
            Some(&ch) => Ok((n, Motion::Find(c, ch))),
        },
        _ => motion_of(c).map(|m| (n, m)).ok_or(Parsed::Invalid),
    }
}

/// The normal-mode command `cmd` so far.
fn parse(cmd: &[char]) -> Parsed {
    let (count, rest) = take_count(cmd);
    let has_count = count.is_some();
    let count = count.unwrap_or(1);
    let Some(&c) = rest.first() else { return Parsed::Incomplete };
    match c {
        'd' | 'c' | 'y' => {
            let rest = &rest[1..];
            if rest.first() == Some(&c) {
                return Parsed::OpLine(count, c);
            }
            // `d3d` is three lines too.
            let (n2, r2) = take_count(rest);
            if r2.first() == Some(&c) {
                return Parsed::OpLine(count.saturating_mul(n2.unwrap_or(1)), c);
            }
            match parse_motion(rest) {
                Ok((n, m)) => Parsed::Op(count.saturating_mul(n), c, m),
                Err(e) => e,
            }
        }
        'r' => match rest.get(1) {
            None => Parsed::Incomplete,
            Some(&ch) => Parsed::Replace(count, ch),
        },
        'g' | 'f' | 'F' | 't' | 'T' => match parse_motion(rest) {
            Ok((n, m)) => Parsed::Motion(count.saturating_mul(n), m),
            Err(e) => e,
        },
        '0' if !has_count => Parsed::Motion(1, Motion::LineStart),
        _ => match motion_of(c) {
            Some(m) => Parsed::Motion(count, m),
            None => Parsed::Simple(count, c),
        },
    }
}

// ---------------------------------------------------------------------------
// the editor
// ---------------------------------------------------------------------------

impl Editor {
    /// An empty document in normal mode.
    pub fn new() -> Self {
        Editor {
            lines: vec![Vec::new()],
            row: 0,
            col: 0,
            mode: Mode::Normal,
            want_col: None,
            pending: Vec::new(),
            anchor: (0, 0),
            reg: None,
            undo: Vec::new(),
            redo: Vec::new(),
            search: Vec::new(),
            search_fwd: true,
            last_search: None,
            block_insert: None,
            top: Cell::new(0),
        }
    }

    /// `text` in normal mode, the cursor on its last character.
    pub fn with_text(text: &str) -> Self {
        let mut e = Self::new();
        e.set_text(text);
        e
    }

    /// The document as text, lines joined with `\n`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.extend(l.iter());
        }
        out
    }

    /// Replace the document; the cursor goes to the end of the text, the
    /// mode stays, the undo history starts over.
    pub fn set_text(&mut self, text: &str) {
        self.lines = to_lines(text);
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.undo.clear();
        self.redo.clear();
        self.pending.clear();
        self.want_col = None;
        self.block_insert = None;
        self.top.set(0);
        if self.mode != Mode::Insert {
            self.mode = Mode::Normal;
            self.clamp();
        }
    }

    /// Empty the document (mode kept, as [`set_text`](Self::set_text)).
    pub fn clear(&mut self) {
        self.set_text("");
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// `(row, col)` of the cursor, in lines and characters.
    pub fn cursor(&self) -> Pos {
        (self.row, self.col)
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Start insert mode at the cursor (what `i` does), from any mode.
    pub fn insert(&mut self) {
        if self.mode != Mode::Insert {
            self.pending.clear();
            self.checkpoint();
            self.mode = Mode::Insert;
        }
    }

    /// The mode tag for a status row; empty in plain normal mode.
    pub fn status(&self) -> String {
        match self.mode {
            Mode::Insert => "-- INSERT --".into(),
            Mode::Visual => "-- VISUAL --".into(),
            Mode::VisualLine => "-- VISUAL LINE --".into(),
            Mode::VisualBlock => "-- VISUAL BLOCK --".into(),
            Mode::Search => {
                let mut s = String::from(if self.search_fwd { "/" } else { "?" });
                s.extend(self.search.iter());
                s
            }
            Mode::Normal => self.pending.iter().collect(),
        }
    }

    /// Insert `text` as is at the cursor (after it in normal mode, as
    /// `p` would), newlines kept.
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let chars: Vec<char> = text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
        match self.mode {
            Mode::Insert => {
                self.block_insert = None;
                let end = self.insert_chars(self.row, self.col, &chars);
                self.row = end.0;
                self.col = end.1;
            }
            Mode::Search => self.search.extend(chars.iter().filter(|c| **c != '\n')),
            _ => {
                self.leave_visual();
                self.checkpoint();
                let col = if self.lines[self.row].is_empty() { 0 } else { self.col + 1 };
                let end = self.insert_chars(self.row, col, &chars);
                self.row = end.0;
                self.col = end.1.saturating_sub(1);
                self.clamp();
            }
        }
        self.want_col = None;
    }

    // --- keys -------------------------------------------------------------

    /// A key, named as tmux names it.
    pub fn key(&mut self, key: &str) -> Event {
        match self.mode {
            Mode::Insert => self.insert_key(key),
            Mode::Search => self.search_key(key),
            Mode::Normal => self.normal_key(key),
            Mode::Visual | Mode::VisualLine | Mode::VisualBlock => self.visual_key(key),
        }
    }

    fn insert_key(&mut self, key: &str) -> Event {
        let line_len = self.lines[self.row].len();
        match key {
            "Enter" => return Event::Submit,
            "Escape" => {
                self.end_insert();
                return Event::Redraw;
            }
            "C-j" => {
                self.block_insert = None;
                self.split_line();
            }
            "BSpace" | "C-h" => {
                if self.col > 0 {
                    self.col -= 1;
                    self.lines[self.row].remove(self.col);
                    if let Some(bi) = self.block_insert.as_mut() {
                        if bi.typed.pop().is_none() {
                            self.block_insert = None;
                        }
                    }
                } else if self.row > 0 {
                    self.block_insert = None;
                    let tail = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                    self.lines[self.row].extend(tail);
                } else {
                    return Event::None;
                }
            }
            "DC" => {
                self.block_insert = None;
                if self.col < line_len {
                    self.lines[self.row].remove(self.col);
                } else if self.row + 1 < self.lines.len() {
                    let tail = self.lines.remove(self.row + 1);
                    self.lines[self.row].extend(tail);
                } else {
                    return Event::None;
                }
            }
            "C-w" => {
                self.block_insert = None;
                let line = &mut self.lines[self.row];
                let mut i = self.col;
                while i > 0 && line[i - 1].is_whitespace() {
                    i -= 1;
                }
                if i > 0 {
                    let cls = class(line[i - 1], false);
                    while i > 0 && class(line[i - 1], false) == cls {
                        i -= 1;
                    }
                }
                if i == self.col {
                    return Event::None;
                }
                line.drain(i..self.col);
                self.col = i;
            }
            "C-u" => {
                self.block_insert = None;
                if self.col == 0 {
                    return Event::None;
                }
                self.lines[self.row].drain(..self.col);
                self.col = 0;
            }
            "Left" => {
                self.block_insert = None;
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            "Right" => {
                self.block_insert = None;
                if self.col < line_len {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            "Up" | "Down" => {
                self.block_insert = None;
                let want = self.want_col.unwrap_or(self.col);
                if key == "Up" {
                    if self.row == 0 {
                        return Event::None;
                    }
                    self.row -= 1;
                } else {
                    if self.row + 1 >= self.lines.len() {
                        return Event::None;
                    }
                    self.row += 1;
                }
                self.col = want.min(self.lines[self.row].len());
                self.want_col = Some(want);
                return Event::Redraw;
            }
            "Home" => {
                self.block_insert = None;
                self.col = 0;
            }
            "End" => {
                self.block_insert = None;
                self.col = line_len;
            }
            _ => {
                let Some(c) = key_char(key) else { return Event::None };
                self.lines[self.row].insert(self.col, c);
                self.col += 1;
                if let Some(bi) = self.block_insert.as_mut() {
                    bi.typed.push(c);
                }
            }
        }
        self.want_col = None;
        Event::Redraw
    }

    fn search_key(&mut self, key: &str) -> Event {
        match key {
            "Escape" => {
                self.mode = Mode::Normal;
                self.search.clear();
            }
            "Enter" => {
                self.mode = Mode::Normal;
                let pat = std::mem::take(&mut self.search);
                let fwd = self.search_fwd;
                let pat = if pat.is_empty() {
                    match &self.last_search {
                        Some((p, _)) => p.clone(),
                        None => return Event::Redraw,
                    }
                } else {
                    pat
                };
                self.last_search = Some((pat.clone(), fwd));
                self.find(&pat, fwd, 1);
            }
            "BSpace" | "C-h" => {
                if self.search.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            "C-u" => self.search.clear(),
            _ => {
                let Some(c) = key_char(key) else { return Event::None };
                self.search.push(c);
            }
        }
        Event::Redraw
    }

    fn normal_key(&mut self, key: &str) -> Event {
        match key {
            "Escape" => {
                if self.pending.is_empty() {
                    return Event::Leave;
                }
                self.pending.clear();
                return Event::Redraw;
            }
            "Enter" => {
                if self.pending.is_empty() {
                    return Event::Submit;
                }
                self.pending.clear();
                return Event::Redraw;
            }
            "C-v" => {
                self.pending.clear();
                self.anchor = (self.row, self.col);
                self.mode = Mode::VisualBlock;
                return Event::Redraw;
            }
            "C-r" => {
                self.pending.clear();
                return self.redo();
            }
            _ => {}
        }
        let Some(c) = key_alias(key).or_else(|| key_char(key)) else { return Event::None };
        self.pending.push(c);
        match parse(&self.pending) {
            Parsed::Incomplete => Event::Redraw,
            Parsed::Invalid => {
                self.pending.clear();
                Event::Redraw
            }
            p => {
                self.pending.clear();
                self.run_normal(p)
            }
        }
    }

    fn run_normal(&mut self, p: Parsed) -> Event {
        match p {
            Parsed::Motion(n, m) => {
                self.move_cursor(m, n);
                Event::Redraw
            }
            Parsed::Op(n, op, m) => self.operate(op, m, n),
            Parsed::OpLine(n, op) => {
                let a = self.row;
                let b = (self.row + n - 1).min(self.lines.len() - 1);
                self.op_lines(op, a, b)
            }
            Parsed::Replace(n, ch) => {
                let line = &self.lines[self.row];
                if line.is_empty() || self.col + n > line.len() {
                    return Event::Redraw;
                }
                self.checkpoint();
                for i in 0..n {
                    self.lines[self.row][self.col + i] = ch;
                }
                self.col += n - 1;
                Event::Redraw
            }
            Parsed::Simple(n, c) => self.simple(c, n),
            Parsed::Incomplete | Parsed::Invalid => Event::Redraw,
        }
    }

    fn simple(&mut self, c: char, n: usize) -> Event {
        match c {
            'i' => self.start_insert(true),
            'a' => {
                self.start_insert(true);
                if !self.lines[self.row].is_empty() {
                    self.col += 1;
                }
            }
            'A' => {
                self.start_insert(true);
                self.col = self.lines[self.row].len();
            }
            'I' => {
                self.start_insert(true);
                self.col = first_non_blank(&self.lines[self.row]);
            }
            'o' | 'O' => {
                self.start_insert(true);
                let at = if c == 'o' { self.row + 1 } else { self.row };
                self.lines.insert(at, Vec::new());
                self.row = at;
                self.col = 0;
            }
            'x' | 'X' => {
                let line_len = self.lines[self.row].len();
                if line_len == 0 {
                    return Event::Redraw;
                }
                let (a, b) = if c == 'x' { (self.col, (self.col + n).min(line_len)) } else { (self.col.saturating_sub(n), self.col) };
                if a == b {
                    return Event::Redraw;
                }
                self.checkpoint();
                let removed: Vec<char> = self.lines[self.row].drain(a..b).collect();
                self.reg = Some(Reg::Chars(removed));
                self.col = a;
                self.clamp();
            }
            'D' => return self.operate('d', Motion::LineEnd, n),
            'C' => return self.operate('c', Motion::LineEnd, n),
            's' => return self.operate('c', Motion::Right, n),
            'S' => {
                let b = (self.row + n - 1).min(self.lines.len() - 1);
                return self.op_lines('c', self.row, b);
            }
            'p' | 'P' => {
                if self.reg.is_none() {
                    return Event::Redraw;
                }
                self.checkpoint();
                self.put(c == 'p', n);
            }
            'J' => {
                if self.row + 1 >= self.lines.len() {
                    return Event::Redraw;
                }
                self.checkpoint();
                for _ in 0..n.max(2) - 1 {
                    if !self.join_next() {
                        break;
                    }
                }
            }
            'u' => {
                for _ in 0..n {
                    if self.undo() == Event::None {
                        break;
                    }
                }
            }
            '~' => {
                let line_len = self.lines[self.row].len();
                if line_len == 0 {
                    return Event::Redraw;
                }
                self.checkpoint();
                let end = (self.col + n).min(line_len);
                for ch in &mut self.lines[self.row][self.col..end] {
                    *ch = if ch.is_lowercase() { ch.to_uppercase().next().unwrap_or(*ch) } else { ch.to_lowercase().next().unwrap_or(*ch) };
                }
                self.col = end;
                self.clamp();
            }
            'v' | 'V' => {
                self.anchor = (self.row, self.col);
                self.mode = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
            }
            '/' | '?' => {
                self.mode = Mode::Search;
                self.search.clear();
                self.search_fwd = c == '/';
            }
            'n' | 'N' => {
                let Some((pat, fwd)) = self.last_search.clone() else { return Event::Redraw };
                self.find(&pat, fwd == (c == 'n'), n);
            }
            _ => return Event::None,
        }
        Event::Redraw
    }

    fn visual_key(&mut self, key: &str) -> Event {
        if key == "Escape" {
            self.leave_visual();
            return Event::Redraw;
        }
        if key == "C-v" {
            self.pending.clear();
            if self.mode == Mode::VisualBlock {
                self.leave_visual();
            } else {
                self.mode = Mode::VisualBlock;
            }
            return Event::Redraw;
        }
        if key == "Enter" {
            self.leave_visual();
            return Event::Submit;
        }
        let Some(c) = key_alias(key).or_else(|| key_char(key)) else { return Event::None };
        // An operator alone acts on the selection; it waits for nothing.
        if self.pending.is_empty() && matches!(c, 'd' | 'c' | 'y') {
            return self.visual_op(c);
        }
        self.pending.push(c);
        let p = parse(&self.pending);
        match p {
            Parsed::Incomplete => return Event::Redraw,
            Parsed::Invalid => {
                self.pending.clear();
                return Event::Redraw;
            }
            _ => self.pending.clear(),
        }
        match p {
            Parsed::Motion(n, m) => {
                self.move_cursor(m, n);
                Event::Redraw
            }
            // An operator key alone acts on the selection.
            Parsed::Op(_, op, _) | Parsed::OpLine(_, op) => self.visual_op(op),
            Parsed::Replace(_, ch) => {
                // Replace every selected character.
                let (a, b) = self.ordered();
                self.checkpoint();
                let block = self.mode == Mode::VisualBlock;
                let linewise = self.mode == Mode::VisualLine;
                for r in a.0..=b.0 {
                    let line = &mut self.lines[r];
                    let (lo, hi) = if linewise {
                        (0, line.len())
                    } else if block {
                        (a.1.min(b.1), (a.1.max(b.1) + 1).min(line.len()))
                    } else {
                        (if r == a.0 { a.1 } else { 0 }, if r == b.0 { (b.1 + 1).min(line.len()) } else { line.len() })
                    };
                    line[lo..hi.max(lo)].fill(ch);
                }
                self.row = a.0;
                self.col = if block { a.1.min(b.1) } else { a.1 };
                self.leave_visual();
                Event::Redraw
            }
            Parsed::Simple(_, c) => match c {
                'v' | 'V' => {
                    let want = if c == 'v' { Mode::Visual } else { Mode::VisualLine };
                    if self.mode == want {
                        self.leave_visual();
                    } else {
                        self.mode = want;
                    }
                    Event::Redraw
                }
                'o' => {
                    let cur = (self.row, self.col);
                    (self.row, self.col) = self.anchor;
                    self.anchor = cur;
                    Event::Redraw
                }
                'x' => self.visual_op('d'),
                'I' | 'A' if self.mode == Mode::VisualBlock => {
                    let (a, b) = self.ordered();
                    let (lo, hi) = (a.1.min(b.1), a.1.max(b.1));
                    self.checkpoint();
                    let col = if c == 'I' { lo } else { hi + 1 };
                    self.mode = Mode::Insert;
                    self.row = a.0;
                    if c == 'A' {
                        while self.lines[self.row].len() < col {
                            self.lines[self.row].push(' ');
                        }
                    }
                    self.col = col.min(self.lines[self.row].len());
                    self.block_insert = Some(BlockInsert { rows: (a.0, b.0), col, pad: c == 'A', min_len: if c == 'A' { 0 } else { col + 1 }, typed: Vec::new() });
                    Event::Redraw
                }
                'p' | 'P' => {
                    // Replace the selection with the register.
                    let Some(reg) = self.reg.clone() else { return Event::Redraw };
                    let (a, _) = self.ordered();
                    self.visual_op('d');
                    self.reg = Some(reg);
                    // The selection reached the end of its line: put after.
                    let at_end = a.1 >= self.lines[self.row].len();
                    self.put(at_end, 1);
                    Event::Redraw
                }
                _ => Event::None,
            },
            Parsed::Incomplete | Parsed::Invalid => Event::Redraw,
        }
    }

    // --- visual -----------------------------------------------------------

    fn leave_visual(&mut self) {
        if matches!(self.mode, Mode::Visual | Mode::VisualLine | Mode::VisualBlock) {
            self.mode = Mode::Normal;
        }
        self.pending.clear();
        self.clamp();
    }

    /// Anchor and cursor, the earlier one first.
    fn ordered(&self) -> (Pos, Pos) {
        let cur = (self.row, self.col);
        if self.anchor <= cur {
            (self.anchor, cur)
        } else {
            (cur, self.anchor)
        }
    }

    /// `y`, `d` or `c` on the selection.
    fn visual_op(&mut self, op: char) -> Event {
        let (a, b) = self.ordered();
        match self.mode {
            Mode::VisualLine => {
                self.mode = Mode::Normal;
                self.op_lines(op, a.0, b.0)
            }
            Mode::VisualBlock => {
                let (lo, hi) = (a.1.min(b.1), a.1.max(b.1));
                let mut block: Vec<Vec<char>> = Vec::new();
                for r in a.0..=b.0 {
                    let line = &self.lines[r];
                    let end = (hi + 1).min(line.len());
                    block.push(if lo < line.len() { line[lo..end].to_vec() } else { Vec::new() });
                }
                let text: String = block.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n");
                self.reg = Some(Reg::Block(block));
                self.mode = Mode::Normal;
                self.row = a.0;
                self.col = lo;
                if op == 'y' {
                    self.clamp();
                    return Event::Yank(text);
                }
                self.checkpoint();
                for r in a.0..=b.0 {
                    let line = &mut self.lines[r];
                    let end = (hi + 1).min(line.len());
                    if lo < line.len() {
                        line.drain(lo..end);
                    }
                }
                if op == 'c' {
                    self.mode = Mode::Insert;
                    self.col = lo.min(self.lines[self.row].len());
                    self.block_insert = Some(BlockInsert { rows: (a.0, b.0), col: lo, pad: false, min_len: lo, typed: Vec::new() });
                } else {
                    self.clamp();
                }
                Event::Redraw
            }
            _ => {
                self.mode = Mode::Normal;
                // Inclusive of the cursor character.
                let end = self.step_pos(b);
                self.op_chars(op, a, end)
            }
        }
    }

    /// One position past `p` (the start of the next line when `p` is
    /// the last character), for an inclusive end.
    fn step_pos(&self, p: Pos) -> Pos {
        let len = self.lines[p.0].len();
        if p.1 < len {
            (p.0, p.1 + 1)
        } else if p.0 + 1 < self.lines.len() {
            (p.0 + 1, 0)
        } else {
            (p.0, len)
        }
    }

    // --- operators --------------------------------------------------------

    /// `d`/`c`/`y` with a motion from the cursor.
    fn operate(&mut self, op: char, m: Motion, n: usize) -> Event {
        let start = (self.row, self.col);
        // `cw` on a word is `ce`: the trailing blank stays.
        let m = match m {
            Motion::WordFwd(big) if op == 'c' && self.lines[self.row].get(self.col).is_some_and(|c| !c.is_whitespace()) => Motion::WordEnd(big),
            m => m,
        };
        let linewise = matches!(m, Motion::Up | Motion::Down | Motion::Top | Motion::Bottom);
        let inclusive = matches!(m, Motion::WordEnd(_) | Motion::Find('f', _) | Motion::Find('t', _));
        let mut target = self.target(m, n);
        if linewise {
            let (a, b) = (start.0.min(target.0), start.0.max(target.0));
            return self.op_lines(op, a, b);
        }
        match m {
            // `dw` on the last word stops at the end of its line.
            Motion::WordFwd(_) if target.0 > start.0 => target = (start.0, self.lines[start.0].len()),
            // `dl` (and `s`, `x`) take the last character too.
            Motion::Right => target = (start.0, (start.1 + n).min(self.lines[start.0].len())),
            // `D` is to the end of the line, the empty line included.
            Motion::LineEnd => target = (target.0, self.lines[target.0].len()),
            _ => {}
        }
        if target == start && !inclusive {
            if op == 'c' {
                self.start_insert(true);
            }
            return Event::Redraw;
        }
        let (a, mut b) = if start <= target { (start, target) } else { (target, start) };
        if inclusive {
            b = self.step_pos(b);
        }
        self.op_chars(op, a, b)
    }

    /// `op` on the characters from `a` to `b` (exclusive; `b.1` may be
    /// the line's length).
    fn op_chars(&mut self, op: char, a: Pos, b: Pos) -> Event {
        let text = self.slice(a, b);
        if op == 'y' {
            let s: String = text.iter().collect();
            self.reg = Some(Reg::Chars(text));
            self.row = a.0;
            self.col = a.1;
            self.clamp();
            return Event::Yank(s);
        }
        self.checkpoint();
        self.remove(a, b);
        self.reg = Some(Reg::Chars(text));
        self.row = a.0;
        self.col = a.1;
        if op == 'c' {
            self.mode = Mode::Insert;
            self.col = self.col.min(self.lines[self.row].len());
        } else {
            self.clamp();
        }
        Event::Redraw
    }

    /// `op` on whole lines `a..=b`.
    fn op_lines(&mut self, op: char, a: usize, b: usize) -> Event {
        let b = b.min(self.lines.len() - 1);
        let taken: Vec<Vec<char>> = self.lines[a..=b].to_vec();
        if op == 'y' {
            let s = taken.iter().map(|l| l.iter().collect::<String>()).collect::<Vec<_>>().join("\n") + "\n";
            self.reg = Some(Reg::Lines(taken));
            self.row = a;
            self.clamp();
            return Event::Yank(s);
        }
        self.checkpoint();
        self.lines.drain(a..=b);
        self.reg = Some(Reg::Lines(taken));
        if op == 'c' {
            self.lines.insert(a, Vec::new());
            self.row = a;
            self.col = 0;
            self.mode = Mode::Insert;
        } else {
            if self.lines.is_empty() {
                self.lines.push(Vec::new());
            }
            self.row = a.min(self.lines.len() - 1);
            self.col = first_non_blank(&self.lines[self.row]);
            self.clamp();
        }
        Event::Redraw
    }

    /// `p`/`P` `n` times.
    fn put(&mut self, after: bool, n: usize) {
        let Some(reg) = self.reg.clone() else { return };
        match reg {
            Reg::Chars(t) => {
                if t.is_empty() {
                    return;
                }
                let mut pos = (self.row, if after && !self.lines[self.row].is_empty() { self.col + 1 } else { self.col });
                for _ in 0..n {
                    pos = self.insert_chars(pos.0, pos.1, &t);
                }
                self.row = pos.0;
                self.col = pos.1.saturating_sub(1);
                self.clamp();
            }
            Reg::Lines(ls) => {
                let at = if after { self.row + 1 } else { self.row };
                let mut i = at;
                for _ in 0..n {
                    for l in &ls {
                        self.lines.insert(i, l.clone());
                        i += 1;
                    }
                }
                self.row = at;
                self.col = first_non_blank(&self.lines[at]);
            }
            Reg::Block(bl) => {
                let col = if after && !self.lines[self.row].is_empty() { self.col + 1 } else { self.col };
                for (k, l) in bl.iter().enumerate() {
                    let r = self.row + k;
                    if r >= self.lines.len() {
                        self.lines.push(Vec::new());
                    }
                    let line = &mut self.lines[r];
                    while line.len() < col {
                        line.push(' ');
                    }
                    for _ in 0..n {
                        for (j, c) in l.iter().enumerate() {
                            line.insert(col + j, *c);
                        }
                    }
                }
                self.col = col;
                self.clamp();
            }
        }
        self.want_col = None;
    }

    /// Join the next line onto this one with one space (none when this
    /// line ends in a blank or the next is empty). False at the end.
    fn join_next(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        let next = self.lines.remove(self.row + 1);
        let start = first_non_blank(&next);
        let next = if next.is_empty() { next } else { next[start..].to_vec() };
        let line = &mut self.lines[self.row];
        while line.last().is_some_and(|c| c.is_whitespace()) {
            line.pop();
        }
        let at = line.len();
        if !next.is_empty() && !line.is_empty() {
            line.push(' ');
        }
        line.extend(next);
        self.col = at;
        self.clamp();
        true
    }

    // --- insert mode plumbing -------------------------------------------

    fn start_insert(&mut self, checkpoint: bool) {
        if checkpoint {
            self.checkpoint();
        }
        self.mode = Mode::Insert;
        self.want_col = None;
    }

    fn end_insert(&mut self) {
        if let Some(bi) = self.block_insert.take() {
            if !bi.typed.is_empty() {
                for r in bi.rows.0 + 1..=bi.rows.1 {
                    if r >= self.lines.len() {
                        break;
                    }
                    let line = &mut self.lines[r];
                    if bi.pad {
                        while line.len() < bi.col {
                            line.push(' ');
                        }
                    }
                    if line.len() >= bi.col && line.len() >= bi.min_len {
                        for (j, c) in bi.typed.iter().enumerate() {
                            line.insert(bi.col + j, *c);
                        }
                    }
                }
            }
        }
        self.mode = Mode::Normal;
        self.col = self.col.saturating_sub(1);
        self.clamp();
        // An insert that changed nothing leaves no undo step.
        if self.undo.last().is_some_and(|s| s.lines == self.lines) {
            self.undo.pop();
        }
    }

    fn split_line(&mut self) {
        let tail: Vec<char> = self.lines[self.row].split_off(self.col);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    // --- document edits ---------------------------------------------------

    /// Insert `chars` at `(row, col)`; the position just after them.
    fn insert_chars(&mut self, row: usize, col: usize, chars: &[char]) -> Pos {
        let col = col.min(self.lines[row].len());
        let tail: Vec<char> = self.lines[row].split_off(col);
        let mut r = row;
        for &c in chars {
            if c == '\n' {
                self.lines.insert(r + 1, Vec::new());
                r += 1;
            } else {
                self.lines[r].push(c);
            }
        }
        let end = (r, self.lines[r].len());
        self.lines[r].extend(tail);
        end
    }

    /// The characters from `a` to `b` (exclusive), lines joined by `\n`.
    fn slice(&self, a: Pos, b: Pos) -> Vec<char> {
        let mut out = Vec::new();
        for r in a.0..=b.0.min(self.lines.len() - 1) {
            let line = &self.lines[r];
            let lo = if r == a.0 { a.1.min(line.len()) } else { 0 };
            let hi = if r == b.0 { b.1.min(line.len()) } else { line.len() };
            if r > a.0 {
                out.push('\n');
            }
            out.extend_from_slice(&line[lo..hi.max(lo)]);
        }
        out
    }

    fn remove(&mut self, a: Pos, b: Pos) {
        if a.0 == b.0 {
            let line = &mut self.lines[a.0];
            let hi = b.1.min(line.len());
            let lo = a.1.min(hi);
            line.drain(lo..hi);
            return;
        }
        let b = (b.0.min(self.lines.len() - 1), b.1);
        let tail: Vec<char> = {
            let last = &self.lines[b.0];
            last[b.1.min(last.len())..].to_vec()
        };
        self.lines[a.0].truncate(a.1);
        self.lines[a.0].extend(tail);
        self.lines.drain(a.0 + 1..=b.0);
    }

    /// Keep the cursor on a character (or column 0 of an empty line).
    fn clamp(&mut self) {
        self.row = self.row.min(self.lines.len() - 1);
        let len = self.lines[self.row].len();
        let max = if self.mode == Mode::Insert { len } else { len.saturating_sub(1) };
        self.col = self.col.min(max);
    }

    // --- undo -------------------------------------------------------------

    fn checkpoint(&mut self) {
        self.undo.push(Snapshot { lines: self.lines.clone(), cursor: (self.row, self.col) });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self) -> Event {
        let Some(s) = self.undo.pop() else { return Event::None };
        self.redo.push(Snapshot { lines: std::mem::replace(&mut self.lines, s.lines), cursor: (self.row, self.col) });
        self.row = s.cursor.0;
        self.col = s.cursor.1;
        self.clamp();
        Event::Redraw
    }

    fn redo(&mut self) -> Event {
        let Some(s) = self.redo.pop() else { return Event::None };
        self.undo.push(Snapshot { lines: std::mem::replace(&mut self.lines, s.lines), cursor: (self.row, self.col) });
        self.row = s.cursor.0;
        self.col = s.cursor.1;
        self.clamp();
        Event::Redraw
    }

    // --- motions ----------------------------------------------------------

    fn move_cursor(&mut self, m: Motion, n: usize) {
        let vertical = matches!(m, Motion::Up | Motion::Down);
        let t = self.target(m, n);
        self.row = t.0;
        self.col = t.1;
        if !vertical {
            self.want_col = None;
        }
        self.clamp();
    }

    /// Where `m` repeated `n` times lands; the column may equal the
    /// line's length (the end of the buffer under `w`, `$` for `d`).
    fn target(&mut self, m: Motion, n: usize) -> Pos {
        let (mut r, mut c) = (self.row, self.col);
        let last = self.lines.len() - 1;
        match m {
            Motion::Left => c = c.saturating_sub(n),
            Motion::Right => c = (c + n).min(self.lines[r].len().saturating_sub(1)),
            Motion::Up | Motion::Down => {
                let want = *self.want_col.get_or_insert(c);
                r = if m == Motion::Up { r.saturating_sub(n) } else { (r + n).min(last) };
                c = want.min(self.lines[r].len().saturating_sub(1));
            }
            Motion::LineStart => c = 0,
            Motion::FirstNonBlank => c = first_non_blank(&self.lines[r]),
            Motion::LineEnd => {
                r = (r + n - 1).min(last);
                c = self.lines[r].len().saturating_sub(1);
            }
            Motion::Top => r = (n - 1).min(last),
            Motion::Bottom => r = if n > 1 { (n - 1).min(last) } else { last },
            Motion::WordFwd(big) => {
                for _ in 0..n {
                    (r, c) = self.word_fwd(r, c, big);
                }
            }
            Motion::WordBack(big) => {
                for _ in 0..n {
                    (r, c) = self.word_back(r, c, big);
                }
            }
            Motion::WordEnd(big) => {
                for _ in 0..n {
                    (r, c) = self.word_end(r, c, big);
                }
            }
            Motion::ParaFwd => {
                for _ in 0..n {
                    (r, c) = self.para_fwd(r);
                }
            }
            Motion::ParaBack => {
                for _ in 0..n {
                    (r, c) = self.para_back(r);
                }
            }
            Motion::Find(kind, ch) => {
                let line = &self.lines[r];
                let mut hits = 0;
                let mut found: Option<usize> = None;
                match kind {
                    'f' | 't' => {
                        for i in c + 1..line.len() {
                            if line[i] == ch {
                                hits += 1;
                                if hits == n {
                                    found = Some(if kind == 't' { i - 1 } else { i });
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        for i in (0..c).rev() {
                            if line[i] == ch {
                                hits += 1;
                                if hits == n {
                                    found = Some(if kind == 'T' { i + 1 } else { i });
                                    break;
                                }
                            }
                        }
                    }
                }
                if let Some(i) = found {
                    c = i;
                }
            }
        }
        if matches!(m, Motion::Top | Motion::Bottom) {
            c = first_non_blank(&self.lines[r]);
        }
        (r, c)
    }

    fn word_fwd(&self, mut r: usize, mut c: usize, big: bool) -> Pos {
        let line = &self.lines[r];
        if c < line.len() {
            let cls = class(line[c], big);
            if cls != 0 {
                while c < line.len() && class(line[c], big) == cls {
                    c += 1;
                }
            }
        }
        loop {
            let line = &self.lines[r];
            while c < line.len() && line[c].is_whitespace() {
                c += 1;
            }
            if c < line.len() {
                return (r, c);
            }
            if r + 1 >= self.lines.len() {
                return (r, line.len());
            }
            r += 1;
            c = 0;
            if self.lines[r].is_empty() {
                return (r, 0);
            }
        }
    }

    fn step_back(&self, p: &mut Pos) -> bool {
        if p.1 > 0 {
            p.1 -= 1;
            true
        } else if p.0 > 0 {
            p.0 -= 1;
            p.1 = self.lines[p.0].len().saturating_sub(1);
            true
        } else {
            false
        }
    }

    fn word_back(&self, r: usize, c: usize, big: bool) -> Pos {
        let mut p = (r, c.min(self.lines[r].len()));
        if !self.step_back(&mut p) {
            return p;
        }
        loop {
            let line = &self.lines[p.0];
            if line.is_empty() {
                return p;
            }
            if !line[p.1].is_whitespace() {
                break;
            }
            if !self.step_back(&mut p) {
                return p;
            }
        }
        let line = &self.lines[p.0];
        let cls = class(line[p.1], big);
        while p.1 > 0 && class(line[p.1 - 1], big) == cls {
            p.1 -= 1;
        }
        p
    }

    fn step_fwd(&self, p: &mut Pos) -> bool {
        if p.1 + 1 < self.lines[p.0].len() {
            p.1 += 1;
            true
        } else if p.0 + 1 < self.lines.len() {
            p.0 += 1;
            p.1 = 0;
            true
        } else {
            false
        }
    }

    fn word_end(&self, r: usize, c: usize, big: bool) -> Pos {
        let mut p = (r, c);
        if !self.step_fwd(&mut p) {
            return (r, self.lines[r].len().saturating_sub(1));
        }
        loop {
            let line = &self.lines[p.0];
            if p.1 < line.len() && !line[p.1].is_whitespace() {
                break;
            }
            if !self.step_fwd(&mut p) {
                return p;
            }
        }
        let line = &self.lines[p.0];
        let cls = class(line[p.1], big);
        while p.1 + 1 < line.len() && class(line[p.1 + 1], big) == cls {
            p.1 += 1;
        }
        p
    }

    fn para_fwd(&self, row: usize) -> Pos {
        let n = self.lines.len();
        let mut r = row;
        while r < n && is_blank_line(&self.lines[r]) {
            r += 1;
        }
        while r < n && !is_blank_line(&self.lines[r]) {
            r += 1;
        }
        if r >= n {
            (n - 1, self.lines[n - 1].len().saturating_sub(1))
        } else {
            (r, 0)
        }
    }

    fn para_back(&self, row: usize) -> Pos {
        let mut r = row;
        while r > 0 && is_blank_line(&self.lines[r]) {
            r -= 1;
        }
        while r > 0 && !is_blank_line(&self.lines[r]) {
            r -= 1;
        }
        (r, 0)
    }

    // --- search -----------------------------------------------------------

    /// Move to the `n`th match of `pat` from the cursor, wrapping.
    fn find(&mut self, pat: &[char], fwd: bool, n: usize) -> bool {
        if pat.is_empty() {
            return false;
        }
        let fold = !pat.iter().any(|c| c.is_uppercase());
        let pat: Vec<char> = if fold { pat.iter().flat_map(|c| c.to_lowercase()).collect() } else { pat.to_vec() };
        let mut pos = (self.row, self.col);
        for _ in 0..n {
            match self.find_from(&pat, fold, pos, fwd) {
                Some(p) => pos = p,
                None => return false,
            }
        }
        self.row = pos.0;
        self.col = pos.1;
        self.want_col = None;
        self.clamp();
        true
    }

    fn find_from(&self, pat: &[char], fold: bool, from: Pos, fwd: bool) -> Option<Pos> {
        let n = self.lines.len();
        let matches_at = |line: &[char], i: usize| -> bool {
            if i + pat.len() > line.len() {
                return false;
            }
            line[i..i + pat.len()].iter().zip(pat).all(|(a, b)| if fold { a.to_lowercase().eq(std::iter::once(*b)) } else { a == b })
        };
        for k in 0..=n {
            let r = if fwd { (from.0 + k) % n } else { (from.0 + n - k % n) % n };
            let line = &self.lines[r];
            if line.len() < pat.len() {
                continue;
            }
            let cols: Vec<usize> = if fwd {
                let start = if k == 0 { from.1 + 1 } else { 0 };
                let end = if k == n { from.1.min(line.len()) } else { line.len() - pat.len() + 1 };
                (start..end.max(start)).collect()
            } else {
                let end = if k == 0 { from.1 } else { line.len() - pat.len() + 1 };
                let start = if k == n { from.1 + 1 } else { 0 };
                (start..end.max(start)).rev().collect()
            };
            for i in cols {
                if matches_at(line, i) {
                    return Some((r, i));
                }
            }
        }
        None
    }

    // --- render -----------------------------------------------------------

    /// Is `(r, c)` inside the visual selection?
    fn selected(&self, r: usize, c: usize) -> bool {
        let (a, b) = self.ordered();
        match self.mode {
            Mode::Visual => (r, c) >= a && (r, c) <= b,
            Mode::VisualLine => r >= a.0 && r <= b.0,
            Mode::VisualBlock => r >= a.0 && r <= b.0 && c >= a.1.min(b.1) && c <= a.1.max(b.1),
            _ => false,
        }
    }

    /// Every display row of the document at `width`, and the index of
    /// the row holding the cursor.
    fn layout(&self, width: usize) -> (Vec<Vec<Styled>>, usize) {
        let width = width.max(1);
        let mut rows: Vec<Vec<Styled>> = Vec::new();
        let mut cursor_row = 0;
        for (r, line) in self.lines.iter().enumerate() {
            let mut cells: Vec<Styled> = Vec::with_capacity(line.len() + 1);
            let mut cursor_cell: Option<usize> = None;
            for (i, &ch) in line.iter().enumerate() {
                let mut style = 0;
                if self.selected(r, i) {
                    style |= ST_INVERT | ST_DIM;
                }
                let on_cursor = r == self.row && i == self.col;
                if on_cursor {
                    style = ST_INVERT;
                    cursor_cell = Some(cells.len());
                }
                let text = if ch == '\t' { " ".to_string() } else { ch.to_string() };
                let before = cells.len();
                cells.extend(glyph_cells(&text, style));
                if cells.len() == before {
                    cells.push((' ', style));
                }
            }
            if r == self.row && cursor_cell.is_none() {
                // The cursor past the end (insert mode, or an empty line).
                cursor_cell = Some(cells.len());
                cells.push((' ', ST_INVERT));
            }
            let total = cells.len();
            let mut start = 0;
            loop {
                let mut end = (start + width).min(total);
                // Never split a wide glyph from its second column.
                if end < total && end > start + 1 && cells[end].0 == listkit::styled::FILL {
                    end -= 1;
                }
                if cursor_cell.is_some_and(|cc| cc >= start && cc < end) {
                    cursor_row = rows.len();
                }
                rows.push(cells[start..end].to_vec());
                start = end;
                if start >= total {
                    break;
                }
            }
        }
        (rows, cursor_row)
    }

    /// How many rows the whole document takes at `width`.
    pub fn rows_needed(&self, width: usize) -> usize {
        self.layout(width).0.len()
    }

    /// `height` rows of cells (the owner pads or clips them to `width`),
    /// scrolled so the cursor row is on screen.
    pub fn render(&self, width: usize, height: usize) -> Vec<Vec<Styled>> {
        if height == 0 {
            return Vec::new();
        }
        let (rows, cursor_row) = self.layout(width);
        let mut top = self.top.get().min(rows.len().saturating_sub(height));
        if cursor_row < top {
            top = cursor_row;
        } else if cursor_row >= top + height {
            top = cursor_row + 1 - height;
        }
        self.top.set(top);
        let mut out: Vec<Vec<Styled>> = rows.into_iter().skip(top).take(height).collect();
        while out.len() < height {
            out.push(Vec::new());
        }
        out
    }
}
