//! The two Anthropic endpoints behind the panel, through curl: the
//! token endpoint (a refresh-token grant, for a slot whose access token
//! has expired while it was not the login) and the usage endpoint (the
//! 5-hour, 7-day and per-model windows). The guest has no sockets, so
//! each call is one `run_job`; the secret travels in a staging file
//! (`-H @file`, `--data-binary @file`) and never on a command line.
//!
//! The usage endpoint budgets non-first-party clients at roughly thirty
//! requests an hour per identity (cswap's measurement), so a slot is
//! refetched at most every three minutes and backs off ten on a 429.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tmux_plugin_sdk::prelude::*;

use crate::claude::{self, sq, Platform};

pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const BETA_HEADER: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "tmux2-accounts/0.1";

/// A slot is not asked about again sooner than this.
pub const MIN_INTERVAL_MS: i64 = 180_000;
/// After a 429, or for a slot with no headroom left.
pub const BACKOFF_MS: i64 = 600_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Window {
    pub pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_ms: Option<i64>,
    /// The window's length, for pace: 5 h, 7 d.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_ms: Option<i64>,
}

impl Window {
    /// Where a steady spend would be by now: elapsed fraction of the
    /// window. None when the reset is unknown.
    pub fn expected_pct(&self, now_ms: i64) -> Option<f64> {
        let (r, s) = (self.resets_ms?, self.span_ms?);
        let elapsed = (s - (r - now_ms)).clamp(0, s);
        Some(elapsed as f64 * 100.0 / s as f64)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Spend {
    pub used: f64,
    pub limit: f64,
    pub pct: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
    /// Per-model weekly windows by display name ("Fable").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scoped: Vec<(String, Window)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<Spend>,
}

impl Usage {
    /// Every window that gates a request: label, window.
    pub fn windows(&self) -> Vec<(String, &Window)> {
        let mut v = Vec::new();
        if let Some(w) = &self.five_hour {
            v.push(("5h".to_string(), w));
        }
        if let Some(w) = &self.seven_day {
            v.push(("7d".to_string(), w));
        }
        for (n, w) in &self.scoped {
            v.push((n.clone(), w));
        }
        v
    }

    /// Room before the binding window: 100 minus the highest
    /// utilization. None with no windows.
    pub fn headroom(&self) -> Option<f64> {
        let max = self.windows().iter().map(|(_, w)| w.pct).fold(f64::NAN, f64::max);
        if max.is_nan() {
            None
        } else {
            Some(100.0 - max)
        }
    }
}

fn window(v: &Value, pct_key: &str, span_ms: Option<i64>) -> Option<Window> {
    let o = v.as_object()?;
    let pct = o.get(pct_key)?.as_f64()?;
    let resets_ms = o.get("resets_at").and_then(Value::as_str).and_then(crate::timefmt::parse_iso);
    Some(Window { pct, resets_ms, span_ms })
}

/// The API's shape to ours. `five_hour` / `seven_day` carry
/// `utilization` and `resets_at`; `limits[]` entries with a
/// `scope.model.display_name` are the per-model weekly windows.
pub fn normalize(raw: &Value) -> Usage {
    let mut u = Usage::default();
    if let Some(v) = raw.get("five_hour") {
        u.five_hour = window(v, "utilization", Some(5 * 3600 * 1000));
    }
    if let Some(v) = raw.get("seven_day") {
        u.seven_day = window(v, "utilization", Some(7 * 86_400 * 1000));
    }
    if let Some(list) = raw.get("limits").and_then(Value::as_array) {
        for lim in list {
            let name = lim
                .get("scope")
                .and_then(|s| s.get("model"))
                .and_then(|m| m.get("display_name"))
                .and_then(Value::as_str);
            let (Some(name), Some(w)) = (name, window(lim, "percent", Some(7 * 86_400 * 1000))) else {
                continue;
            };
            u.scoped.push((name.to_string(), w));
        }
    }
    if let Some(eu) = raw.get("extra_usage").filter(|e| e.get("is_enabled").and_then(Value::as_bool) == Some(true)) {
        let g = |k: &str| eu.get(k).and_then(Value::as_f64);
        if let (Some(used), Some(limit), Some(pct)) = (g("used_credits"), g("monthly_limit"), g("utilization")) {
            u.spend = Some(Spend {
                used: used / 100.0,
                limit: limit / 100.0,
                pct,
                currency: eu.get("currency").and_then(Value::as_str).unwrap_or("USD").to_string(),
            });
        }
    }
    u
}

// ---------------------------------------------------------------------------
// curl
// ---------------------------------------------------------------------------

pub struct Http {
    pub status: u16,
    pub body: String,
    pub retry_after_s: Option<f64>,
}

/// One request. Headers and body go by file; the status code rides on
/// the last output line (`-w`), the Retry-After header on the one before
/// (`-D`), so a run_job's single combined stream carries all three.
async fn request(p: &Platform, method: &str, url: &str, headers: &str, body: Option<&str>) -> Result<Http, String> {
    if !p.curl {
        return Err("curl is not installed on this server".into());
    }
    let n = format!("{}-{}", now_ms(), url.len());
    let hdr_rel = format!("stage/hdr-{n}");
    let hdr_abs = format!("{}/{hdr_rel}", p.root);
    let _ = run_job(&format!("umask 077; mkdir -p {}", sq(&format!("{}/stage", p.root))), None).await;
    fs_write(&hdr_rel, headers.as_bytes().to_vec(), false).await.map_err(|e| e.message.clone())?;
    let mut body_rel = None;
    let mut cmd = format!(
        "curl -sS -m 12 -X {method} -A {} -H @{} -D - -w '\\n@@STATUS %{{http_code}}' {}",
        sq(USER_AGENT),
        sq(&hdr_abs),
        sq(url)
    );
    if let Some(b) = body {
        let rel = format!("stage/body-{n}");
        fs_write(&rel, b.as_bytes().to_vec(), false).await.map_err(|e| e.message.clone())?;
        cmd.push_str(&format!(" --data-binary @{}", sq(&format!("{}/{rel}", p.root))));
        body_rel = Some(rel);
    }
    let out = run_job(&cmd, None).await;
    let _ = fs_remove(&hdr_rel).await;
    if let Some(r) = body_rel {
        let _ = fs_remove(&r).await;
    }
    let out = out.map_err(|e| e.message.clone())?;
    let text = out.output;
    let Some((rest, status)) = text.rsplit_once("\n@@STATUS ") else {
        return Err(format!("curl: {}", text.trim().lines().last().unwrap_or("no output")));
    };
    let status: u16 = status.trim().parse().unwrap_or(0);
    if status == 0 {
        return Err(format!("curl: {}", rest.trim().lines().last().unwrap_or("failed")));
    }
    // `-D -` puts the response headers first, then a blank line, then
    // the body. A redirect would stack two header blocks; take the last.
    let mut retry_after_s = None;
    let (headers_text, body_text) = match rest.rsplit_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => rest.rsplit_once("\n\n").unwrap_or(("", rest)),
    };
    for line in headers_text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("retry-after") {
                retry_after_s = v.trim().parse::<f64>().ok();
            }
        }
    }
    Ok(Http { status, body: body_text.to_string(), retry_after_s })
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RefreshError {
    /// The grant was rejected: this refresh token is dead, log in again.
    Permanent(String),
    /// Network or server trouble; the token may still be fine.
    Transient(String),
}

impl RefreshError {
    pub fn message(&self) -> &str {
        match self {
            RefreshError::Permanent(m) | RefreshError::Transient(m) => m,
        }
    }
}

/// Rotate a credential blob through the token endpoint. The returned
/// blob has the new access token, expiry and (when issued) refresh
/// token; every other key is kept.
pub async fn refresh(p: &Platform, creds: &str) -> Result<(String, Option<claude::Identity>), RefreshError> {
    let Some(rt) = claude::refresh_token(creds) else {
        return Err(RefreshError::Permanent("no refresh token stored; log in again".into()));
    };
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": rt,
        "client_id": CLIENT_ID,
    })
    .to_string();
    let r = request(p, "POST", TOKEN_URL, "Content-Type: application/json\n", Some(&body))
        .await
        .map_err(RefreshError::Transient)?;
    if r.status != 200 {
        let err = serde_json::from_str::<Value>(&r.body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_default();
        if matches!(r.status, 400 | 401 | 403) && (err == "invalid_grant" || err == "invalid_client") {
            return Err(RefreshError::Permanent(format!("token endpoint: {err}; log in again")));
        }
        return Err(RefreshError::Transient(format!("token endpoint: HTTP {}", r.status)));
    }
    let resp: Value = serde_json::from_str(&r.body).map_err(|e| RefreshError::Transient(format!("token endpoint: {e}")))?;
    let Ok(Value::Object(mut blob)) = serde_json::from_str::<Value>(creds) else {
        return Err(RefreshError::Transient("stored credential is not JSON".into()));
    };
    let Some(Value::Object(oauth)) = blob.get_mut("claudeAiOauth") else {
        return Err(RefreshError::Permanent("stored credential has no claudeAiOauth".into()));
    };
    let Some(access) = resp.get("access_token").and_then(Value::as_str) else {
        return Err(RefreshError::Transient("token endpoint: no access_token".into()));
    };
    let expires_in = resp.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    oauth.insert("accessToken".into(), Value::String(access.to_string()));
    oauth.insert("expiresAt".into(), Value::from(now_ms() as i64 + expires_in * 1000));
    if let Some(rt) = resp.get("refresh_token").and_then(Value::as_str) {
        oauth.insert("refreshToken".into(), Value::String(rt.to_string()));
    }
    if let Some(scope) = resp.get("scope").and_then(Value::as_str) {
        oauth.insert("scopes".into(), Value::Array(scope.split_whitespace().map(|s| Value::String(s.into())).collect()));
    }
    // The grant may name the account; a free identity check.
    let ident = resp.get("account").and_then(Value::as_object).and_then(|a| {
        let uuid = a.get("uuid")?.as_str()?.to_string();
        Some(claude::Identity {
            email: a.get("email_address").and_then(Value::as_str).unwrap_or("").to_string(),
            org_uuid: resp.get("organization").and_then(|o| o.get("uuid")).and_then(Value::as_str).unwrap_or("").to_string(),
            org_name: String::new(),
            account_uuid: uuid,
        })
    });
    Ok((Value::Object(blob).to_string(), ident))
}

// ---------------------------------------------------------------------------
// usage and profile
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum UsageError {
    /// 429: back off for this long (seconds) if the server said.
    RateLimited(Option<f64>),
    /// 401: the access token does not authenticate.
    Unauthorized,
    Other(String),
}

pub async fn fetch_usage(p: &Platform, access_token: &str) -> Result<Usage, UsageError> {
    let headers = format!("Authorization: Bearer {access_token}\nanthropic-beta: {BETA_HEADER}\n");
    let r = request(p, "GET", USAGE_URL, &headers, None).await.map_err(UsageError::Other)?;
    match r.status {
        200 => {
            let raw: Value = serde_json::from_str(&r.body).map_err(|e| UsageError::Other(format!("usage: {e}")))?;
            Ok(normalize(&raw))
        }
        429 => Err(UsageError::RateLimited(r.retry_after_s)),
        401 => Err(UsageError::Unauthorized),
        s => Err(UsageError::Other(format!("usage: HTTP {s}"))),
    }
}

/// Whose token this is, from the profile endpoint; None on any failure.
pub async fn fetch_profile(p: &Platform, access_token: &str) -> Option<claude::Identity> {
    let headers = format!("Authorization: Bearer {access_token}\nContent-Type: application/json\n");
    let r = request(p, "GET", PROFILE_URL, &headers, None).await.ok()?;
    if r.status != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&r.body).ok()?;
    let a = v.get("account")?.as_object()?;
    let uuid = a.get("uuid")?.as_str()?.trim().to_string();
    if uuid.is_empty() {
        return None;
    }
    let org = v.get("organization");
    Some(claude::Identity {
        email: a.get("email").and_then(Value::as_str).unwrap_or("").to_string(),
        org_uuid: org.and_then(|o| o.get("uuid")).and_then(Value::as_str).unwrap_or("").to_string(),
        org_name: org.and_then(|o| o.get("name")).and_then(Value::as_str).unwrap_or("").to_string(),
        account_uuid: uuid,
    })
}
