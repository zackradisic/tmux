//! The engine's screen, as one ANSI string: the consumer sends it with
//! `mode_write` and sets the returned preview rect with `mode_preview`.
//! Nothing here calls the host.

use crate::engine::Engine;
use crate::lines::Line;
use crate::node::{NodeKind, Preview};
use crate::query::{self, typing_token};
use crate::styled::{cells_text, emit_cells, fit_cells, markdown_lines, plain_cells, Styled, ST_BOLD, ST_DIM};
use crate::text::{clip, keyname, pretty_key};

use tmux_plugin_sdk::prelude::PreviewRect;

/// The frame the row area is drawn with: how a row at a depth is
/// prefixed, and the glyphs for an open and a closed group.
const OPEN: char = '▿';
const CLOSED: char = '▹';

impl Engine {
    /// Draw everything. Returns the bytes and the live preview rect, if
    /// the highlighted node has a pane and no card is over it.
    pub fn render(&mut self) -> (String, Option<PreviewRect>) {
        self.note_render();
        let w = self.width as usize;
        let h = self.height as usize;
        let list_w = self.list_w();
        let mut out = String::from("\x1b[2J\x1b[H");

        // Title line.
        out.push_str(&format!("\x1b[1;1H\x1b[1m {}\x1b[0m \x1b[2m{}\x1b[0m", self.title, self.header_tag));
        let query = self.query();
        if query.has_filters() {
            // The active tokens, at the right edge of the header, so a
            // narrowed list never passes for the whole.
            let t = query.tokens();
            let col = list_w.saturating_sub(t.chars().count() + 1).max(1);
            out.push_str(&format!("\x1b[1;{col}H\x1b[33m{}\x1b[0m", clip(&t, list_w.saturating_sub(col))));
        }
        // The search / prompt line.
        if let Some(p) = &self.prompt {
            out.push_str(&format!("\x1b[2;1H  \x1b[2m{}\x1b[0m {}\x1b[7m \x1b[0m", clip(&p.label, 12), p.buf));
        } else if self.filtering {
            out.push_str(&format!("\x1b[2;1H  \x1b[2msearch\x1b[0m {}\x1b[7m \x1b[0m", self.filter));
        } else if self.filter.is_empty() {
            let fk = keyname(self.keys.key_of("filter")).to_string();
            out.push_str(&format!("\x1b[2;1H  \x1b[2msearch\x1b[0m \x1b[2m({fk} to search)\x1b[0m"));
        } else {
            out.push_str(&format!("\x1b[2;1H  \x1b[2msearch\x1b[0m {}", self.filter));
        }
        out.push_str(&format!("\x1b[3;1H  \x1b[2m{}\x1b[0m", "─".repeat(list_w.saturating_sub(2))));

        // The list.
        let list_h = self.list_h();
        if self.view.is_empty() {
            out.push_str(&format!("\x1b[4;1H  \x1b[2m{}\x1b[0m", clip(&self.empty_text, list_w.saturating_sub(3))));
        } else {
            for (line_i, li) in (self.top..(self.top + list_h).min(self.lines.len())).enumerate() {
                let row = 4 + line_i;
                match &self.lines[li] {
                    Line::Spacer => {}
                    Line::Header { id, .. } => {
                        // A header is a place the cursor can be (h goes up
                        // to it, j/k walk its level); folded, it says how
                        // many rows it hides.
                        let n = &self.nodes[self.view[*id]];
                        let cur = *id == self.sel;
                        let open = self.is_expanded(n);
                        let mut cells: Vec<Styled> = match n.header_glyph {
                            Some(g) => vec![(g, ST_BOLD), (' ', 0)],
                            None => std::iter::repeat((' ', 0)).take(n.indent_cells()).collect(),
                        };
                        if !open {
                            cells.push((CLOSED, ST_DIM));
                            cells.push((' ', 0));
                        }
                        let bold = if n.header_glyph.is_some() { ST_BOLD } else { 0 };
                        cells.extend(n.left.iter().map(|&(c, s)| (c, s | bold)));
                        if !open {
                            let count = self.descendants(self.view[*id]);
                            cells.extend(plain_cells(&format!(" ({count})"), ST_DIM));
                        }
                        if !n.right.is_empty() {
                            cells.push((' ', 0));
                            cells.push((' ', 0));
                            cells.extend(n.right.iter().cloned());
                        }
                        let cells = fit_cells(&cells, list_w.saturating_sub(1));
                        if cur {
                            // The cursor takes the first cell (the glyph
                            // or the indent) and keeps a space after it.
                            let width = list_w.saturating_sub(1);
                            let sgr = if self.preview_focus { "2;7" } else { "7" };
                            let rest: String = cells_text(&cells).chars().skip(1).collect();
                            let rest = if rest.starts_with(' ') { rest } else { format!(" {rest}") };
                            out.push_str(&format!("\x1b[{row};1H\x1b[{sgr}m▸{rest:<width$}\x1b[0m"));
                        } else {
                            out.push_str(&format!("\x1b[{row};1H{}", emit_cells(&cells, false)));
                        }
                    }
                    Line::Item(vpos) => {
                        let vpos = *vpos;
                        let n = &self.nodes[self.view[vpos]];
                        let cur = vpos == self.sel;
                        let marked = self.marked.contains(&n.key);
                        let dim = n.dim || !self.matched.get(vpos).copied().unwrap_or(true);
                        // prefix: marker, a space, the indent, a glyph
                        // for a group.
                        let mut cells: Vec<Styled> = vec![(if cur { '▸' } else { ' ' }, 0), (' ', 0)];
                        cells.extend(std::iter::repeat((' ', 0)).take(n.indent_cells()));
                        let mut folded_count: Vec<Styled> = Vec::new();
                        if let NodeKind::Group { .. } = n.kind {
                            let open = self.is_expanded(n);
                            let g = if open { OPEN } else { CLOSED };
                            cells.push((g, ST_DIM));
                            cells.push((' ', 0));
                            if !open {
                                // A folded group says how much it holds.
                                let count = self.descendants(self.view[vpos]);
                                folded_count = plain_cells(&format!(" ({count})"), ST_DIM);
                            }
                        }
                        let prefix = cells.len();
                        let right_w = n.right.len();
                        let label_w = list_w.saturating_sub(1).saturating_sub(prefix + 2 + right_w).max(8);
                        let mut left_cells = n.left.clone();
                        left_cells.extend(folded_count);
                        let left = fit_cells(&left_cells, label_w);
                        let pad = label_w.saturating_sub(left.len());
                        cells.extend(left);
                        cells.extend(std::iter::repeat((' ', 0)).take(pad + 2));
                        cells.extend(n.right.iter().cloned());
                        let cells = fit_cells(&cells, list_w.saturating_sub(1));
                        let width = list_w.saturating_sub(1);
                        if cur {
                            // The cursor row: reverse video across the
                            // width. Dimmed while the preview has the
                            // keyboard, so the bright thing on screen is
                            // where keys go.
                            let sgr = if self.preview_focus { "2;7" } else { "7" };
                            out.push_str(&format!("\x1b[{row};1H\x1b[{sgr}m{:<width$}\x1b[0m", cells_text(&cells)));
                        } else if marked {
                            out.push_str(&format!("\x1b[{row};1H\x1b[97;44m{:<width$}\x1b[0m", cells_text(&cells)));
                        } else if dim {
                            out.push_str(&format!("\x1b[{row};1H\x1b[2m{}\x1b[0m", cells_text(&cells)));
                        } else {
                            out.push_str(&format!("\x1b[{row};1H{}", emit_cells(&cells, false)));
                        }
                        if n.here {
                            let g = if cur { "▸" } else { "▎" };
                            out.push_str(&format!("\x1b[{row};1H\x1b[1;94m{g}\x1b[0m"));
                        }
                    }
                }
            }
        }

        // The separator lights up while the preview has the keyboard.
        let sep = if self.preview_focus || self.separator_lit { "\x1b[1;36m┃" } else { "\x1b[2m│" };
        for r in 1..=h {
            out.push_str(&format!("\x1b[{r};{c}H{sep}\x1b[0m", c = list_w + 1));
        }

        if let Some(s) = &self.status {
            out.push_str(&format!("\x1b[{r};1H\x1b[36m  {}\x1b[0m", clip(s, list_w.saturating_sub(4)), r = h.saturating_sub(1)));
        }
        let footer = if self.show_help {
            "? back · Esc back".to_string()
        } else if self.preview_focus {
            let to = self.selected().map(|n| cells_text(&n.left)).unwrap_or_default();
            format!(
                "typing into {} · keys go to its pane · {} back to list (or select-pane -L)",
                clip(to.trim(), 16),
                pretty_key(self.keys.key_of("unfocus"))
            )
        } else if self.prompt.is_some() {
            "type · Enter accept · Esc cancel".to_string()
        } else if self.filtering {
            let sig: Vec<String> = self.sigils.iter().map(|s| format!("{}{}", s.ch, s.noun)).collect();
            format!("type to search · {} · Esc unfocus", sig.join(" "))
        } else if self.footer.is_empty() {
            format!(
                "j/k move · {} open · {} search · ? help · q/{} close",
                keyname(self.keys.key_of("activate")),
                keyname(self.keys.key_of("filter")),
                keyname(self.keys.key_of("close")),
            )
        } else {
            self.footer.clone()
        };
        out.push_str(&format!("\x1b[{h};1H  \x1b[2m{}\x1b[0m", clip(&footer, list_w.saturating_sub(4))));
        self.draw_completions(&mut out, list_w);

        // The preview column.
        let rect = self.preview_rect();
        if rect.is_some() {
            // The consumer's lines above the blit.
            if let Some(lines) = self.preview_header() {
                let x = list_w + 2;
                let pw = w.saturating_sub(list_w + 2);
                for (i, line) in lines.iter().enumerate() {
                    let cells = fit_cells(line, pw);
                    out.push_str(&format!("\x1b[{};{x}H{}", i + 1, emit_cells(&cells, false)));
                }
            }
        } else {
            let x = list_w + 2;
            let pw = w.saturating_sub(list_w + 2);
            let ph = h.saturating_sub(1);
            if self.show_help {
                self.draw_help(&mut out, x, pw, ph);
            } else if let Some(n) = self.selected() {
                let lines: Option<Vec<Vec<Styled>>> = match &n.preview {
                    Preview::Text(l) => Some(l.clone()),
                    Preview::Markdown(s) => Some(markdown_lines(s, pw, 0)),
                    _ => None,
                };
                if let Some(lines) = lines {
                    let top = self.preview_top.min(lines.len().saturating_sub(ph));
                    self.preview_top = top;
                    for (i, line) in lines.iter().skip(top).take(ph).enumerate() {
                        let cells = fit_cells(line, pw);
                        out.push_str(&format!("\x1b[{};{x}H{}", i + 1, emit_cells(&cells, false)));
                    }
                }
            }
        }
        debug_assert!(out.len() < 200 * 1024, "frame too large for mode_write");
        (out, rect)
    }

    /// The dropdown: under the search box, aligned with the token being
    /// typed, one line per value with its row count; the highlighted one
    /// in reverse. Drawn last, over the top of the list.
    fn draw_completions(&self, out: &mut String, list_w: usize) {
        if !self.filtering || self.completions.is_empty() {
            return;
        }
        let chars = self.sigil_chars();
        let Some((sigil, partial)) = typing_token(&self.filter, &chars) else { return };
        // "  search " is nine cells; the sigil sits where the token starts.
        let col = query::dropdown_col(&self.filter, partial, 10);
        let wmax = self.completions.iter().map(|(v, _)| v.chars().count()).max().unwrap_or(0);
        let width = (wmax + 8).min(list_w.saturating_sub(col + 1)).max(4);
        for (i, (v, n)) in self.completions.iter().enumerate() {
            let row = 3 + i;
            if row >= self.height as usize - 1 {
                break;
            }
            let line = clip(&format!(" {sigil}{v}  {n}"), width);
            let sgr = if i == self.completion_idx { "7" } else { "48;5;238" };
            out.push_str(&format!("\x1b[{row};{col}H\x1b[{sgr}m{line:<width$}\x1b[0m"));
        }
    }

    /// The quick reference, in the preview column: every key by area,
    /// from the table, then the search box's sigils.
    fn draw_help(&self, out: &mut String, x: usize, pw: usize, ph: usize) {
        let mut lines = self.keys.help();
        if !self.sigils.is_empty() {
            // The tokens go with the search box's own keys, right after
            // its title, so a long key list does not push them below the
            // card's bottom; a table with no such section gets them as a
            // section of their own.
            let mut toks: Vec<(String, String)> = Vec::new();
            for s in &self.sigils {
                let mut k = format!("{}{}", s.ch, s.noun);
                if let Some(nk) = s.narrow_key {
                    k = format!("{k} / {nk}");
                }
                toks.push((k, s.help.to_string()));
            }
            toks.push(("\\sigil".into(), "the character as a plain word".into()));
            match lines.iter().position(|(k, _)| k == "\0search box") {
                Some(at) => {
                    for (i, t) in toks.into_iter().enumerate() {
                        lines.insert(at + 1 + i, t);
                    }
                }
                None => {
                    lines.push((String::new(), String::new()));
                    lines.push(("\0search box tokens".into(), String::new()));
                    lines.extend(toks);
                }
            }
        }
        out.push_str(&format!("\x1b[1;{x}H\x1b[1;36m{}\x1b[0m", clip("quick reference · ? or Esc back", pw)));
        let key_w = 12;
        let body_w = pw.saturating_sub(key_w + 1).max(8);
        let mut row = 2;
        for (key, what) in &lines {
            if row > ph {
                break;
            }
            if let Some(title) = key.strip_prefix('\0') {
                out.push_str(&format!("\x1b[{row};{x}H\x1b[1;2m{}\x1b[0m", clip(title, pw)));
            } else if !key.is_empty() {
                out.push_str(&format!("\x1b[{row};{x}H\x1b[33m{:<key_w$}\x1b[0m {}", clip(key, key_w), clip(what, body_w)));
            }
            row += 1;
        }
    }
}

/// A styled one-liner for a node's text: a convenience for consumers.
pub fn cells(text: &str, style: u16) -> Vec<Styled> {
    plain_cells(text, style)
}
