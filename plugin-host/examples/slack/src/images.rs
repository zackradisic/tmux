//! Pictures for the channel view: the files a message shares, fetched
//! and converted once, outside wasm, into `img/<file id>.png` under the
//! plugin's data directory, where the terminal reads them itself.
//!
//! Nothing here decodes, resizes or base64-encodes pixels. The thumbnail
//! comes down through the host's `net` import (the session's token and
//! cookie, as every `files.slack.com` URL needs) and is written as it
//! came; `sips` (macOS) or ImageMagick turns it into a PNG of at most
//! [`MAX_PX`] on its longer side - a GIF's first frame - and the plugin
//! reads back the 24 bytes of the IHDR for the size. The channel view
//! then names the file in one kitty graphics transmit (listkit::image).
//!
//! The pictures of attachments (a Giphy post's `image` block, an
//! unfurl's `image_url`) go the same way under ids of their own; only a
//! Slack host gets the session along. The sidecar's `net` allowlist
//! decides which hosts are reachable at all (`*.slack.com`, Giphy,
//! Slack's image proxy); a picture elsewhere is logged and left as its
//! caption line.
//!
//! Each file id is asked once per process (a failure is not retried
//! until the plugin reloads; the log says why). The directory keeps the
//! newest [`KEEP`] pictures; the rest go at boot.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};

use listkit::image::png_size;
use tmux_plugin_sdk::prelude::*;

use crate::store::{FileRef, Identity};
use crate::{chat, http, Shared, State, NAME};

/// Converted pictures kept in the data directory.
pub const KEEP: usize = 200;
/// The longer side of a converted picture, in pixels: a block is at most
/// 80 cells wide, which no font makes wider than this.
const MAX_PX: u32 = 1024;
/// The directory under the data root.
const DIR: &str = "img";
const TIMEOUT_MS: u64 = 30_000;

/// What the view knows about pictures: the ones on disk by file id, and
/// the ones already asked for (in flight or failed).
#[derive(Default, Clone)]
pub struct Cache {
    /// File id -> pixel size of `img/<id>.png`.
    pub ready: HashMap<String, (u32, u32)>,
    /// File ids asked for this process: in flight, done, or failed.
    pub asked: HashSet<String>,
}

thread_local! {
    /// The plugin state, for a fetch to report into once it is done.
    /// The SDK is single-threaded, so a thread-local is a plain global.
    static SHARED: RefCell<Option<Weak<RefCell<State>>>> = const { RefCell::new(None) };
}

/// Remember the state the fetches report to (once, at init).
pub fn attach(st: &Shared) {
    SHARED.with(|s| *s.borrow_mut() = Some(Rc::downgrade(st)));
}

fn shared() -> Option<Shared> {
    SHARED.with(|s| s.borrow().as_ref().and_then(Weak::upgrade))
}

/// The converted picture's name under the data root.
pub fn png_name(id: &str) -> String {
    format!("{DIR}/{id}.png")
}

/// Ask for every image among `files` that has not been asked for yet.
/// Each fetch reports back through [`attach`]'s state and redraws the
/// open view; without one (tests) nothing is started.
pub fn request(s: &mut State, files: Vec<FileRef>) {
    let Some(ident) = s.ident.clone() else { return };
    for f in files {
        if !f.is_image() || s.images.asked.contains(&f.id) || s.images.ready.contains_key(&f.id) {
            continue;
        }
        s.images.asked.insert(f.id.clone());
        let ident = ident.clone();
        spawn(async move {
            let r = fetch(&ident, &f).await;
            let Some(st) = shared() else { return };
            {
                let mut s = st.borrow_mut();
                match r {
                    Ok(size) => {
                        s.images.ready.insert(f.id.clone(), size);
                    }
                    Err(e) => log(&format!("{NAME}: image {} ({}): {e}", f.id, f.name)),
                }
            }
            chat::on_image(&st);
            crate::sidebar::on_image(&st);
        });
    }
}

/// Have the picture for `f` on disk: an earlier run's copy, else the
/// thumbnail downloaded and converted. Resolves with its pixel size.
async fn fetch(ident: &Identity, f: &FileRef) -> Result<(u32, u32), String> {
    let png = png_name(&f.id);
    if let Ok((head, _)) = fs_read(&png, 0, 32).await {
        if let Some(size) = png_size(&head) {
            return Ok(size);
        }
    }
    let url = f.fetch_url().ok_or("no URL")?;
    let body = download(ident, url).await?;
    if body.is_empty() {
        return Err("empty body".into());
    }
    if body.starts_with(b"<") {
        return Err("got a page, not a picture (session refused?)".into());
    }
    let src = format!("{DIR}/{}.src", f.id);
    fs_write(&src, body, false).await.map_err(|e| format!("write: {}", e.message))?;
    let root = fs_root().map_err(|e| e.message.clone())?;
    let out = run_job(&convert_cmd(&format!("{root}/{src}"), &format!("{root}/{png}")), None)
        .await
        .map_err(|e| format!("convert: {}", e.message))?;
    let _ = fs_remove(&src).await;
    if out.status != 0 {
        let _ = fs_remove(&png).await;
        return Err(format!("convert: exit {}: {}", out.status, out.output.trim()));
    }
    let (head, _) = fs_read(&png, 0, 32).await.map_err(|e| format!("read back: {}", e.message))?;
    png_size(&head).ok_or_else(|| "the converter wrote no PNG".to_string())
}

/// GET `url` as the session; one redirect is followed by hand (the host
/// follows none), with the credentials only if it stays on Slack.
async fn download(ident: &Identity, url: &str) -> Result<Vec<u8>, String> {
    let mut url = url.to_string();
    for hop in 0..2 {
        let mut req = HttpRequest::get(&url).timeout_ms(TIMEOUT_MS).header("User-Agent", http::USER_AGENT);
        if is_slack(&url) {
            req = req.header("Authorization", &format!("Bearer {}", ident.token)).header("Cookie", &format!("d={}", ident.cookie));
        }
        let r = http_request(&req).await.map_err(|e| format!("GET: {}", e.message))?;
        if matches!(r.status, 301 | 302 | 303 | 307 | 308) && hop == 0 {
            let Some(next) = r.header("location").map(str::to_string) else {
                return Err(format!("HTTP {} with no Location", r.status));
            };
            url = next;
            continue;
        }
        if !r.ok() {
            return Err(format!("HTTP {}", r.status));
        }
        return Ok(r.body);
    }
    Err("too many redirects".into())
}

/// Is the URL on Slack (the token and cookie go along) - or on the test
/// fake, which stands in for it?
fn is_slack(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host).split(':').next().unwrap_or("");
    host == "slack.com" || host.ends_with(".slack.com") || host == "127.0.0.1" || host == "localhost"
}

/// The shell line that turns `src` (any raster image, a GIF's first
/// frame) into a PNG at `out`, no longer than [`MAX_PX`] on a side:
/// `sips` where there is one (macOS), ImageMagick otherwise. Exit 3 when
/// the machine has neither.
pub fn convert_cmd(src: &str, out: &str) -> String {
    let (src, out) = (http::sq(src), http::sq(out));
    format!(
        concat!(
            "if command -v sips >/dev/null 2>&1; then ",
            "m=$(sips -g pixelWidth -g pixelHeight {src} 2>/dev/null | awk '/pixel/ {{ if ($2+0 > m) m = $2+0 }} END {{ print m+0 }}'); ",
            "if [ \"$m\" -gt {px} ]; then z=\"-Z {px}\"; else z=; fi; ",
            "sips -s format png $z {src} --out {out} >/dev/null 2>&1; ",
            "elif command -v magick >/dev/null 2>&1; then magick {src}'[0]' -resize '{px}x{px}>' {out}; ",
            "elif command -v convert >/dev/null 2>&1; then convert {src}'[0]' -resize '{px}x{px}>' {out}; ",
            "else echo 'no image converter (sips, magick or convert)'; exit 3; fi"
        ),
        src = src,
        out = out,
        px = MAX_PX
    )
}

/// Keep the directory bounded: the newest [`KEEP`] pictures stay, older
/// ones and any half-written `.src` go. Run once at boot, as a job (the
/// plugin has no directory listing of its own; `ls -t` ranks by time).
pub async fn prune() {
    let Ok(root) = fs_root() else { return };
    let _ = run_job(&prune_cmd(&format!("{root}/{DIR}")), None).await;
}

pub fn prune_cmd(dir: &str) -> String {
    format!(
        "cd {} 2>/dev/null || exit 0; rm -f -- *.src; ls -t | awk 'NR > {KEEP}' | while IFS= read -r f; do rm -f -- \"$f\"; done",
        http::sq(dir)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_hosts_get_the_session() {
        assert!(is_slack("https://files.slack.com/files-tmb/T1-F1/x_480.png"));
        assert!(is_slack("https://slack.com/x"));
        assert!(is_slack("http://127.0.0.1:4321/_img/F1.png"));
        assert!(!is_slack("https://notslack.com/a"));
        assert!(!is_slack("https://evil.example/slack.com/"));
        assert!(!is_slack("https://slack.com.evil.example/"));
    }

    #[test]
    fn convert_line_quotes_and_bounds() {
        let cmd = convert_cmd("/d/img/F1.src", "/d/img/F1.png");
        assert!(cmd.contains("sips -s format png $z '/d/img/F1.src' --out '/d/img/F1.png'"), "{cmd}");
        assert!(cmd.contains("-gt 1024"));
        assert!(cmd.contains("magick '/d/img/F1.src''[0]' -resize '1024x1024>'"));
        assert!(cmd.ends_with("exit 3; fi"));
        assert_eq!(png_name("F1"), "img/F1.png");
        assert_eq!(prune_cmd("/d/img"), "cd '/d/img' 2>/dev/null || exit 0; rm -f -- *.src; ls -t | awk 'NR > 200' | while IFS= read -r f; do rm -f -- \"$f\"; done");
    }
}
