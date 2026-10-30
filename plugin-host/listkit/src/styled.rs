//! A line as styled cells, and light Markdown rendered into them.
//!
//! The renderer is hand-rolled: no Markdown crate is in the offline
//! registry, and what agents write is a small, regular subset -
//! headings, emphasis, inline and fenced code, lists, quotes, rules,
//! links and pipe tables.

use std::cell::RefCell;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::text::clip;

pub const ST_BOLD: u16 = 1;
pub const ST_ITALIC: u16 = 2;
pub const ST_CODE: u16 = 4;
pub const ST_UNDER: u16 = 8;
pub const ST_DIM: u16 = 16;
pub const ST_HIT: u16 = 32;
pub const ST_CYAN: u16 = 64;
pub const ST_RED: u16 = 128;
pub const ST_GREEN: u16 = 256;
pub const ST_MAGENTA: u16 = 512;
/// Reverse video: the cell's colour as its background.
pub const ST_INVERT: u16 = 1024;
/// The bright ANSI variant of the colour.
pub const ST_BRIGHT: u16 = 2048;
/// Orange (256-colour 208): between green and red, for "busy, but
/// not by itself".
pub const ST_ORANGE: u16 = 4096;

/// One cell of a rendered line: a character and its style bits.
///
/// A cell is one terminal column. A glyph two columns wide (an emoji,
/// a CJK character) takes its own cell and a [`FILL`] cell after it,
/// so `len()` stays the column count. A glyph of several scalars (a
/// ZWJ sequence, an emoji with FE0F or a skin tone, a flag's two
/// regional indicators) is one cell too: a registered grapheme (see
/// [`grapheme_cell`]) that [`emit_cells`] expands back to its text; the
/// terminal (tmux's `screen_write_combine`) draws the sequence as one
/// glyph. [`push_glyph`] builds such cells from text.
pub type Styled = (char, u16);

/// The second column of a wide glyph: drawn as nothing.
pub const FILL: char = '\u{10FFFF}';

/// Registered graphemes are addressed from plane 16's private use area,
/// which no font and no text uses.
const GRAPHEME_BASE: u32 = 0x10_0000;
const GRAPHEME_MAX: u32 = 0xFFFD;

thread_local! {
    static GRAPHEMES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A multi-scalar grapheme as one cell's char. The same text always
/// gives the same char; when the registry is full (unlikely: 65533
/// distinct sequences) the first scalar stands in.
pub fn grapheme_cell(text: &str) -> char {
    let mut chars = text.chars();
    let first = chars.next().unwrap_or(' ');
    if chars.next().is_none() {
        return first;
    }
    GRAPHEMES.with(|g| {
        let mut g = g.borrow_mut();
        let i = match g.iter().position(|s| s == text) {
            Some(i) => i,
            None if g.len() as u32 >= GRAPHEME_MAX => return first,
            None => {
                g.push(text.to_string());
                g.len() - 1
            }
        };
        char::from_u32(GRAPHEME_BASE + i as u32).unwrap_or(first)
    })
}

/// The text a registered grapheme cell stands for.
pub fn grapheme_text(c: char) -> Option<String> {
    let u = c as u32;
    if !(GRAPHEME_BASE..GRAPHEME_BASE + GRAPHEME_MAX).contains(&u) {
        return None;
    }
    GRAPHEMES.with(|g| g.borrow().get((u - GRAPHEME_BASE) as usize).cloned())
}

/// Append `text` as cells, with wide and multi-scalar glyphs as the cell
/// model wants them (see [`Styled`]). Control characters are dropped.
pub fn push_glyph(out: &mut Vec<Styled>, text: &str, style: u16) {
    let chars: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
    let mut i = 0;
    while i < chars.len() {
        let mut j = i + 1;
        // Everything that attaches to the base: variation selectors,
        // skin tones, keycap, combining marks, a ZWJ and what follows it,
        // a second regional indicator.
        while j < chars.len() {
            let c = chars[j];
            if c == '\u{200D}' {
                j += 2;
                continue;
            }
            let attaches = matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}')
                || c.width() == Some(0)
                || (is_regional(c) && j == i + 1 && is_regional(chars[i]));
            if !attaches {
                break;
            }
            j += 1;
        }
        let j = j.min(chars.len());
        let s: String = chars[i..j].iter().collect();
        let width = if j - i > 1 && !s.chars().any(|c| c.width().unwrap_or(0) > 0) {
            // Only combining pieces (no base): shown as is, one column.
            1
        } else {
            s.width().clamp(1, 2)
        };
        out.push((grapheme_cell(&s), style));
        if width == 2 {
            out.push((FILL, style));
        }
        i = j;
    }
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// `text` as cells (as [`plain_cells`]) with the cell model's wide and
/// multi-scalar glyphs.
pub fn glyph_cells(text: &str, style: u16) -> Vec<Styled> {
    let mut out = Vec::with_capacity(text.len());
    push_glyph(&mut out, text, style);
    out
}

/// The visible text of a line.
pub fn cells_text(cells: &[Styled]) -> String {
    let mut out = String::with_capacity(cells.len());
    for &(c, _) in cells {
        if c == FILL {
            continue;
        }
        match grapheme_text(c) {
            Some(s) => out.push_str(&s),
            None => out.push(c),
        }
    }
    out
}

/// The line cut to `width` cells, an ellipsis at the end when something
/// was cut, in the last cell's style.
pub fn fit_cells(cells: &[Styled], width: usize) -> Vec<Styled> {
    if cells.len() <= width {
        return cells.to_vec();
    }
    let mut keep = width.saturating_sub(1);
    // Never cut a wide glyph from its second column.
    if keep > 0 && cells.get(keep).is_some_and(|c| c.0 == FILL) {
        keep -= 1;
    }
    let mut out: Vec<Styled> = cells[..keep].to_vec();
    if width > 0 {
        out.push(('…', cells.last().map(|c| c.1).unwrap_or(0)));
    }
    out
}

/// Plain text as cells, one style throughout.
pub fn plain_cells(text: &str, style: u16) -> Vec<Styled> {
    text.chars().filter(|c| !c.is_control() || *c == '\n').map(|c| (c, style)).collect()
}

/// `text` clipped to `width` as plain cells: a one-line convenience.
pub fn clipped_cells(text: &str, width: usize, style: u16) -> Vec<Styled> {
    plain_cells(&clip(text, width), style)
}

/// A block of Markdown as wrapped, styled lines. Handles what agents
/// actually write: `#` headings, `**bold**`, `*italic*`, `` `code` ``,
/// fenced code blocks, `-`/`*`/`1.` lists, `>` quotes, `---` rules and
/// `[text](url)` links (the text, underlined). `base` is OR'd into every
/// cell (a prompt is bold throughout).
pub fn markdown_lines(text: &str, width: usize, base: u16) -> Vec<Vec<Styled>> {
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
fn table_lines(rows: &[&str], sep: &str, width: usize, base: u16) -> Vec<Vec<Styled>> {
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
pub fn inline_cells(text: &str, base: u16) -> Vec<Styled> {
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
pub fn wrap_cells(cells: &[Styled], width: usize) -> Vec<Vec<Styled>> {
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

pub fn hard_wrap(cells: &[Styled], width: usize) -> Vec<Vec<Styled>> {
    let width = width.max(1);
    if cells.is_empty() {
        return vec![Vec::new()];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        let mut j = (i + width).min(cells.len());
        // A wide glyph's two cells stay on one line.
        if j < cells.len() && j > i + 1 && cells[j].0 == FILL {
            j -= 1;
        }
        out.push(cells[i..j].to_vec());
        i = j;
    }
    out
}

/// Mark every occurrence of a term in the line (case-insensitively, on
/// the visible text) with the hit bit. Returns whether any was marked.
pub fn mark_hits(line: &mut [Styled], lower_terms: &[String]) -> bool {
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
pub fn emit_cells(line: &[Styled], current: bool) -> String {
    let mut out = String::with_capacity(line.len() + 16);
    let mut cur: Option<u16> = None;
    for &(c, st) in line {
        if c == FILL {
            continue;
        }
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
            if st & ST_INVERT != 0 {
                out.push_str(";7");
            }
            let bright = st & ST_BRIGHT != 0;
            if st & ST_HIT != 0 {
                out.push_str(if current { ";1;30;103" } else { ";30;43" });
            } else if st & ST_CODE != 0 {
                out.push_str(if bright { ";93" } else { ";33" });
            } else if st & ST_CYAN != 0 {
                out.push_str(if bright { ";96" } else { ";36" });
            } else if st & ST_RED != 0 {
                out.push_str(if bright { ";91" } else { ";31" });
            } else if st & ST_GREEN != 0 {
                out.push_str(if bright { ";92" } else { ";32" });
            } else if st & ST_MAGENTA != 0 {
                out.push_str(if bright { ";95" } else { ";35" });
            } else if st & ST_ORANGE != 0 {
                out.push_str(if bright { ";38;5;214" } else { ";38;5;208" });
            }
            out.push('m');
            cur = Some(st);
        }
        match grapheme_text(c) {
            Some(s) => out.push_str(&s),
            None => out.push(c),
        }
    }
    if cur.is_some() {
        out.push_str("\x1b[0m");
    }
    out
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::text::strip_sgr;

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

    #[test]
    fn wide_glyphs_take_two_cells() {
        // One scalar, two columns.
        let c = glyph_cells("a\u{1F64C}b", 0);
        assert_eq!(c.len(), 4);
        assert_eq!(c[2].0, FILL);
        assert_eq!(cells_text(&c), "a\u{1F64C}b");
        assert_eq!(emit_cells(&c, false), "\x1b[0ma\u{1F64C}b\x1b[0m");
        // Several scalars, one cell: a ZWJ sequence, FE0F, a skin tone, a flag.
        for s in ["\u{1F926}\u{200D}\u{2642}\u{FE0F}", "\u{2764}\u{FE0F}", "\u{1F44D}\u{1F3FD}", "\u{1F1E8}\u{1F1E6}", "#\u{FE0F}\u{20E3}"] {
            let c = glyph_cells(s, ST_DIM);
            assert_eq!(c.len(), 2, "{s:?}");
            assert_eq!(c[1].0, FILL);
            assert_eq!(cells_text(&c), s);
            assert!(emit_cells(&c, false).contains(s));
            assert_eq!(grapheme_cell(s), c[0].0);
        }
        // CJK is wide too; a combining mark rides on its base.
        assert_eq!(glyph_cells("\u{65E5}\u{672C}", 0).len(), 4);
        assert_eq!(glyph_cells("e\u{301}x", 0).len(), 2);
        // Wrapping and clipping never split a glyph from its filler.
        let c = glyph_cells("\u{1F64C}\u{1F64C}\u{1F64C}", 0);
        let w = hard_wrap(&c, 3);
        assert_eq!(w.len(), 3);
        assert!(w.iter().all(|l| l.len() == 2));
        let f = fit_cells(&c, 4);
        assert_eq!(cells_text(&f), "\u{1F64C}…");
        let w = wrap_cells(&glyph_cells("\u{1F64C} ab \u{1F64C}\u{1F64C}", 0), 5);
        assert_eq!(w.iter().map(|l| cells_text(l)).collect::<Vec<_>>(), vec!["\u{1F64C} ab", "\u{1F64C}\u{1F64C}"]);
    }
}
