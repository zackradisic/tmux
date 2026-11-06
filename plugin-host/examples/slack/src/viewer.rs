//! The picture viewer: `z` on a message with a picture (or a click on
//! the picture) in the channel view opens it as big as the window
//! allows, so a screenshot can actually be read. The timeline shows
//! Slack's thumbnail; the viewer fetches the file itself
//! (`images::fetch_full`, up to [`images::MAX_PX_FULL`] a side) and
//! places it once in a float the size of the window, centred, with the
//! file's name and pixel size on the last row. `q`, `Escape`, `z` or
//! `Enter` close it; `o` opens the PNG in the system's viewer. A resize
//! refits and re-sends the picture; closing deletes it from the
//! terminal, so a day's screenshots do not pile up there.

use std::cell::RefCell;
use std::rc::Rc;

use listkit::image::{cell_aspect, kitty_delete, kitty_transmit_file, placeholder_row, preview_fit, MAX_BLOCK};
use tmux_plugin_sdk::prelude::*;

use crate::store::FileRef;
use crate::{images, Shared, NAME};

/// The rows the picture may not use: the last one carries the caption.
const CAPTION_ROWS: u32 = 1;

pub struct Viewer {
    mode: ModeId,
    file: FileRef,
    width: u32,
    height: u32,
    cell_aspect: f64,
    data_root: String,
    /// The pixel size of the PNG on disk, once fetched.
    size: Option<(u32, u32)>,
    /// What the terminal holds: (kitty id, cols, rows).
    placed: Option<(u32, u32, u32)>,
    /// Why there is no picture yet (fetching, or what went wrong).
    status: String,
    /// Tells a fetch that finished late from the viewer it was for.
    gen: u64,
}

thread_local! {
    static STATE: Rc<RefCell<Option<Viewer>>> = Rc::new(RefCell::new(None));
    static GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn state() -> Rc<RefCell<Option<Viewer>>> {
    STATE.with(Rc::clone)
}

/// Whether the mode id is the viewer's.
pub fn owns(mode: Option<i64>) -> bool {
    state().borrow().as_ref().is_some_and(|v| Some(v.mode.0 as i64) == mode)
}

/// Close the viewer, telling the terminal to forget the picture first.
pub fn close() {
    let taken = state().borrow_mut().take();
    if let Some(v) = taken {
        if let Some((id, _, _)) = v.placed {
            let _ = mode_write(v.mode, kitty_delete(id).as_bytes());
        }
        let _ = mode_close(v.mode);
    }
}

/// Open `f` big, over the window the pressing client (or pane) shows.
pub fn open(st: &Shared, f: FileRef, client: Option<u64>, pane: Option<u32>) {
    close();
    let Some(window) = listkit::window_for(client, pane) else {
        let _ = display_message(&format!("{NAME}: no window to show the picture in"));
        return;
    };
    // The window less the float's border, which is what the host gives
    // a float that asks for more; asking for exactly that keeps the
    // rows from wrapping (the host sends no resize for its own clamp).
    let (ww, wh) = resolve_window(WindowId(window)).map(|w| (w.width, w.height)).unwrap_or((80, 24));
    let border = get_option_in(OptionTarget::Window(WindowId(window)), "pane-border-lines")
        .map(|v| v.trim() != "none")
        .unwrap_or(true) as u32;
    let (width, height) = (ww.saturating_sub(2 * border).max(1), wh.saturating_sub(2 * border).max(1));
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
        x: Some(0),
        y: Some(0),
        title: Some(format!("{NAME} {}", f.name)),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("{NAME}: cannot open the picture: {}", e.message));
            return;
        }
    };
    let gen = GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    let v = Viewer {
        mode,
        file: f.clone(),
        width,
        height,
        cell_aspect: cell_aspect(window),
        data_root: fs_root().unwrap_or_default(),
        size: None,
        placed: None,
        status: format!("fetching {}…", f.name),
        gen,
    };
    paint(&v);
    *state().borrow_mut() = Some(v);
    let Some(ident) = st.borrow().ident.clone() else {
        set_status(gen, "not signed in".into());
        return;
    };
    spawn(async move {
        match images::fetch_full(&ident, &f).await {
            Ok(size) => {
                let st = state();
                let mut b = st.borrow_mut();
                if let Some(v) = b.as_mut().filter(|v| v.gen == gen) {
                    v.size = Some(size);
                    v.status.clear();
                    paint(v);
                }
            }
            Err(e) => {
                log(&format!("{NAME}: picture {} ({}): {e}", f.id, f.name));
                set_status(gen, format!("cannot fetch the picture: {e}"));
            }
        }
    });
}

fn set_status(gen: u64, status: String) {
    let st = state();
    let mut b = st.borrow_mut();
    if let Some(v) = b.as_mut().filter(|v| v.gen == gen) {
        v.status = status;
        paint(v);
    }
}

/// The block a `w`×`h` picture takes in a `width`×`height` view (the
/// caption row kept clear) and where it starts, centred: (cols, rows,
/// x, y), 0-based.
pub fn block(width: u32, height: u32, w: u32, h: u32, cell_aspect: f64) -> (u32, u32, u32, u32) {
    let max_cols = width.min(MAX_BLOCK).max(1);
    let max_rows = height.saturating_sub(CAPTION_ROWS).min(MAX_BLOCK).max(1);
    let (cols, rows) = preview_fit(w, h, cell_aspect, max_cols, max_rows);
    let x = (width.saturating_sub(cols)) / 2;
    let y = (max_rows.saturating_sub(rows)) / 2;
    (cols, rows, x, y)
}

fn paint(v: &Viewer) {
    let mut out = String::from("\x1b[2J\x1b[H");
    let caption = match v.size {
        Some((w, h)) => {
            let (cols, rows, x, y) = block(v.width, v.height, w, h, v.cell_aspect);
            // The same block stays placed; another size (a resize) is
            // sent again under a new id, the old one forgotten.
            let id = match v.placed {
                Some((id, c, r)) if c == cols && r == rows => id,
                old => {
                    if let Some((old_id, _, _)) = old {
                        out.push_str(&kitty_delete(old_id));
                    }
                    let id = crate::chat::next_image_id();
                    let path = format!("{}/{}", v.data_root, images::full_name(&v.file.id));
                    out.push_str(&kitty_transmit_file(id, &path, cols, rows));
                    id
                }
            };
            for r in 0..rows {
                out.push_str(&format!("\x1b[{};{}H{}", y + r + 1, x + 1, placeholder_row(id, r, cols)));
            }
            // Recorded for the delete on close; `paint` takes the viewer
            // by reference, so through the cell below.
            PLACED.with(|p| p.set(Some((id, cols, rows))));
            format!("{} · {w}×{h}", v.file.name)
        }
        None => v.status.clone(),
    };
    let hint = " · o open · q/Esc/z close";
    let room = (v.width as usize).saturating_sub(2 + hint.len());
    let shown: String = caption.chars().take(room).collect();
    out.push_str(&format!("\x1b[{};2H\x1b[2m{shown}{hint}\x1b[0m", v.height));
    let _ = mode_write(v.mode, out.as_bytes());
}

thread_local! {
    /// What the last paint placed, read back into the viewer by whoever
    /// holds it mutably (paint borrows it shared).
    static PLACED: std::cell::Cell<Option<(u32, u32, u32)>> = const { std::cell::Cell::new(None) };
}

fn sync_placed(v: &mut Viewer) {
    if let Some(p) = PLACED.with(|p| p.take()) {
        v.placed = Some(p);
    }
}

pub fn on_mode_event(name: &str, event: &Event) {
    match name {
        "mode-key" => {
            let Some(key) = event.get_str("key") else { return };
            on_key(key);
        }
        "mode-resize" => {
            let st = state();
            let mut b = st.borrow_mut();
            let Some(v) = b.as_mut() else { return };
            sync_placed(v);
            if let (Some(w), Some(h)) = (event.get_i64("width"), event.get_i64("height")) {
                v.width = w.max(1) as u32;
                v.height = h.max(1) as u32;
            }
            paint(v);
            sync_placed(v);
        }
        "mode-closed" => {
            // Closed from outside (the window went): nothing to write to.
            *state().borrow_mut() = None;
        }
        _ => {}
    }
}

fn on_key(key: &str) {
    {
        let st = state();
        let mut b = st.borrow_mut();
        if let Some(v) = b.as_mut() {
            sync_placed(v);
        }
    }
    match key {
        "q" | "Escape" | "z" | "Enter" => close(),
        "o" => {
            let path = state().borrow().as_ref().filter(|v| v.size.is_some()).map(|v| format!("{}/{}", v.data_root, images::full_name(&v.file.id)));
            if let Some(p) = path {
                spawn(async move {
                    let _ = run_job(&format!("command -v open >/dev/null 2>&1 && open {p} || xdg-open {p}", p = crate::http::sq(&p)), None).await;
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_picture_fills_the_width_and_sits_centred() {
        // 400x50 at cell aspect 2 in a 200x50 window: 16 cells per row
        // of height, so the width (200) binds: 200 cols, 12.5 rows
        // rounded up, centred in the 49 rows above the caption.
        assert_eq!(block(200, 50, 400, 50, 2.0), (200, 13, 0, 18));
    }

    #[test]
    fn a_tall_picture_leaves_the_caption_row() {
        // 100x1000: the 23 rows above the caption bind; 23 * 0.2 = 5 cols.
        assert_eq!(block(80, 24, 100, 1000, 2.0), (5, 23, 37, 0));
    }
}
