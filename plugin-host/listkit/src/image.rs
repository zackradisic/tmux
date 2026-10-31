//! Images in a plugin mode: the kitty graphics protocol through tmux's
//! `DCS tmux;` passthrough, with Unicode placeholders for the cells.
//!
//! The shape of it (first built for the scp plugin's clipboard preview,
//! now shared with the slack channel view): the plugin never decodes,
//! resizes or base64-encodes pixels. It names a PNG on disk in one
//! transmit (`t=f`), which the terminal reads itself, and draws the block
//! as `U+10EEEE` cells whose 24-bit foreground is the image id and whose
//! two combining diacritics number the row and column. To tmux the block
//! is text (it copies, scrolls and clips like any other line); to kitty
//! or Ghostty it is the image; anywhere else it is blank cells of the
//! block's size.
//!
//! The host seam: a passthrough written into a plugin mode is forwarded
//! by `window_plugin_passthrough` to every terminal showing the mode's
//! window (needs a server at or after `input_set_passthrough`, Oct 2026).

use base64::Engine as _;
use tmux_plugin_sdk::prelude::*;

/// The placeholder cell of the protocol.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Cell height over cell width when the terminal does not say.
pub const DEFAULT_CELL_ASPECT: f64 = 2.0;

/// The diacritics that number a placeholder's row and column: the
/// protocol's `rowcolumn-diacritics.txt`, all 297, in order. A block
/// wider or taller than that clamps to the last one (the terminal then
/// repeats the edge cell), so callers keep blocks under this size.
pub const DIACRITICS: [char; 297] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}',
    '\u{07EF}', '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}',
    '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}', '\u{0F83}', '\u{0F86}', '\u{0F87}',
    '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}', '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}',
    '\u{1A77}', '\u{1A78}', '\u{1A79}', '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}',
    '\u{1B6E}', '\u{1B6F}', '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}',
    '\u{1CD2}', '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}', '\u{1DD1}',
    '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}', '\u{1DD8}', '\u{1DD9}',
    '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}', '\u{1DDF}', '\u{1DE0}', '\u{1DE1}',
    '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}', '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}',
    '\u{20D4}', '\u{20D5}', '\u{20D6}', '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}',
    '\u{20E9}', '\u{20F0}', '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}',
    '\u{2DE3}', '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}', '\u{2DF2}',
    '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}', '\u{2DF9}', '\u{2DFA}',
    '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}', '\u{A66F}', '\u{A67C}', '\u{A67D}',
    '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}', '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}',
    '\u{A8E6}', '\u{A8E7}', '\u{A8E8}', '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}',
    '\u{A8EE}', '\u{A8EF}', '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}',
    '\u{AAB8}', '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}', '\u{1D187}',
    '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}', '\u{1D242}', '\u{1D243}',
    '\u{1D244}',
];

/// The widest or tallest block the diacritics can number.
pub const MAX_BLOCK: u32 = DIACRITICS.len() as u32;

/// One row of the placeholder block: the image id as the foreground
/// colour, then a cell per column carrying its row and column. The SGR
/// is reset at the end, so the row can be written into any line.
pub fn placeholder_row(id: u32, row: u32, cols: u32) -> String {
    let mut s = format!("\x1b[38;2;{};{};{}m", (id >> 16) & 255, (id >> 8) & 255, id & 255);
    let r = DIACRITICS[(row as usize).min(DIACRITICS.len() - 1)];
    for col in 0..cols {
        s.push(PLACEHOLDER);
        s.push(r);
        s.push(DIACRITICS[(col as usize).min(DIACRITICS.len() - 1)]);
    }
    s.push_str("\x1b[0m");
    s
}

/// An escape sequence for the terminal, wrapped so tmux hands it on
/// instead of reading it: `DCS tmux; <seq with every ESC doubled> ST`.
pub fn passthrough(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

/// The transmit by file: the terminal is on this machine and reads the
/// PNG itself (`t=f`), so the image never passes through tmux; one
/// sequence carries the path (base64) and the virtual placement of
/// `cols`×`rows` for the placeholders to show.
pub fn kitty_transmit_file(id: u32, path: &str, cols: u32, rows: u32) -> String {
    let p = base64::engine::general_purpose::STANDARD.encode(path.as_bytes());
    passthrough(&format!("\x1b_Ga=T,t=f,f=100,i={id},U=1,c={cols},r={rows},q=2;{p}\x1b\\"))
}

/// The transmit by data: the PNG, base64, in chunks of 4096, the first
/// carrying the image's keys and the last `m=0`; then a virtual
/// placement of `cols`×`rows` for the placeholders to show. For a
/// terminal that cannot read our files.
pub fn kitty_transmit(id: u32, png: &[u8], cols: u32, rows: u32) -> Vec<String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(4096).collect();
    let n = chunks.len();
    let mut out = Vec::with_capacity(n + 1);
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < n { 1 } else { 0 };
        let ctl = if i == 0 { format!("a=t,f=100,i={id},q=2,m={more}") } else { format!("m={more}") };
        let data = std::str::from_utf8(chunk).unwrap_or("");
        out.push(passthrough(&format!("\x1b_G{ctl};{data}\x1b\\")));
    }
    out.push(passthrough(&format!("\x1b_Ga=p,U=1,i={id},c={cols},r={rows},q=2\x1b\\")));
    out
}

/// Tell the terminal to forget the image: on close, so a view's worth
/// of pictures does not pile up in it.
pub fn kitty_delete(id: u32) -> String {
    passthrough(&format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\"))
}

/// A PNG's pixel size, from its IHDR (the first chunk, by the spec).
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Some((be(16), be(20)))
}

/// The block for an image of `w`×`h` pixels inside `max_cols`×`max_rows`
/// cells: as many rows as allowed, narrower when the image is tall; as
/// wide as allowed, fewer rows when it is wide. `cell_aspect` is cell
/// height over cell width. Never wider or taller than the diacritics
/// can number.
pub fn preview_fit(w: u32, h: u32, cell_aspect: f64, max_cols: u32, max_rows: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let max_cols = max_cols.clamp(1, MAX_BLOCK);
    let max_rows = max_rows.clamp(1, MAX_BLOCK);
    // Columns per row for the image's shape, in cells.
    let ratio = (w as f64 / h as f64) * cell_aspect;
    let mut rows = max_rows;
    let mut cols = (rows as f64 * ratio).round() as u32;
    if cols > max_cols {
        cols = max_cols;
        rows = (cols as f64 / ratio).round() as u32;
    }
    (cols.max(1), rows.clamp(1, max_rows))
}

/// Cell height over cell width for a window, from the terminal's report
/// (`window_cell_width` / `window_cell_height`); the default when it has
/// none.
pub fn cell_aspect(window: u32) -> f64 {
    let Ok(line) = format_expand(
        OptionTarget::Window(WindowId(window)),
        "#{window_cell_width}\t#{window_cell_height}",
    ) else {
        return DEFAULT_CELL_ASPECT;
    };
    let mut it = line.split('\t').map(|v| v.trim().parse::<f64>().unwrap_or(0.0));
    match (it.next(), it.next()) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => h / w,
        _ => DEFAULT_CELL_ASPECT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_header() {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&2050u32.to_be_bytes());
        b.extend_from_slice(&1426u32.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert_eq!(png_size(&b), Some((2050, 1426)));
        assert_eq!(png_size(b"not a png at all, nothing here"), None);
    }

    #[test]
    fn fit_shapes() {
        // A screenshot takes every row; the columns follow its shape.
        assert_eq!(preview_fit(2050, 1426, 2.0, 76, 14), (40, 14));
        // A panorama fills the width; rows follow.
        assert_eq!(preview_fit(4000, 400, 2.0, 76, 14), (76, 4));
        // A tall image takes every row and few columns.
        assert_eq!(preview_fit(400, 1600, 2.0, 76, 14), (7, 14));
        assert_eq!(preview_fit(0, 0, 2.0, 76, 14), (1, 1));
        // A ribbon never rounds to zero rows.
        assert_eq!(preview_fit(320, 10, 2.0, 80, 12), (80, 1));
        // Nothing wider than the diacritics can number.
        assert_eq!(preview_fit(10_000, 10, 2.0, 1000, 12).0, MAX_BLOCK);
    }

    #[test]
    fn diacritics_are_the_protocol_list() {
        assert_eq!(DIACRITICS.len(), 297);
        assert_eq!(DIACRITICS[0], '\u{0305}');
        assert_eq!(DIACRITICS[79], '\u{06E2}');
        assert_eq!(DIACRITICS[296], '\u{1D244}');
        let mut sorted = DIACRITICS.to_vec();
        sorted.dedup();
        assert_eq!(sorted.len(), 297, "no diacritic repeats");
    }

    #[test]
    fn placeholder_cells_carry_id_row_and_column() {
        let row = placeholder_row(0x0a0b0c, 1, 2);
        assert!(row.starts_with("\x1b[38;2;10;11;12m"));
        let cells: Vec<char> = row.trim_start_matches("\x1b[38;2;10;11;12m").trim_end_matches("\x1b[0m").chars().collect();
        assert_eq!(cells, vec![PLACEHOLDER, '\u{030D}', '\u{0305}', PLACEHOLDER, '\u{030D}', '\u{030D}']);
    }

    #[test]
    fn passthrough_doubles_escapes() {
        assert_eq!(passthrough("\x1b_Gx\x1b\\"), "\x1bPtmux;\x1b\x1b_Gx\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn transmit_is_chunked_and_placed() {
        let png = vec![7u8; 10_000];
        let seqs = kitty_transmit(5, &png, 30, 10);
        // 10000 bytes -> 13336 base64 chars -> 4 chunks, then the placement.
        assert_eq!(seqs.len(), 5);
        assert!(seqs[0].contains("a=t,f=100,i=5,q=2,m=1;"));
        assert!(seqs[1].starts_with("\x1bPtmux;\x1b\x1b_Gm=1;"));
        assert!(seqs[3].starts_with("\x1bPtmux;\x1b\x1b_Gm=0;"));
        assert!(seqs[4].contains("a=p,U=1,i=5,c=30,r=10"));
        assert!(kitty_delete(5).contains("a=d,d=I,i=5"));
    }

    #[test]
    fn file_transmit_names_the_path() {
        let seq = kitty_transmit_file(9, "/Users/z/.local/share/tmux/plugins/scp/clip-1.png", 40, 14);
        assert!(seq.starts_with("\x1bPtmux;\x1b\x1b_Ga=T,t=f,f=100,i=9,U=1,c=40,r=14,q=2;"));
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(b"/Users/z/.local/share/tmux/plugins/scp/clip-1.png");
        assert!(seq.contains(&b64));
        assert!(seq.ends_with("\x1b\x1b\\\x1b\\"));
    }
}
