//! Host-initiated downloads through the tmux job runner.
//!
//! The server has no HTTP client of its own; `curl` does the transfer as
//! a tmux job (the same way `tmux update` fetches a release), and the
//! completion comes back through `pgh_async_complete` with a token from
//! `tokens::allocate_host`. Callbacks run at drain time, so they may
//! load and reload plugins like any other drain work.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};

use crate::hostlog;

/// Seconds `curl` may spend on one transfer.
const MAX_TIME: u32 = 300;

pub type Done<T> = Box<dyn FnOnce(Result<T, String>)>;

struct Job {
    url: String,
    tmp: PathBuf,
    done: Done<PathBuf>,
}

thread_local! {
    static JOBS: RefCell<HashMap<u64, Job>> = RefCell::new(HashMap::new());
}

/// Quote for `sh`: single quotes, with embedded quotes spliced.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Only http(s) and file urls: the string ends up on a shell line.
fn check_url(url: &str) -> Result<(), String> {
    let ok = url.starts_with("https://") || url.starts_with("http://") || url.starts_with("file://");
    if !ok {
        return Err(format!("unsupported url {url:?}"));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("bad characters in url {url:?}"));
    }
    Ok(())
}

/// Download `url` to a temporary file under the plugin cache; `done`
/// gets the path (the callee owns the file) or an error message.
pub fn download(url: &str, done: Done<PathBuf>) {
    if let Err(e) = check_url(url) {
        done(Err(e));
        return;
    }
    let Some(vt) = crate::vtable() else {
        done(Err("host vtable unavailable".into()));
        return;
    };
    let tmp_dir = match crate::cas::tmp_dir() {
        Ok(d) => d,
        Err(e) => {
            done(Err(e));
            return;
        }
    };
    let token = crate::tokens::allocate_host();
    let tmp = tmp_dir.join(format!("dl-{}-{token}.part", std::process::id()));
    let cmd = format!(
        "curl -fsSL --max-time {MAX_TIME} -o {} {}",
        shell_quote(&tmp.to_string_lossy()),
        shell_quote(url)
    );
    let Ok(ccmd) = CString::new(cmd) else {
        crate::tokens::discard(token);
        done(Err("bad command".into()));
        return;
    };
    let rc = unsafe { (vt.run_job)(ccmd.as_ptr(), std::ptr::null(), token) };
    if rc != 0 {
        crate::tokens::discard(token);
        done(Err("failed to start curl (is it installed?)".into()));
        return;
    }
    hostlog::debug("fetch", &format!("get {url}"));
    JOBS.with(|j| {
        j.borrow_mut().insert(token, Job { url: url.to_string(), tmp, done });
    });
}

/// Download `url` and hand over its text; the temporary file is removed.
pub fn download_text(url: &str, done: Done<String>) {
    download(
        url,
        Box::new(move |res| {
            done(res.and_then(|tmp| {
                let text = std::fs::read_to_string(&tmp)
                    .map_err(|e| format!("{}: {e}", tmp.display()));
                let _ = std::fs::remove_file(&tmp);
                text
            }))
        }),
    );
}

/// Fetch a module (and its sidecar, when `sidecar_url` is given), check
/// it against `hash` (hex) and store it in the cache. `done` gets the
/// cache path.
pub fn fetch_module(url: &str, hash: String, sidecar_url: Option<String>, done: Done<PathBuf>) {
    download(
        url,
        Box::new(move |res| {
            let tmp = match res {
                Ok(t) => t,
                Err(e) => {
                    done(Err(e));
                    return;
                }
            };
            match sidecar_url {
                None => done(crate::cas::put_file(&tmp, &hash, None)),
                Some(surl) => download_text(
                    &surl,
                    Box::new(move |side| match side {
                        Ok(text) => done(crate::cas::put_file(&tmp, &hash, Some(&text))),
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            done(Err(format!("sidecar: {e}")));
                        }
                    }),
                ),
            }
        }),
    );
}

/// A host job finished (from `deliver_async`). `v0` is the exit status,
/// `v1` whether it was a signal, `data` the combined output.
pub fn complete(token: u64, err: i32, v0: i64, v1: i64, data: &[u8]) {
    let Some(job) = JOBS.with(|j| j.borrow_mut().remove(&token)) else { return };
    let output = String::from_utf8_lossy(data).trim().to_string();
    let result = if err != 0 {
        Err(format!("{}: {}", job.url, if output.is_empty() { "job failed" } else { &output }))
    } else if v1 != 0 {
        Err(format!("{}: curl killed by signal {v0}", job.url))
    } else if v0 != 0 {
        Err(format!(
            "{}: curl exit {v0}{}",
            job.url,
            if output.is_empty() { String::new() } else { format!(": {output}") }
        ))
    } else if !job.tmp.is_file() {
        Err(format!("{}: curl wrote nothing", job.url))
    } else {
        Ok(job.tmp.clone())
    };
    if result.is_err() {
        let _ = std::fs::remove_file(&job.tmp);
    }
    (job.done)(result);
}

/// Jobs still in flight (for tests and `show-plugins -v`).
pub fn in_flight() -> usize {
    JOBS.with(|j| j.borrow().len())
}

#[allow(dead_code)]
pub fn tmp_of(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "part")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn urls() {
        assert!(check_url("https://example.com/x.wasm").is_ok());
        assert!(check_url("file:///tmp/x.wasm").is_ok());
        assert!(check_url("ftp://example.com/x").is_err());
        assert!(check_url("https://example.com/x y").is_err());
        assert!(check_url("https://example.com/x\n").is_err());
    }
}
