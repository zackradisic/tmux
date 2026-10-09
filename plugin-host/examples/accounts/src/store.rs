//! The accounts and their cached usage, in the plugin's SQLite store.
//!
//! One row per slot. The credential blob is the JSON Claude Code itself
//! stores (`{"claudeAiOauth": {...}, ...}`), kept whole so a switch puts
//! back exactly what was captured; `oauth_account` is the `oauthAccount`
//! object from `~/.claude.json`, the identity Claude shows and refetches.
//! The store file is world-readable by default, so `init` makes the
//! plugin's data directory 0700 (see `lib.rs`).

use tmux_plugin_sdk::prelude::*;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    slot INTEGER PRIMARY KEY,
    email TEXT NOT NULL,
    org_name TEXT NOT NULL DEFAULT '',
    org_uuid TEXT NOT NULL DEFAULT '',
    account_uuid TEXT NOT NULL DEFAULT '',
    alias TEXT NOT NULL DEFAULT '',
    credentials TEXT NOT NULL,
    oauth_account TEXT NOT NULL DEFAULT '',
    disabled INTEGER NOT NULL DEFAULT 0,
    added_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    slot INTEGER PRIMARY KEY,
    fetched_ms INTEGER NOT NULL,
    json TEXT NOT NULL,
    error TEXT NOT NULL DEFAULT '',
    next_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
PRAGMA user_version = 1;";

#[derive(Debug, Clone, Default)]
pub struct Account {
    pub slot: i64,
    pub email: String,
    pub org_name: String,
    pub org_uuid: String,
    pub account_uuid: String,
    pub alias: String,
    pub credentials: String,
    pub oauth_account: String,
    pub disabled: bool,
    pub added_ms: i64,
    pub updated_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageRow {
    pub slot: i64,
    pub fetched_ms: i64,
    pub json: String,
    pub error: String,
    pub next_ms: i64,
}

fn s(row: &Row, name: &str) -> String {
    row.get_named(name).and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn i(row: &Row, name: &str) -> i64 {
    row.get_named(name).and_then(|v| v.as_i64()).unwrap_or(0)
}

fn account(row: &Row) -> Account {
    Account {
        slot: i(row, "slot"),
        email: s(row, "email"),
        org_name: s(row, "org_name"),
        org_uuid: s(row, "org_uuid"),
        account_uuid: s(row, "account_uuid"),
        alias: s(row, "alias"),
        credentials: s(row, "credentials"),
        oauth_account: s(row, "oauth_account"),
        disabled: i(row, "disabled") != 0,
        added_ms: i(row, "added_ms"),
        updated_ms: i(row, "updated_ms"),
    }
}

const COLS: &str = "slot, email, org_name, org_uuid, account_uuid, alias, credentials, \
                    oauth_account, disabled, added_ms, updated_ms";

pub async fn all() -> Result<Vec<Account>, HostError> {
    let rows = db_query(&format!("SELECT {COLS} FROM accounts ORDER BY slot"), &[]).await?;
    Ok(rows.iter().map(|r| account(&r)).collect())
}

pub async fn get(slot: i64) -> Result<Option<Account>, HostError> {
    let rows =
        db_query(&format!("SELECT {COLS} FROM accounts WHERE slot = ?1"), params![slot]).await?;
    let a = rows.iter().next().map(|r| account(&r));
    Ok(a)
}

/// A slot by number, alias, or email (the first slot with that email).
pub async fn resolve(ident: &str) -> Result<Option<Account>, HostError> {
    let ident = ident.trim();
    if let Ok(n) = ident.parse::<i64>() {
        return get(n).await;
    }
    let all = all().await?;
    Ok(all
        .iter()
        .find(|a| !a.alias.is_empty() && a.alias.eq_ignore_ascii_case(ident))
        .or_else(|| all.iter().find(|a| a.email.eq_ignore_ascii_case(ident)))
        .cloned())
}

/// The slot holding this identity: same email AND same organization.
pub fn find_identity<'a>(all: &'a [Account], email: &str, org_uuid: &str) -> Option<&'a Account> {
    all.iter().find(|a| {
        a.email.eq_ignore_ascii_case(email)
            && (org_uuid.is_empty() || a.org_uuid.is_empty() || a.org_uuid == org_uuid)
    })
}

pub fn next_slot(all: &[Account]) -> i64 {
    let mut n = 1;
    while all.iter().any(|a| a.slot == n) {
        n += 1;
    }
    n
}

/// Insert or replace a slot.
pub async fn put(a: &Account) -> Result<(), HostError> {
    db_exec(
        "INSERT INTO accounts (slot, email, org_name, org_uuid, account_uuid, alias, \
         credentials, oauth_account, disabled, added_ms, updated_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
         ON CONFLICT(slot) DO UPDATE SET email = excluded.email, \
         org_name = excluded.org_name, org_uuid = excluded.org_uuid, \
         account_uuid = excluded.account_uuid, alias = excluded.alias, \
         credentials = excluded.credentials, oauth_account = excluded.oauth_account, \
         disabled = excluded.disabled, updated_ms = excluded.updated_ms",
        params![
            a.slot,
            a.email.as_str(),
            a.org_name.as_str(),
            a.org_uuid.as_str(),
            a.account_uuid.as_str(),
            a.alias.as_str(),
            a.credentials.as_str(),
            a.oauth_account.as_str(),
            i64::from(a.disabled),
            a.added_ms,
            a.updated_ms
        ],
    )
    .await?;
    Ok(())
}

/// Replace a slot's credential blob (a rotation seen on the live login,
/// or a refresh this plugin ran).
pub async fn set_credentials(slot: i64, creds: &str, now_ms: i64) -> Result<(), HostError> {
    db_exec(
        "UPDATE accounts SET credentials = ?2, updated_ms = ?3 WHERE slot = ?1",
        params![slot, creds, now_ms],
    )
    .await?;
    Ok(())
}

pub async fn set_identity(
    slot: i64,
    email: &str,
    org_name: &str,
    org_uuid: &str,
    account_uuid: &str,
    oauth_account: &str,
) -> Result<(), HostError> {
    db_exec(
        "UPDATE accounts SET email = ?2, org_name = ?3, org_uuid = ?4, account_uuid = ?5, \
         oauth_account = CASE WHEN ?6 = '' THEN oauth_account ELSE ?6 END WHERE slot = ?1",
        params![slot, email, org_name, org_uuid, account_uuid, oauth_account],
    )
    .await?;
    Ok(())
}

pub async fn set_disabled(slot: i64, on: bool) -> Result<i64, HostError> {
    let r = db_exec(
        "UPDATE accounts SET disabled = ?2 WHERE slot = ?1",
        params![slot, i64::from(on)],
    )
    .await?;
    Ok(r.changes)
}

pub async fn set_alias(slot: i64, alias: &str) -> Result<i64, HostError> {
    let r = db_exec("UPDATE accounts SET alias = ?2 WHERE slot = ?1", params![slot, alias])
        .await?;
    Ok(r.changes)
}

pub async fn remove(slot: i64) -> Result<i64, HostError> {
    let _ = db_exec("DELETE FROM usage WHERE slot = ?1", params![slot]).await;
    let r = db_exec("DELETE FROM accounts WHERE slot = ?1", params![slot]).await?;
    Ok(r.changes)
}

pub async fn usage_all() -> Result<Vec<UsageRow>, HostError> {
    let rows = db_query("SELECT slot, fetched_ms, json, error, next_ms FROM usage", &[]).await?;
    Ok(rows
        .iter()
        .map(|r| UsageRow {
            slot: i(&r, "slot"),
            fetched_ms: i(&r, "fetched_ms"),
            json: s(&r, "json"),
            error: s(&r, "error"),
            next_ms: i(&r, "next_ms"),
        })
        .collect())
}

/// Record a fetch: the usage (kept from the previous fetch when this one
/// failed, so the panel keeps showing the last good numbers), the error
/// if any, and when the next fetch may run.
pub async fn put_usage(
    slot: i64,
    fetched_ms: i64,
    json: Option<&str>,
    error: &str,
    next_ms: i64,
) -> Result<(), HostError> {
    match json {
        Some(j) => {
            db_exec(
                "INSERT INTO usage (slot, fetched_ms, json, error, next_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(slot) DO UPDATE SET fetched_ms = excluded.fetched_ms, \
                 json = excluded.json, error = excluded.error, next_ms = excluded.next_ms",
                params![slot, fetched_ms, j, error, next_ms],
            )
            .await?;
        }
        None => {
            db_exec(
                "INSERT INTO usage (slot, fetched_ms, json, error, next_ms) \
                 VALUES (?1, 0, '', ?2, ?3) \
                 ON CONFLICT(slot) DO UPDATE SET error = excluded.error, \
                 next_ms = excluded.next_ms",
                params![slot, error, next_ms],
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn get_setting(key: &str) -> Result<Option<String>, HostError> {
    let rows = db_query("SELECT value FROM settings WHERE key = ?1", params![key]).await?;
    let v = rows.iter().next().map(|r| s(&r, "value"));
    Ok(v)
}

pub async fn set_setting(key: &str, value: &str) -> Result<(), HostError> {
    db_exec(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .await?;
    Ok(())
}
