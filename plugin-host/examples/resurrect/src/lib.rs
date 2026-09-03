//! Session save/restore for the C-development loop: kill the server
//! without losing your layout.
//!
//! Verbs (wire them to keys or run them from the prompt):
//!
//!   plugin-command resurrect save      # snapshot everything to disk
//!   plugin-command resurrect kill      # snapshot, then kill-server
//!   plugin-command resurrect restore   # rebuild on a fresh server
//!   plugin-command resurrect status    # what the save file holds
//!   plugin-command resurrect pick      # open the picker (needs -c mode)
//!
//! Picker: a filter list of saved snapshots. Type to filter; Up/Down or
//! C-p/C-n move; Enter restores the highlighted snapshot; C-d deletes it
//! (with a y/n confirm); C-k saves then kills the server; C-r saves then
//! restarts the server in place (processes stay live); Esc closes. The
//! action keys are configurable (pick_restore, pick_delete, pick_kill,
//! pick_restart, pick_close in the manifest config).
//!
//! Autosave (manifest config): `autosave = "5m"` saves on a timer. The
//! first autosave waits one full period, so a restore after a server
//! restart is never overwritten by an autosave of the empty new world.
//! `keep = "3"` rotates old snapshots into state.1.bin, state.2.bin
//! (higher = older) before each publish. Defaults: autosave off, keep 1.
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
//!       -c mode ~/.tmux/plugins/resurrect.wasm
//!   bind-key C-r plugin-command resurrect pick
//!
//! The dev loop: prefix C-r, then C-r again to save and restart in place
//! (processes stay live), or C-k to save and kill for a full restart.
//!
//! Save data lives in the plugin's sandbox
//! (~/.local/share/tmux/plugins/resurrect/) as one container file,
//! state.bin: an 8-byte magic, a u32 metadata length, JSON metadata
//! carrying a Unix timestamp, then the raw pane contents as byte ranges
//! the metadata points into. A save writes state.bin.tmp and publishes
//! it with one atomic rename, so a reader sees the old snapshot or the
//! new one, never a mix, and a crash mid-save loses nothing. Restore
//! materializes the ranges back into per-pane files for the cat-wrapper
//! (the v1 state.json layout still restores).

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

/// Rows per capture_pane page: safely under the host's 2000-line cap
/// while keeping pages comfortably sized.
const PAGE_ROWS: i32 = 800;
/// Bytes per fs_write call. The ABI caps nothing here; chunking keeps
/// each write short so pane teardown never waits on a big one.
const WRITE_CHUNK: usize = 256 * 1024;

/// The live snapshot, and the temp name a save publishes from.
const STATE_BIN: &str = "state.bin";
const STATE_TMP: &str = "state.bin.tmp";
/// Container magic; the trailing digit is the format version.
const MAGIC: &[u8; 8] = b"TMUXRES2";

/// The picker float: a fixed width and the rows it grows to hold.
const PICK_WIDTH: u32 = 76;
const PICK_HEIGHT: u32 = 8;
/// Rows on screen at once; a longer list scrolls.
const LIST_MAX: usize = 10;
/// Fixed lines around the row list (title, filter, rule, gap, status,
/// footer). The float asks for this plus the visible rows.
const PICK_CHROME: u32 = 6;

#[derive(Serialize, Deserialize, Clone)]
struct SavedPane {
    id: u32,
    floating: bool,
    cwd: String,
    /// What was running at save time. Informational for now; the future
    /// process-restore feature builds on it.
    command: String,
    /// v1: content file name in the data dir. v2 only sets it at
    /// restore, after materializing the blob. None for dead panes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    /// v2: (offset, length) of the content inside state.bin's blob
    /// region (relative to the end of the metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blob: Option<(u64, u64)>,
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
    /// Unix time of the save, milliseconds. Zero in v1 files.
    #[serde(default)]
    saved_at_ms: u64,
    sessions: Vec<SavedSession>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResurrectConfig {
    /// Autosave period: "5m", "90s", "300" (seconds); "0" turns it off.
    autosave: Option<serde_json::Value>,
    /// Snapshots to keep. "1" = state.bin only; "3" also rotates the
    /// two previous snapshots into state.1.bin and state.2.bin.
    keep: Option<serde_json::Value>,
    /// Picker keys, as tmux key strings (e.g. "Enter", "C-d", "x").
    /// Printable keys go into the filter, so an action wants a control
    /// or a named key. Unset uses the default below.
    pick_restore: Option<String>,
    pick_delete: Option<String>,
    pick_kill: Option<String>,
    pick_restart: Option<String>,
    pick_close: Option<String>,
}

/// The keys the picker acts on. The footer shows them, so a config
/// change is visible without reading the manifest.
#[derive(Clone)]
struct PickKeys {
    restore: String,
    delete: String,
    kill: String,
    restart: String,
    close: String,
}

impl Default for PickKeys {
    fn default() -> Self {
        Self {
            restore: "Enter".into(),
            delete: "C-d".into(),
            kill: "C-k".into(),
            restart: "C-r".into(),
            close: "Escape".into(),
        }
    }
}

impl PickKeys {
    fn from_config(c: &ResurrectConfig) -> Self {
        let d = PickKeys::default();
        let pick = |v: &Option<String>, def: String| {
            v.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or(def)
        };
        Self {
            restore: pick(&c.pick_restore, d.restore),
            delete: pick(&c.pick_delete, d.delete),
            kill: pick(&c.pick_kill, d.kill),
            restart: pick(&c.pick_restart, d.restart),
            close: pick(&c.pick_close, d.close),
        }
    }
}

/// A manifest value: TOML lets the user write "5m" or plain 300.
fn cfg_str(v: &serde_json::Value) -> String {
    match v.as_str() {
        Some(s) => s.to_string(),
        None => v.to_string(),
    }
}

/// "5m" / "90s" / "1h" / bare seconds -> milliseconds.
fn parse_period(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (s, 1000)
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("bad autosave period {s:?}"))
}

struct Resurrect {
    busy: Rc<Cell<bool>>,
    /// Snapshots to keep, 1..=8 (config `keep`).
    keep: u32,
    /// Picker action keys, from config or defaults.
    keys: PickKeys,
    /// The open picker, or None. Shared with the async actions it spawns.
    picker: Rc<RefCell<Option<Picker>>>,
}

impl Plugin for Resurrect {
    const NAME: &'static str = "resurrect";
    type Config = ResurrectConfig;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"])
            .map_err(|e| e.message.clone())?;
        let keys = PickKeys::from_config(&config);
        let every = match &config.autosave {
            None => 0,
            Some(v) => parse_period(&cfg_str(v))?,
        };
        let keep = match &config.keep {
            None => 1,
            Some(v) => cfg_str(v)
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("bad keep {v}"))?
                .clamp(1, 8),
        };
        let busy = Rc::new(Cell::new(false));
        if every > 0 {
            // Floor, so a config typo cannot hammer the server.
            let every = every.max(10_000);
            let busy = Rc::clone(&busy);
            // Sleep first: after a server restart the world is empty,
            // and an immediate autosave would overwrite the snapshot
            // the user is about to restore.
            ctx.spawn(async move {
                loop {
                    if sleep_ms(every).await.is_err() {
                        break;
                    }
                    if busy.get() {
                        continue;
                    }
                    busy.set(true);
                    autosave(keep).await;
                    busy.set(false);
                }
            });
        }
        Ok(Self {
            busy,
            keep,
            keys,
            picker: Rc::new(RefCell::new(None)),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => self.on_command(ctx, &event),
            "mode-key" => self.on_mode_key(ctx, &event),
            "mode-resize" => {
                let mut b = self.picker.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if event.get_i64("mode") != Some(p.mode.0 as i64) {
                    return;
                }
                if let Some(w) = event.get_i64("width") {
                    p.width = w as u32;
                }
                if let Some(h) = event.get_i64("height") {
                    p.height = h as u32;
                }
                // The host has spoken: do not ask for the old size again.
                p.sized = (p.width, p.height);
                pick_render(p);
            }
            "mode-closed" => {
                let mut b = self.picker.borrow_mut();
                if b.as_ref().is_some_and(|p| {
                    event.get_i64("mode") == Some(p.mode.0 as i64)
                }) {
                    *b = None;
                }
            }
            _ => {}
        }
    }
}

impl Resurrect {
    /// The `plugin-command` verbs. `pick` opens the picker (not guarded
    /// by `busy`, since it is interactive); the rest run one save/restore.
    fn on_command(&mut self, ctx: &Ctx, event: &Event) {
        let verb = event.get_str("text").unwrap_or("").to_string();
        if verb == "pick" {
            if self.picker.borrow().is_some() {
                let _ = display_message("resurrect: picker already open");
                return;
            }
            let client = event.scope.client.map(u64::from);
            let picker = Rc::clone(&self.picker);
            let keys = self.keys.clone();
            ctx.spawn(pick_open(picker, keys, client));
            return;
        }
        if self.busy.get() {
            let _ = display_message("resurrect: busy");
            return;
        }
        self.busy.set(true);
        let busy = Rc::clone(&self.busy);
        let keep = self.keep;
        ctx.spawn(async move {
            match verb.as_str() {
                "save" => save(false, keep).await,
                "kill" => save(true, keep).await,
                "restore" => restore().await,
                "status" => status().await,
                other => {
                    let _ = display_message(&format!(
                        "resurrect: unknown verb {other:?} (save|kill|restore|status|pick)"
                    ));
                }
            }
            busy.set(false);
        });
    }

    /// A key inside the picker float. Decide the action under the borrow,
    /// then act after dropping it (an action spawns and re-borrows).
    fn on_mode_key(&mut self, ctx: &Ctx, event: &Event) {
        let mode_id = event.get_i64("mode");
        let key = event.get_str("key").unwrap_or("").to_string();
        let mut after = PickAfter::None;
        {
            let mut b = self.picker.borrow_mut();
            let Some(p) = b.as_mut() else { return };
            if mode_id != Some(p.mode.0 as i64) {
                return;
            }
            // An action is running: ignore keys until it settles.
            if self.busy.get() {
                return;
            }
            p.status = None;
            if let Some(c) = p.confirm.clone() {
                match key.as_str() {
                    "y" | "Y" => {
                        p.confirm = None;
                        after = match c {
                            Confirm::Delete(i) => match p.snaps.get(i) {
                                Some(s) => PickAfter::Delete(s.file.clone()),
                                None => PickAfter::None,
                            },
                            Confirm::Kill => PickAfter::Kill,
                            Confirm::Restart => PickAfter::Restart,
                        };
                    }
                    "n" | "N" | "Escape" => {
                        p.confirm = None;
                        pick_render(p);
                    }
                    _ => {}
                }
            } else if key == self.keys.close {
                after = PickAfter::Close(p.mode);
            } else if key == self.keys.restore {
                if let Some(&i) = p.view.get(p.sel) {
                    after = PickAfter::Restore(p.snaps[i].file.clone());
                }
            } else if key == self.keys.delete {
                if let Some(&i) = p.view.get(p.sel) {
                    p.confirm = Some(Confirm::Delete(i));
                    pick_render(p);
                }
            } else if key == self.keys.kill {
                p.confirm = Some(Confirm::Kill);
                pick_render(p);
            } else if key == self.keys.restart {
                p.confirm = Some(Confirm::Restart);
                pick_render(p);
            } else {
                match key.as_str() {
                    "Down" | "C-n" | "C-j" => {
                        if !p.view.is_empty() {
                            p.sel = (p.sel + 1).min(p.view.len() - 1);
                            p.scroll_to_selection();
                            pick_render(p);
                        }
                    }
                    // A configured action key is matched above, so the
                    // nav keys here only fire when they are still free.
                    "Up" | "C-p" | "C-k" => {
                        p.sel = p.sel.saturating_sub(1);
                        p.scroll_to_selection();
                        pick_render(p);
                    }
                    "BSpace" => {
                        p.filter.pop();
                        pick_refilter(p);
                        pick_render(p);
                    }
                    "C-u" => {
                        p.filter.clear();
                        pick_refilter(p);
                        pick_render(p);
                    }
                    "Space" => {
                        p.filter.push(' ');
                        pick_refilter(p);
                        pick_render(p);
                    }
                    k if k.chars().count() == 1
                        && !k.chars().next().unwrap().is_control() =>
                    {
                        p.filter.push_str(k);
                        pick_refilter(p);
                        pick_render(p);
                    }
                    _ => {}
                }
            }
        }
        match after {
            PickAfter::None => {}
            PickAfter::Close(mode) => {
                let _ = mode_close(mode);
            }
            PickAfter::Restore(file) => {
                ctx.spawn(pick_restore(
                    Rc::clone(&self.picker),
                    Rc::clone(&self.busy),
                    file,
                ));
            }
            PickAfter::Delete(file) => {
                ctx.spawn(pick_delete(
                    Rc::clone(&self.picker),
                    Rc::clone(&self.busy),
                    file,
                ));
            }
            PickAfter::Kill => {
                ctx.spawn(pick_kill(
                    Rc::clone(&self.picker),
                    Rc::clone(&self.busy),
                    self.keep,
                ));
            }
            PickAfter::Restart => {
                ctx.spawn(pick_restart(
                    Rc::clone(&self.picker),
                    Rc::clone(&self.busy),
                    self.keep,
                ));
            }
        }
    }
}

tmux_plugin!(Resurrect);

// ---------------------------------------------------------------------------
// Picker: a filterable list of snapshots, with restore/delete/kill/restart.
// ---------------------------------------------------------------------------

/// One saved snapshot on disk: the live state.bin or an archive.
#[derive(Clone)]
struct Snapshot {
    /// The file name in the data dir (e.g. "state.bin", "state.1.bin").
    file: String,
    /// state.bin, the newest snapshot.
    live: bool,
    saved_at_ms: u64,
    sessions: usize,
    panes: usize,
    names: Vec<String>,
}

/// A destructive action waiting for a y/n answer.
#[derive(Clone)]
enum Confirm {
    /// Delete the snapshot at this index into `snaps`.
    Delete(usize),
    /// Save, then kill the server (processes die).
    Kill,
    /// Save, then restart the server in place (processes stay live).
    Restart,
}

/// What a key press asks for, decided under the borrow and run after it.
enum PickAfter {
    None,
    Close(ModeId),
    Restore(String),
    Delete(String),
    Kill,
    Restart,
}

struct Picker {
    mode: ModeId,
    width: u32,
    height: u32,
    /// The size last asked for through mode_resize; a render only asks
    /// again when the wanted size changes.
    sized: (u32, u32),
    /// All snapshots, newest first.
    snaps: Vec<Snapshot>,
    /// Indices into `snaps`, filtered and ranked.
    view: Vec<usize>,
    /// Index into `view`.
    sel: usize,
    /// First visible row of `view`.
    top: usize,
    /// The typed filter.
    filter: String,
    /// Snapshot of the clock at open, for the age column.
    now_ms: u64,
    /// A pending y/n action, or None.
    confirm: Option<Confirm>,
    keys: PickKeys,
    /// A transient line (e.g. "deleted state.1.bin").
    status: Option<String>,
}

impl Picker {
    /// Visible rows: the filtered list, capped, at least one so an empty
    /// list still draws its "no saved states" line.
    fn height(&self) -> usize {
        self.view.len().min(LIST_MAX).max(1)
    }

    fn scroll_to_selection(&mut self) {
        let h = self.view.len().min(LIST_MAX);
        if h == 0 {
            self.top = 0;
            return;
        }
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + h {
            self.top = self.sel + 1 - h;
        }
    }

    fn wanted_size(&self) -> (u32, u32) {
        (self.width, PICK_CHROME + self.height() as u32)
    }
}

/// Rank a snapshot's haystack against the filter (session_creator's
/// scheme): 0 best, None rejects. Prefix beats substring beats
/// subsequence, so a name you spell out rises to the top.
fn rank(hay: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(4);
    }
    if hay.starts_with(needle) {
        return Some(0);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if h.starts_with(&n) {
        return Some(1);
    }
    if h.contains(&n) {
        return Some(2);
    }
    let mut chars = h.chars();
    if n.chars().all(|c| chars.any(|x| x == c)) {
        return Some(3);
    }
    None
}

/// Probe the live file and the archive chain, reading each header for
/// its metadata. Missing or unreadable files are skipped. Newest first.
async fn enumerate() -> Vec<Snapshot> {
    let mut candidates = vec![STATE_BIN.to_string()];
    // keep <= 8, so archives never pass state.7.bin; probe a little past.
    for i in 1..=8u32 {
        candidates.push(format!("state.{i}.bin"));
    }
    let mut out = Vec::new();
    for file in candidates {
        let Ok((state, _)) = read_state(&file).await else { continue };
        let panes: usize = state
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .map(|w| w.panes.len())
            .sum();
        out.push(Snapshot {
            live: file == STATE_BIN,
            file,
            saved_at_ms: state.saved_at_ms,
            sessions: state.sessions.len(),
            panes,
            names: state.sessions.iter().map(|s| s.name.clone()).collect(),
        });
    }
    // Newest first; a stable sort keeps the probe order for equal times.
    out.sort_by(|a, b| b.saved_at_ms.cmp(&a.saved_at_ms));
    out
}

/// Re-rank the rows against the filter, keeping the highlighted snapshot
/// highlighted when it survives.
fn pick_refilter(p: &mut Picker) {
    let keep = p.view.get(p.sel).map(|&i| p.snaps[i].file.clone());
    let mut scored: Vec<(u8, usize)> = Vec::new();
    for (i, s) in p.snaps.iter().enumerate() {
        let tag = if s.live { "live" } else { "archive" };
        let hay = format!("{} {} {}", s.names.join(" "), s.file, tag);
        if let Some(r) = rank(&hay, p.filter.trim()) {
            scored.push((r, i));
        }
    }
    // Stable: equal ranks keep the newest-first order from `snaps`.
    scored.sort_by(|a, b| a.0.cmp(&b.0));
    p.view = scored.into_iter().map(|(_, i)| i).collect();
    p.sel = keep
        .and_then(|f| p.view.iter().position(|&i| p.snaps[i].file == f))
        .unwrap_or(0);
    if p.sel >= p.view.len() {
        p.sel = p.view.len().saturating_sub(1);
    }
    p.top = 0;
    p.scroll_to_selection();
}

/// Truncate to `max` display chars, keeping the head and marking the cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Friendly key label for the footer: "Escape" reads as "Esc".
fn keyname(k: &str) -> &str {
    match k {
        "Escape" => "Esc",
        other => other,
    }
}

fn pick_render(p: &mut Picker) {
    // Ask for the size the content wants; the mode-resize that follows
    // carries the size the window actually allowed.
    let want = p.wanted_size();
    if want != p.sized && mode_resize(p.mode, want.0, want.1).is_ok() {
        p.sized = want;
    }
    let w = p.width as usize;
    let mut out = String::from("\x1b[2J\x1b[H");
    out.push_str(&format!(
        "\x1b[1m saved states\x1b[0m \x1b[2m({} on disk)\x1b[0m\r\n",
        p.snaps.len()
    ));
    // Filter line, with a block cursor at the end of the typed text.
    out.push_str(&format!(
        "  \x1b[2mfilter\x1b[0m {}\x1b[7m \x1b[0m\r\n",
        p.filter
    ));
    let rule = "─".repeat(w.saturating_sub(2));
    out.push_str(&format!("  \x1b[2m{rule}\x1b[0m\r\n"));

    if p.view.is_empty() {
        out.push_str("  \x1b[2m(no saved states)\x1b[0m\r\n");
    } else {
        let vh = p.view.len().min(LIST_MAX);
        for vi in p.top..(p.top + vh).min(p.view.len()) {
            let s = &p.snaps[p.view[vi]];
            let cur = vi == p.sel;
            let marker = if cur { "▸" } else { " " };
            let dot = if s.live { "●" } else { "·" };
            let age = if s.saved_at_ms > 0 {
                fmt_age(p.now_ms.saturating_sub(s.saved_at_ms) / 1000)
            } else {
                "unknown".to_string()
            };
            let names = if s.names.is_empty() {
                "(empty)".to_string()
            } else {
                s.names.join(", ")
            };
            let meta = format!("{} sess / {} panes", s.sessions, s.panes);
            let line = format!(
                "{marker} {dot} {age:>9}  {meta:<18}  {names}"
            );
            let line = clip(&line, w.saturating_sub(1));
            if cur {
                out.push_str(&format!(
                    "\x1b[7m{line:<pad$}\x1b[0m\r\n",
                    pad = w.saturating_sub(1)
                ));
            } else {
                let dim = if s.live { "" } else { "\x1b[2m" };
                let end = if s.live { "" } else { "\x1b[0m" };
                out.push_str(&format!("{dim}{line}{end}\r\n"));
            }
        }
    }

    // The message line: a confirm prompt, a transient status, or blank.
    out.push_str("\r\n");
    if let Some(c) = &p.confirm {
        let msg = match c {
            Confirm::Delete(i) => {
                let f = p.snaps.get(*i).map(|s| s.file.as_str()).unwrap_or("?");
                format!("delete {f}? y/n")
            }
            Confirm::Kill => {
                "save & KILL server — processes die, restore later. y/n"
                    .to_string()
            }
            Confirm::Restart => {
                "save & restart server — processes stay live. y/n"
                    .to_string()
            }
        };
        out.push_str(&format!("  \x1b[33m{}\x1b[0m\r\n", clip(&msg, w - 4)));
    } else if let Some(msg) = &p.status {
        out.push_str(&format!("  \x1b[36m{}\x1b[0m\r\n", clip(msg, w - 4)));
    } else {
        out.push_str("\r\n");
    }

    let k = &p.keys;
    let footer = format!(
        "{} restore · {} del · {} save+kill · {} save+restart · {} close",
        keyname(&k.restore),
        keyname(&k.delete),
        keyname(&k.kill),
        keyname(&k.restart),
        keyname(&k.close),
    );
    out.push_str(&format!("  \x1b[2m{}\x1b[0m", clip(&footer, w - 4)));
    let _ = mode_write(p.mode, out.as_bytes());
}

/// Open the picker float in the pressing client's current window.
async fn pick_open(
    picker: Rc<RefCell<Option<Picker>>>,
    keys: PickKeys,
    client: Option<u64>,
) {
    let window = client
        .and_then(|cid| {
            list_clients().ok()?.into_iter().find(|c| u64::from(c.id) == cid)
        })
        .and_then(|c| c.session)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window);
    let Some(window) = window else {
        let _ = display_message("resurrect: no client to open the picker");
        return;
    };
    let snaps = enumerate().await;
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: PICK_WIDTH,
        height: PICK_HEIGHT,
        title: Some("resurrect".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!(
                "resurrect: cannot open picker: {}",
                e.message
            ));
            return;
        }
    };
    let mut p = Picker {
        mode,
        width: PICK_WIDTH,
        height: PICK_HEIGHT,
        sized: (PICK_WIDTH, PICK_HEIGHT),
        snaps,
        view: Vec::new(),
        sel: 0,
        top: 0,
        filter: String::new(),
        now_ms: now_ms(),
        confirm: None,
        keys,
        status: None,
    };
    pick_refilter(&mut p);
    pick_render(&mut p);
    *picker.borrow_mut() = Some(p);
}

/// Restore the picked snapshot. Close the float first, so it never lands
/// in whatever the restore rebuilds.
async fn pick_restore(
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
    file: String,
) {
    busy.set(true);
    {
        let b = picker.borrow();
        if let Some(p) = b.as_ref() {
            let _ = mode_close(p.mode);
        }
    }
    match do_restore(&file).await {
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
    busy.set(false);
}

/// Delete the snapshot file, then refresh the list in place.
async fn pick_delete(
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
    file: String,
) {
    busy.set(true);
    let result = fs_remove(&file).await;
    {
        let mut b = picker.borrow_mut();
        if let Some(p) = b.as_mut() {
            match result {
                Ok(()) => {
                    p.snaps.retain(|s| s.file != file);
                    p.status = Some(format!("deleted {file}"));
                    pick_refilter(p);
                }
                Err(e) => {
                    p.status =
                        Some(format!("delete failed: {}", e.message));
                }
            }
            pick_render(p);
        }
    }
    busy.set(false);
}

/// Save, then kill the server. Close the float and let its pane go
/// before the capture, so the picker never saves itself.
async fn pick_kill(
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
    keep: u32,
) {
    busy.set(true);
    {
        let b = picker.borrow();
        if let Some(p) = b.as_ref() {
            let _ = mode_close(p.mode);
        }
    }
    // Let the float's pane tear down before list_panes runs.
    let _ = sleep_ms(80).await;
    save(true, keep).await;
    busy.set(false);
}

/// Save a safety snapshot, then restart the server in place. The execve
/// handoff keeps every pane process alive and picks up the rebuilt
/// binary at the same path.
async fn pick_restart(
    picker: Rc<RefCell<Option<Picker>>>,
    busy: Rc<Cell<bool>>,
    keep: u32,
) {
    busy.set(true);
    {
        let b = picker.borrow();
        if let Some(p) = b.as_ref() {
            let _ = mode_close(p.mode);
        }
    }
    let _ = sleep_ms(80).await;
    match do_save(keep).await {
        Ok((s, pn)) => {
            let _ = display_message(&format!(
                "resurrect: saved {s} sessions / {pn} panes; restarting (processes stay live)"
            ));
        }
        Err(e) => {
            let _ = display_message(&format!(
                "resurrect: save failed: {e}; restarting anyway"
            ));
        }
    }
    // The completion never arrives (the process execs); the await parks.
    let _ = run_command("restart-server").await;
    busy.set(false);
}

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

/// Timer-driven save: quiet on success (a toast every period is
/// noise), loud on failure.
async fn autosave(keep: u32) {
    if let Err(e) = do_save(keep).await {
        let _ =
            display_message(&format!("resurrect: autosave failed: {e}"));
    }
}

async fn save(kill: bool, keep: u32) {
    match do_save(keep).await {
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

async fn do_save(keep: u32) -> Result<(usize, usize), String> {
    let sessions = list_sessions().map_err(|e| e.to_string())?;
    let windows = list_windows().map_err(|e| e.to_string())?;
    let panes = list_panes().map_err(|e| e.to_string())?;
    let window_by_id: std::collections::HashMap<u32, &WindowInfo> =
        windows.iter().map(|w| (w.id, w)).collect();
    let pane_by_id: std::collections::HashMap<u32, &PaneInfo> =
        panes.iter().map(|p| (p.id, p)).collect();

    let mut out = SaveFile {
        version: 2,
        saved_at_ms: now_ms(),
        sessions: Vec::new(),
    };
    let mut blobs: Vec<u8> = Vec::new();
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
                sw.panes.push(save_pane(p, &mut blobs)?);
                npanes += 1;
            }
            saved.windows.push(sw);
        }
        out.sessions.push(saved);
    }

    let meta =
        serde_json::to_vec(&out).map_err(|e| format!("serialize: {e}"))?;
    let mut file = Vec::with_capacity(12 + meta.len() + blobs.len());
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    file.extend_from_slice(&meta);
    file.extend_from_slice(&blobs);
    let mut first = true;
    for chunk in file.chunks(WRITE_CHUNK) {
        fs_write(STATE_TMP, chunk.to_vec(), !first)
            .await
            .map_err(|e| format!("{STATE_TMP}: {e}"))?;
        first = false;
    }
    publish(keep).await?;
    Ok((out.sessions.len(), npanes))
}

/// Make the temp file the live snapshot. Plain rename is the atomic
/// replace; the exchange path keeps `state.bin` present at every
/// instant while the previous snapshot rotates into the archive chain.
async fn publish(keep: u32) -> Result<(), String> {
    let name = |i: u32| format!("state.{i}.bin");
    // `keep` counts snapshots in total: the live state.bin plus
    // keep - 1 archives. Shift the archives oldest-last, dropping the
    // one that falls off the end (its name is simply renamed over).
    for i in (1..keep.saturating_sub(1)).rev() {
        // A hole in the chain is fine: the source may not exist yet.
        let _ = fs_rename(&name(i), &name(i + 1), RenameFlag::Replace).await;
    }
    if keep > 1 {
        match fs_rename(STATE_TMP, STATE_BIN, RenameFlag::Exchange).await {
            Ok(()) => {
                // The temp name now holds the previous snapshot.
                fs_rename(STATE_TMP, &name(1), RenameFlag::Replace)
                    .await
                    .map_err(|e| format!("archive: {}", e.message))
            }
            // No snapshot yet: nothing to exchange with.
            Err(e) if e.code == ErrorCode::NoSuchObject => {
                fs_rename(STATE_TMP, STATE_BIN, RenameFlag::Replace)
                    .await
                    .map_err(|e| format!("publish: {}", e.message))
            }
            Err(e) => Err(format!("publish: {}", e.message)),
        }
    } else {
        fs_rename(STATE_TMP, STATE_BIN, RenameFlag::Replace)
            .await
            .map_err(|e| format!("publish: {}", e.message))
    }
}

fn save_pane(p: &PaneInfo, blobs: &mut Vec<u8>) -> Result<SavedPane, String> {
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
        blob: None,
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

    let start_off = blobs.len() as u64;
    let mut start = -history;
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
        blobs.append(&mut unescape(&buf));
        start = end + 1;
    }
    saved.blob = Some((start_off, blobs.len() as u64 - start_off));
    Ok(saved)
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

async fn restore() {
    match do_restore(STATE_BIN).await {
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

/// Load a snapshot's metadata from `file`. Returns the state plus the
/// absolute offset of the blob region (0 for a v1 state.json save, which
/// has no blobs). Only state.bin falls back to the v1 layout.
async fn read_state(file: &str) -> Result<(SaveFile, u64), String> {
    let hdr = match fs_read(file, 0, 12).await {
        Ok((hdr, _)) => hdr,
        Err(e) if e.code == ErrorCode::NoSuchObject => {
            // No container yet: fall back to v1, but only for state.bin.
            if file == STATE_BIN {
                return read_state_v1().await.map(|s| (s, 0));
            }
            return Err(format!("{file}: {e}"));
        }
        Err(e) => return Err(format!("{file}: {e}")),
    };
    if hdr.len() != 12 || &hdr[0..8] != MAGIC {
        return Err(format!("{file} is unreadable (bad header); save again"));
    }
    let meta_len =
        u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize;
    let mut meta = Vec::with_capacity(meta_len);
    let mut offset = 12u64;
    while meta.len() < meta_len {
        let want = (meta_len - meta.len()).min(WRITE_CHUNK);
        let (page, eof) = fs_read(file, offset, want)
            .await
            .map_err(|e| format!("{file}: {e}"))?;
        if page.is_empty() || (eof && meta.len() + page.len() < meta_len) {
            return Err(format!("{file} is truncated; save again"));
        }
        offset += page.len() as u64;
        meta.extend_from_slice(&page);
    }
    let state = serde_json::from_slice(&meta)
        .map_err(|e| format!("{file} is unreadable ({e}); save again"))?;
    Ok((state, 12 + meta_len as u64))
}

async fn read_state_v1() -> Result<SaveFile, String> {
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

/// Copy one blob out of the snapshot `src` into `dest`, so the restore
/// wrapper can cat it. `dest` is a transient restore artifact, not part
/// of the snapshot.
async fn extract_blob(
    src: &str,
    mut off: u64,
    len: u64,
    dest: &str,
) -> Result<(), String> {
    let mut left = len as usize;
    let mut first = true;
    loop {
        let want = left.min(WRITE_CHUNK);
        if want == 0 && !first {
            break;
        }
        // A zero-length blob still writes the file (empty pane).
        let page = if want == 0 {
            Vec::new()
        } else {
            let (page, _) = fs_read(src, off, want)
                .await
                .map_err(|e| format!("{dest}: {e}"))?;
            if page.is_empty() {
                return Err(format!("{dest}: {src} is truncated"));
            }
            page
        };
        off += page.len() as u64;
        left -= page.len();
        fs_write(dest, page, !first)
            .await
            .map_err(|e| format!("{dest}: {e}"))?;
        first = false;
        if left == 0 {
            break;
        }
    }
    Ok(())
}

async fn do_restore(file: &str) -> Result<(usize, usize), String> {
    let (mut state, blob_base) = read_state(file).await?;
    for session in &mut state.sessions {
        for window in &mut session.windows {
            for pane in &mut window.panes {
                let Some((off, len)) = pane.blob else { continue };
                let dest = format!("pane-{}.txt", pane.id);
                extract_blob(file, blob_base + off, len, &dest).await?;
                pane.content = Some(dest);
            }
        }
    }
    let state = state;
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

fn fmt_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h{:02}m ago", secs / 3600, (secs % 3600) / 60)
    }
}

async fn status() {
    match read_state(STATE_BIN).await {
        Ok((state, _)) => {
            let panes: usize =
                state.sessions.iter().flat_map(|s| &s.windows)
                    .map(|w| w.panes.len())
                    .sum();
            let names: Vec<&str> = state
                .sessions
                .iter()
                .map(|s| s.name.as_str())
                .collect();
            let age = if state.saved_at_ms > 0 {
                let secs =
                    now_ms().saturating_sub(state.saved_at_ms) / 1000;
                format!(", saved {}", fmt_age(secs))
            } else {
                String::new()
            };
            let _ = display_message(&format!(
                "resurrect: save holds {} sessions ({}) / {panes} panes{age}",
                state.sessions.len(),
                names.join(", ")
            ));
        }
        Err(e) => {
            let _ = display_message(&format!("resurrect: {e}"));
        }
    }
}
