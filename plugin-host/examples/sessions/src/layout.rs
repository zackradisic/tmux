//! tmux's layout string, parsed, and drawn as a small map of the
//! window's panes for the preview column.
//!
//! The grammar (layout-custom.c): an optional `xxxx,` checksum, then a
//! cell `WxH,X,Y` followed by `,id` for a pane, `{cell,cell,..}` for a
//! left-right split or `[cell,cell,..]` for a top-bottom one. A `,N`
//! followed by `x` is the next sibling's size, not an id. This fork may
//! append `>...` for floats; parsing stops there.

use std::collections::HashMap;

use listkit::styled::{Styled, ST_BOLD, ST_CYAN, ST_DIM};

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// The pane id of a leaf.
    pub pane: Option<u32>,
    pub children: Vec<Cell>,
}

pub fn parse(layout: &str) -> Option<Cell> {
    let b = layout.as_bytes();
    let mut i = 0;
    // The checksum: four hex digits and a comma.
    if b.len() > 5 && b[4] == b',' && b[..4].iter().all(|c| c.is_ascii_hexdigit()) {
        i = 5;
    }
    parse_cell(b, &mut i)
}

fn number(b: &[u8], i: &mut usize) -> Option<u32> {
    let start = *i;
    while *i < b.len() && b[*i].is_ascii_digit() {
        *i += 1;
    }
    std::str::from_utf8(&b[start..*i]).ok()?.parse().ok()
}

fn parse_cell(b: &[u8], i: &mut usize) -> Option<Cell> {
    let w = number(b, i)?;
    if b.get(*i) != Some(&b'x') {
        return None;
    }
    *i += 1;
    let h = number(b, i)?;
    if b.get(*i) != Some(&b',') {
        return None;
    }
    *i += 1;
    let x = number(b, i)?;
    if b.get(*i) != Some(&b',') {
        return None;
    }
    *i += 1;
    let y = number(b, i)?;
    let mut cell = Cell { x, y, w, h, pane: None, children: Vec::new() };
    // `,id` unless the number is a sibling's `WxH`.
    if b.get(*i) == Some(&b',') {
        let saved = *i;
        *i += 1;
        let id = number(b, i);
        if b.get(*i) == Some(&b'x') {
            *i = saved;
        } else {
            cell.pane = id;
        }
    }
    let close = match b.get(*i) {
        Some(b'{') => b'}',
        Some(b'[') => b']',
        _ => return Some(cell),
    };
    loop {
        *i += 1;
        let child = parse_cell(b, i)?;
        cell.children.push(child);
        if b.get(*i) != Some(&b',') {
            break;
        }
    }
    if b.get(*i) != Some(&close) {
        return None;
    }
    *i += 1;
    Some(cell)
}

/// The leaves, in layout order.
pub fn leaves(c: &Cell, out: &mut Vec<Cell>) {
    if c.children.is_empty() {
        out.push(c.clone());
    } else {
        for ch in &c.children {
            leaves(ch, out);
        }
    }
}

const N: u8 = 1;
const S: u8 = 2;
const E: u8 = 4;
const W: u8 = 8;

fn glyph(m: u8) -> char {
    match m {
        0 => ' ',
        x if x == E | W || x == E || x == W => '─',
        x if x == N | S || x == N || x == S => '│',
        x if x == S | E => '┌',
        x if x == S | W => '┐',
        x if x == N | E => '└',
        x if x == N | W => '┘',
        x if x == N | S | E => '├',
        x if x == N | S | W => '┤',
        x if x == E | W | S => '┬',
        x if x == E | W | N => '┴',
        _ => '┼',
    }
}

/// A pane's box in the map: cell columns `x0..=x1`, rows `y0..=y1`.
pub type MapBox = (usize, usize, usize, usize, u32);

/// The panes of the layout as boxes scaled into a `width` x `height`
/// map. Only the panes the caller knows: a float this fork appends to
/// the layout (`..,180x45,10,2,3]<180x45,10,2,3>`) has no label and is
/// not drawn. `None` when the layout does not parse.
pub fn boxes(layout: &str, width: usize, height: usize, labels: &HashMap<u32, String>) -> Option<Vec<MapBox>> {
    let root = parse(layout)?;
    if width < 4 || height < 2 || root.w == 0 || root.h == 0 {
        return None;
    }
    let mut lv = Vec::new();
    leaves(&root, &mut lv);
    let sx = |x: u32| ((x as usize) * (width - 1) / (root.w as usize)).min(width - 1);
    let sy = |y: u32| ((y as usize) * (height - 1) / (root.h as usize)).min(height - 1);
    let mut out = Vec::new();
    for l in &lv {
        let Some(pane) = l.pane.filter(|p| labels.contains_key(p)) else { continue };
        let x0 = sx(l.x);
        let x1 = sx(l.x + l.w).max(x0 + 1);
        let y0 = sy(l.y);
        let y1 = sy(l.y + l.h).max(y0 + 1);
        out.push((x0, y0, x1, y1, pane));
    }
    Some(out)
}

/// The pane whose box holds cell (x, y) of the map, borders included.
pub fn pane_at(layout: &str, width: usize, height: usize, labels: &HashMap<u32, String>, x: usize, y: usize) -> Option<u32> {
    boxes(layout, width, height, labels)?
        .into_iter()
        .find(|&(x0, y0, x1, y1, _)| x >= x0 && x <= x1 && y >= y0 && y <= y1)
        .map(|b| b.4)
}

/// The window's panes as boxes in a `width` x `height` map, each
/// labelled with its index (`labels`), the `highlight` pane's box
/// bright. Borders between neighbours are shared. `None` when the
/// layout does not parse.
pub fn strip(
    layout: &str,
    width: usize,
    height: usize,
    labels: &HashMap<u32, String>,
    highlight: Option<u32>,
) -> Option<Vec<Vec<Styled>>> {
    let boxes = boxes(layout, width, height, labels)?;
    let mut mask = vec![vec![0u8; width]; height];
    let mut hi = vec![vec![false; width]; height];
    for &(x0, y0, x1, y1, pane) in &boxes {
        let bright = Some(pane) == highlight;
        for x in x0..=x1 {
            if x > x0 {
                mask[y0][x] |= W;
                mask[y1][x] |= W;
            }
            if x < x1 {
                mask[y0][x] |= E;
                mask[y1][x] |= E;
            }
            if bright {
                hi[y0][x] = true;
                hi[y1][x] = true;
            }
        }
        for y in y0..=y1 {
            if y > y0 {
                mask[y][x0] |= N;
                mask[y][x1] |= N;
            }
            if y < y1 {
                mask[y][x0] |= S;
                mask[y][x1] |= S;
            }
            if bright {
                hi[y][x0] = true;
                hi[y][x1] = true;
            }
        }
    }
    let mut out: Vec<Vec<Styled>> = (0..height)
        .map(|y| {
            (0..width)
                .map(|x| {
                    let st = if hi[y][x] { ST_BOLD | ST_CYAN } else { ST_DIM };
                    (glyph(mask[y][x]), st)
                })
                .collect()
        })
        .collect();
    // Labels on the top border, just inside the corner.
    for &(x0, y0, x1, _, pane) in &boxes {
        let Some(label) = labels.get(&pane) else { continue };
        let bright = Some(pane) == highlight;
        let st = if bright { ST_BOLD | ST_CYAN } else { 0 };
        let room = x1.saturating_sub(x0 + 1);
        for (k, ch) in label.chars().take(room).enumerate() {
            out[y0][x0 + 1 + k] = (ch, st);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_splits_and_sibling_rule() {
        let l = "b1d2,200x50,0,0{100x50,0,0,1,99x50,101,0[99x25,101,0,2,99x24,101,26,3]}";
        let c = parse(l).unwrap();
        assert_eq!((c.w, c.h, c.x, c.y), (200, 50, 0, 0));
        assert_eq!(c.children.len(), 2);
        assert_eq!(c.children[0].pane, Some(1));
        let right = &c.children[1];
        assert_eq!(right.pane, None);
        assert_eq!(right.children.iter().map(|c| c.pane).collect::<Vec<_>>(), vec![Some(2), Some(3)]);
        let mut lv = Vec::new();
        leaves(&c, &mut lv);
        assert_eq!(lv.len(), 3);
        // A single pane, and the float suffix this fork adds.
        assert_eq!(parse("80x24,0,0,5").unwrap().pane, Some(5));
        assert_eq!(parse("80x24,0,0,5>stuff").unwrap().pane, Some(5));
        assert!(parse("garbage").is_none());
    }

    #[test]
    fn strip_draws_shared_borders() {
        let l = "200x50,0,0{100x50,0,0,1,99x50,101,0,2}";
        let labels: HashMap<u32, String> = [(1, "0".to_string()), (2, "1".to_string())].into_iter().collect();
        // The float cell (pane 9) has no label and is left out.
        let l = "200x50,0,0{100x50,0,0,1,99x50,101,0,2,180x45,10,2,9}<180x45,10,2,9>";
        let s = strip(l, 21, 3, &labels, Some(2)).unwrap();
        let text: Vec<String> = s.iter().map(|l| l.iter().map(|c| c.0).collect()).collect();
        assert_eq!(text[0], "┌0────────┬1────────┐");
        assert_eq!(text[1], "│         │         │");
        assert_eq!(text[2], "└─────────┴─────────┘");
        // The highlighted box is bright; the other dim.
        assert!(s[1][20].1 & ST_CYAN != 0);
        assert!(s[1][0].1 & ST_DIM != 0);
    }
}
