//! The Slack web API, one call at a time, over the host's `net` import.
//!
//! Slack accepts the desktop session's `xoxc-` token as a bearer token
//! as long as the `d` cookie of the same session rides along. The host
//! does the transfer on a thread of its own; a non-2xx status comes back
//! as a response, so a 429 can be retried after Slack's `Retry-After`.

use std::cell::RefCell;

use serde_json::Value;
use tmux_plugin_sdk::prelude::*;

use crate::store::Identity;

pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
const TIMEOUT_MS: u64 = 25_000;

thread_local! {
    /// The workspace's origin (`https://<workspace>.slack.com`, or the
    /// configured `api_base`), set once at init. The SDK is
    /// single-threaded, so a thread-local is a plain global here.
    static API_BASE: RefCell<String> = RefCell::new(String::new());
}

pub fn set_api_base(base: &str) {
    API_BASE.with(|b| *b.borrow_mut() = base.trim_end_matches('/').to_string());
}

/// `https://<domain>.slack.com` unless `set_api_base` said otherwise.
pub fn api_base(domain: &str) -> String {
    let set = API_BASE.with(|b| b.borrow().clone());
    if set.is_empty() {
        format!("https://{domain}.slack.com")
    } else {
        set
    }
}

/// Quote for `sh` (the few jobs the plugin still runs: `security`,
/// `sqlite3`, `open`).
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One web API method. Errors are Slack's `error` string (`ok: false`)
/// or the transport's.
pub async fn call(id: &Identity, method: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    let url = format!("{}/api/{method}", api_base(&id.domain));
    let mut attempt = 0;
    loop {
        attempt += 1;
        let req = HttpRequest::post(&url)
            .header("Authorization", &format!("Bearer {}", id.token))
            .header("Cookie", &format!("d={}", id.cookie))
            .header("User-Agent", USER_AGENT)
            .form(form)
            .timeout_ms(TIMEOUT_MS);
        let r = http_request(&req).await.map_err(|e| format!("{method}: {}", e.message))?;
        if r.status == 429 && attempt < 3 {
            let wait = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(3).clamp(1, 30);
            let _ = sleep_ms(wait * 1000).await;
            continue;
        }
        if !r.ok() {
            return Err(format!("{method}: HTTP {}", r.status));
        }
        let v: Value = r.json().map_err(|e| format!("{method}: bad JSON: {}", e.message))?;
        if v.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = v.get("error").and_then(Value::as_str).unwrap_or("not ok");
            return Err(format!("{method}: {err}"));
        }
        return Ok(v);
    }
}

/// A GET with only the session cookie (the workspace page that mints a
/// token). Returns the status and the body: Slack serves that page with
/// a 403 to a signed-in session and the token is in it all the same.
pub async fn get_with_cookie(url: &str, cookie: &str) -> Result<(u16, String), String> {
    let req = HttpRequest::get(url)
        .header("Cookie", &format!("d={cookie}"))
        .header("User-Agent", USER_AGENT)
        .timeout_ms(TIMEOUT_MS);
    let r = http_request(&req).await.map_err(|e| format!("GET {url}: {}", e.message))?;
    Ok((r.status, r.text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sq("it's"), "'it'\\''s'");
    }
}
