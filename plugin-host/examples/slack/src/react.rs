//! `e` on a message: the reaction picker. A grid of every emoji, in
//! Slack's order under its category headers, with the reactions used
//! last on top; typing narrows it to the names that match (`fir` ->
//! 🔥 and the rest, the exact name first, then the names that start
//! so, then the ones that contain it, then the titles), the arrows,
//! C-h/j/k/l, Tab and the mouse move over it, Enter reacts with the highlighted
//! one and a click reacts at once. A precise name goes in as typed:
//! `+1::skin-tone-3` narrows to 👍 and Enter sends that very name; a
//! name the grid does not know (an image-only custom emoji, say) is
//! sent as typed too when nothing matches. C-t cycles the skin tone
//! the grid shows and Enter sends, and it is remembered.
//!
//! The grid is `emoji_table::GRID` (iamcal/emoji-data's order, Slack's
//! own), the workspace's custom emoji after it; the recent list is the
//! `recent_reactions` setting, seeded with Slack's quick reactions.

use listkit::styled::{plain_cells, push_glyph, Styled, ST_CYAN, ST_DIM, ST_INVERT};

use crate::emoji;
use crate::emoji_table::{ALIASES, CATEGORIES, GRID};

/// The settings key of the names reacted with last, newest first.
pub const RECENT_SETTING: &str = "recent_reactions";
/// The settings key of the skin tone the picker shows (0, or 2..=6).
pub const TONE_SETTING: &str = "reaction_tone";
/// Most names the recent list keeps.
pub const RECENT_MAX: usize = 24;
/// What the recent row shows before any reaction was sent: Slack's
/// quick reactions.
pub const DEFAULT_RECENT: &[&str] = &["+1", "white_check_mark", "eyes", "raised_hands", "joy", "tada", "heart", "pray", "fire", "thinking_face"];
/// Cells per emoji in the grid: a blank, the glyph (two cells), a blank.
pub const SLOT: usize = 4;
/// Most grid rows shown under the search line.
pub const MAX_GRID_ROWS: usize = 8;

/// One thing in the list the grid is laid out from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A section header: a row of its own.
    Head(String),
    /// An emoji: its name (what a reaction is sent as), the glyph the
    /// grid draws (none for an image-only custom emoji), whether a skin
    /// tone applies.
    Emoji { name: String, glyph: Option<String>, skin: bool },
}

/// A laid-out grid row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// The header item at this index.
    Head(usize),
    /// The emoji items at these indexes, left to right.
    Slots(Vec<usize>),
}

/// What a key asks the owner for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    None,
    Close,
    /// React with this name.
    Pick(String),
    /// The skin tone changed: remember it.
    Tone(u8),
}

pub struct Picker {
    /// The message the reaction goes on.
    pub ts: String,
    pub query: String,
    /// Index into `items` of the highlighted emoji, when there is one.
    pub cursor: usize,
    /// The first grid row shown.
    pub top: usize,
    /// The skin tone shown and sent: 0 for none, else 2..=6.
    pub tone: u8,
    pub items: Vec<Item>,
    /// The names reacted with last, newest first.
    pub recent: Vec<String>,
}

/// The search text as the matcher sees it: no colons around it, lower
/// case, blanks as underscores; and the `::suffix` typed after it, if
/// any (`+1::skin-tone-3` -> `+1`, `skin-tone-3`).
fn parse(query: &str) -> (String, Option<String>) {
    let q = query.trim().trim_matches(':');
    let (base, suffix) = match q.split_once("::") {
        Some((b, s)) => (b, Some(s.trim_matches(':').to_string()).filter(|s| !s.is_empty())),
        None => (q, None),
    };
    (base.to_lowercase().replace(' ', "_"), suffix)
}

/// The glyph of `name` with the skin tone when it takes one.
fn glyph(name: &str, skin: bool, tone: u8) -> Option<String> {
    if skin && tone >= 2 {
        if let Some(g) = emoji::lookup(&format!("{name}::skin-tone-{tone}")) {
            return Some(g);
        }
    }
    emoji::lookup(name)
}

fn emoji_item(name: &str, skin: bool, tone: u8) -> Item {
    Item::Emoji { name: name.to_string(), glyph: glyph(name, skin, tone), skin }
}

/// Whether a grid name takes a skin tone.
fn takes_skin(name: &str) -> bool {
    GRID.iter().find(|(n, _, _, _)| *n == name).is_some_and(|(_, _, _, s)| *s)
}

/// The whole grid: the recent row, then every category, then the
/// workspace's custom emoji.
fn everything(recent: &[String], tone: u8) -> Vec<Item> {
    let mut out = Vec::with_capacity(GRID.len() + 16);
    let recent: Vec<&str> = if recent.is_empty() { DEFAULT_RECENT.to_vec() } else { recent.iter().map(String::as_str).collect() };
    out.push(Item::Head("recent".into()));
    for name in recent {
        out.push(emoji_item(name, takes_skin(name), tone));
    }
    let mut cat: Option<u8> = None;
    for (name, _, c, skin) in GRID {
        if cat != Some(*c) {
            cat = Some(*c);
            out.push(Item::Head(CATEGORIES.get(*c as usize).unwrap_or(&"").to_lowercase()));
        }
        out.push(emoji_item(name, *skin, tone));
    }
    let custom = emoji::custom_names();
    if !custom.is_empty() {
        out.push(Item::Head("custom".into()));
        for name in custom {
            out.push(emoji_item(&name, false, tone));
        }
    }
    out
}

/// How well `name` matches `q`: 0 exact, 1 starts with it, 2 contains
/// it, none otherwise.
fn tier(name: &str, q: &str) -> Option<u8> {
    if name == q {
        Some(0)
    } else if name.starts_with(q) {
        Some(1)
    } else if name.contains(q) {
        Some(2)
    } else {
        None
    }
}

/// The emoji whose name, alias or title has `q` in it, best match
/// first: the exact name, then the names that start with it, then the
/// ones that contain it, then the titles; the shorter name first within
/// a tier (`fir` -> fire before firefighter), Slack's order after that.
fn hits(q: &str, tone: u8) -> Vec<Item> {
    // (tier, order) by grid index; custom emoji follow the grid.
    let mut best: Vec<Option<u8>> = vec![None; GRID.len()];
    let title_q = q.replace('_', " ");
    for (i, (name, title, _, _)) in GRID.iter().enumerate() {
        let t = tier(name, q).or_else(|| title.contains(&title_q).then_some(3));
        best[i] = t;
    }
    for (alias, name) in ALIASES {
        let Some(t) = tier(alias, q) else { continue };
        if let Some(i) = GRID.iter().position(|(n, _, _, _)| n == name) {
            best[i] = Some(best[i].map_or(t, |b| b.min(t)));
        }
    }
    let mut found: Vec<(u8, usize, Item)> = best
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|t| (t, i, emoji_item(GRID[i].0, GRID[i].3, tone))))
        .collect();
    for (k, name) in emoji::custom_names().into_iter().enumerate() {
        if let Some(t) = tier(&name, q) {
            found.push((t, GRID.len() + k, emoji_item(&name, false, tone)));
        }
    }
    found.sort_by_key(|(t, i, it)| (*t, match it { Item::Emoji { name, .. } => name.len(), _ => 0 }, *i));
    found.into_iter().map(|(_, _, it)| it).collect()
}

impl Picker {
    pub fn open(ts: String, recent: Vec<String>, tone: u8) -> Self {
        let mut p = Picker { ts, query: String::new(), cursor: 0, top: 0, tone, items: Vec::new(), recent };
        p.refresh();
        p
    }

    /// Lay the list out again for the query as it stands; the cursor
    /// goes to the first emoji.
    fn refresh(&mut self) {
        let (base, _) = parse(&self.query);
        self.items = if base.is_empty() { everything(&self.recent, self.tone) } else { hits(&base, self.tone) };
        self.cursor = self.items.iter().position(|it| matches!(it, Item::Emoji { .. })).unwrap_or(0);
        self.top = 0;
    }

    /// The highlighted emoji, if the list has any.
    pub fn selected(&self) -> Option<(&str, bool)> {
        match self.items.get(self.cursor) {
            Some(Item::Emoji { name, skin, .. }) => Some((name, *skin)),
            _ => None,
        }
    }

    /// The name a reaction with the highlighted emoji is sent as: the
    /// suffix typed after `::` wins, else the remembered skin tone when
    /// the emoji takes one.
    pub fn pick_name(&self) -> Option<String> {
        let (name, skin) = self.selected()?;
        let (_, suffix) = parse(&self.query);
        Some(match suffix {
            Some(s) => format!("{name}::{s}"),
            None if skin && self.tone >= 2 => format!("{name}::skin-tone-{}", self.tone),
            None => name.to_string(),
        })
    }

    /// Grid slots per row at `width`.
    pub fn cols(width: usize) -> usize {
        (width / SLOT).max(1)
    }

    /// The rows the list lays out into at `cols` slots per row.
    pub fn rows(&self, cols: usize) -> Vec<Row> {
        let mut rows: Vec<Row> = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        for (i, it) in self.items.iter().enumerate() {
            match it {
                Item::Head(_) => {
                    if !run.is_empty() {
                        rows.push(Row::Slots(std::mem::take(&mut run)));
                    }
                    rows.push(Row::Head(i));
                }
                Item::Emoji { .. } => {
                    run.push(i);
                    if run.len() == cols {
                        rows.push(Row::Slots(std::mem::take(&mut run)));
                    }
                }
            }
        }
        if !run.is_empty() {
            rows.push(Row::Slots(run));
        }
        rows
    }

    /// Rows the picker wants: the search line plus the grid, at most
    /// [`MAX_GRID_ROWS`] of it and never more than `avail` in all.
    pub fn height(&self, width: usize, avail: usize) -> usize {
        let grid = self.rows(Self::cols(width)).len().clamp(1, MAX_GRID_ROWS);
        (1 + grid).min(avail.max(2))
    }

    /// The row the cursor is on and its column in it.
    fn position(&self, rows: &[Row]) -> Option<(usize, usize)> {
        rows.iter().enumerate().find_map(|(r, row)| match row {
            Row::Slots(v) => v.iter().position(|&i| i == self.cursor).map(|k| (r, k)),
            Row::Head(_) => None,
        })
    }

    /// Keep the cursor's row on screen, with its header when it is the
    /// row just above.
    fn reveal(&mut self, rows: &[Row], grid_rows: usize) {
        let Some((r, _)) = self.position(rows) else { return };
        let grid_rows = grid_rows.max(1);
        if r < self.top || (r > 0 && matches!(rows[r - 1], Row::Head(_)) && r - 1 < self.top) {
            self.top = r;
            if r > 0 && matches!(rows[r - 1], Row::Head(_)) && grid_rows > 1 {
                self.top = r - 1;
            }
        }
        if r >= self.top + grid_rows {
            self.top = r + 1 - grid_rows;
        }
        self.top = self.top.min(rows.len().saturating_sub(grid_rows));
    }

    /// Move the highlight by `by` emoji (headers skipped).
    fn step(&mut self, by: isize) {
        let idx: Vec<usize> = self.items.iter().enumerate().filter(|(_, it)| matches!(it, Item::Emoji { .. })).map(|(i, _)| i).collect();
        if idx.is_empty() {
            return;
        }
        let p = idx.iter().position(|&i| i == self.cursor).unwrap_or(0) as isize;
        let np = (p + by).clamp(0, idx.len() as isize - 1) as usize;
        self.cursor = idx[np];
    }

    /// Move the highlight `by` rows, to the same column (or the last).
    fn step_rows(&mut self, rows: &[Row], by: isize) {
        let Some((r, k)) = self.position(rows) else { return };
        let mut target = r as isize;
        let mut left = by.abs();
        let dir = by.signum();
        while left > 0 {
            let next = target + dir;
            if next < 0 || next >= rows.len() as isize {
                break;
            }
            target = next;
            if matches!(rows[target as usize], Row::Slots(_)) {
                left -= 1;
            }
        }
        // A header at the edge: stay on the last slots row passed.
        while target != r as isize && matches!(rows[target as usize], Row::Head(_)) {
            target -= dir;
        }
        if let Row::Slots(v) = &rows[target as usize] {
            self.cursor = v[k.min(v.len() - 1)];
        }
    }

    /// Change the search text: the key's text, a backspace, C-u (all
    /// of it), C-w (the last word).
    fn edit(&mut self, key: &str) -> bool {
        match key {
            "BSpace" => {
                self.query.pop();
            }
            "C-u" => self.query.clear(),
            "C-w" => {
                let t = self.query.trim_end_matches(|c: char| matches!(c, ' ' | '_' | ':' | '-'));
                let cut = t.rfind(|c: char| matches!(c, ' ' | '_' | ':' | '-')).map_or(0, |i| i + 1);
                self.query.truncate(cut);
            }
            "Space" => self.query.push(' '),
            k if k.chars().count() == 1 && !k.chars().next().is_some_and(char::is_control) => self.query.push_str(k),
            _ => return false,
        }
        true
    }

    /// A key while the picker is open; `width` is the cells it draws
    /// in, `grid_rows` the grid rows on screen.
    pub fn key(&mut self, key: &str, width: usize, grid_rows: usize) -> Step {
        let cols = Self::cols(width);
        match key {
            "Escape" => return Step::Close,
            "Enter" => {
                if let Some(name) = self.pick_name() {
                    return Step::Pick(name);
                }
                let (base, suffix) = parse(&self.query);
                if base.is_empty() {
                    return Step::Close;
                }
                return Step::Pick(match suffix {
                    Some(s) => format!("{base}::{s}"),
                    None => base,
                });
            }
            "Right" | "Tab" | "C-n" | "C-l" => self.step(1),
            "Left" | "BTab" | "C-p" | "C-h" => self.step(-1),
            "Down" | "C-j" => {
                let rows = self.rows(cols);
                self.step_rows(&rows, 1);
            }
            "Up" | "C-k" => {
                let rows = self.rows(cols);
                self.step_rows(&rows, -1);
            }
            "NPage" => {
                let rows = self.rows(cols);
                self.step_rows(&rows, grid_rows.max(1) as isize);
            }
            "PPage" => {
                let rows = self.rows(cols);
                self.step_rows(&rows, -(grid_rows.max(1) as isize));
            }
            "Home" => self.step(-(self.items.len() as isize)),
            "End" => self.step(self.items.len() as isize),
            "C-t" => {
                self.tone = match self.tone {
                    0 | 1 => 2,
                    6 => 0,
                    t => t + 1,
                };
                let keep = self.cursor;
                let top = self.top;
                self.refresh();
                self.cursor = keep.min(self.items.len().saturating_sub(1));
                self.top = top;
                return Step::Tone(self.tone);
            }
            _ => {
                if !self.edit(key) {
                    return Step::None;
                }
                self.refresh();
            }
        }
        let rows = self.rows(cols);
        self.reveal(&rows, grid_rows);
        Step::None
    }

    /// Scroll the grid by `by` rows (the wheel); the highlight follows
    /// onto the screen.
    pub fn scroll(&mut self, by: isize, width: usize, grid_rows: usize) {
        let rows = self.rows(Self::cols(width));
        let max_top = rows.len().saturating_sub(grid_rows.max(1));
        self.top = (self.top as isize + by).clamp(0, max_top as isize) as usize;
        if let Some((r, k)) = self.position(&rows) {
            if r < self.top || r >= self.top + grid_rows.max(1) {
                let want = if r < self.top { self.top } else { self.top + grid_rows.max(1) - 1 };
                let near = (want..rows.len()).chain((0..want).rev()).find(|&i| matches!(rows[i], Row::Slots(_)));
                if let Some(Row::Slots(v)) = near.map(|i| &rows[i]) {
                    self.cursor = v[k.min(v.len() - 1)];
                }
            }
        }
    }

    /// A click at grid row `row` (0 is the first row under the search
    /// line), cell `col` from the picker's left edge: the emoji there is
    /// the pick.
    pub fn click(&mut self, col: usize, row: usize, width: usize) -> Step {
        let rows = self.rows(Self::cols(width));
        match rows.get(self.top + row) {
            Some(Row::Slots(v)) => match v.get(col / SLOT) {
                Some(&i) => {
                    self.cursor = i;
                    self.pick_name().map_or(Step::None, Step::Pick)
                }
                None => Step::None,
            },
            _ => Step::None,
        }
    }

    /// The picker as `height` rows `width` wide: the search line (the
    /// prompt, the text, the cursor, the highlighted emoji's name at the
    /// right), then the grid from `top`, the highlight in reverse video,
    /// a header dim on a row of its own.
    pub fn render(&self, width: usize, height: usize) -> Vec<Vec<Styled>> {
        let mut out: Vec<Vec<Styled>> = Vec::with_capacity(height);
        let mut line = plain_cells("react :", ST_DIM);
        line.extend(plain_cells(&self.query, 0));
        line.push((' ', ST_INVERT));
        let tag = match self.selected() {
            Some(_) => {
                let name = self.pick_name().unwrap_or_default();
                let n = self.items.iter().filter(|it| matches!(it, Item::Emoji { .. })).count();
                if parse(&self.query).0.is_empty() {
                    format!(":{name}:")
                } else {
                    format!(":{name}:  {n}")
                }
            }
            None if parse(&self.query).0.is_empty() => String::new(),
            None => "no match · Enter sends it as typed".to_string(),
        };
        if !tag.is_empty() && line.len() + 2 + tag.chars().count() <= width {
            line.resize(width - tag.chars().count(), (' ', 0));
            line.extend(plain_cells(&tag, ST_CYAN | ST_DIM));
        }
        line.truncate(width);
        out.push(line);
        let rows = self.rows(Self::cols(width));
        for row in rows.iter().skip(self.top).take(height.saturating_sub(1)) {
            let mut cells: Vec<Styled> = Vec::with_capacity(width);
            match row {
                Row::Head(i) => {
                    if let Some(Item::Head(label)) = self.items.get(*i) {
                        cells.extend(plain_cells(&format!(" {label}"), ST_DIM));
                    }
                }
                Row::Slots(v) => {
                    for &i in v {
                        let Some(Item::Emoji { glyph, .. }) = self.items.get(i) else { continue };
                        let st = if i == self.cursor { ST_INVERT } else { 0 };
                        let start = cells.len();
                        cells.push((' ', st));
                        match glyph {
                            Some(g) => push_glyph(&mut cells, g, st),
                            None => cells.push(('□', st | ST_DIM)),
                        }
                        cells.resize((start + SLOT).max(cells.len()), (' ', st));
                        cells.truncate(start + SLOT);
                    }
                }
            }
            cells.truncate(width);
            out.push(cells);
        }
        out
    }
}

/// The foot's hint while the picker is open.
pub fn hints() -> &'static str {
    "type to search · arrows / C-hjkl move · Enter react · click reacts · C-t skin tone · Esc close"
}

/// Put `name` (its skin tone dropped) at the head of the recent list.
pub fn note_recent(recent: &mut Vec<String>, name: &str) {
    let base = name.split("::").next().unwrap_or(name).to_string();
    if base.is_empty() {
        return;
    }
    recent.retain(|n| *n != base);
    recent.insert(0, base);
    recent.truncate(RECENT_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;
    use listkit::styled::cells_text;

    fn names(p: &Picker) -> Vec<&str> {
        p.items
            .iter()
            .filter_map(|it| match it {
                Item::Emoji { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_query_shows_recent_then_every_category() {
        let p = Picker::open("1.2".into(), vec!["fire".into(), "+1".into()], 0);
        assert_eq!(p.items[0], Item::Head("recent".into()));
        assert_eq!(&names(&p)[..2], &["fire", "+1"]);
        assert_eq!(p.items[3], Item::Head("smileys & emotion".into()));
        let heads: Vec<&str> = p.items.iter().filter_map(|it| if let Item::Head(h) = it { Some(h.as_str()) } else { None }).collect();
        assert_eq!(heads.len(), 1 + CATEGORIES.len(), "{heads:?}");
        assert_eq!(heads.last().copied(), Some("flags"));
        assert_eq!(names(&p).len(), 2 + GRID.len());
        assert_eq!(p.selected(), Some(("fire", false)), "the cursor starts on the first recent emoji");
        // With nothing sent yet, Slack's quick reactions stand in.
        let p = Picker::open("1.2".into(), Vec::new(), 0);
        assert_eq!(names(&p)[0], "+1");
        assert_eq!(p.pick_name().as_deref(), Some("+1"));
    }

    #[test]
    fn typing_narrows_exact_prefix_contains_title() {
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in ["f", "i", "r", "e"] {
            p.key(k, 40, 5);
        }
        let n = names(&p);
        assert_eq!(n[0], "fire", "{n:?}");
        assert!(n.contains(&"fire_engine") && n.contains(&"firecracker"), "{n:?}");
        assert!(n.iter().position(|x| *x == "fireworks").unwrap() < n.iter().position(|x| *x == "female-firefighter").unwrap(), "prefix before contains: {n:?}");
        assert!(n.iter().position(|x| *x == "fireworks").unwrap() < n.iter().position(|x| *x == "fire_engine").unwrap(), "the shorter prefix match first: {n:?}");
        assert!(!p.items.iter().any(|it| matches!(it, Item::Head(_))), "no headers while searching");
        assert_eq!(p.pick_name().as_deref(), Some("fire"));
        // An alias finds its grid name; a title word does too.
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in "thumbsup".chars() {
            p.key(&k.to_string(), 40, 5);
        }
        assert_eq!(names(&p)[0], "+1");
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in ["c", "a", "n", "a", "d", "a"] {
            p.key(k, 40, 5);
        }
        assert_eq!(names(&p), vec!["flag-ca"]);
        // A blank types as an underscore; the exact name leads.
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in ["w", "h", "i", "t", "e", "Space", "c", "h", "e", "c", "k"] {
            p.key(k, 40, 5);
        }
        assert_eq!(names(&p)[0], "white_check_mark");
        // Nothing matches: Enter sends the text as typed.
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in "morphx".chars() {
            p.key(&k.to_string(), 40, 5);
        }
        assert!(names(&p).is_empty());
        assert_eq!(p.key("Enter", 40, 5), Step::Pick("morphx".into()));
        // Backspace and C-u edit the text.
        p.key("BSpace", 40, 5);
        assert_eq!(p.query, "morph");
        p.key("C-u", 40, 5);
        assert_eq!(p.query, "");
        assert_eq!(p.items[0], Item::Head("recent".into()), "the whole grid is back");
    }

    #[test]
    fn precise_name_and_skin_tone() {
        let mut p = Picker::open("1.2".into(), Vec::new(), 0);
        for k in "+1::skin-tone-3".chars() {
            p.key(&k.to_string(), 40, 5);
        }
        assert_eq!(names(&p)[0], "+1");
        assert_eq!(p.key("Enter", 40, 5), Step::Pick("+1::skin-tone-3".into()), "the suffix goes as typed");
        // C-t cycles the tone the grid shows and Enter sends.
        let mut p = Picker::open("1.2".into(), vec!["+1".into(), "fire".into()], 0);
        assert_eq!(p.key("C-t", 40, 5), Step::Tone(2));
        assert_eq!(p.pick_name().as_deref(), Some("+1::skin-tone-2"));
        match &p.items[1] {
            Item::Emoji { glyph: Some(g), .. } => assert!(g.contains('\u{1F3FB}'), "{g:?}"),
            other => panic!("{other:?}"),
        }
        p.key("Right", 40, 5);
        assert_eq!(p.pick_name().as_deref(), Some("fire"), "no tone on an emoji that takes none");
        for _ in 0..5 {
            p.key("C-t", 40, 5);
        }
        assert_eq!(p.tone, 0, "2..6 then off");
    }

    #[test]
    fn movement_over_rows_and_headers() {
        let mut p = Picker::open("1.2".into(), vec!["fire".into(), "tada".into(), "eyes".into()], 0);
        // Three recent (one row at 10 slots), then the smileys header, then the grid.
        p.key("C-u", 40, 5);
        let rows = p.rows(10);
        assert_eq!(rows[0], Row::Head(0));
        assert!(matches!(&rows[1], Row::Slots(v) if v.len() == 3));
        assert_eq!(rows[2], Row::Head(4));
        p.key("Right", 40, 5);
        p.key("Right", 40, 5);
        assert_eq!(p.cursor, 3);
        p.key("Down", 40, 5);
        assert!(matches!(&rows[3], Row::Slots(v) if v[2] == p.cursor), "down skips the header, same column");
        p.key("Up", 40, 5);
        assert_eq!(p.cursor, 3);
        p.key("Up", 40, 5);
        assert_eq!(p.cursor, 3, "the top row stays");
        p.key("End", 40, 5);
        assert_eq!(p.selected().map(|(n, _)| n), names(&p).last().copied());
        assert!(p.top > 0, "the grid scrolled to the cursor");
        p.key("Home", 40, 5);
        assert_eq!((p.cursor, p.top), (1, 0));
        // Page down moves by the visible rows; Tab is Right.
        p.key("NPage", 40, 5);
        assert!(p.position(&rows).unwrap().0 >= 5, "{:?}", p.position(&rows));
        let before = p.cursor;
        p.key("Tab", 40, 5);
        assert_eq!(p.cursor, before + 1);
        // C-h/j/k/l are Left/Down/Up/Right (plain hjkl type into the search).
        p.key("C-h", 40, 5);
        assert_eq!(p.cursor, before);
        p.key("C-l", 40, 5);
        assert_eq!(p.cursor, before + 1);
        let (r, k) = p.position(&rows).unwrap();
        p.key("C-j", 40, 5);
        assert_eq!(p.position(&rows).unwrap(), (r + 1, k));
        p.key("C-k", 40, 5);
        assert_eq!(p.position(&rows).unwrap(), (r, k));
        p.key("h", 40, 5);
        assert_eq!(p.query, "h");
    }

    #[test]
    fn render_rows_and_click() {
        let mut p = Picker::open("1.2".into(), vec!["fire".into(), "+1".into()], 0);
        let rows = p.render(24, 4);
        assert_eq!(rows.len(), 4);
        let head = cells_text(&rows[0]);
        assert!(head.starts_with("react : "), "{head:?}");
        assert!(head.ends_with(":fire:"), "{head:?}");
        assert_eq!(cells_text(&rows[1]).trim_end(), " recent");
        let grid = cells_text(&rows[2]);
        assert!(grid.starts_with(" 🔥"), "{grid:?}");
        assert!(grid.contains("👍"), "{grid:?}");
        assert!(rows[2].iter().take(SLOT).all(|c| c.1 & ST_INVERT != 0), "the highlight covers the slot");
        assert_eq!(rows[2][SLOT].1 & ST_INVERT, 0);
        // A click on the second slot of the first grid row reacts with it.
        assert_eq!(p.click(SLOT + 1, 1, 24), Step::Pick("+1".into()));
        assert_eq!(p.click(0, 0, 24), Step::None, "a header row");
        // The height: search line + grid, capped.
        assert_eq!(p.height(24, 30), 1 + MAX_GRID_ROWS);
        assert_eq!(p.height(24, 3), 3);
        // Search typed: the count beside the name, no match says so.
        p.key("f", 24, 3);
        p.key("i", 24, 3);
        p.key("r", 24, 3);
        let head = cells_text(&p.render(40, 2)[0]);
        assert!(head.contains(":fire:  ") && head.trim_end().chars().last().unwrap().is_ascii_digit(), "{head:?}");
        p.key("C-u", 24, 3);
        for k in "zzzz".chars() {
            p.key(&k.to_string(), 40, 3);
        }
        assert!(cells_text(&p.render(60, 2)[0]).contains("no match"));
    }

    #[test]
    fn recent_list_keeps_bases_newest_first() {
        let mut r: Vec<String> = vec!["fire".into(), "+1".into()];
        note_recent(&mut r, "+1::skin-tone-3");
        assert_eq!(r, vec!["+1", "fire"]);
        note_recent(&mut r, "tada");
        assert_eq!(r, vec!["tada", "+1", "fire"]);
        for i in 0..RECENT_MAX {
            note_recent(&mut r, &format!("e{i}"));
        }
        assert_eq!(r.len(), RECENT_MAX);
    }
}
