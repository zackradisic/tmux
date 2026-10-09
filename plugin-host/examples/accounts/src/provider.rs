//! The provider half: the accounts on THIS server and everything that
//! changes them. The panel on a workstation calls these as services
//! (`list`, `switch`, `best`, `add`, `refresh`, `remove`, `disable`,
//! `alias`, `profile`, `import`); the local panel calls the functions
//! directly. Every mutation emits the `changed` topic.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tmux_plugin_sdk::prelude::*;

use crate::claude::{self, Identity, Platform};
use crate::oauth::{self, Usage};
use crate::store::{self, Account};

pub const TOPIC: &str = "changed";

// ---------------------------------------------------------------------------
// wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListReq {
    /// Fetch usage for the slots that are due before answering.
    #[serde(default)]
    pub refresh: bool,
    /// Fetch every slot, due or not (the panel's `r`).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlotReq {
    pub slot: i64,
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub alias: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddReq {
    #[serde(default)]
    pub slot: Option<i64>,
    #[serde(default)]
    pub alias: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

impl Reply {
    fn ok(message: impl Into<String>) -> Reply {
        Reply { ok: true, message: message.into(), slot: None, dir: None }
    }
    fn err(message: impl Into<String>) -> Reply {
        Reply { ok: false, message: message.into(), slot: None, dir: None }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountRow {
    pub slot: i64,
    pub email: String,
    pub org_name: String,
    pub org_uuid: String,
    pub alias: String,
    pub disabled: bool,
    /// This slot is the machine's login now.
    pub active: bool,
    pub has_refresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub usage_fetched_ms: i64,
    pub usage_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub now_ms: i64,
    pub host: String,
    pub macos: bool,
    pub user: String,
    /// Who is logged in (from `~/.claude.json`), managed or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<Identity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_slot: Option<i64>,
    pub accounts: Vec<AccountRow>,
    /// Something the panel should say under the server's name.
    pub warning: String,
}

// ---------------------------------------------------------------------------
// a tiny join
// ---------------------------------------------------------------------------

struct JoinAll {
    futs: Vec<Pin<Box<dyn Future<Output = ()>>>>,
}

impl Future for JoinAll {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.futs.retain_mut(|f| f.as_mut().poll(cx).is_pending());
        if self.futs.is_empty() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

pub fn join_all(futs: Vec<Pin<Box<dyn Future<Output = ()>>>>) -> impl Future<Output = ()> {
    JoinAll { futs }
}

// ---------------------------------------------------------------------------
// the live login
// ---------------------------------------------------------------------------

/// What is logged in on this machine: the identity from the config, the
/// credential, and the slot that holds it (by identity, else lineage).
pub struct Live {
    pub identity: Option<Identity>,
    pub oauth_account: Option<Value>,
    pub creds: Option<String>,
    pub slot: Option<i64>,
}

pub async fn live(p: &Platform, accounts: &[Account]) -> Live {
    let oauth_account = claude::live_oauth_account(p).await.ok().flatten();
    let identity = oauth_account.as_ref().and_then(claude::identity_of);
    let creds = claude::read_live_credentials(p).await.ok().flatten();
    let mut slot = identity
        .as_ref()
        .and_then(|i| store::find_identity(accounts, &i.email, &i.org_uuid))
        .map(|a| a.slot);
    if slot.is_none() {
        if let Some(c) = &creds {
            let fp = claude::fingerprint(c);
            slot = accounts.iter().find(|a| claude::fingerprint(&a.credentials) == fp).map(|a| a.slot);
        }
    }
    Live { identity, oauth_account, creds, slot }
}

/// Keep the live slot's backup current: a rotation claude did on the
/// login, or a fuller `oauthAccount` than the one stored.
async fn track_live(live: &Live, accounts: &[Account], now: i64) {
    let Some(slot) = live.slot else { return };
    let Some(a) = accounts.iter().find(|a| a.slot == slot) else { return };
    if let Some(c) = &live.creds {
        if c != &a.credentials && claude::refresh_token(c).is_some() {
            let _ = store::set_credentials(slot, c, now).await;
        }
    }
    if let (Some(oa), Some(id)) = (&live.oauth_account, &live.identity) {
        let text = oa.to_string();
        if text != a.oauth_account {
            let _ = store::set_identity(slot, &id.email, &id.org_name, &id.org_uuid, &id.account_uuid, &text).await;
        }
    }
}

// ---------------------------------------------------------------------------
// usage
// ---------------------------------------------------------------------------

/// A usable access token for a slot, refreshing when expired. For the
/// live slot the refresh happens under claude's locks and the result is
/// installed, so a running claude never finds its token pulled out from
/// under it; for any other slot the rotated blob is stored.
async fn fresh_token(p: &Platform, a: &Account, is_live: bool, now: i64) -> Result<String, String> {
    let creds = a.credentials.clone();
    if !claude::token_expired(&creds, now) {
        return claude::access_token(&creds).ok_or_else(|| "no access token".into());
    }
    if is_live {
        // Under the locks, with a re-read: claude may have refreshed
        // since the snapshot was taken.
        let under = claude::lock_credentials(p).await?;
        let r = async {
            if let Some(c) = &under {
                if !claude::token_expired(c, now) {
                    return claude::access_token(c).ok_or_else(|| "no access token".to_string());
                }
            }
            let base = under.clone().unwrap_or(creds.clone());
            let (rotated, _) = oauth::refresh(p, &base).await.map_err(|e| e.message().to_string())?;
            claude::write_credentials_locked(p, &rotated).await?;
            let _ = store::set_credentials(a.slot, &rotated, now).await;
            claude::access_token(&rotated).ok_or_else(|| "no access token".to_string())
        }
        .await;
        claude::unlock_credentials(p).await;
        return r;
    }
    let (rotated, _) = oauth::refresh(p, &creds).await.map_err(|e| e.message().to_string())?;
    let _ = store::set_credentials(a.slot, &rotated, now).await;
    claude::access_token(&rotated).ok_or_else(|| "no access token".into())
}

async fn fetch_one(p: Rc<Platform>, a: Account, is_live: bool, now: i64) {
    let token = match fresh_token(&p, &a, is_live, now).await {
        Ok(t) => t,
        Err(e) => {
            let _ = store::put_usage(a.slot, now, None, &e, now + oauth::BACKOFF_MS).await;
            return;
        }
    };
    match oauth::fetch_usage(&p, &token).await {
        Ok(u) => {
            let json = serde_json::to_string(&u).unwrap_or_default();
            // A slot with nothing left is not asked again soon; the
            // reset is what changes it, not another request.
            let next = if u.headroom().is_some_and(|h| h <= 0.0) {
                now + oauth::BACKOFF_MS
            } else {
                now + oauth::MIN_INTERVAL_MS
            };
            let _ = store::put_usage(a.slot, now, Some(&json), "", next).await;
        }
        Err(oauth::UsageError::RateLimited(after)) => {
            let wait = after.map(|s| (s * 1000.0) as i64 + 60_000).unwrap_or(oauth::BACKOFF_MS).max(oauth::BACKOFF_MS);
            let _ = store::put_usage(a.slot, now, None, "usage endpoint rate limited; backing off", now + wait).await;
        }
        Err(oauth::UsageError::Unauthorized) => {
            let _ = store::put_usage(a.slot, now, None, "token rejected; log in again and press a", now + oauth::BACKOFF_MS).await;
        }
        Err(oauth::UsageError::Other(e)) => {
            let _ = store::put_usage(a.slot, now, None, &e, now + oauth::MIN_INTERVAL_MS).await;
        }
    }
}

/// Fetch usage for the slots that are due (or all, with `force`).
pub async fn refresh_usage(p: &Rc<Platform>, accounts: &[Account], live_slot: Option<i64>, force: bool) {
    let now = now_ms() as i64;
    let rows = store::usage_all().await.unwrap_or_default();
    let mut futs: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
    for a in accounts {
        if a.disabled {
            continue;
        }
        let next = rows.iter().find(|u| u.slot == a.slot).map(|u| u.next_ms).unwrap_or(0);
        if !force && now < next {
            continue;
        }
        futs.push(Box::pin(fetch_one(Rc::clone(p), a.clone(), live_slot == Some(a.slot), now)));
    }
    join_all(futs).await;
}

// ---------------------------------------------------------------------------
// the snapshot
// ---------------------------------------------------------------------------

pub async fn snapshot(req: &ListReq) -> Snapshot {
    let p = claude::platform().await;
    let now = now_ms() as i64;
    let accounts = store::all().await.unwrap_or_default();
    let lv = live(&p, &accounts).await;
    track_live(&lv, &accounts, now).await;
    if req.refresh || req.force {
        refresh_usage(&p, &accounts, lv.slot, req.force).await;
    }
    let accounts = store::all().await.unwrap_or_default();
    let usage = store::usage_all().await.unwrap_or_default();
    let rows = accounts
        .iter()
        .map(|a| {
            let u = usage.iter().find(|u| u.slot == a.slot);
            AccountRow {
                slot: a.slot,
                email: a.email.clone(),
                org_name: a.org_name.clone(),
                org_uuid: a.org_uuid.clone(),
                alias: a.alias.clone(),
                disabled: a.disabled,
                active: lv.slot == Some(a.slot),
                has_refresh: claude::refresh_token(&a.credentials).is_some(),
                usage: u.and_then(|u| serde_json::from_str(&u.json).ok()),
                usage_fetched_ms: u.map(|u| u.fetched_ms).unwrap_or(0),
                usage_error: u.map(|u| u.error.clone()).unwrap_or_default(),
            }
        })
        .collect();
    let mut warning = String::new();
    if p.no_shell {
        warning = "this copy cannot run a shell: add run-process, fs-write and fs-read-any to plugin-remote-caps on this server (ACCOUNTS.md)".into();
    } else if !p.curl {
        warning = "curl is not installed here: no usage".into();
    } else if lv.identity.is_some() && lv.slot.is_none() {
        warning = "the login here is not a slot yet: a adds it".into();
    } else if lv.identity.is_none() {
        warning = "nobody is logged in here".into();
    }
    Snapshot {
        now_ms: now,
        host: p.host.clone(),
        macos: p.macos,
        user: p.user.clone(),
        live: lv.identity,
        live_slot: lv.slot,
        accounts: rows,
        warning,
    }
}

// ---------------------------------------------------------------------------
// mutations
// ---------------------------------------------------------------------------

fn changed() {
    let _ = service::emit_json(TOPIC, &serde_json::json!({ "at": now_ms() }));
}

/// The `oauthAccount` to install for a slot: the stored object, or one
/// built from the identity fields when a slot has none (imported).
fn oauth_account_for(a: &Account) -> Value {
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&a.oauth_account) {
        return v;
    }
    let mut m = serde_json::Map::new();
    m.insert("emailAddress".into(), Value::String(a.email.clone()));
    if !a.org_uuid.is_empty() {
        m.insert("organizationUuid".into(), Value::String(a.org_uuid.clone()));
    }
    if !a.org_name.is_empty() {
        m.insert("organizationName".into(), Value::String(a.org_name.clone()));
    }
    if !a.account_uuid.is_empty() {
        m.insert("accountUuid".into(), Value::String(a.account_uuid.clone()));
    }
    Value::Object(m)
}

/// Make `slot` the machine's login.
pub async fn switch(slot: i64) -> Reply {
    let p = claude::platform().await;
    let now = now_ms() as i64;
    let accounts = store::all().await.unwrap_or_default();
    let Some(target) = accounts.iter().find(|a| a.slot == slot).cloned() else {
        return Reply::err(format!("no slot {slot}"));
    };
    let lv = live(&p, &accounts).await;
    track_live(&lv, &accounts, now).await;
    let target = store::get(slot).await.ok().flatten().unwrap_or(target);
    let label = format!("{} {}", target.slot, target.email);

    // Already the login: make sure the identity matches and stop.
    if lv.slot == Some(slot) {
        if let Some(c) = &lv.creds {
            if claude::fingerprint(c) == claude::fingerprint(&target.credentials) {
                return Reply { slot: Some(slot), ..Reply::ok(format!("already on {label}")) };
            }
        }
    }

    // A fresh access token first, outside the locks (network under a
    // lock is forbidden): claude then starts without a refresh of its
    // own. A dead refresh token is the one hard failure.
    let mut creds = target.credentials.clone();
    if claude::token_expired(&creds, now) {
        match oauth::refresh(&p, &creds).await {
            Ok((rotated, _)) => {
                let _ = store::set_credentials(slot, &rotated, now).await;
                creds = rotated;
            }
            Err(oauth::RefreshError::Permanent(m)) => {
                return Reply::err(format!("{label}: {m}"));
            }
            Err(oauth::RefreshError::Transient(_)) => {}
        }
    }

    // Under claude's locks: read what is live now, back it up into its
    // slot, compose and write the target.
    let under = match claude::lock_credentials(&p).await {
        Ok(u) => u,
        Err(e) => return Reply::err(e),
    };
    if let (Some(prev_slot), Some(c)) = (lv.slot, &under) {
        if prev_slot != slot && claude::refresh_token(c).is_some() {
            let _ = store::set_credentials(prev_slot, c, now).await;
        }
    }
    let composed = claude::compose_for_install(&creds, under.as_deref());
    let w = claude::write_credentials_locked(&p, &composed).await;
    claude::unlock_credentials(&p).await;
    if let Err(e) = w {
        return Reply::err(e);
    }
    if let Err(e) = claude::install_oauth_account(&p, &oauth_account_for(&target)).await {
        return Reply::err(format!("credentials switched but {e}"));
    }
    let _ = store::set_setting("active", &slot.to_string()).await;
    changed();
    Reply { slot: Some(slot), ..Reply::ok(format!("switched to {label}")) }
}

/// The slot with the most headroom, switched to.
pub async fn best() -> Reply {
    let p = claude::platform().await;
    let accounts = store::all().await.unwrap_or_default();
    let lv = live(&p, &accounts).await;
    refresh_usage(&p, &accounts, lv.slot, false).await;
    let usage = store::usage_all().await.unwrap_or_default();
    let mut ranked: Vec<(f64, i64)> = accounts
        .iter()
        .filter(|a| !a.disabled)
        .filter_map(|a| {
            let u = usage.iter().find(|u| u.slot == a.slot)?;
            let parsed: Usage = serde_json::from_str(&u.json).ok()?;
            Some((parsed.headroom()?, a.slot))
        })
        .collect();
    if ranked.is_empty() {
        return Reply::err("no usage known for any slot; refresh first");
    }
    // Most headroom; the current login wins a tie.
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| {
        let cur = |s: i64| i32::from(lv.slot != Some(s));
        cur(a.1).cmp(&cur(b.1))
    }));
    let (head, slot) = ranked[0];
    if lv.slot == Some(slot) {
        return Reply { slot: Some(slot), ..Reply::ok(format!("already on the best pick ({head:.0}% left)")) };
    }
    let mut r = switch(slot).await;
    if r.ok {
        r.message = format!("{} ({head:.0}% left)", r.message);
    }
    r
}

/// Register the machine's current login as a slot (or refresh the slot
/// that already holds it).
pub async fn add(req: &AddReq) -> Reply {
    let p = claude::platform().await;
    let now = now_ms() as i64;
    let accounts = store::all().await.unwrap_or_default();
    let lv = live(&p, &accounts).await;
    let Some(creds) = lv.creds.clone() else {
        return Reply::err("no login here: run claude and log in, then add");
    };
    if claude::refresh_token(&creds).is_none() {
        return Reply::err("the login here has no refresh token (an API key?); only OAuth logins can be stored");
    }
    // Identity from the config; when it lacks one, ask the API.
    let mut identity = lv.identity.clone();
    if identity.is_none() {
        if let Some(tok) = claude::access_token(&creds) {
            identity = oauth::fetch_profile(&p, &tok).await;
        }
    }
    let Some(id) = identity else {
        return Reply::err("cannot tell whose login this is (no oauthAccount in ~/.claude.json)");
    };
    let oauth_account = lv.oauth_account.clone().map(|v| v.to_string()).unwrap_or_default();
    let existing = store::find_identity(&accounts, &id.email, &id.org_uuid).cloned();
    let slot = match (req.slot, &existing) {
        (Some(s), _) => s,
        (None, Some(e)) => e.slot,
        (None, None) => store::next_slot(&accounts),
    };
    if let Some(taken) = accounts.iter().find(|a| a.slot == slot) {
        if existing.as_ref().map(|e| e.slot) != Some(slot) {
            return Reply::err(format!("slot {slot} holds {}; remove it first or pick another", taken.email));
        }
    }
    let alias = if req.alias.is_empty() {
        existing.as_ref().map(|e| e.alias.clone()).unwrap_or_default()
    } else {
        req.alias.clone()
    };
    let a = Account {
        slot,
        email: id.email.clone(),
        org_name: id.org_name.clone(),
        org_uuid: id.org_uuid.clone(),
        account_uuid: id.account_uuid.clone(),
        alias,
        credentials: creds,
        oauth_account,
        disabled: false,
        added_ms: existing.as_ref().map(|e| e.added_ms).unwrap_or(now),
        updated_ms: now,
    };
    if let Err(e) = store::put(&a).await {
        return Reply::err(e.message.clone());
    }
    // If the login moved slots (same account re-added under a number),
    // the old row would be a duplicate.
    if let Some(e) = existing.filter(|e| e.slot != slot) {
        let _ = store::remove(e.slot).await;
    }
    let _ = store::set_setting("active", &slot.to_string()).await;
    changed();
    Reply { slot: Some(slot), ..Reply::ok(format!("slot {slot}: {} [{}]", id.email, id.org_name)) }
}

pub async fn remove(slot: i64) -> Reply {
    match store::remove(slot).await {
        Ok(0) => Reply::err(format!("no slot {slot}")),
        Ok(_) => {
            changed();
            Reply::ok(format!("removed slot {slot} (the login itself is untouched)"))
        }
        Err(e) => Reply::err(e.message.clone()),
    }
}

pub async fn set_disabled(slot: i64, on: bool) -> Reply {
    match store::set_disabled(slot, on).await {
        Ok(0) => Reply::err(format!("no slot {slot}")),
        Ok(_) => {
            changed();
            Reply::ok(if on { format!("slot {slot} held out of best pick") } else { format!("slot {slot} back in") })
        }
        Err(e) => Reply::err(e.message.clone()),
    }
}

pub async fn set_alias(slot: i64, alias: &str) -> Reply {
    match store::set_alias(slot, alias.trim()).await {
        Ok(0) => Reply::err(format!("no slot {slot}")),
        Ok(_) => {
            changed();
            Reply::ok(format!("slot {slot} is {alias}"))
        }
        Err(e) => Reply::err(e.message.clone()),
    }
}

/// A `CLAUDE_CONFIG_DIR` for one claude to run as this slot.
pub async fn profile(slot: i64) -> Reply {
    let p = claude::platform().await;
    let now = now_ms() as i64;
    let Some(a) = store::get(slot).await.ok().flatten() else {
        return Reply::err(format!("no slot {slot}"));
    };
    let mut creds = a.credentials.clone();
    if claude::token_expired(&creds, now) {
        match oauth::refresh(&p, &creds).await {
            Ok((rotated, _)) => {
                let _ = store::set_credentials(slot, &rotated, now).await;
                creds = rotated;
            }
            Err(oauth::RefreshError::Permanent(m)) => return Reply::err(format!("slot {slot}: {m}")),
            Err(oauth::RefreshError::Transient(_)) => {}
        }
    }
    match claude::ensure_profile(&p, slot, &a.email, &creds, &oauth_account_for(&a)).await {
        Ok(dir) => Reply { slot: Some(slot), dir: Some(dir.clone()), ..Reply::ok(dir) },
        Err(e) => Reply::err(e),
    }
}

/// Bring cswap's accounts over (macOS: its Keychain items, named by
/// `~/.claude-swap-backup/sequence.json`).
pub async fn import_cswap() -> Reply {
    let p = claude::platform().await;
    let now = now_ms() as i64;
    let seq_path = format!("{}/.claude-swap-backup/sequence.json", p.home);
    let Ok(Some(text)) = claude::read_file(&seq_path).await else {
        return Reply::err("no ~/.claude-swap-backup/sequence.json here");
    };
    let Ok(seq) = serde_json::from_str::<Value>(&text) else {
        return Reply::err("sequence.json is not JSON");
    };
    let Some(list) = seq.get("accounts").and_then(Value::as_object) else {
        return Reply::err("sequence.json has no accounts");
    };
    let existing = store::all().await.unwrap_or_default();
    let mut n = 0;
    let mut skipped = Vec::new();
    for (num, rec) in list {
        let Ok(slot) = num.parse::<i64>() else { continue };
        let g = |k: &str| rec.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let email = g("email");
        if email.is_empty() {
            continue;
        }
        let creds = if p.macos {
            let cmd = format!(
                "security find-generic-password -a {} -s claude-swap -w 2>/dev/null",
                claude::sq(&format!("account-{slot}-{email}"))
            );
            run_job(&cmd, None).await.ok().filter(|o| o.status == 0).map(|o| o.output.trim_end_matches('\n').to_string())
        } else {
            let base = format!("{}/.local/share/claude-swap/credentials", p.home);
            claude::read_file(&format!("{base}/.credentials-{slot}-{email}.json")).await.ok().flatten()
        };
        let Some(creds) = creds.filter(|c| claude::refresh_token(c).is_some()) else {
            skipped.push(format!("{slot} {email}"));
            continue;
        };
        let cfg = claude::read_file(&format!("{}/.claude-swap-backup/configs/.claude-config-{slot}-{email}.json", p.home))
            .await
            .ok()
            .flatten()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("oauthAccount").cloned())
            .map(|v| v.to_string())
            .unwrap_or_default();
        let prev = existing.iter().find(|a| a.slot == slot);
        let a = Account {
            slot,
            email: email.clone(),
            org_name: g("organizationName"),
            org_uuid: g("organizationUuid"),
            account_uuid: prev.map(|a| a.account_uuid.clone()).unwrap_or_default(),
            alias: if g("alias").is_empty() { prev.map(|a| a.alias.clone()).unwrap_or_default() } else { g("alias") },
            credentials: creds,
            oauth_account: if cfg.is_empty() { prev.map(|a| a.oauth_account.clone()).unwrap_or_default() } else { cfg },
            disabled: prev.map(|a| a.disabled).unwrap_or(false),
            added_ms: prev.map(|a| a.added_ms).unwrap_or(now),
            updated_ms: now,
        };
        if store::put(&a).await.is_ok() {
            n += 1;
        }
    }
    changed();
    let mut msg = format!("imported {n} from cswap");
    if !skipped.is_empty() {
        msg.push_str(&format!("; no credential for {}", skipped.join(", ")));
    }
    Reply::ok(msg)
}

// ---------------------------------------------------------------------------
// the service surface
// ---------------------------------------------------------------------------

pub const METHODS: [&str; 10] =
    ["list", "switch", "best", "add", "refresh", "remove", "disable", "alias", "profile", "import"];

pub async fn handle(req: ServiceRequest) {
    let r: Result<(), HostError> = match req.method.as_str() {
        "list" => {
            let lr: ListReq = req.json().unwrap_or_default();
            req.reply_json(&snapshot(&lr).await)
        }
        "refresh" => req.reply_json(&snapshot(&ListReq { refresh: true, force: true }).await),
        "switch" => match req.json::<SlotReq>() {
            Ok(s) => req.reply_json(&switch(s.slot).await),
            Err(e) => req.fail(&e.message),
        },
        "best" => req.reply_json(&best().await),
        "add" => {
            let a: AddReq = req.json().unwrap_or_default();
            req.reply_json(&add(&a).await)
        }
        "remove" => match req.json::<SlotReq>() {
            Ok(s) => req.reply_json(&remove(s.slot).await),
            Err(e) => req.fail(&e.message),
        },
        "disable" => match req.json::<SlotReq>() {
            Ok(s) => req.reply_json(&set_disabled(s.slot, s.on).await),
            Err(e) => req.fail(&e.message),
        },
        "alias" => match req.json::<SlotReq>() {
            Ok(s) => req.reply_json(&set_alias(s.slot, &s.alias).await),
            Err(e) => req.fail(&e.message),
        },
        "profile" => match req.json::<SlotReq>() {
            Ok(s) => req.reply_json(&profile(s.slot).await),
            Err(e) => req.fail(&e.message),
        },
        "import" => req.reply_json(&import_cswap().await),
        _ => req.fail("unknown method"),
    };
    if let Err(e) = r {
        log(&format!("accounts: reply {}: {}", req.method, e.message));
    }
}
