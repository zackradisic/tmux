//! The Slack web API, one call at a time, as curl jobs.
//!
//! A wasm plugin has no socket of its own. Until the host's `net`
//! imports land, every request is a `curl` job: the headers (the
//! session token and cookie) and the body go by file under the plugin's
//! data directory (a 0700 `stage/` dir, so the secret is never on a
//! command line or in `ps`), the response is written to a file there
//! too and read back with `fs_read`, which has no size cap - a channel
//! history or a user list can run to megabytes. The status code is the
//! job's only stdout.
//!
//! Slack accepts the desktop session's `xoxc-` token as a bearer token
//! as long as the `d` cookie of the same session rides along.

use std::cell::Cell;

use serde_json::Value;
use tmux_plugin_sdk::prelude::*;

use crate::store::Identity;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
/// Seconds one request may take.
const TIMEOUT_S: u32 = 25;

thread_local! {
    static STAGED: Cell<bool> = const { Cell::new(false) };
    static SEQ: Cell<u64> = const { Cell::new(0) };
}

/// Quote for `sh`.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `application/x-www-form-urlencoded` (spaces as %20).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn form_body(form: &[(&str, &str)]) -> String {
    form.iter().map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v))).collect::<Vec<_>>().join("&")
}

async fn stage_dir() -> Result<String, String> {
    let root = fs_root().map_err(|e| e.message.clone())?;
    if !STAGED.with(|s| s.get()) {
        let out = run_job(&format!("umask 077; mkdir -p {}", sq(&format!("{root}/stage"))), None)
            .await
            .map_err(|e| e.message.clone())?;
        if out.status != 0 {
            return Err(format!("stage dir: {}", out.output.trim()));
        }
        STAGED.with(|s| s.set(true));
    }
    Ok(root)
}

fn next_name() -> String {
    let n = SEQ.with(|s| {
        let v = s.get() + 1;
        s.set(v);
        v
    });
    format!("{}-{n}", now_ms())
}

/// Read a whole staged file.
async fn read_all(rel: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut off = 0u64;
    loop {
        let (chunk, eof) = fs_read(rel, off, 1 << 20).await.map_err(|e| e.message.clone())?;
        off += chunk.len() as u64;
        let empty = chunk.is_empty();
        out.extend_from_slice(&chunk);
        if eof || empty {
            break;
        }
    }
    Ok(out)
}

/// A raw transfer: `method` on `url` with `headers` (one per line) and
/// an optional body. Returns (status, body bytes).
pub async fn transfer(method: &str, url: &str, headers: &str, body: Option<&str>) -> Result<(u16, Vec<u8>), String> {
    let root = stage_dir().await?;
    let n = next_name();
    let hdr_rel = format!("stage/h-{n}");
    let body_rel = format!("stage/b-{n}");
    let resp_rel = format!("stage/r-{n}");
    fs_write(&hdr_rel, headers.as_bytes().to_vec(), false).await.map_err(|e| e.message.clone())?;
    let mut cmd = format!(
        "curl -sS -m {TIMEOUT_S} -X {method} -A {} -H @{} -o {} -w '%{{http_code}}' ",
        sq(USER_AGENT),
        sq(&format!("{root}/{hdr_rel}")),
        sq(&format!("{root}/{resp_rel}"))
    );
    if let Some(b) = body {
        fs_write(&body_rel, b.as_bytes().to_vec(), false).await.map_err(|e| e.message.clone())?;
        cmd.push_str(&format!("--data-binary @{} ", sq(&format!("{root}/{body_rel}"))));
    }
    cmd.push_str(&sq(url));
    let out = run_job(&cmd, None).await;
    let _ = fs_remove(&hdr_rel).await;
    if body.is_some() {
        let _ = fs_remove(&body_rel).await;
    }
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let _ = fs_remove(&resp_rel).await;
            return Err(format!("curl: {}", e.message));
        }
    };
    let text = out.output;
    let status: u16 = text.trim().lines().last().unwrap_or("").trim().parse().unwrap_or(0);
    if out.status != 0 || status == 0 {
        let _ = fs_remove(&resp_rel).await;
        let why = text.trim().lines().find(|l| !l.trim().is_empty() && l.trim().parse::<u16>().is_err()).unwrap_or("failed");
        return Err(format!("curl: {}", why.trim()));
    }
    let data = read_all(&resp_rel).await;
    let _ = fs_remove(&resp_rel).await;
    Ok((status, data?))
}

fn auth_headers(id: &Identity) -> String {
    format!(
        "Authorization: Bearer {}\nCookie: d={}\nContent-Type: application/x-www-form-urlencoded\n",
        id.token, id.cookie
    )
}

/// One web API method. Errors are Slack's `error` string (`ok: false`)
/// or the transport's.
pub async fn call(id: &Identity, method: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    let url = format!("https://{}.slack.com/api/{method}", id.domain);
    let body = form_body(form);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let (status, data) = transfer("POST", &url, &auth_headers(id), Some(&body)).await?;
        if status == 429 && attempt < 3 {
            // Rate limited: Slack says how long; we do not read the header
            // here, so wait a flat few seconds.
            let _ = sleep_ms(3000).await;
            continue;
        }
        if status != 200 {
            return Err(format!("{method}: HTTP {status}"));
        }
        let v: Value = serde_json::from_slice(&data).map_err(|e| format!("{method}: bad JSON: {e}"))?;
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
    let headers = format!("Cookie: d={cookie}\n");
    let (status, data) = transfer("GET", url, &headers, None).await?;
    Ok((status, String::from_utf8_lossy(&data).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_form() {
        assert_eq!(urlencode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
        assert_eq!(form_body(&[("channel", "C1"), ("text", "hi there")]), "channel=C1&text=hi%20there");
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sq("it's"), "'it'\\''s'");
    }
}
