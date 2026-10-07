//! OCR of the clipboard image: `tmux ocr <png>` run as a job (Vision,
//! in a child process of the server's own binary; see ocr-darwin.c),
//! its observations put back into lines, and the two-column view that
//! shows them beside the image.
//!
//! The view takes the whole float: a header, the text on the left with
//! its indentation and blank lines rebuilt from the boxes, the image on
//! the right as a fresh kitty placement sized to the column. Enter puts
//! the text on the clipboard (`load-buffer -w`), `p` pastes it into the
//! pane the form was opened from, `s` makes it the clipboard tab's text
//! so it can go to a host, `c` runs again with language correction
//! toggled, `j`/`k` scroll, Esc goes back to the form.

use tmux_plugin_sdk::prelude::*;

use crate::Preview;

/// One observation as `tmux ocr` prints it: a box normalised to the
/// image with its origin at the bottom left, as Vision reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Obs {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub conf: f32,
    pub text: String,
}

/// `x y w h conf <TAB> text`, one per line; anything else is skipped.
pub fn parse_obs(out: &str) -> Vec<Obs> {
    out.lines()
        .filter_map(|line| {
            let (nums, text) = line.split_once('\t')?;
            let mut it = nums.split_whitespace().map(|v| v.parse::<f64>().ok());
            let x = it.next()??;
            let y = it.next()??;
            let w = it.next()??;
            let h = it.next()??;
            let conf = it.next()?? as f32;
            Some(Obs { x, y, w, h, conf, text: text.to_string() })
        })
        .collect()
}

fn median(v: &mut Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // The lower middle: with two values (two line pitches, one of them
    // across a blank line) the smaller is the ordinary one.
    Some(v[(v.len() - 1) / 2])
}

/// The observations as lines of text, in pixel space (`img_w`×`img_h`):
/// boxes whose centres sit within half a text height of each other are
/// one row, joined left to right with the gap between them as spaces;
/// a row is indented by its distance from the leftmost row; a vertical
/// gap of more than one line pitch becomes blank lines. All in units of
/// a character width estimated from the longest boxes, so a code
/// screenshot comes back as code.
pub fn assemble(obs: &[Obs], img_w: u32, img_h: u32) -> Vec<String> {
    if obs.is_empty() {
        return Vec::new();
    }
    let (iw, ih) = (img_w.max(1) as f64, img_h.max(1) as f64);
    struct Px {
        left: f64,
        right: f64,
        cy: f64,
        h: f64,
        text: String,
    }
    let mut px: Vec<Px> = obs
        .iter()
        .filter(|o| !o.text.trim().is_empty())
        .map(|o| {
            let top = (1.0 - (o.y + o.h)) * ih;
            let bottom = (1.0 - o.y) * ih;
            Px { left: o.x * iw, right: (o.x + o.w) * iw, cy: (top + bottom) / 2.0, h: bottom - top, text: o.text.clone() }
        })
        .collect();
    if px.is_empty() {
        return Vec::new();
    }
    let mh = median(&mut px.iter().map(|p| p.h).collect()).unwrap_or(10.0).max(1.0);
    // A character's width: from the boxes long enough to average over.
    let mut cws: Vec<f64> = px
        .iter()
        .filter(|p| p.text.chars().count() >= 8)
        .map(|p| (p.right - p.left) / p.text.chars().count() as f64)
        .filter(|&w| w > 0.0)
        .collect();
    let cw = median(&mut cws)
        .or_else(|| {
            let mut all: Vec<f64> =
                px.iter().map(|p| (p.right - p.left) / p.text.chars().count().max(1) as f64).filter(|&w| w > 0.0).collect();
            median(&mut all)
        })
        .unwrap_or(mh * 0.6)
        .max(0.5);

    px.sort_by(|a, b| a.cy.partial_cmp(&b.cy).unwrap_or(std::cmp::Ordering::Equal).then(a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal)));
    // Rows: a new one when the centre leaves the current row's band.
    let mut rows: Vec<Vec<Px>> = Vec::new();
    for p in px {
        match rows.last_mut() {
            Some(row) if (p.cy - row[0].cy).abs() <= mh * 0.5 => row.push(p),
            _ => rows.push(vec![p]),
        }
    }
    for row in rows.iter_mut() {
        row.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal));
    }
    let min_left = rows.iter().map(|r| r[0].left).fold(f64::MAX, f64::min);
    let cys: Vec<f64> = rows.iter().map(|r| r.iter().map(|p| p.cy).sum::<f64>() / r.len() as f64).collect();
    let mut pitches: Vec<f64> = cys.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0.0).collect();
    let pitch = median(&mut pitches).unwrap_or(mh * 1.3).max(mh * 0.8);

    let cells = |d: f64| ((d / cw).round().max(0.0)) as usize;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            let gap = cys[i] - cys[i - 1];
            let blanks = ((gap / pitch).round() as i64 - 1).clamp(0, 3);
            for _ in 0..blanks {
                out.push(String::new());
            }
        }
        let mut line = " ".repeat(cells(row[0].left - min_left));
        let mut prev_right = None;
        for p in row {
            if let Some(r) = prev_right {
                // Boxes that touch or overlap are one word Vision cut in
                // two: no space between them.
                line.push_str(&" ".repeat(cells(p.left - r)));
            }
            line.push_str(p.text.trim_end());
            prev_right = Some(p.right);
        }
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------------------
// the view
// ---------------------------------------------------------------------------

/// The view's state while it is up.
#[derive(Debug)]
pub struct View {
    pub state: State,
    /// Vision's language correction for the run: on like Preview.app by
    /// default; off suits code.
    pub correction: bool,
    /// Which run a late result belongs to.
    pub gen: u64,
    pub scroll: usize,
    pub started_ms: u64,
    /// Three seconds in with no result: say the first run on a machine
    /// is slow.
    pub slow: bool,
    /// The text, written to the data directory (a name relative to it)
    /// once a run is done, for the clipboard, the paste and the send.
    pub text_name: Option<String>,
    /// The image's placement in the right column.
    pub preview: Option<Preview>,
    /// Columns and rows the float has for us (the last size confirmed).
    pub width: u32,
    pub height: u32,
    /// The size last asked of the host. Asked once: the host clamps to
    /// the window and answers with a resize event, and asking again on
    /// that event would never end.
    pub asked: Option<(u32, u32)>,
}

#[derive(Debug)]
pub enum State {
    Running,
    Done { lines: Vec<String>, ms: u64 },
    Failed(String),
}

impl View {
    pub fn new(correction: bool, width: u32, height: u32) -> View {
        View {
            state: State::Running,
            correction,
            gen: 1,
            scroll: 0,
            started_ms: now_ms(),
            slow: false,
            text_name: None,
            preview: None,
            width,
            height,
            asked: None,
        }
    }

    pub fn lines(&self) -> &[String] {
        match &self.state {
            State::Done { lines, .. } => lines,
            _ => &[],
        }
    }

    /// Rows of text the layout shows at once.
    pub fn text_rows(&self) -> usize {
        (self.height as usize).saturating_sub(4).max(1)
    }

    pub fn scroll_by(&mut self, delta: i64) {
        let max = self.lines().len().saturating_sub(self.text_rows());
        self.scroll = (self.scroll as i64 + delta).clamp(0, max as i64) as usize;
    }
}

/// The float the view wants: most of the window, within reason.
pub fn wanted_size(window_w: u32, window_h: u32) -> (u32, u32) {
    let w = window_w.saturating_sub(4).clamp(60, 180);
    let h = window_h.saturating_sub(2).clamp(12, 50);
    (w, h)
}

/// The image column for a float of `width`×`height`: as wide as half
/// the float allows for the image's shape, as tall as the text area.
pub fn image_box(width: u32, height: u32, img_w: u32, img_h: u32, cell_aspect: f64) -> (u32, u32) {
    let max_cols = (width / 2).saturating_sub(3).max(10);
    let max_rows = height.saturating_sub(4).max(3);
    crate::preview_fit(img_w, img_h, cell_aspect, max_cols, max_rows)
}

/// A line clipped to `width` cells with an ellipsis, tabs widened.
fn clip_line(l: &str, width: usize) -> String {
    let clean: String = l.replace('\t', "    ").chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > width {
        let mut out: String = clean.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    } else {
        clean
    }
}

/// `0.3 s`, `12 s`.
fn secs(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} s", ms / 1000)
    }
}

/// The whole screen: header, rule, the text column and the image's
/// placeholder block side by side, the hints. `image` is the block's
/// id, columns and rows once it is placed.
pub fn screen(v: &View, image_label: &str, image: Option<(u32, u32, u32)>) -> String {
    let w = v.width as usize;
    let h = v.height as usize;
    let mut out = String::from("\x1b[2J\x1b[H");
    let status = match &v.state {
        State::Running if v.slow => "recognising… (the first run on a Mac can take a minute)".to_string(),
        State::Running => "recognising…".to_string(),
        State::Done { lines, ms } => format!(
            "{} line{} · {} · correction {}",
            lines.len(),
            if lines.len() == 1 { "" } else { "s" },
            secs(*ms),
            if v.correction { "on" } else { "off" }
        ),
        State::Failed(e) => format!("failed: {e}"),
    };
    let head = format!("  \x1b[1mOCR\x1b[0m  \x1b[2m·\x1b[0m  {image_label}  \x1b[2m·  {status}\x1b[0m");
    out.push_str(&head);
    out.push_str("\r\n");
    out.push_str(&format!("  \x1b[2m{}\x1b[0m\r\n", "─".repeat(w.saturating_sub(4))));

    let (img_cols, img_rows, img_id) = match image {
        Some((id, c, r)) => (c as usize, r as usize, id),
        None => (0, 0, 0),
    };
    let text_w = if img_cols > 0 { w.saturating_sub(img_cols + 6) } else { w.saturating_sub(4) }.max(8);
    let img_col = w.saturating_sub(img_cols + 1).max(1);
    let rows = v.text_rows();
    let lines = v.lines();
    for r in 0..rows {
        let row = 3 + r;
        out.push_str(&format!("\x1b[{row};1H"));
        match &v.state {
            State::Done { .. } => {
                if let Some(l) = lines.get(v.scroll + r) {
                    out.push_str(&format!("  {}", clip_line(l, text_w)));
                } else if lines.is_empty() && r == 0 {
                    out.push_str("  \x1b[2m(no text found)\x1b[0m");
                }
            }
            State::Running if r == 0 => out.push_str("  \x1b[2mrecognising…\x1b[0m"),
            State::Failed(e) if r == 0 => out.push_str(&format!("  \x1b[31m{}\x1b[0m", clip_line(e, text_w))),
            _ => {}
        }
        if img_cols > 0 && r < img_rows {
            out.push_str(&format!("\x1b[{row};{img_col}H{}", crate::placeholder_row(img_id, r as u32, img_cols as u32)));
        }
    }
    let more = if lines.len() > v.scroll + rows { format!("{} more below · ", lines.len() - v.scroll - rows) } else { String::new() };
    let corr = if v.correction { "off" } else { "on" };
    let hint = format!("{more}Enter copy · p paste into pane · s send as text · c correction {corr} · j/k scroll · Esc back");
    out.push_str(&format!("\x1b[{h};1H  \x1b[2m{}\x1b[0m", clip_line(&hint, w.saturating_sub(2))));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 31 boxes `tmux ocr` returned for a 1153×1185 render of 36 lines of
    /// this crate (Menlo, dark background), five of them blank, one split
    /// in two by Vision at the same height.
    const FIXTURE: &str = "\
-0.0000 0.9696 0.3449 0.0270 1.00\tconst TO_PATH: usize = 3;
-0.0000 0.9157 0.4558 0.0270 1.00\t/// Field indices, clipboard tab.
-0.0003 0.8885 0.3042 0.0335 1.00\tconst CLIP: usize = 0;
-0.0000 0.8600 0.3449 0.0287 1.00\tconst CLIP_TO: usize = 1;
-0.0000 0.8347 0.3726 0.0270 1.00\tconst CLIP_PATH: usize = 2;
-0.0000 0.7825 0.4281 0.0236 1.00\t/// The key that switches tabs.
-0.0001 0.7511 0.4024 0.0336 1.00\tconst KIND_KEY: &str = \"C-v\";
-0.0000 0.6998 0.9983 0.0253 1.00\t/// Where the clipboard goes when nothing was ever entered, and the file
-0.0000 0.6745 0.4298 0.0236 1.00\t/// name a directory path gets.
0.0000 0.6526 0.2565 0.0219 1.00\tconst DEFAULT CLIP
0.2530 0.6476 0.4818 0.0287 1.00\t_PATH: &str = \"/tmp/clipboard.png\";
0.0000 0.6189 0.5546 0.0304 1.00\tconst CLIP_NAME: &str = \"clipboard.png\";
-0.0001 0.5935 0.5410 0.0282 1.00\t/// The same for text on the clipboard.
-0.0001 0.5627 0.8077 0.0334 1.00\tconst DEFAULT_CLIP_TEXT_PATH: &str = \"/tmp/clipboard.txt\";
-0.0000 0.5363 0.6239 0.0320 1.00\tconst CLIP_TEXT_NAME: &str = \"clipboard.txt\";
-0.0001 0.5086 0.9585 0.0299 1.00\t/// Lines of clipboard text shown under the form, and how much of the
-0.0001 0.4873 0.3607 0.0243 1.00\t/// file is read for them.
-0.0001 0.4546 0.4855 0.0316 1.00\tconst TEXT_PREVIEW_ROWS: usize = 8;
-0.0000 0.4300 0.6101 0.0287 1.00\tconst TEXT_PREVIEW_BYTES: usize = 16 * 1024;
-0.0000 0.3744 0.7071 0.0270 1.00\t/// Remembered destinations, in the data directory.
-0.0001 0.3465 0.4994 0.0337 1.00\tconst STATE_FILE: &str = \"scp.json\";
-0.0000 0.2921 0.9984 0.0304 1.00\t/// The preview block: at most this many rows, and the form's width less
-0.0002 0.2714 0.2084 0.0250 1.00\t/// the indent.
-0.0000 0.2411 0.4575 0.0270 1.00\tconst PREVIEW_MAX_ROWS: u32 = 14;
-0.0000 0.2142 0.6239 0.0270 1.00\tconst PREVIEW_MAX_COLS: u32 = FORM_WIDTH - 4;
-0.0001 0.1848 0.9707 0.0323 1.00\t/// The preview PNG's longest side, in pixels. Keeps the transmit to a
-0.0000 0.1619 0.6655 0.0236 1.00\t/// few hundred KB however large the screenshot.
-0.0003 0.1313 0.3891 0.0358 1.00\tconst PREVIEW_PX: u32 = 900;
-0.0000 0.1079 0.8180 0.0253 1.00\t/// Cell height over cell width when the window cannot say.
-0.0000 0.0776 0.5113 0.0304 1.00\tconst DEFAULT_CELL_ASPECT: f64 = 2.0;
-0.0001 0.0225 0.5132 0.0316 1.00\t#[derive(Clone, Copy, PartialEq, Eq)l
";

    #[test]
    fn observations_parse() {
        let obs = parse_obs(FIXTURE);
        assert_eq!(obs.len(), 31);
        assert_eq!(obs[0].text, "const TO_PATH: usize = 3;");
        assert!((obs[0].y - 0.9696).abs() < 1e-9);
        assert!(parse_obs("garbage\nmore garbage\t\n").is_empty());
    }

    #[test]
    fn lines_come_back_with_blanks_and_merged_rows() {
        let lines = assemble(&parse_obs(FIXTURE), 1153, 1185);
        // 31 boxes, 30 rows after the merge, six blank lines between the
        // groups: the 36 lines of the source.
        assert_eq!(lines.len(), 36, "{lines:#?}");
        assert_eq!(lines[0], "const TO_PATH: usize = 3;");
        assert_eq!(lines[1], "", "a blank line follows the first group");
        assert_eq!(lines[2], "/// Field indices, clipboard tab.");
        let merged = lines.iter().find(|l| l.contains("DEFAULT CLIP")).expect("the split line is there");
        assert!(merged.contains("CLIP_PATH: &str"), "the two boxes at one height are one line, no space where they touch: {merged:?}");
        assert!(lines.iter().all(|l| !l.starts_with(' ')), "everything starts at the margin: {lines:#?}");
    }

    #[test]
    fn indentation_is_rebuilt() {
        // Three lines in a 1000×300 image, 10 px a character, 50 px a
        // line: the second indented by four characters, the third after
        // a blank line.
        let obs = vec![
            Obs { x: 0.0, y: 0.80, w: 0.10, h: 0.10, conf: 1.0, text: "fn main() {".into() },
            Obs { x: 0.04, y: 0.63, w: 0.10, h: 0.10, conf: 1.0, text: "let x = 1;".into() },
            Obs { x: 0.0, y: 0.30, w: 0.01, h: 0.10, conf: 1.0, text: "}".into() },
        ];
        let lines = assemble(&obs, 1000, 300);
        assert_eq!(lines, vec!["fn main() {", "    let x = 1;", "", "}"]);
    }

    #[test]
    fn image_column_fits_the_float() {
        let (c, r) = image_box(120, 30, 2050, 1426, 2.0);
        assert!(c <= 57 && r <= 26, "{c}x{r}");
        assert!(c > 0 && r > 0);
    }

    #[test]
    fn screen_has_the_header_the_text_and_the_block() {
        let mut v = View::new(true, 110, 12);
        v.state = State::Done { lines: vec!["hello".into(), "world".into()], ms: 300 };
        let s = screen(&v, "Image (10x10)", Some((7, 20, 5)));
        assert!(s.contains("2 lines · 0.3 s · correction on"));
        assert!(s.contains("  hello"));
        assert!(s.contains('\u{10EEEE}'));
        assert!(s.contains("Esc back"));
    }
}
