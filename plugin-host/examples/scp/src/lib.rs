//! SDK example plugin: copy files between this machine and the hosts
//! behind the remote session links, with `scp` - and the image on the
//! clipboard to any of them.
//!
//! Two tabs, `C-v` switches:
//!
//! **files** - four fields: where the source is (`from`: `local` or a
//! host), its path, where it goes (`to`), and the path there. The host
//! fields are dropdowns: Tab lists `local` and every host with a
//! `remote-attach` link (plus any in the `hosts` config), and cycles
//! through them; any other ssh host can be typed in. The path fields
//! complete: on `local` from a listing of the directory being typed in,
//! on a host from one `ls` over ssh. A directory row ends in `/`, so
//! taking it and pressing Tab again steps into it. `C-t` swaps the two
//! sides.
//!
//! **clipboard** - what is on the clipboard (`Image (2050x1426)`, or
//! `Text (1.2 KB)` with its first lines shown under the form), a `to`
//! host and a path. The host writes it as a file - a PNG, or UTF-8 text -
//! into the plugin's data directory when the form opens (`clipboard_read`: the
//! macOS pasteboard read natively, a few milliseconds, nothing through
//! the guest; elsewhere a script with wl-paste, xclip or osascript); it
//! is previewed under the fields (see below), and on Enter moved to a
//! local path or sent with `scp` to a
//! host. A path ending in `/` gets `clipboard.png` or `clipboard.txt`
//! appended; the two kinds remember their paths separately.
//!
//! Enter runs `scp -r` (`-3` when both sides are hosts) in a popup on
//! the pressing client, so scp's own progress shows; a copy that fails
//! leaves the popup open with the error, Escape closes it. With the
//! config `run = "job"` the copy runs in the background instead and the
//! form waits with "copying…", showing a failure in place. A local
//! destination for the clipboard is a plain move, no popup.
//!
//! The form opens prefilled from the pane it was called on: a pane in a
//! mirrored session starts with `from` set to that host and the pane's
//! remote cwd, a local pane with its cwd and `to` set to the first
//! connected host. Destination paths are remembered across opens (the
//! last one entered on each tab, in `scp.json` in the data directory),
//! and the clipboard tab remembers its host too.
//!
//! The preview uses the kitty graphics protocol with Unicode
//! placeholders: one sequence through a `DCS tmux;` passthrough names
//! the PNG's path (`t=f`: the terminal, on this machine, reads the file
//! itself, so the image never passes through tmux; the script fallback
//! sends a downscaled copy as data instead) and places it virtually;
//! the block under the fields is `U+10EEEE` cells whose colour and
//! diacritics name the image and the
//! cell - ordinary text cells to tmux, an image to a terminal that
//! implements this part of the protocol (kitty, Ghostty). Elsewhere the
//! block is blank. The passthrough needs a tmux that routes a plugin
//! mode's passthrough to its clients (`input_set_passthrough`).
//!
//! Wire keys to it in ~/.tmux.conf:
//!
//! ```tmux
//! bind t plugin-command scp copy        # files; clipboard when an image is there
//! bind T plugin-command scp clipboard   # the clipboard tab
//! ```
//!
//! Load server-scoped with caps `mode`, `run-process`, `run-command`,
//! `fs-list`, `fs-read-any` (the last three are `formkit::complete::CAPS`),
//! `fs-read` and `fs-write` (the clipboard image and the remembered
//! paths, in the data directory) and `clipboard` (the host's read of
//! it). Role `view`: nothing here needs to run on the
//! remote, so the manifest entry says `role = "view"` and the link does
//! not push it.
//!
//! Config (all strings): `run` = `popup` (default) or `job`; `args` =
//! extra scp flags (default `-r`); `hosts` = comma-separated ssh hosts
//! to offer besides the linked ones.
//!
//! Build: cargo build -p scp --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::rc::Rc;

use base64::Engine;
use formkit::complete::Source;
use formkit::form::{self, field, Action, Field, Form, Model, Shared};
use formkit::text::{expand_home, last_line, quote, scan_base};
use tmux_plugin_sdk::prelude::*;
use tmux_plugin_sdk::abi::ErrorCode;

/// Form geometry (cells). The height is the closed form; an open list
/// adds a rule and up to `LIST_MAX` rows through `mode_resize`, and a
/// preview adds its rows.
const FORM_WIDTH: u32 = 76;
const FORM_HEIGHT: u32 = 12;

/// The word that stands for this machine in a host field. An empty host
/// field means the same.
const LOCAL: &str = "local";

/// Field indices, files tab. Two fields share the label `path`, so the
/// form is addressed by position, never by label.
const FROM: usize = 0;
const FROM_PATH: usize = 1;
const TO: usize = 2;
const TO_PATH: usize = 3;

/// Field indices, clipboard tab.
const CLIP: usize = 0;
const CLIP_TO: usize = 1;
const CLIP_PATH: usize = 2;

/// The key that switches tabs.
const KIND_KEY: &str = "C-v";

/// Where the clipboard goes when nothing was ever entered, and the file
/// name a directory path gets.
const DEFAULT_CLIP_PATH: &str = "/tmp/clipboard.png";
const CLIP_NAME: &str = "clipboard.png";
/// The same for text on the clipboard.
const DEFAULT_CLIP_TEXT_PATH: &str = "/tmp/clipboard.txt";
const CLIP_TEXT_NAME: &str = "clipboard.txt";
/// Lines of clipboard text shown under the form, and how much of the
/// file is read for them.
const TEXT_PREVIEW_ROWS: usize = 8;
const TEXT_PREVIEW_BYTES: usize = 16 * 1024;

/// Remembered destinations, in the data directory.
const STATE_FILE: &str = "scp.json";

/// The preview block: at most this many rows, and the form's width less
/// the indent.
const PREVIEW_MAX_ROWS: u32 = 14;
const PREVIEW_MAX_COLS: u32 = FORM_WIDTH - 4;
/// The preview PNG's longest side, in pixels. Keeps the transmit to a
/// few hundred KB however large the screenshot.
const PREVIEW_PX: u32 = 900;
/// Cell height over cell width when the window cannot say.
const DEFAULT_CELL_ASPECT: f64 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    /// `display-popup -EE` on the pressing client: progress shows, a
    /// failure stays on screen.
    Popup,
    /// `run_job` behind the form: the form waits, a failure renders in it.
    Job,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Files,
    Clipboard,
}

#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    hosts: Option<String>,
}

/// What was entered last time, per tab. Written after a copy is sent.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug)]
struct Remembered {
    #[serde(default)]
    to_path: Option<String>,
    #[serde(default)]
    clip_to: Option<String>,
    #[serde(default)]
    clip_path: Option<String>,
    #[serde(default)]
    clip_text_path: Option<String>,
}

impl Remembered {
    fn load() -> Remembered {
        let mut buf = Vec::with_capacity(4096);
        match fs_read_sync(STATE_FILE, 0, &mut buf) {
            Ok(_) => serde_json::from_slice(&buf).unwrap_or_default(),
            Err(_) => Remembered::default(),
        }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            if let Err(e) = fs_write_sync(STATE_FILE, &json, false) {
                log(&format!("scp: could not save {STATE_FILE}: {}", e.message));
            }
        }
    }
}

/// A host a host field can offer, and whether its link is up right now
/// (a typed-in host is not in this list at all).
#[derive(Clone, Debug)]
struct Host {
    name: String,
    /// `None` for a host from the config: no link to ask.
    connected: Option<bool>,
}

fn is_local(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v == LOCAL
}

/// The clipboard image, written to a file when the form opened. The
/// files live in the plugin's data directory: the host's file calls want
/// them by name, relative to it; the shell wants the absolute path.
#[derive(Clone, Debug)]
struct Clip {
    /// The data directory.
    root: String,
    /// The PNG as it was on the clipboard: what gets copied, and what the
    /// terminal reads for the preview when it can (`local`).
    name: String,
    /// The host wrote the file (`clipboard_image`): the terminal is on
    /// this machine and reads the file itself. `false` for the fallback
    /// (wl-paste, xclip, osascript from a script), where a downscaled
    /// copy in `preview_name` is sent as data instead.
    local: bool,
    preview_name: Option<String>,
    /// An image's pixel size; zero for text.
    width: u32,
    height: u32,
    bytes: u64,
    /// Text: its first lines, ready to draw under the form. `None` for
    /// an image.
    text: Option<Vec<String>>,
}

impl Clip {
    fn path(&self) -> String {
        format!("{}/{}", self.root, self.name)
    }

    fn is_text(&self) -> bool {
        self.text.is_some()
    }

    /// The clipboard field's text: what is there, and how big.
    fn label(&self) -> String {
        if self.is_text() {
            format!("Text ({})", human_bytes(self.bytes))
        } else {
            format!("Image ({}x{})", self.width, self.height)
        }
    }

    /// The file name a directory destination gets.
    fn default_name(&self) -> &'static str {
        if self.is_text() { CLIP_TEXT_NAME } else { CLIP_NAME }
    }

    fn preview(&self) -> Option<String> {
        self.preview_name.as_ref().map(|n| format!("{}/{}", self.root, n))
    }
}

#[derive(Clone, Debug)]
enum ClipState {
    Probing,
    /// Nothing usable, or no way to read it: why.
    Missing(String),
    Ready(Clip),
}

/// The preview as the terminal knows it, once transmitted.
#[derive(Clone, Copy, Debug)]
struct Preview {
    /// The kitty image id: the lower 24 bits ride in the placeholder's
    /// foreground colour.
    id: u32,
    cols: u32,
    rows: u32,
    /// The transmit reached the mode. Until then the block is not drawn.
    sent: bool,
}

/// What the copy form knows: the hosts to offer, the client to open the
/// popup on, how to run, which tab is up and what the clipboard holds.
struct Copier {
    hosts: Vec<Host>,
    client_name: Option<String>,
    run: Run,
    args: String,
    kind: Kind,
    clip: ClipState,
    preview: Option<Preview>,
    /// Cell height over cell width, for the preview's shape.
    cell_aspect: f64,
    remembered: Remembered,
    /// The other tab's fields, kept as they were for the switch back.
    stash: Vec<Field>,
}

impl Copier {
    fn host_source(&self) -> Source {
        let mut words = vec![(LOCAL.to_string(), "this machine".to_string())];
        for h in &self.hosts {
            let meta = match h.connected {
                Some(true) => "linked, connected",
                Some(false) => "linked, disconnected",
                None => "ssh",
            };
            words.push((h.name.clone(), meta.to_string()));
        }
        Source::Choice { title: "hosts".into(), words }
    }

    fn path_source(&self, fields: &[Field], host_field: usize, i: usize) -> Source {
        let host = fields[host_field].value.trim().to_string();
        let base = scan_base(&fields[i].value);
        if is_local(&host) {
            Source::Files { base }
        } else {
            Source::Remote { host, base }
        }
    }

    fn host_mark(&self, v: &str) -> String {
        let v = v.trim();
        let state = if is_local(v) {
            String::new()
        } else {
            match self.hosts.iter().find(|h| h.name == v) {
                Some(Host { connected: Some(true), .. }) => " \x1b[32m●\x1b[0m".to_string(),
                Some(Host { connected: Some(false), .. }) => {
                    " \x1b[31m○ link down\x1b[0m".to_string()
                }
                _ => " \x1b[2mssh\x1b[0m".to_string(),
            }
        };
        format!("\x1b[2m▾\x1b[0m{state}")
    }
}

impl Model for Copier {
    fn source(&self, fields: &[Field], i: usize) -> Option<Source> {
        match (self.kind, i) {
            (Kind::Files, FROM | TO) => Some(self.host_source()),
            (Kind::Files, FROM_PATH | TO_PATH) => Some(self.path_source(fields, i - 1, i)),
            (Kind::Clipboard, CLIP_TO) => Some(self.host_source()),
            (Kind::Clipboard, CLIP_PATH) => Some(self.path_source(fields, CLIP_TO, i)),
            _ => None,
        }
    }

    /// Nothing follows anything: a destination is the user's to say.
    fn mirror(&mut self, _fields: &mut [Field]) {}

    fn title(&self) -> String {
        "Copy Files".into()
    }

    fn kinds(&self) -> Option<(Vec<&'static str>, usize)> {
        let active = match self.kind {
            Kind::Files => 0,
            Kind::Clipboard => 1,
        };
        Some((vec!["files", "clipboard"], active))
    }

    fn toggle_hint(&self) -> Option<String> {
        match self.kind {
            Kind::Files => Some("swap".into()),
            Kind::Clipboard => None,
        }
    }

    fn extra_hint(&self) -> Option<String> {
        Some(match self.kind {
            Kind::Files => format!("{KIND_KEY} clipboard"),
            Kind::Clipboard => format!("{KIND_KEY} files"),
        })
    }

    fn banner(&self, _fields: &[Field]) -> Option<String> {
        match (&self.kind, &self.clip) {
            (Kind::Clipboard, ClipState::Missing(why)) => Some(format!("{why} — {KIND_KEY} for files")),
            _ => None,
        }
    }

    fn mark(&self, fields: &[Field], i: usize) -> Option<String> {
        match (self.kind, i) {
            (Kind::Files, FROM | TO) | (Kind::Clipboard, CLIP_TO) => {
                Some(self.host_mark(&fields[i].value))
            }
            _ => None,
        }
    }

    fn status(&self, fields: &[Field], busy: bool) -> Option<String> {
        if busy {
            let local = self.kind == Kind::Clipboard && is_local(&fields[CLIP_TO].value);
            return Some(if local { "saving…" } else { "copying…" }.to_string());
        }
        match (self.kind, &self.clip) {
            (Kind::Clipboard, ClipState::Probing) => Some("reading the clipboard…".to_string()),
            (Kind::Clipboard, ClipState::Ready(_)) => {
                match self.preview {
                    Some(p) if !p.sent => Some("preparing the preview…".to_string()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn submit_label(&self) -> &'static str {
        "copy"
    }

    fn footer_rows(&self, _fields: &[Field]) -> u32 {
        if self.kind != Kind::Clipboard {
            return 0;
        }
        if let ClipState::Ready(Clip { text: Some(lines), .. }) = &self.clip {
            return lines.len() as u32;
        }
        match self.preview {
            Some(p) if p.sent => p.rows,
            _ => 0,
        }
    }

    fn footer(&self, _fields: &[Field], rows: u32) -> Vec<String> {
        if let ClipState::Ready(Clip { text: Some(lines), .. }) = &self.clip {
            return lines.iter().take(rows as usize).map(|l| format!("\x1b[2m{l}\x1b[0m")).collect();
        }
        let Some(p) = self.preview.filter(|p| p.sent) else { return Vec::new() };
        (0..rows.min(p.rows)).map(|row| placeholder_row(p.id, row, p.cols)).collect()
    }
}

/// `345 B`, `1.2 KB`, `3.4 MB`.
fn human_bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// The first lines of clipboard text as the footer draws them: tabs
/// widened, control characters dropped, each line clipped to the form.
fn text_preview(head: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(head);
    let width = (FORM_WIDTH - 4) as usize;
    text.lines()
        .take(TEXT_PREVIEW_ROWS)
        .map(|l| {
            let clean: String = l
                .replace('\t', "    ")
                .chars()
                .filter(|c| !c.is_control())
                .collect();
            if clean.chars().count() > width {
                let mut out: String = clean.chars().take(width.saturating_sub(1)).collect();
                out.push('…');
                out
            } else {
                clean
            }
        })
        .collect()
}

/// The clipboard tab's path field takes the remembered path for what the
/// clipboard turned out to hold, unless the user already typed one.
fn prefill_clip_path(form: &mut Form<Copier>, text: bool) {
    let want = if text {
        form.model.remembered.clip_text_path.clone().unwrap_or_else(|| DEFAULT_CLIP_TEXT_PATH.to_string())
    } else {
        form.model.remembered.clip_path.clone().unwrap_or_else(|| DEFAULT_CLIP_PATH.to_string())
    };
    let fields = match form.model.kind {
        Kind::Clipboard => &mut form.fields,
        Kind::Files => &mut form.model.stash,
    };
    if let Some(f) = fields.get_mut(CLIP_PATH) {
        if !f.touched {
            f.value = want;
        }
    }
}

type State = Shared<Copier>;

struct Scp {
    state: State,
    run: Run,
    args: String,
    extra_hosts: Vec<String>,
}

/// Every host with a link, connected or not, in session order and
/// without repeats (two links to one host are one row).
fn linked_hosts() -> Vec<Host> {
    let mut hosts: Vec<Host> = Vec::new();
    for s in list_sessions().unwrap_or_default() {
        let Ok(line) = format_expand(
            OptionTarget::Session(SessionId(s.id)),
            "#{remote_host}\t#{remote_connected}",
        ) else {
            continue;
        };
        let mut it = line.split('\t');
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let up = it.next().unwrap_or("").trim() == "1";
        match hosts.iter_mut().find(|h| h.name == name) {
            // Any live link makes the host reachable.
            Some(h) => h.connected = Some(h.connected.unwrap_or(false) || up),
            None => hosts.push(Host { name: name.to_string(), connected: Some(up) }),
        }
    }
    hosts
}

/// A path field's prefill: the directory with a trailing `/`, so the
/// first Tab lists what is in it.
fn dir_prefill(cwd: &str) -> String {
    if cwd.is_empty() {
        String::new()
    } else {
        format!("{}/", cwd.trim_end_matches('/'))
    }
}

/// Cell height over cell width for a window, from the terminal's report;
/// the default when it has none.
fn cell_aspect(window: u32) -> f64 {
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

/// Context gathering + form open, run asynchronously after the
/// plugin-command event. `kind` is the tab to open on; with `Files`, an
/// image on the clipboard switches to the clipboard tab when the probe
/// finds one.
async fn open_form(
    state: State,
    run: Run,
    args: String,
    extra_hosts: Vec<String>,
    target_pane: Option<u64>,
    client: Option<u64>,
    kind: Kind,
) {
    let pane = target_pane.and_then(|p| resolve_pane(PaneId(p as u32)).ok());
    let mut hosts = linked_hosts();
    for name in extra_hosts {
        if !hosts.iter().any(|h| h.name == name) {
            hosts.push(Host { name, connected: None });
        }
    }

    // The form must open where the user is looking: the pressing
    // client's current window (the target may be in another session).
    let client_info = client.and_then(|cid| {
        list_clients().ok()?.iter().find_map(|c| {
            (u64::from(c.id) == cid).then(|| (Some(c.name.clone()), c.session))
        })
    });
    let window = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window);
    let Some(window) = window else {
        log("scp: cannot resolve the client's current window");
        let _ = display_message("scp: no client to open the form for");
        return;
    };

    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("copy files".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("scp: mode_open failed: {}", e.message));
            return;
        }
    };

    let remembered = Remembered::load();
    let first_up = hosts
        .iter()
        .find(|h| h.connected == Some(true))
        .map(|h| h.name.clone())
        .unwrap_or_else(|| LOCAL.to_string());

    // A pane in a mirrored session copies FROM its host; a local pane
    // copies TO the first connected host. The destination path is the
    // last one entered.
    let (from, from_path, to) = match &pane {
        Some(p) if p.remote && !p.host.is_empty() => {
            (p.host.clone(), dir_prefill(&p.cwd), LOCAL.to_string())
        }
        Some(p) => (LOCAL.to_string(), dir_prefill(&p.cwd), first_up.clone()),
        None => (LOCAL.to_string(), String::new(), LOCAL.to_string()),
    };
    let files = vec![
        field(LABELS[FROM], from),
        field(LABELS[FROM_PATH], from_path),
        field(LABELS[TO], to),
        field(LABELS[TO_PATH], remembered.to_path.clone().unwrap_or_default()),
    ];
    let clipboard = vec![
        field(CLIP_LABELS[CLIP], "…".to_string()),
        field(CLIP_LABELS[CLIP_TO], remembered.clip_to.clone().unwrap_or(first_up)),
        field(
            CLIP_LABELS[CLIP_PATH],
            remembered.clip_path.clone().unwrap_or_else(|| DEFAULT_CLIP_PATH.to_string()),
        ),
    ];
    let (fields, stash, focused) = match kind {
        Kind::Files => (files, clipboard, FROM_PATH),
        Kind::Clipboard => (clipboard, files, CLIP_PATH),
    };

    let model = Copier {
        hosts,
        client_name: client_info.and_then(|(name, _)| name),
        run,
        args,
        kind,
        clip: ClipState::Probing,
        preview: None,
        cell_aspect: cell_aspect(window),
        remembered,
        stash,
    };
    let mut form = Form::new(mode, FORM_WIDTH, FORM_HEIGHT, fields, model);
    // The side is prefilled; the thing to copy is what is missing.
    form.focused = focused;
    form::render(&mut form);
    *state.borrow_mut() = Some(form);

    form::start_scan(&state, mode, false);
    spawn(probe_clipboard(Rc::clone(&state), mode, kind == Kind::Files));
}

/// The labels, by position. Two fields say `path`: the form is
/// addressed by index, and a swap keeps the labels where they are.
const LABELS: [&str; 4] = ["from", "path", "to", "path"];
const CLIP_LABELS: [&str; 3] = ["clipboard", "to", "path"];

/// C-t on the files tab: the two sides change places, edits and all.
fn swap(form: &mut Form<Copier>) {
    form.fields.swap(FROM, TO);
    form.fields.swap(FROM_PATH, TO_PATH);
    for (f, l) in form.fields.iter_mut().zip(LABELS) {
        f.label = l;
    }
    form.error = None;
    form.list = None;
}

/// C-v: the other tab comes up with its fields as they were left.
fn switch_kind(form: &mut Form<Copier>) {
    form.model.kind = match form.model.kind {
        Kind::Files => Kind::Clipboard,
        Kind::Clipboard => Kind::Files,
    };
    let mut other = std::mem::take(&mut form.model.stash);
    std::mem::swap(&mut form.fields, &mut other);
    form.model.stash = other;
    form.focused = match form.model.kind {
        Kind::Files => FROM_PATH,
        Kind::Clipboard => CLIP_PATH,
    };
    form.error = None;
    form.list = None;
}

/// The clipboard field's text, on whichever side it currently lives.
fn set_clip_label(form: &mut Form<Copier>, text: String) {
    let fields = match form.model.kind {
        Kind::Clipboard => &mut form.fields,
        Kind::Files => &mut form.model.stash,
    };
    if let Some(f) = fields.get_mut(CLIP) {
        f.value = text;
    }
}

/// One side of the copy as scp wants it: `path` here, `host:path` there.
/// A local `~` is expanded here (scp does not); a remote one is left to
/// the remote. An empty remote path is that host's home.
fn endpoint(host: &str, path: &str, home: Option<&str>) -> String {
    if is_local(host) {
        quote(&expand_home(path, home))
    } else {
        quote(&format!("{}:{}", host.trim(), path))
    }
}

/// tmux expands formats in a popup's command; a `#` in a path must not
/// start one.
fn no_formats(s: &str) -> String {
    s.replace('#', "##")
}

// ---------------------------------------------------------------------------
// the clipboard
// ---------------------------------------------------------------------------

/// Read the clipboard image to `$1` as PNG and a downscaled copy to
/// `$2`, the longest side at most `$3` pixels. macOS in ONE process:
/// an `osascript` JavaScript that takes the PNG (or TIFF, re-encoded)
/// straight off the pasteboard as bytes and makes the preview with
/// ImageIO - the AppleScript way (`the clipboard as «class PNGf»`,
/// then `sips`) is two more processes and a hex round trip of the
/// whole image, three times slower on a screenshot. Wayland through
/// wl-paste, X11 through xclip, the preview with ImageMagick when it is
/// there. Exit 1 with a reason on stdout when there is no image, 2 when
/// it could not be read.
const PROBE_SCRIPT: &str = r#"f="$1"; p="$2"; px="$3"
mkdir -p "$(dirname "$f")" 2>/dev/null
if command -v osascript >/dev/null 2>&1; then
  r=$(osascript -l JavaScript -e "$JXA" "$f" "$p" "$px" 2>/dev/null) || { echo "the clipboard could not be read"; exit 2; }
  case "$r" in
    ok*) ;;
    none) echo "no image on the clipboard"; exit 1 ;;
    *) echo "the clipboard image could not be read"; exit 2 ;;
  esac
elif command -v wl-paste >/dev/null 2>&1 && wl-paste --list-types 2>/dev/null | grep -q '^image/png'; then
  wl-paste -t image/png > "$f" 2>/dev/null || { echo "the clipboard image could not be read"; exit 2; }
  { command -v magick >/dev/null 2>&1 && magick "$f" -resize "${px}x${px}>" "$p"; } >/dev/null 2>&1 || cp "$f" "$p"
elif command -v xclip >/dev/null 2>&1 && xclip -selection clipboard -t TARGETS -o 2>/dev/null | grep -q '^image/png'; then
  xclip -selection clipboard -t image/png -o > "$f" 2>/dev/null || { echo "the clipboard image could not be read"; exit 2; }
  { command -v magick >/dev/null 2>&1 && magick "$f" -resize "${px}x${px}>" "$p"; } >/dev/null 2>&1 || cp "$f" "$p"
else
  echo "no image on the clipboard"; exit 1
fi
echo ok
"#;

/// The macOS half of the probe (JavaScript for Automation). Prints
/// `ok`, `none` or `unreadable`. The preview is ImageIO's thumbnail:
/// decoded once, scaled, re-encoded as PNG; a small image comes out as
/// is.
const JXA_SCRIPT: &str = r#"ObjC.import('AppKit'); ObjC.import('ImageIO');
function run(argv) {
  const out = argv[0], prev = argv[1], maxpx = parseInt(argv[2], 10) || 900;
  const pb = $.NSPasteboard.generalPasteboard;
  let data = pb.dataForType('public.png');
  if (data.isNil()) {
    const t = pb.dataForType('public.tiff');
    if (t.isNil()) return 'none';
    const rep = $.NSBitmapImageRep.imageRepWithData(t);
    if (rep.isNil()) return 'unreadable';
    data = rep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
  }
  if (data.isNil()) return 'unreadable';
  if (!data.writeToFileAtomically(out, true)) return 'unwritable';
  const src = $.CGImageSourceCreateWithData(data, $());
  // The option keys by their string values: the ImageIO constants do
  // not bridge into JavaScript, and a dictionary keyed by undefined
  // yields a full-size "thumbnail".
  const opts = $.NSDictionary.dictionaryWithObjectsForKeys(
    $([$(true), $(maxpx), $(true)]),
    $(['kCGImageSourceCreateThumbnailFromImageAlways', 'kCGImageSourceThumbnailMaxPixelSize',
       'kCGImageSourceCreateThumbnailWithTransform']));
  const thumb = $.CGImageSourceCreateThumbnailAtIndex(src, 0, opts);
  const trep = $.NSBitmapImageRep.alloc.initWithCGImage(thumb);
  const png = trep.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
  if (png.isNil() || !png.writeToFileAtomically(prev, true)) data.writeToFileAtomically(prev, true);
  return 'ok';
}"#;

/// A PNG's pixel size, from its IHDR (the first chunk, by the spec).
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    Some((be(16), be(20)))
}

/// The preview block for an image of `w`×`h` pixels: as many rows as
/// allowed, narrower when the image is tall; as wide as allowed, fewer
/// rows when it is wide. `cell_aspect` is cell height over cell width.
fn preview_box(w: u32, h: u32, cell_aspect: f64) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    // Columns per row for the image's shape, in cells.
    let ratio = (w as f64 / h as f64) * cell_aspect;
    let mut rows = PREVIEW_MAX_ROWS;
    let mut cols = (rows as f64 * ratio).round() as u32;
    if cols > PREVIEW_MAX_COLS {
        cols = PREVIEW_MAX_COLS;
        rows = (cols as f64 / ratio).round() as u32;
    }
    (cols.max(1), rows.max(1))
}

/// Read the clipboard off the form's path, then tell the form what was
/// found. `auto_switch` brings the clipboard tab up on an image.
///
/// The host does the read (`clipboard_image`: the pasteboard to a PNG in
/// the data directory, nothing through the guest); a host without a
/// clipboard, or one that did not grant it, gets the script below.
async fn probe_clipboard(state: State, mode: ModeId, auto_switch: bool) {
    let Ok(root) = fs_root() else {
        form::fail(&state, mode, "scp: no data directory for the clipboard image".into());
        return;
    };
    let root = root.trim_end_matches('/').to_string();
    let stamp = now_ms();
    let t0 = stamp;
    let found = match clipboard_read() {
        Ok(Some(Clipboard::Image { name, width, height, bytes })) => Ok(Clip {
            root: root.clone(),
            name,
            local: true,
            preview_name: None,
            width,
            height,
            bytes,
            text: None,
        }),
        Ok(Some(Clipboard::Text { name, bytes })) => {
            let head = fs_read(&name, 0, TEXT_PREVIEW_BYTES).await.map(|(b, _)| b).unwrap_or_default();
            Ok(Clip {
                root: root.clone(),
                name,
                local: true,
                preview_name: None,
                width: 0,
                height: 0,
                bytes,
                text: Some(text_preview(&head)),
            })
        }
        Ok(None) => Err("nothing on the clipboard".to_string()),
        Err(e) if matches!(e.code, ErrorCode::Unsupported | ErrorCode::CapDenied) => {
            probe_by_script(&root).await
        }
        Err(e) => Err(format!("the clipboard could not be read: {}", e.message)),
    };
    let read_ms = now_ms().saturating_sub(t0);
    let image = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else {
            // The form went while the clipboard was read: nothing to show,
            // nothing to keep.
            if let Ok(clip) = &found {
                spawn(remove_clip(clip.clone()));
            }
            return;
        };
        match found {
            Ok(clip) => {
                set_clip_label(form, clip.label());
                prefill_clip_path(form, clip.is_text());
                let image = if clip.is_text() {
                    None
                } else {
                    let (cols, rows) = preview_box(clip.width, clip.height, form.model.cell_aspect);
                    let id = ((stamp & 0x00ff_ffff) as u32).max(1);
                    form.model.preview = Some(Preview { id, cols, rows, sent: false });
                    Some((id, cols, rows))
                };
                form.model.clip = ClipState::Ready(clip.clone());
                // An image is worth switching for; text is always there.
                if image.is_some() && auto_switch && form.model.kind == Kind::Files {
                    switch_kind(form);
                }
                form::render(form);
                match image {
                    Some((id, cols, rows)) => Some((clip, id, cols, rows)),
                    None => {
                        log(&format!(
                            "scp: clipboard text ({}) read in {read_ms} ms",
                            human_bytes(clip.bytes)
                        ));
                        None
                    }
                }
            }
            Err(why) => {
                set_clip_label(form, "none".to_string());
                form.model.clip = ClipState::Missing(why);
                form::render(form);
                None
            }
        }
    };
    if let Some((clip, id, cols, rows)) = image {
        form::start_scan(&state, mode, false);
        let t1 = now_ms();
        let how = transmit_preview(state, mode, &clip, id, cols, rows).await;
        log(&format!(
            "scp: clipboard {}x{} read in {read_ms} ms; preview {how} in {} ms as {cols}x{rows} cells",
            clip.width,
            clip.height,
            now_ms().saturating_sub(t1)
        ));
    } else if !matches!(
        state.borrow().as_ref().map(|f| &f.model.clip),
        Some(ClipState::Ready(_))
    ) {
        log(&format!("scp: clipboard probe took {read_ms} ms: nothing usable"));
    }
}

/// The fallback read, as a shell job: the script writes the PNG and a
/// downscaled copy into the data directory.
async fn probe_by_script(root: &str) -> Result<Clip, String> {
    let stamp = now_ms();
    let written = Clip {
        root: root.to_string(),
        name: format!("clip-{stamp}.png"),
        local: false,
        preview_name: Some(format!("clip-{stamp}.preview.png")),
        width: 0,
        height: 0,
        bytes: 0,
        text: None,
    };
    // The JavaScript rides in the environment: no quoting of a quoted
    // script inside a quoted script.
    let cmd = format!(
        "JXA={} sh -c {} sh {} {} {}",
        quote(JXA_SCRIPT),
        quote(PROBE_SCRIPT),
        quote(&written.path()),
        quote(written.preview().as_deref().unwrap_or("")),
        PREVIEW_PX
    );
    match run_job(&cmd, None).await {
        Ok(out) if out.status == 0 => match fs_read(&written.name, 0, 64).await {
            Ok((head, _)) => match png_size(&head) {
                Some((width, height)) => Ok(Clip { width, height, ..written }),
                None => {
                    spawn(remove_clip(written));
                    Err("the clipboard image could not be read".to_string())
                }
            },
            Err(e) => {
                // The files are there but unreadable to us (a grant
                // missing): say so in the log, not just the form.
                log(&format!("scp: cannot read {}: {}", written.path(), e.message));
                spawn(remove_clip(written));
                Err("the clipboard image could not be read".to_string())
            }
        },
        Ok(out) => Err(last_line(&out.output, "no image on the clipboard")),
        Err(e) => Err(format!("the clipboard could not be read: {}", e.message)),
    }
}

/// Delete the clipboard files. Best effort, off the caller's path.
async fn remove_clip(clip: Clip) {
    if let Err(e) = fs_remove(&clip.name).await {
        log(&format!("scp: cannot remove {}: {}", clip.path(), e.message));
    }
    if let Some(p) = &clip.preview_name {
        let _ = fs_remove(p).await;
    }
}

// ---------------------------------------------------------------------------
// the preview: kitty graphics through a tmux passthrough
// ---------------------------------------------------------------------------

/// The placeholder cell, and the diacritics that number its row and
/// column (the protocol's `rowcolumn-diacritics.txt`, in order; enough
/// for the widest block the form draws).
const PLACEHOLDER: char = '\u{10EEEE}';
const DIACRITICS: [char; 80] = [
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
];

/// One row of the placeholder block: the image id as the foreground
/// colour, then a cell per column carrying its row and column.
fn placeholder_row(id: u32, row: u32, cols: u32) -> String {
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
fn passthrough(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

/// The transmit by file: the terminal is on this machine and reads the
/// PNG itself (`t=f`), so the image never passes through tmux; one
/// sequence carries the path (base64) and the virtual placement of
/// `cols`×`rows` for the placeholders to show.
fn kitty_transmit_file(id: u32, path: &str, cols: u32, rows: u32) -> String {
    let p = base64::engine::general_purpose::STANDARD.encode(path.as_bytes());
    passthrough(&format!("\x1b_Ga=T,t=f,f=100,i={id},U=1,c={cols},r={rows},q=2;{p}\x1b\\"))
}

/// The transmit by data: the PNG, base64, in chunks of 4096, the first
/// carrying the image's keys and the last `m=0`; then a virtual
/// placement of `cols`×`rows` for the placeholders to show. For a
/// terminal that cannot read our files.
fn kitty_transmit(id: u32, png: &[u8], cols: u32, rows: u32) -> Vec<String> {
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

/// Tell the terminal to forget the image: on close, so a form's worth
/// of previews does not pile up in it.
fn kitty_delete(id: u32) -> String {
    passthrough(&format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\"))
}

/// A mode write takes at most this much at once (the host's cap, with
/// room to spare); a transmit goes in as many writes as it needs.
const WRITE_MAX: usize = 200 * 1024;

/// Hand the preview to the terminal through the mode, then draw the
/// block. By path when the file is local to the terminal (a hundred
/// bytes), else the downscaled copy as data. Returns a word for the log.
async fn transmit_preview(
    state: State,
    mode: ModeId,
    clip: &Clip,
    id: u32,
    cols: u32,
    rows: u32,
) -> String {
    let how = if clip.local {
        let seq = kitty_transmit_file(id, &clip.path(), cols, rows);
        if mode_write(mode, seq.as_bytes()).is_err() {
            return "not sent".into();
        }
        "placed by path".to_string()
    } else {
        let Some(name) = clip.preview_name.as_deref() else { return "no preview file".into() };
        let Ok((png, _)) = fs_read(name, 0, 8 * 1024 * 1024).await else {
            return "preview unreadable".into();
        };
        if png.is_empty() {
            return "preview empty".into();
        }
        let mut batch = String::new();
        for seq in kitty_transmit(id, &png, cols, rows) {
            if batch.len() + seq.len() > WRITE_MAX {
                if mode_write(mode, batch.as_bytes()).is_err() {
                    return "not sent".into();
                }
                batch.clear();
            }
            batch.push_str(&seq);
        }
        if !batch.is_empty() && mode_write(mode, batch.as_bytes()).is_err() {
            return "not sent".into();
        }
        format!("{} KB sent as data", png.len() / 1024)
    };
    let mut st = state.borrow_mut();
    let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return how };
    if let Some(p) = form.model.preview.as_mut().filter(|p| p.id == id) {
        p.sent = true;
    }
    form::render(form);
    how
}

/// Leave the terminal and the data directory as they were: the image
/// forgotten, the files gone (unless a copy is consuming them). Called
/// with the form still open (the delete goes through the mode) right
/// before it closes; the clip is forgotten too, so the mode-closed event
/// that follows has nothing left to remove.
fn discard(form: &mut Form<Copier>, keep_files: bool) {
    if let Some(p) = form.model.preview.filter(|p| p.sent) {
        let _ = mode_write(form.mode, kitty_delete(p.id).as_bytes());
    }
    if let ClipState::Ready(clip) = &form.model.clip {
        if !keep_files {
            spawn(remove_clip(clip.clone()));
        }
        form.model.clip = ClipState::Missing("the form is closing".to_string());
    }
}

// ---------------------------------------------------------------------------
// submit
// ---------------------------------------------------------------------------

/// Validate and run the copy. A popup takes it from here; a job keeps
/// the form up until it is done.
async fn submit(state: State) {
    let kind = state.borrow().as_ref().map(|f| f.model.kind);
    match kind {
        Some(Kind::Files) => submit_files(state).await,
        Some(Kind::Clipboard) => submit_clipboard(state).await,
        None => {}
    }
}

/// Run `cmd` the configured way. With a popup the form is done here;
/// with a job it waits, and a failure lands in it. `what` names the copy
/// for the message a job's success shows.
async fn run_copy(state: State, mode: ModeId, cmd: String, what: String) {
    let (run, client_name) = {
        let st = state.borrow();
        let Some(form) = st.as_ref() else { return };
        (form.model.run, form.model.client_name.clone())
    };
    match (run, client_name) {
        (Run::Popup, Some(client)) => {
            // The popup is the progress display; the form is done.
            if let Some(form) = state.borrow_mut().as_mut() {
                discard(form, true);
            }
            let _ = mode_close(mode);
            *state.borrow_mut() = None;
            let shell = format!(
                "{cmd} || {{ printf '\\n[scp failed - Escape closes]\\n'; exit 1; }}"
            );
            let popup = format!(
                "display-popup -c {} -EE -w 80% -h 30% -T {} {}",
                quote(&client),
                quote(" scp "),
                quote(&no_formats(&shell))
            );
            if let Err(e) = run_command(&popup).await {
                log(&format!("scp: display-popup failed: {}", e.message));
                let _ = display_message(&format!("scp: could not open the popup: {}", e.message));
            }
        }
        _ => match run_job(&cmd, None).await {
            Ok(out) if out.status == 0 => {
                if let Some(form) = state.borrow_mut().as_mut().filter(|f| f.mode.0 == mode.0) {
                    discard(form, true);
                }
                let _ = mode_close(mode);
                *state.borrow_mut() = None;
                let _ = display_message(&format!("copied: {what}"));
            }
            Ok(out) => form::fail(&state, mode, last_line(&out.output, "scp failed")),
            Err(e) => form::fail(&state, mode, format!("scp could not run: {}", e.message)),
        },
    }
}

async fn submit_files(state: State) {
    let (mode, cmd, what) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut() else { return };
        let from = form.fields[FROM].value.trim().to_string();
        let from_path = form.fields[FROM_PATH].value.trim().to_string();
        let to = form.fields[TO].value.trim().to_string();
        let to_path = form.fields[TO_PATH].value.trim().to_string();
        let err = if from_path.is_empty() {
            Some("a source path is required".to_string())
        } else if is_local(&to) && to_path.is_empty() {
            Some("a destination path is required (a remote one may be empty: its home)".to_string())
        } else if is_local(&from) && is_local(&to) && from_path == to_path {
            Some("source and destination are the same".to_string())
        } else {
            None
        };
        if let Some(err) = err {
            form.error = Some(err);
            form::render(form);
            return;
        }
        let home = form.home.clone();
        let src = endpoint(&from, &from_path, home.as_deref());
        let dst = endpoint(&to, &to_path, home.as_deref());
        // Host to host goes through here (-3): the two need not see
        // each other, and the keys are here.
        let three = if !is_local(&from) && !is_local(&to) { " -3" } else { "" };
        let args = form.model.args.trim();
        let args = if args.is_empty() { String::new() } else { format!(" {args}") };
        let cmd = format!("scp{args}{three} {src} {dst}");
        let what = format!("{} → {}", src.trim_matches('\''), dst.trim_matches('\''));
        form.model.remembered.to_path = Some(to_path);
        form.model.remembered.save();
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        (form.mode, cmd, what)
    };
    run_copy(state, mode, cmd, what).await;
}

/// The clipboard's destination as a file path: a directory (a trailing
/// `/`, or nothing) gets the default file name for what it holds.
fn clip_dest(path: &str, name: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path.ends_with('/') {
        format!("{path}{name}")
    } else {
        path.to_string()
    }
}

/// The directory part of a path, for `mkdir -p`; `.` for a bare name.
fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((dir, _)) => dir.to_string(),
        None => ".".to_string(),
    }
}

async fn submit_clipboard(state: State) {
    let (mode, cmd, what, local) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut() else { return };
        let to = form.fields[CLIP_TO].value.trim().to_string();
        let path = form.fields[CLIP_PATH].value.trim().to_string();
        let clip = match &form.model.clip {
            ClipState::Ready(c) => Some(c.clone()),
            _ => None,
        };
        let err = match (&clip, &form.model.clip) {
            (None, ClipState::Probing) => Some("still reading the clipboard".to_string()),
            (None, _) => Some("nothing on the clipboard to copy".to_string()),
            (Some(_), _) if is_local(&to) && path.is_empty() => {
                Some("a destination path is required (a remote one may be empty: its home)".to_string())
            }
            _ => None,
        };
        if let Some(err) = err {
            form.error = Some(err);
            form::render(form);
            return;
        }
        let clip = clip.unwrap();
        let home = form.home.clone();
        let dest = clip_dest(&path, clip.default_name());
        let local = is_local(&to);
        // The preview copy, when the fallback made one, goes with the copy.
        let extra = clip.preview().map(|p| format!(" {}", quote(&p))).unwrap_or_default();
        let (cmd, what) = if local {
            let dest = expand_home(&dest, home.as_deref());
            (
                format!(
                    "mkdir -p {} && mv -f {} {}{}",
                    quote(&parent_dir(&dest)),
                    quote(&clip.path()),
                    quote(&dest),
                    if extra.is_empty() { String::new() } else { format!(" && rm -f{extra}") }
                ),
                dest,
            )
        } else {
            let dst = endpoint(&to, &dest, home.as_deref());
            (
                format!(
                    "scp {} {} && rm -f {}{extra}",
                    quote(&clip.path()),
                    dst,
                    quote(&clip.path())
                ),
                format!("clipboard → {}", dst.trim_matches('\'')),
            )
        };
        form.model.remembered.clip_to = Some(if to.is_empty() { LOCAL.to_string() } else { to });
        if clip.is_text() {
            form.model.remembered.clip_text_path = Some(path);
        } else {
            form.model.remembered.clip_path = Some(path);
        }
        form.model.remembered.save();
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        (form.mode, cmd, what, local)
    };
    if local {
        // A move: quick, nothing to watch. The form waits for it.
        match run_job(&cmd, None).await {
            Ok(out) if out.status == 0 => {
                if let Some(form) = state.borrow_mut().as_mut().filter(|f| f.mode.0 == mode.0) {
                    discard(form, true);
                }
                let _ = mode_close(mode);
                *state.borrow_mut() = None;
                let _ = display_message(&format!("saved: {what}"));
            }
            Ok(out) => form::fail(&state, mode, last_line(&out.output, "could not save the image")),
            Err(e) => form::fail(&state, mode, format!("could not save the image: {}", e.message)),
        }
    } else {
        run_copy(state, mode, cmd, what).await;
    }
}

impl Plugin for Scp {
    const NAME: &'static str = "scp";
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        let run = match config.run.as_deref().map(str::trim) {
            Some("job") => Run::Job,
            _ => Run::Popup,
        };
        let args = config.args.unwrap_or_else(|| "-r".to_string());
        let extra_hosts = config
            .hosts
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Scp { state: Rc::new(RefCell::new(None)), run, args, extra_hosts })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => {
                let kind = match event.get_str("text").map(str::trim) {
                    Some("copy") | Some("") | None => Kind::Files,
                    Some("clipboard") => Kind::Clipboard,
                    _ => return,
                };
                if self.state.borrow().is_some() {
                    let _ = display_message("scp: form already open");
                    return;
                }
                let target_pane = event.scope.pane.map(u64::from);
                let client = event.scope.client.map(u64::from);
                ctx.spawn(open_form(
                    Rc::clone(&self.state),
                    self.run,
                    self.args.clone(),
                    self.extra_hosts.clone(),
                    target_pane,
                    client,
                    kind,
                ));
            }
            "mode-key" => {
                let Some(key) = event.get_str("key") else { return };
                // What the key did, decided inside the borrow and acted
                // on after it: the form's own keys through formkit, the
                // swap and the tab switch here.
                let (mode, action) = {
                    let mut st = self.state.borrow_mut();
                    let Some(form) = st.as_mut() else { return };
                    if event.get_i64("mode") != Some(form.mode.0 as i64) {
                        return;
                    }
                    let mode = form.mode;
                    let mut action = if key == KIND_KEY && !form.busy {
                        switch_kind(form);
                        form::render(form);
                        Action::Rescan { reveal: false }
                    } else {
                        form.key(key, Some("C-t"))
                    };
                    if action == Action::Toggle {
                        action = if form.model.kind == Kind::Files {
                            swap(form);
                            form::render(form);
                            Action::Rescan { reveal: false }
                        } else {
                            Action::None
                        };
                    }
                    // The clipboard field is a label, not an input: focus
                    // steps over it the way it was going.
                    if form.model.kind == Kind::Clipboard && form.focused == CLIP {
                        form.focused = match key {
                            "Up" | "C-k" | "BTab" | "C-p" => CLIP_PATH,
                            _ => CLIP_TO,
                        };
                        form::render(form);
                    }
                    if action == Action::Close {
                        discard(form, false);
                    }
                    (mode, action)
                };
                match action {
                    Action::None | Action::Toggle => {}
                    Action::Rescan { reveal } => form::start_scan(&self.state, mode, reveal),
                    Action::Probe => form::kick_probe(&self.state, mode),
                    Action::Submit => {
                        ctx.spawn(submit(Rc::clone(&self.state)));
                    }
                    Action::Close => {
                        let _ = mode_close(mode);
                    }
                }
            }
            "mode-resize" => {
                let mut st = self.state.borrow_mut();
                let Some(form) = st.as_mut() else { return };
                if event.get_i64("mode") != Some(form.mode.0 as i64) {
                    return;
                }
                form.resized(event.get_i64("width"), event.get_i64("height"));
                form::render(form);
            }
            "mode-closed" => {
                let mut st = self.state.borrow_mut();
                if st.as_ref().is_some_and(|f| event.get_i64("mode") == Some(f.mode.0 as i64)) {
                    // Closed from outside (the window went): the files
                    // can still go; the terminal's image cannot.
                    if let Some(form) = st.as_ref() {
                        if let ClipState::Ready(clip) = &form.model.clip {
                            spawn(remove_clip(clip.clone()));
                        }
                    }
                    *st = None;
                }
            }
            _ => {}
        }
    }
}

tmux_plugin!(Scp);

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
    fn preview_shapes() {
        // A screenshot takes every row; the columns follow its shape.
        assert_eq!(preview_box(2050, 1426, 2.0), (40, PREVIEW_MAX_ROWS));
        // A panorama fills the width; rows follow.
        let (c, r) = preview_box(4000, 400, 2.0);
        assert_eq!(c, PREVIEW_MAX_COLS);
        assert_eq!(r, 4);
        // A tall image takes every row and few columns.
        let (c, r) = preview_box(400, 1600, 2.0);
        assert_eq!(r, PREVIEW_MAX_ROWS);
        assert_eq!(c, 7);
        assert_eq!(preview_box(0, 0, 2.0), (1, 1));
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

    #[test]
    fn destinations() {
        assert_eq!(clip_dest("/tmp/", CLIP_NAME), "/tmp/clipboard.png");
        assert_eq!(clip_dest("", CLIP_TEXT_NAME), "clipboard.txt");
        assert_eq!(clip_dest("/tmp/shot.png", CLIP_NAME), "/tmp/shot.png");
        assert_eq!(human_bytes(345), "345 B");
        assert_eq!(human_bytes(1234), "1.2 KB");
        assert_eq!(human_bytes(3_600_000), "3.4 MB");
        let lines = text_preview(b"a\tb\nsecond\x07 line\n\nfour\n");
        assert_eq!(lines, vec!["a    b", "second line", "", "four"]);
        let long = "x".repeat(200);
        assert_eq!(text_preview(long.as_bytes())[0].chars().count(), (FORM_WIDTH - 4) as usize);
        assert_eq!(parent_dir("/tmp/shot.png"), "/tmp");
        assert_eq!(parent_dir("/shot.png"), "/");
        assert_eq!(parent_dir("shot.png"), ".");
    }
}
