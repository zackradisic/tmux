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
//! **clipboard** - the image on the clipboard (`Image (2050x1426)`), a
//! `to` host and a path. The image is read into a PNG under the plugin's
//! data directory when the form opens, previewed under the fields (see
//! below), and on Enter moved to a local path or sent with `scp` to a
//! host. A path ending in `/` gets `clipboard.png` appended.
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
//! placeholders: the PNG (downscaled) is transmitted once through a
//! `DCS tmux;` passthrough, placed virtually, and drawn as a block of
//! `U+10EEEE` cells whose colour and diacritics name the image and the
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
//! paths, in the data directory). Role `view`: nothing here needs to run on the
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

/// The clipboard image, read to a file when the form opened. Both files
/// live in the plugin's data directory: the host's file calls want them
/// by name, relative to it; the shell wants the absolute path.
#[derive(Clone, Debug)]
struct Clip {
    /// The data directory.
    root: String,
    /// The PNG as it was on the clipboard: what gets copied.
    name: String,
    /// A downscaled copy for the preview.
    preview_name: String,
    width: u32,
    height: u32,
}

impl Clip {
    fn path(&self) -> String {
        format!("{}/{}", self.root, self.name)
    }

    fn preview(&self) -> String {
        format!("{}/{}", self.root, self.preview_name)
    }
}

#[derive(Clone, Debug)]
enum ClipState {
    Probing,
    /// No image, or no way to read it: why.
    Missing(String),
    Image(Clip),
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
            (Kind::Clipboard, ClipState::Image(_)) => {
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
        match (self.kind, self.preview) {
            (Kind::Clipboard, Some(p)) if p.sent => p.rows,
            _ => 0,
        }
    }

    fn footer(&self, _fields: &[Field], rows: u32) -> Vec<String> {
        let Some(p) = self.preview.filter(|p| p.sent) else { return Vec::new() };
        (0..rows.min(p.rows)).map(|row| placeholder_row(p.id, row, p.cols)).collect()
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
async fn probe_clipboard(state: State, mode: ModeId, auto_switch: bool) {
    let Ok(root) = fs_root() else {
        form::fail(&state, mode, "scp: no data directory for the clipboard image".into());
        return;
    };
    let stamp = now_ms();
    let written = Clip {
        root: root.trim_end_matches('/').to_string(),
        name: format!("clip-{stamp}.png"),
        preview_name: format!("clip-{stamp}.preview.png"),
        width: 0,
        height: 0,
    };
    // The JavaScript rides in the environment: no quoting of a quoted
    // script inside a quoted script.
    let cmd = format!(
        "JXA={} sh -c {} sh {} {} {}",
        quote(JXA_SCRIPT),
        quote(PROBE_SCRIPT),
        quote(&written.path()),
        quote(&written.preview()),
        PREVIEW_PX
    );
    let t0 = now_ms();
    let found = match run_job(&cmd, None).await {
        Ok(out) if out.status == 0 => {
            match fs_read(&written.name, 0, 64).await {
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
            }
        }
        Ok(out) => Err(last_line(&out.output, "no image on the clipboard")),
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
                set_clip_label(form, format!("Image ({}x{})", clip.width, clip.height));
                let (cols, rows) = preview_box(clip.width, clip.height, form.model.cell_aspect);
                let id = ((stamp & 0x00ff_ffff) as u32).max(1);
                form.model.preview = Some(Preview { id, cols, rows, sent: false });
                form.model.clip = ClipState::Image(clip.clone());
                if auto_switch && form.model.kind == Kind::Files {
                    switch_kind(form);
                }
                form::render(form);
                Some((clip, id, cols, rows))
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
        let bytes = transmit_preview(state, mode, clip.preview_name.clone(), id, cols, rows).await;
        log(&format!(
            "scp: clipboard {}x{} read in {read_ms} ms; preview {} KB sent in {} ms as {cols}x{rows} cells",
            clip.width,
            clip.height,
            bytes / 1024,
            now_ms().saturating_sub(t1)
        ));
    } else {
        log(&format!("scp: clipboard probe took {read_ms} ms: no image"));
    }
}

/// Delete the clipboard files. Best effort, off the caller's path.
async fn remove_clip(clip: Clip) {
    if let Err(e) = fs_remove(&clip.name).await {
        log(&format!("scp: cannot remove {}: {}", clip.path(), e.message));
    }
    let _ = fs_remove(&clip.preview_name).await;
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

/// The transmit: the PNG, base64, in chunks of 4096, the first carrying
/// the image's keys and the last `m=0`; then a virtual placement of
/// `cols`×`rows` for the placeholders to show.
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

/// Send the preview PNG (`name`, in the data directory) to the terminal
/// through the mode, then draw the block. Returns the PNG's size.
async fn transmit_preview(
    state: State,
    mode: ModeId,
    name: String,
    id: u32,
    cols: u32,
    rows: u32,
) -> usize {
    let Ok((png, _)) = fs_read(&name, 0, 8 * 1024 * 1024).await else { return 0 };
    if png.is_empty() {
        return 0;
    }
    let size = png.len();
    let mut batch = String::new();
    for seq in kitty_transmit(id, &png, cols, rows) {
        if batch.len() + seq.len() > WRITE_MAX {
            if mode_write(mode, batch.as_bytes()).is_err() {
                return size;
            }
            batch.clear();
        }
        batch.push_str(&seq);
    }
    if !batch.is_empty() && mode_write(mode, batch.as_bytes()).is_err() {
        return size;
    }
    let mut st = state.borrow_mut();
    let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return size };
    if let Some(p) = form.model.preview.as_mut().filter(|p| p.id == id) {
        p.sent = true;
    }
    form::render(form);
    size
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
    if let ClipState::Image(clip) = &form.model.clip {
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

/// The clipboard image's destination as a file path: a directory (a
/// trailing `/`, or nothing) gets the default file name.
fn clip_dest(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() || path.ends_with('/') {
        format!("{path}{CLIP_NAME}")
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
            ClipState::Image(c) => Some(c.clone()),
            _ => None,
        };
        let err = match (&clip, &form.model.clip) {
            (None, ClipState::Probing) => Some("still reading the clipboard".to_string()),
            (None, _) => Some("no image on the clipboard".to_string()),
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
        let dest = clip_dest(&path);
        let local = is_local(&to);
        let (cmd, what) = if local {
            let dest = expand_home(&dest, home.as_deref());
            (
                format!(
                    "mkdir -p {} && mv -f {} {} && rm -f {}",
                    quote(&parent_dir(&dest)),
                    quote(&clip.path()),
                    quote(&dest),
                    quote(&clip.preview())
                ),
                dest,
            )
        } else {
            let dst = endpoint(&to, &dest, home.as_deref());
            (
                format!(
                    "scp {} {} && rm -f {} {}",
                    quote(&clip.path()),
                    dst,
                    quote(&clip.path()),
                    quote(&clip.preview())
                ),
                format!("clipboard → {}", dst.trim_matches('\'')),
            )
        };
        form.model.remembered.clip_to = Some(if to.is_empty() { LOCAL.to_string() } else { to });
        form.model.remembered.clip_path = Some(path);
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
                        if let ClipState::Image(clip) = &form.model.clip {
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
    fn destinations() {
        assert_eq!(clip_dest("/tmp/"), "/tmp/clipboard.png");
        assert_eq!(clip_dest(""), "clipboard.png");
        assert_eq!(clip_dest("/tmp/shot.png"), "/tmp/shot.png");
        assert_eq!(parent_dir("/tmp/shot.png"), "/tmp");
        assert_eq!(parent_dir("/shot.png"), "/");
        assert_eq!(parent_dir("shot.png"), ".");
    }
}
