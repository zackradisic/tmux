//! Session save/restore for the C-development loop: kill the server
//! without losing your layout.
//!
//! Verbs (wire them to keys or run them from the prompt):
//!
//!   plugin-command resurrect save      # snapshot everything to disk
//!   plugin-command resurrect kill      # snapshot, then kill-server
//!   plugin-command resurrect restore   # rebuild on a fresh server
//!   plugin-command resurrect status    # what the save file holds
//!
//! Saved: sessions, windows (name, size, layout incl. floating panes,
//! automatic-rename state), panes (cwd, running command as a note, full
//! scrollback + screen contents with colors). Restored panes run the
//! default shell in the saved cwd, with the saved contents replayed
//! above the fresh prompt (cat-wrapper, tmux-resurrect style). Processes
//! do not survive - that is the future server-handoff feature.
//!
//! Load (tmux.conf):
//!   load-plugin -c capture-pane -c run-command -c fs-read -c fs-write \
//!       ~/.tmux/plugins/resurrect.wasm
//!   bind-key C-s plugin-command resurrect kill
//!
//! The dev loop becomes: prefix C-s, rebuild tmux, start it, then
//! `plugin-command resurrect restore`.
//!
//! Save data lives in the plugin's sandbox
//! (~/.local/share/tmux/plugins/resurrect/): state.json plus one raw
//! content file per pane.

use std::cell::Cell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

/// Rows per capture_pane page: safely under the host's 2000-line cap
/// while keeping pages comfortably sized.
const PAGE_ROWS: i32 = 800;
/// Bytes per fs_write call. The ABI caps nothing here; chunking keeps
/// each write short so pane teardown never waits on a big one.
const WRITE_CHUNK: usize = 256 * 1024;

#[derive(Serialize, Deserialize, Clone)]
struct SavedPane {
    id: u32,
    floating: bool,
    cwd: String,
    /// What was running at save time. Informational for now; the future
    /// process-restore feature builds on it.
    command: String,
    /// Content file name in the data dir; None for dead panes.
    content: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SavedWindow {
    index: u32,
    name: String,
    auto_rename: bool,
    width: u32,
    height: u32,
    /// Raw #{window_layout} (checksum + tiled cells + <floats>).
    layout: String,
    active_pane: Option<u32>,
    /// TAILQ order (the order layout_parse assigns positionally).
    panes: Vec<SavedPane>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SavedSession {
    name: String,
    current_window_index: Option<u32>,
    windows: Vec<SavedWindow>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SaveFile {
    version: u32,
    sessions: Vec<SavedSession>,
}

struct Resurrect {
    busy: Rc<Cell<bool>>,
}

impl Plugin for Resurrect {
    const NAME: &'static str = "resurrect";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"])
            .map_err(|e| e.message.clone())?;
        Ok(Self { busy: Rc::new(Cell::new(false)) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        if !event.is("plugin-command") {
            return;
        }
        let verb = event.get_str("text").unwrap_or("").to_string();
        if self.busy.get() {
            let _ = display_message("resurrect: busy");
            return;
        }
        self.busy.set(true);
        let busy = Rc::clone(&self.busy);
        ctx.spawn(async move {
            match verb.as_str() {
                "save" => save(false).await,
                "kill" => save(true).await,
                "restore" => restore().await,
                "status" => status().await,
                other => {
                    let _ = display_message(&format!(
                        "resurrect: unknown verb {other:?} (save|kill|restore|status)"
                    ));
                }
            }
            busy.set(false);
        });
    }
}

tmux_plugin!(Resurrect);

/// tmux-level single-quoting (session_creator's quote()).
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Decode capture_pane's escape encoding: "\033" (4 chars) -> ESC,
/// "\\" -> backslash, left to right.
fn unescape(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'\\'
            && i + 3 < input.len()
            && input[i + 1] == b'0'
            && input[i + 2] == b'3'
            && input[i + 3] == b'3'
        {
            out.push(0x1b);
            i += 4;
        } else if b == b'\\' && i + 1 < input.len() && input[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    out
}

/// Port of tmux's layout_checksum (layout-custom.c).
fn layout_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for &b in body.as_bytes() {
        csum = (csum >> 1) + ((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(b));
    }
    csum
}

/// A parsed layout cell (the tmux custom-layout grammar).
struct LayoutCell {
    header: String,
    id: Option<u32>,
    children: Option<(char, Vec<LayoutCell>)>, // '{' left-right or '[' top-bottom
}

/// Recursive-descent parser for layout cells: "WxH,X,Y[,ID][{...}|[...]]".
/// The ",ID" is only an id when the digit run is not followed by 'x' (which
/// would make it the next sibling's width) - same lookahead tmux uses.
fn parse_cell(b: &[u8], i: &mut usize) -> Option<LayoutCell> {
    fn num(b: &[u8], i: &mut usize) -> Option<()> {
        let s = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        (*i > s).then_some(())
    }
    let start = *i;
    num(b, i)?;
    if b.get(*i) != Some(&b'x') {
        return None;
    }
    *i += 1;
    num(b, i)?;
    for _ in 0..2 {
        if b.get(*i) != Some(&b',') {
            return None;
        }
        *i += 1;
        num(b, i)?;
    }
    let header = String::from_utf8_lossy(&b[start..*i]).into_owned();

    let mut id = None;
    if b.get(*i) == Some(&b',') {
        let save = *i;
        *i += 1;
        let ds = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i > ds && b.get(*i) != Some(&b'x') {
            id = String::from_utf8_lossy(&b[ds..*i]).parse().ok();
        } else {
            *i = save; // the digits were the next sibling's width
        }
    }

    let children = match b.get(*i) {
        Some(&open @ (b'{' | b'[')) => {
            let close = if open == b'{' { b'}' } else { b']' };
            *i += 1;
            let mut kids = Vec::new();
            loop {
                kids.push(parse_cell(b, i)?);
                match b.get(*i) {
                    Some(&b',') => *i += 1,
                    Some(&c) if c == close => {
                        *i += 1;
                        break;
                    }
                    _ => return None,
                }
            }
            Some((open as char, kids))
        }
        _ => None,
    };
    Some(LayoutCell { header, id, children })
}

fn emit_cell(c: &LayoutCell, out: &mut String) {
    out.push_str(&c.header);
    if let Some(id) = c.id {
        out.push(',');
        out.push_str(&id.to_string());
    }
    if let Some((open, kids)) = &c.children {
        let (open, close) = if *open == '{' { ('{', '}') } else { ('[', ']') };
        out.push(open);
        for (i, kid) in kids.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            emit_cell(kid, out);
        }
        out.push(close);
    }
}

/// Remove floating leaves (they carry no tiled geometry); a container
/// left with one child is replaced by that child.
fn strip_floats(
    c: LayoutCell,
    floats: &std::collections::HashSet<u32>,
) -> Option<LayoutCell> {
    match c.children {
        None => {
            if c.id.is_some_and(|id| floats.contains(&id)) {
                None
            } else {
                Some(c)
            }
        }
        Some((open, kids)) => {
            let mut kept: Vec<LayoutCell> = kids
                .into_iter()
                .filter_map(|k| strip_floats(k, floats))
                .collect();
            match kept.len() {
                0 => None,
                1 => Some(kept.remove(0)),
                _ => Some(LayoutCell {
                    header: c.header,
                    id: c.id,
                    children: Some((open, kept)),
                }),
            }
        }
    }
}

/// Tiled leaf pane ids in tree order - the order panes must be created
/// in for layout_parse's positional assignment.
fn leaf_ids(c: &LayoutCell, out: &mut Vec<u32>) {
    match &c.children {
        None => {
            if let Some(id) = c.id {
                out.push(id);
            }
        }
        Some((_, kids)) => {
            for kid in kids {
                leaf_ids(kid, out);
            }
        }
    }
}

/// Split a saved #{window_layout} into (tiled-only layout string with a
/// recomputed checksum, tiled leaf order, float cells). This fork parents
/// float cells INSIDE the tiled tree and appends a `<...>` z-order
/// section; layout_parse rejects both, so floats are stripped from the
/// tree and recreated afterwards with new-pane.
fn split_layout(full: &str) -> (String, Vec<u32>, Vec<FloatCell>) {
    let fallback = || (full.to_string(), Vec::new(), Vec::new());
    let Some((_, body)) = full.split_once(',') else {
        return fallback();
    };
    let (tree, floats) = match body.find('<') {
        Some(at) => {
            let inner = body[at + 1..].trim_end_matches('>');
            (&body[..at], parse_floats(inner))
        }
        None => (body, Vec::new()),
    };
    let float_ids: std::collections::HashSet<u32> =
        floats.iter().map(|f| f.id).collect();
    let mut i = 0;
    let Some(root) = parse_cell(tree.as_bytes(), &mut i) else {
        return fallback();
    };
    let Some(stripped) = strip_floats(root, &float_ids) else {
        return fallback();
    };
    let mut order = Vec::new();
    leaf_ids(&stripped, &mut order);
    let mut tiled = String::new();
    emit_cell(&stripped, &mut tiled);
    (
        format!("{:04x},{}", layout_checksum(&tiled), tiled),
        order,
        floats,
    )
}

/// One floating pane cell from the layout's `<WxH,X,Y,ID,...>` section,
/// in dump order (topmost first).
struct FloatCell {
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    id: u32,
}

fn parse_floats(section: &str) -> Vec<FloatCell> {
    let mut out = Vec::new();
    let tokens: Vec<&str> = section.split(',').collect();
    for chunk in tokens.chunks(4) {
        let [size, x, y, id] = chunk else { break };
        let Some((w, h)) = size.split_once('x') else { continue };
        let (Ok(w), Ok(h), Ok(x), Ok(y), Ok(id)) = (
            w.parse(),
            h.parse(),
            x.parse(),
            y.parse(),
            id.parse(),
        ) else {
            continue;
        };
        out.push(FloatCell { w, h, x, y, id });
    }
    out
}

// ---------------------------------------------------------------------------
// Save
// ---------------------------------------------------------------------------

async fn save(kill: bool) {
    match do_save().await {
        Ok((sessions, panes)) => {
            let _ = display_message(&format!(
                "resurrect: saved {sessions} sessions / {panes} panes{}",
                if kill { "; killing server" } else { "" }
            ));
            if kill {
                // The completion never arrives (the server dies); the
                // await parks this task forever, which is fine.
                let _ = run_command("kill-server").await;
            }
        }
        Err(e) => {
            let _ = display_message(&format!("resurrect: save failed: {e}"));
        }
    }
}

async fn do_save() -> Result<(usize, usize), String> {
    let sessions = list_sessions().map_err(|e| e.to_string())?;
    let windows = list_windows().map_err(|e| e.to_string())?;
    let panes = list_panes().map_err(|e| e.to_string())?;
    let window_by_id: std::collections::HashMap<u32, &WindowInfo> =
        windows.iter().map(|w| (w.id, w)).collect();
    let pane_by_id: std::collections::HashMap<u32, &PaneInfo> =
        panes.iter().map(|p| (p.id, p)).collect();

    let mut out = SaveFile { version: 1, sessions: Vec::new() };
    let mut npanes = 0;

    for s in &sessions {
        let mut saved = SavedSession {
            name: s.name.clone(),
            current_window_index: s
                .windows
                .iter()
                .find(|(_, id)| Some(*id) == s.current_window)
                .map(|(idx, _)| *idx),
            windows: Vec::new(),
        };
        for &(index, win_id) in &s.windows {
            let Some(w) = window_by_id.get(&win_id) else { continue };
            let layout = format_expand(
                OptionTarget::Window(WindowId(win_id)),
                c"#{window_layout}",
            )
            .map_err(|e| format!("window_layout: {e}"))?;
            let auto_rename = get_option_in(
                OptionTarget::Window(WindowId(win_id)),
                c"automatic-rename",
            )
            .map(|v| v == "on")
            .unwrap_or(true);

            let mut sw = SavedWindow {
                index,
                name: w.name.clone(),
                auto_rename,
                width: w.width,
                height: w.height,
                layout,
                active_pane: w.active_pane,
                panes: Vec::new(),
            };
            for &pane_id in &w.panes {
                let Some(p) = pane_by_id.get(&pane_id) else { continue };
                sw.panes.push(save_pane(p).await?);
                npanes += 1;
            }
            saved.windows.push(sw);
        }
        out.sessions.push(saved);
    }

    // state.json last: a truncated save fails to parse and restore
    // aborts instead of rebuilding half a world.
    let json =
        serde_json::to_vec(&out).map_err(|e| format!("serialize: {e}"))?;
    let mut first = true;
    for chunk in json.chunks(WRITE_CHUNK) {
        fs_write("state.json", chunk.to_vec(), !first)
            .await
            .map_err(|e| format!("state.json: {e}"))?;
        first = false;
    }
    Ok((out.sessions.len(), npanes))
}

async fn save_pane(p: &PaneInfo) -> Result<SavedPane, String> {
    let pane = PaneId(p.id);
    let command =
        format_expand(OptionTarget::Pane(pane), c"#{pane_current_command}")
            .unwrap_or_default();
    let mut saved = SavedPane {
        id: p.id,
        floating: p.floating,
        cwd: p.cwd.clone(),
        command,
        content: None,
    };
    if p.dead {
        return Ok(saved);
    }

    let history: i64 =
        format_expand(OptionTarget::Pane(pane), c"#{history_size}")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    let cursor_y: i64 =
        format_expand(OptionTarget::Pane(pane), c"#{cursor_y}")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

    let file = format!("pane-{}.txt", p.id);
    let mut start = -history;
    let mut first = true;
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    while start <= cursor_y {
        let end = (start + i64::from(PAGE_ROWS) - 1).min(cursor_y);
        capture_pane_into(
            pane,
            Some(start as i32),
            Some(end as i32),
            true,
            &mut buf,
        )
        .map_err(|e| format!("capture %{}: {e}", p.id))?;
        let bytes = unescape(&buf);
        // fs_write caps one transfer; split big pages.
        if bytes.is_empty() && first {
            fs_write(&file, Vec::new(), false)
                .await
                .map_err(|e| format!("{file}: {e}"))?;
            first = false;
        }
        for chunk in bytes.chunks(WRITE_CHUNK) {
            fs_write(&file, chunk.to_vec(), !first)
                .await
                .map_err(|e| format!("{file}: {e}"))?;
            first = false;
        }
        start = end + 1;
    }
    if first {
        // Zero pages (empty pane): still create the file for the wrapper.
        fs_write(&file, Vec::new(), false)
            .await
            .map_err(|e| format!("{file}: {e}"))?;
    }
    saved.content = Some(file);
    Ok(saved)
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

async fn restore() {
    match do_restore().await {
        Ok((restored, skipped)) => {
            let _ = display_message(&format!(
                "resurrect: restored {restored} sessions{}",
                if skipped > 0 {
                    format!(" ({skipped} already existed)")
                } else {
                    String::new()
                }
            ));
        }
        Err(e) => {
            let _ =
                display_message(&format!("resurrect: restore failed: {e}"));
        }
    }
}

async fn read_state() -> Result<SaveFile, String> {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    loop {
        let (page, eof) = fs_read("state.json", offset, WRITE_CHUNK)
            .await
            .map_err(|e| format!("state.json: {e}"))?;
        offset += page.len() as u64;
        let done = eof || page.is_empty();
        bytes.extend_from_slice(&page);
        if done {
            break;
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("state.json is unreadable ({e}); save again"))
}

async fn do_restore() -> Result<(usize, usize), String> {
    let state = read_state().await?;
    let root = fs_root().map_err(|e| e.to_string())?;

    let existing = list_sessions().map_err(|e| e.to_string())?;
    let shell = existing
        .first()
        .and_then(|s| {
            get_option_in(
                OptionTarget::Session(SessionId(s.id)),
                c"default-shell",
            )
            .ok()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string());
    let existing: std::collections::HashSet<String> =
        existing.into_iter().map(|s| s.name).collect();

    let mut restored = 0;
    let mut skipped = 0;
    for session in &state.sessions {
        if existing.contains(&session.name) {
            skipped += 1;
            continue;
        }
        restore_session(session, &root, &shell).await?;
        restored += 1;
    }
    Ok((restored, skipped))
}

/// The pane's start command: replay saved contents, then hand over to
/// the default shell in the saved cwd.
fn wrapper(pane: &SavedPane, root: &str, shell: &str) -> String {
    match &pane.content {
        Some(file) => format!("sh -c 'cat {root}/{file}; exec {shell}'"),
        None => shell.to_string(),
    }
}

/// Find a session's live info by name.
fn find_session(name: &str) -> Result<SessionInfo, String> {
    list_sessions()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("session {name:?} did not appear"))
}

async fn restore_session(
    session: &SavedSession,
    root: &str,
    shell: &str,
) -> Result<(), String> {
    let mut windows = session.windows.clone();
    windows.sort_by_key(|w| w.index);
    let Some(first_window) = windows.first() else {
        return Ok(());
    };
    let name = &session.name;

    // The first window's first tiled pane (in layout tree order, which
    // is the positional-assignment order) rides new-session itself.
    let first_pane = tiled_order(first_window)
        .into_iter()
        .next()
        .ok_or_else(|| format!("window {} has no tiled pane", first_window.index))?
        .clone();
    let first_pane = &first_pane;
    run_command(&format!(
        "new-session -d -s {} -x {} -y {} -c {} {}",
        q(name),
        first_window.width,
        first_window.height,
        q(&first_pane.cwd),
        q(&wrapper(first_pane, root, shell)),
    ))
    .await
    .map_err(|e| e.to_string())?;

    // run_command completions never carry failure; verify by existence.
    let live = find_session(name)?;

    // base-index may differ from the saved first index.
    if let Some(&(actual, _)) = live.windows.first() {
        if actual != first_window.index {
            run_command(&format!(
                "move-window -s {} -t {}",
                q(&format!("{name}:{actual}")),
                q(&format!("{name}:{}", first_window.index)),
            ))
            .await
            .map_err(|e| e.to_string())?;
        }
    }

    for (i, w) in windows.iter().enumerate() {
        if i > 0 {
            let order = tiled_order(w);
            let wp = order
                .first()
                .ok_or_else(|| format!("window {} has no tiled pane", w.index))?;
            run_command(&format!(
                "new-window -d -t {} -c {} {}",
                q(&format!("{name}:{}", w.index)),
                q(&wp.cwd),
                q(&wrapper(wp, root, shell)),
            ))
            .await
            .map_err(|e| e.to_string())?;
        }
        restore_window(name, w, root, shell).await?;
    }

    if let Some(current) = session.current_window_index {
        let _ = run_command(&format!(
            "select-window -t {}",
            q(&format!("{name}:{current}"))
        ))
        .await;
    }
    Ok(())
}

/// Tiled panes in layout-tree-leaf order (the order layout_parse assigns
/// panes to cells); falls back to the saved TAILQ order filtered to
/// non-floating panes when the layout does not parse.
fn tiled_order(w: &SavedWindow) -> Vec<&SavedPane> {
    let (_, order, _) = split_layout(&w.layout);
    let by_id: std::collections::HashMap<u32, &SavedPane> =
        w.panes.iter().map(|p| (p.id, p)).collect();
    let mapped: Vec<&SavedPane> =
        order.iter().filter_map(|id| by_id.get(id).copied()).collect();
    if !mapped.is_empty() {
        mapped
    } else {
        w.panes.iter().filter(|p| !p.floating).collect()
    }
}

async fn restore_window(
    session: &str,
    w: &SavedWindow,
    root: &str,
    shell: &str,
) -> Result<(), String> {
    let target = format!("{session}:{}", w.index);
    let tiled = tiled_order(w);

    // Remaining tiled panes in layout-tree order: each split makes the
    // new pane active, so repeated window-targeted splits append in
    // order; the interleaved `tiled` layout keeps panes big enough.
    for pane in tiled.iter().skip(1) {
        run_command(&format!(
            "split-window -t {} -c {} {}",
            q(&target),
            q(&pane.cwd),
            q(&wrapper(pane, root, shell)),
        ))
        .await
        .map_err(|e| e.to_string())?;
        let _ = run_command(&format!("select-layout -t {} tiled", q(&target)))
            .await;
    }

    // Apply the saved layout (floats stripped, checksum recomputed;
    // panes are assigned positionally in tree order).
    let (layout, _, floats) = split_layout(&w.layout);
    run_command(&format!("select-layout -t {} {}", q(&target), q(&layout)))
        .await
        .map_err(|e| e.to_string())?;

    // Floats afterwards (layout_parse must not see them), bottom-most
    // first: the dump lists topmost first and each new float goes to the
    // top of the z-order. Track the id each one gets for the active-pane
    // step.
    let mut float_ids: std::collections::HashMap<u32, u32> =
        std::collections::HashMap::new();
    let win_id = window_id_of(session, w.index)?;
    // The saved layout records the INNER cell; new-pane's -x/-y are the
    // outer request (shrunk by 2 for the border, offsets shifted by 1
    // unless pane-border-lines is none). Compensate so floats round-trip
    // at their exact geometry.
    let bordered = get_option_in(
        OptionTarget::Window(WindowId(win_id)),
        c"pane-border-lines",
    )
    .map(|v| v != "none")
    .unwrap_or(true);
    let (grow, shift) = if bordered { (2, 1) } else { (0, 0) };
    for cell in floats.iter().rev() {
        let saved = w.panes.iter().find(|p| p.id == cell.id);
        let (cwd, wrap) = match saved {
            Some(p) => (p.cwd.clone(), wrapper(p, root, shell)),
            None => (String::new(), shell.to_string()),
        };
        let before: std::collections::HashSet<u32> =
            resolve_window(WindowId(win_id))
                .map_err(|e| e.to_string())?
                .panes
                .into_iter()
                .collect();
        let mut cmd = format!(
            "new-pane -d -t {} -x {} -y {} -X {} -Y {}",
            q(&target),
            cell.w + grow,
            cell.h + grow,
            cell.x.saturating_sub(shift),
            cell.y.saturating_sub(shift)
        );
        if !cwd.is_empty() {
            cmd.push_str(&format!(" -c {}", q(&cwd)));
        }
        cmd.push_str(&format!(" {}", q(&wrap)));
        run_command(&cmd).await.map_err(|e| e.to_string())?;
        if let Ok(after) = resolve_window(WindowId(win_id)) {
            if let Some(new) =
                after.panes.iter().find(|id| !before.contains(id))
            {
                float_ids.insert(cell.id, *new);
            }
        }
    }

    // Active pane: positional for tiled panes, tracked ids for floats.
    if let Some(active) = w.active_pane {
        let new_id = if let Some(pos) =
            tiled.iter().position(|p| p.id == active)
        {
            resolve_window(WindowId(win_id))
                .ok()
                .and_then(|info| {
                    let mut tiled_new = Vec::new();
                    for id in info.panes {
                        if let Ok(p) = resolve_pane(PaneId(id)) {
                            if !p.floating {
                                tiled_new.push(id);
                            }
                        }
                    }
                    tiled_new.get(pos).copied()
                })
        } else {
            float_ids.get(&active).copied()
        };
        if let Some(id) = new_id {
            let _ = run_command(&format!("select-pane -t %{id}")).await;
        }
    }

    // Restore the name; rename-window pins automatic-rename off, so
    // re-enable it for windows that had it on (the name will then track
    // the restored shell - truthful, the old command is gone).
    run_command(&format!(
        "rename-window -t {} {}",
        q(&target),
        q(&w.name)
    ))
    .await
    .map_err(|e| e.to_string())?;
    if w.auto_rename {
        let _ = run_command(&format!(
            "set-option -w -t {} automatic-rename on",
            q(&target)
        ))
        .await;
    }
    Ok(())
}

/// The live window id behind session:index.
fn window_id_of(session: &str, index: u32) -> Result<u32, String> {
    find_session(session)?
        .windows
        .iter()
        .find(|(idx, _)| *idx == index)
        .map(|(_, id)| *id)
        .ok_or_else(|| format!("window {session}:{index} did not appear"))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

async fn status() {
    match read_state().await {
        Ok(state) => {
            let panes: usize =
                state.sessions.iter().flat_map(|s| &s.windows)
                    .map(|w| w.panes.len())
                    .sum();
            let names: Vec<&str> = state
                .sessions
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            let _ = display_message(&format!(
                "resurrect: save holds {} sessions ({}) / {panes} panes",
                state.sessions.len(),
                names.join(", ")
            ));
        }
        Err(e) => {
            let _ = display_message(&format!("resurrect: {e}"));
        }
    }
}
