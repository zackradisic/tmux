//! The display lines of a list, and the scroll rule over them.
//!
//! A list is drawn from `lines`, not from its rows: headers and spacers
//! take lines of their own, so the selection scrolls in "display space"
//! and stays visible however many headers sit between the rows. The
//! consumer builds `lines` from its visible rows; this module keeps the
//! selection on screen.

/// One rendered line. `Header` is a group line at some nesting `level`
/// (0 = a server, 1 = a band under it, ...) with an `id` the consumer
/// resolves (an index into its own names, a band number); `Item` is a
/// selectable row, by its position in the consumer's visible list;
/// `Spacer` is the blank line above a group header that separates one
/// group from the last.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    Spacer,
    Header { level: u8, id: usize },
    Item(usize),
}

impl Line {
    pub fn is_header(&self) -> bool {
        matches!(self, Line::Header { .. })
    }
}

/// The display line of the node at visible position `sel` (a row or a
/// header the cursor moved onto), or 0.
pub fn sel_line(lines: &[Line], sel: usize) -> usize {
    lines
        .iter()
        .position(|l| matches!(l, Line::Item(v) if *v == sel) || matches!(l, Line::Header { id, .. } if *id == sel))
        .unwrap_or(0)
}

/// The first display line of a list window `h` lines tall that shows
/// `sel_line`, starting from `top`: scroll only as far as it takes to
/// bring the selection in. A header directly above the window wastes a
/// line, so it is pulled in too, but never so far that the selection
/// drops out the bottom again: with the cursor on the last row of a
/// window whose top lands right under a band header, pulling the header
/// in used to push the cursor one line below the screen, and the next
/// `j` looked like it did nothing.
///
/// A spacer is never the top line: it would read as an empty row. One
/// that lands there is left above the window (its header still comes
/// in), or skipped when the window started on it.
pub fn scroll_window(mut top: usize, sel_line: usize, h: usize, lines: &[Line]) -> usize {
    let h = h.max(1);
    if sel_line < top {
        top = sel_line;
    } else if sel_line >= top + h {
        top = sel_line + 1 - h;
    }
    if matches!(lines.get(top), Some(Line::Spacer)) && sel_line > top {
        top += 1;
    }
    while top > 0
        && sel_line < top - 1 + h
        && matches!(lines.get(top), Some(Line::Item(_)) | Some(Line::Header { .. }))
        && matches!(lines.get(top - 1), Some(Line::Header { .. }))
    {
        top -= 1;
    }
    top
}

/// The box a picker's size is clamped to, and the fraction of the window
/// (in tenths) its default size fills.
#[derive(Clone, Copy, Debug)]
pub struct SizeBox {
    pub min_w: u32,
    pub min_h: u32,
    pub max_w: u32,
    pub max_h: u32,
    pub fill_tenths: u32,
}

impl Default for SizeBox {
    fn default() -> Self {
        Self { min_w: 72, min_h: 16, max_w: 180, max_h: 54, fill_tenths: 9 }
    }
}

/// Keep a dimension within the window (2 cells spare for the border) and
/// at or above `min`.
pub fn clamp_dim(v: u32, min: u32, avail: u32) -> u32 {
    v.min(avail.saturating_sub(2).max(min)).max(min)
}

/// The default picker size for a window: a fraction of it, clamped to the
/// box.
pub fn default_size(ww: u32, wh: u32, b: &SizeBox) -> (u32, u32) {
    let w = (ww * b.fill_tenths / 10).min(b.max_w);
    let h = (wh * b.fill_tenths / 10).min(b.max_h);
    (clamp_dim(w, b.min_w, ww), clamp_dim(h, b.min_h, wh))
}

#[cfg(test)]
mod scroll_tests {
    use super::*;

    /// Two servers, two bands each, five rows a band; a spacer above the
    /// second server.
    fn lines() -> Vec<Line> {
        let mut v = Vec::new();
        let mut item = 0;
        for (si, _s) in ["alpha", "beta"].iter().enumerate() {
            if si > 0 {
                v.push(Line::Spacer);
            }
            v.push(Line::Header { level: 0, id: si });
            for band in 0..2usize {
                v.push(Line::Header { level: 1, id: band });
                for _ in 0..5 {
                    v.push(Line::Item(item));
                    item += 1;
                }
            }
        }
        v
    }

    #[test]
    fn selection_never_leaves_the_window() {
        let lines = lines();
        for h in 1..=lines.len() + 2 {
            let mut top = 0;
            let mut order: Vec<usize> = (0..20).collect();
            order.extend((0..20).rev());
            for sel in order {
                let sl = sel_line(&lines, sel);
                top = scroll_window(top, sl, h, &lines);
                assert!(sl >= top && sl < top + h, "h={h} sel={sel} line={sl} top={top}");
                assert_ne!(lines[top], Line::Spacer, "h={h} sel={sel} top={top}");
            }
        }
    }

    #[test]
    fn headers_pulled_in_only_when_there_is_room() {
        let lines = lines();
        // Line 0 is alpha's server line, 1 its first header, 2..6 items
        // 0-4, 7 the second header, 8..12 items 5-9, 13 the spacer, 14
        // beta's server line. A header right above the window comes in
        // when the cursor has room to spare.
        assert_eq!(scroll_window(8, sel_line(&lines, 6), 10, &lines), 7);
        assert_eq!(scroll_window(2, sel_line(&lines, 0), 10, &lines), 0);
        // The case that used to lose the cursor: stepping down with the
        // cursor on the window's last line, the top lands on the first item
        // of a band (line 8, under the header at 7). The old code pulled
        // the header in and left the cursor one line below the screen.
        let sl = sel_line(&lines, 9);
        let top = scroll_window(3, sl, 5, &lines);
        assert_eq!(top, 8);
        assert!(sl < top + 5);
    }

    #[test]
    fn spacer_stays_above_the_window() {
        let lines = lines();
        // beta's first item is line 16; a 3-line window ending on it
        // would start on the spacer (13): it starts on the header instead.
        let sl = sel_line(&lines, 10);
        let top = scroll_window(0, sl, 3, &lines);
        assert_eq!(top, 14);
        // Scrolling up onto beta's header pulls the header in, not the
        // spacer.
        let top = scroll_window(16, sl, 10, &lines);
        assert_eq!(top, 14);
    }

    #[test]
    fn default_size_respects_the_box() {
        let b = SizeBox::default();
        assert_eq!(default_size(200, 60, &b), (180, 54));
        assert_eq!(default_size(80, 20, &b), (72, 18));
        assert_eq!(clamp_dim(500, 10, 100), 98);
        assert_eq!(clamp_dim(5, 10, 100), 10);
    }
}
