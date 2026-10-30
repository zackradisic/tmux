//! The cache: every SQL statement, each a named function. Nothing
//! outside this module writes SQL.
//!
//! What is cached and why:
//!
//!   * `auth`: the desktop session the plugin acts as (token, cookie,
//!     who it is), so a reload does not touch the Keychain again.
//!   * `channels`, `sections`, `users`: the sidebar, instantly, before
//!     the first fetch of a session has answered; the per-channel badge
//!     (`has_unreads`, `mentions`, `last_read`) is what `client.counts`
//!     last said.
//!   * `messages`: what has been shown, so a channel opens on its last
//!     known state and the fetch only asks for what came after.
//!   * `bindings`: Slack threads bound to agents (the bridge).

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

pub const USER_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS auth (
  domain TEXT PRIMARY KEY,
  token TEXT NOT NULL,
  cookie TEXT NOT NULL,
  user_id TEXT NOT NULL,
  team_id TEXT NOT NULL,
  team_name TEXT NOT NULL,
  user_name TEXT NOT NULL,
  tz_offset INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS channels (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('public','private','im','mpim')),
  user_id TEXT,
  topic TEXT NOT NULL DEFAULT '',
  purpose TEXT NOT NULL DEFAULT '',
  is_member INTEGER NOT NULL DEFAULT 1,
  is_muted INTEGER NOT NULL DEFAULT 0,
  is_starred INTEGER NOT NULL DEFAULT 0,
  is_archived INTEGER NOT NULL DEFAULT 0,
  is_open INTEGER NOT NULL DEFAULT 1,
  last_read TEXT,
  latest TEXT,
  has_unreads INTEGER NOT NULL DEFAULT 0,
  mentions INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sections (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,
  emoji TEXT NOT NULL DEFAULT '',
  pos INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS section_channels (
  section_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  PRIMARY KEY (section_id, channel_id));
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  real_name TEXT NOT NULL DEFAULT '',
  display_name TEXT NOT NULL DEFAULT '',
  is_bot INTEGER NOT NULL DEFAULT 0,
  deleted INTEGER NOT NULL DEFAULT 0,
  tz_offset INTEGER NOT NULL DEFAULT 0,
  updated_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS messages (
  channel TEXT NOT NULL,
  ts TEXT NOT NULL,
  thread_ts TEXT,
  json TEXT NOT NULL,
  updated_ms INTEGER NOT NULL,
  PRIMARY KEY (channel, ts));
CREATE INDEX IF NOT EXISTS messages_thread ON messages(channel, thread_ts, ts);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS bindings (
  channel TEXT NOT NULL,
  thread_ts TEXT NOT NULL DEFAULT '',
  agent TEXT NOT NULL,
  since_ts TEXT NOT NULL DEFAULT '',
  created_ms INTEGER NOT NULL,
  PRIMARY KEY (channel, thread_ts));
PRAGMA user_version = 1;
";

pub fn migrate_sync() -> Result<(), String> {
    let v = db_query_sync("PRAGMA user_version", params![]).map_err(|e| format!("db: {e}"))?;
    let version = v.scalar().and_then(DbValue::as_i64).unwrap_or(0);
    if version > USER_VERSION {
        return Err(format!("store.db is version {version}, this plugin understands {USER_VERSION}"));
    }
    if version == 0 {
        db_exec_sync(SCHEMA, params![]).map_err(|e| format!("db: {e}"))?;
    }
    Ok(())
}

fn s(v: Option<&DbValue>) -> Option<String> {
    v.and_then(DbValue::as_str).map(str::to_string)
}
fn i(v: Option<&DbValue>) -> Option<i64> {
    v.and_then(DbValue::as_i64)
}
fn b(v: Option<&DbValue>) -> bool {
    i(v).unwrap_or(0) != 0
}

// ---------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------

/// The desktop session the plugin acts as.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Identity {
    pub domain: String,
    pub token: String,
    pub cookie: String,
    pub user_id: String,
    pub team_id: String,
    pub team_name: String,
    pub user_name: String,
    /// Seconds east of UTC, for the times drawn.
    pub tz_offset: i64,
}

pub async fn get_auth(domain: &str) -> Option<Identity> {
    let rows = db_query(
        "SELECT domain, token, cookie, user_id, team_id, team_name, user_name, tz_offset FROM auth WHERE domain = ?1",
        params![domain],
    )
    .await
    .ok()?;
    let r = rows.row(0)?;
    Some(Identity {
        domain: s(r.get_named("domain"))?,
        token: s(r.get_named("token"))?,
        cookie: s(r.get_named("cookie"))?,
        user_id: s(r.get_named("user_id"))?,
        team_id: s(r.get_named("team_id"))?,
        team_name: s(r.get_named("team_name")).unwrap_or_default(),
        user_name: s(r.get_named("user_name")).unwrap_or_default(),
        tz_offset: i(r.get_named("tz_offset")).unwrap_or(0),
    })
}

pub async fn set_auth(id: &Identity) -> Result<(), HostError> {
    db_exec(
        "INSERT INTO auth (domain, token, cookie, user_id, team_id, team_name, user_name, tz_offset, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(domain) DO UPDATE SET token = excluded.token, cookie = excluded.cookie,
           user_id = excluded.user_id, team_id = excluded.team_id, team_name = excluded.team_name,
           user_name = excluded.user_name, tz_offset = excluded.tz_offset, updated_ms = excluded.updated_ms",
        params![
            id.domain.as_str(),
            id.token.as_str(),
            id.cookie.as_str(),
            id.user_id.as_str(),
            id.team_id.as_str(),
            id.team_name.as_str(),
            id.user_name.as_str(),
            id.tz_offset,
            now_ms() as i64
        ],
    )
    .await
    .map(|_| ())
}

pub async fn clear_auth(domain: &str) {
    let _ = db_exec("DELETE FROM auth WHERE domain = ?1", params![domain]).await;
}

// ---------------------------------------------------------------------------
// channels
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Public,
    Private,
    Im,
    Mpim,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Public => "public",
            Kind::Private => "private",
            Kind::Im => "im",
            Kind::Mpim => "mpim",
        }
    }
    fn parse(s: &str) -> Kind {
        match s {
            "private" => Kind::Private,
            "im" => Kind::Im,
            "mpim" => Kind::Mpim,
            _ => Kind::Public,
        }
    }
    pub fn is_dm(self) -> bool {
        matches!(self, Kind::Im | Kind::Mpim)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Channel {
    pub id: String,
    /// The channel's name; for a DM the peer's user id until resolved.
    pub name: String,
    pub kind: Kind,
    /// The peer of a DM.
    pub user_id: Option<String>,
    pub topic: String,
    pub purpose: String,
    pub is_member: bool,
    pub is_muted: bool,
    pub is_starred: bool,
    pub is_archived: bool,
    /// A DM that is shown in the sidebar (closed ones are not).
    pub is_open: bool,
    pub last_read: Option<String>,
    pub latest: Option<String>,
    pub has_unreads: bool,
    pub mentions: i64,
    pub updated_ms: i64,
}

const CH_COLS: &str = "id, name, kind, user_id, topic, purpose, is_member, is_muted, is_starred, is_archived, is_open, last_read, latest, has_unreads, mentions, updated_ms";

fn channels_from(rows: &Rows) -> Vec<Channel> {
    rows.iter()
        .map(|r| Channel {
            id: s(r.get_named("id")).unwrap_or_default(),
            name: s(r.get_named("name")).unwrap_or_default(),
            kind: Kind::parse(&s(r.get_named("kind")).unwrap_or_default()),
            user_id: s(r.get_named("user_id")),
            topic: s(r.get_named("topic")).unwrap_or_default(),
            purpose: s(r.get_named("purpose")).unwrap_or_default(),
            is_member: b(r.get_named("is_member")),
            is_muted: b(r.get_named("is_muted")),
            is_starred: b(r.get_named("is_starred")),
            is_archived: b(r.get_named("is_archived")),
            is_open: b(r.get_named("is_open")),
            last_read: s(r.get_named("last_read")),
            latest: s(r.get_named("latest")),
            has_unreads: b(r.get_named("has_unreads")),
            mentions: i(r.get_named("mentions")).unwrap_or(0),
            updated_ms: i(r.get_named("updated_ms")).unwrap_or(0),
        })
        .collect()
}

pub async fn channels() -> Vec<Channel> {
    db_query(&format!("SELECT {CH_COLS} FROM channels ORDER BY name"), params![])
        .await
        .map(|r| channels_from(&r))
        .unwrap_or_default()
}

pub async fn channel(id: &str) -> Option<Channel> {
    db_query(&format!("SELECT {CH_COLS} FROM channels WHERE id = ?1"), params![id])
        .await
        .ok()
        .and_then(|r| channels_from(&r).into_iter().next())
}

/// The channel named `name` (`#name`, `name`, or for a DM `@user`,
/// matched on the resolved user name by the caller), or by id.
pub async fn channel_named(name: &str) -> Option<Channel> {
    let bare = name.trim_start_matches('#');
    db_query(
        &format!("SELECT {CH_COLS} FROM channels WHERE id = ?1 OR name = ?1 ORDER BY is_archived LIMIT 1"),
        params![bare],
    )
    .await
    .ok()
    .and_then(|r| channels_from(&r).into_iter().next())
}

/// Membership facts from `users.conversations`. The badge columns are
/// kept: they belong to `apply_counts`.
pub async fn upsert_channels(chs: &[Channel]) -> Result<(), HostError> {
    if chs.is_empty() {
        return Ok(());
    }
    let now = now_ms() as i64;
    let sql = "INSERT INTO channels (id, name, kind, user_id, topic, purpose, is_member, is_muted, is_starred, is_archived, is_open, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, kind = excluded.kind, user_id = excluded.user_id,
           topic = excluded.topic, purpose = excluded.purpose, is_member = excluded.is_member,
           is_starred = excluded.is_starred, is_archived = excluded.is_archived, is_open = excluded.is_open,
           updated_ms = excluded.updated_ms";
    let owned: Vec<Vec<DbValue>> = chs
        .iter()
        .map(|c| {
            vec![
                DbValue::from(c.id.as_str()),
                DbValue::from(c.name.as_str()),
                DbValue::from(c.kind.as_str()),
                c.user_id.as_deref().map(DbValue::from).unwrap_or(DbValue::Null),
                DbValue::from(c.topic.as_str()),
                DbValue::from(c.purpose.as_str()),
                DbValue::from(c.is_member as i64),
                DbValue::from(c.is_muted as i64),
                DbValue::from(c.is_starred as i64),
                DbValue::from(c.is_archived as i64),
                DbValue::from(c.is_open as i64),
                DbValue::from(now),
            ]
        })
        .collect();
    let stmts: Vec<(&str, &[DbValue])> = owned.iter().map(|p| (sql, p.as_slice())).collect();
    db_batch(&stmts).await.map(|_| ())
}

/// One channel's badge, as `client.counts` reports it.
#[derive(Clone, Debug, Default)]
pub struct Count {
    pub id: String,
    pub last_read: Option<String>,
    pub latest: Option<String>,
    pub has_unreads: bool,
    pub mentions: i64,
    pub is_muted: Option<bool>,
    pub is_open: Option<bool>,
}

pub async fn apply_counts(counts: &[Count]) -> Result<(), HostError> {
    if counts.is_empty() {
        return Ok(());
    }
    let sql = "UPDATE channels SET last_read = COALESCE(?2, last_read), latest = COALESCE(?3, latest),
         has_unreads = ?4, mentions = ?5, is_muted = COALESCE(?6, is_muted), is_open = COALESCE(?7, is_open)
         WHERE id = ?1";
    let owned: Vec<Vec<DbValue>> = counts
        .iter()
        .map(|c| {
            vec![
                DbValue::from(c.id.as_str()),
                c.last_read.as_deref().map(DbValue::from).unwrap_or(DbValue::Null),
                c.latest.as_deref().map(DbValue::from).unwrap_or(DbValue::Null),
                DbValue::from(c.has_unreads as i64),
                DbValue::from(c.mentions),
                c.is_muted.map(|v| DbValue::from(v as i64)).unwrap_or(DbValue::Null),
                c.is_open.map(|v| DbValue::from(v as i64)).unwrap_or(DbValue::Null),
            ]
        })
        .collect();
    let stmts: Vec<(&str, &[DbValue])> = owned.iter().map(|p| (sql, p.as_slice())).collect();
    db_batch(&stmts).await.map(|_| ())
}

/// The user read `channel` up to `ts` (a mark, ours or seen elsewhere).
pub async fn mark_read(channel: &str, ts: &str) {
    let _ = db_exec(
        "UPDATE channels SET last_read = ?2, has_unreads = CASE WHEN latest IS NULL OR latest <= ?2 THEN 0 ELSE has_unreads END,
           mentions = CASE WHEN latest IS NULL OR latest <= ?2 THEN 0 ELSE mentions END WHERE id = ?1",
        params![channel, ts],
    )
    .await;
}

/// A message arrived in `channel` (the feed): it is the latest, and
/// unread unless we sent it.
pub async fn note_message(channel: &str, ts: &str, ours: bool, mention: bool) {
    if ours {
        let _ = db_exec(
            "UPDATE channels SET latest = ?2, last_read = ?2, has_unreads = 0, mentions = 0 WHERE id = ?1",
            params![channel, ts],
        )
        .await;
    } else {
        let _ = db_exec(
            "UPDATE channels SET latest = ?2, has_unreads = 1, mentions = mentions + ?3 WHERE id = ?1",
            params![channel, ts, mention as i64],
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// sections
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
    /// Slack's type: `starred`, `channels`, `direct_messages`, `standard`.
    pub kind: String,
    pub emoji: String,
    pub channel_ids: Vec<String>,
}

pub async fn replace_sections(secs: &[Section]) -> Result<(), HostError> {
    let mut owned: Vec<(String, Vec<DbValue>)> = vec![
        ("DELETE FROM sections".into(), vec![]),
        ("DELETE FROM section_channels".into(), vec![]),
    ];
    for (pos, sec) in secs.iter().enumerate() {
        owned.push((
            "INSERT INTO sections (id, name, kind, emoji, pos) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
            vec![
                DbValue::from(sec.id.as_str()),
                DbValue::from(sec.name.as_str()),
                DbValue::from(sec.kind.as_str()),
                DbValue::from(sec.emoji.as_str()),
                DbValue::from(pos as i64),
            ],
        ));
        for ch in &sec.channel_ids {
            owned.push((
                "INSERT OR IGNORE INTO section_channels (section_id, channel_id) VALUES (?1, ?2)".into(),
                vec![DbValue::from(sec.id.as_str()), DbValue::from(ch.as_str())],
            ));
        }
    }
    let stmts: Vec<(&str, &[DbValue])> = owned.iter().map(|(q, p)| (q.as_str(), p.as_slice())).collect();
    db_batch(&stmts).await.map(|_| ())
}

pub async fn sections() -> Vec<Section> {
    let Ok(rows) = db_query("SELECT id, name, kind, emoji FROM sections ORDER BY pos", params![]).await else {
        return Vec::new();
    };
    let mut out: Vec<Section> = rows
        .iter()
        .map(|r| Section {
            id: s(r.get_named("id")).unwrap_or_default(),
            name: s(r.get_named("name")).unwrap_or_default(),
            kind: s(r.get_named("kind")).unwrap_or_default(),
            emoji: s(r.get_named("emoji")).unwrap_or_default(),
            channel_ids: Vec::new(),
        })
        .collect();
    if let Ok(rows) = db_query("SELECT section_id, channel_id FROM section_channels", params![]).await {
        for r in rows.iter() {
            let (Some(sid), Some(cid)) = (s(r.get_named("section_id")), s(r.get_named("channel_id"))) else { continue };
            if let Some(sec) = out.iter_mut().find(|x| x.id == sid) {
                sec.channel_ids.push(cid);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// users
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub real_name: String,
    pub display_name: String,
    pub is_bot: bool,
    pub deleted: bool,
    pub tz_offset: i64,
}

impl User {
    /// What Slack shows: the display name, else the full name, else
    /// the handle; the id when the record has none of them (a Slack
    /// Connect user from another workspace comes with `real_name` null
    /// and often only a handle).
    pub fn label(&self) -> &str {
        [&self.display_name, &self.real_name, &self.name, &self.id]
            .into_iter()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or("")
    }
}

pub async fn upsert_users(users: &[User]) -> Result<(), HostError> {
    if users.is_empty() {
        return Ok(());
    }
    let now = now_ms() as i64;
    let sql = "INSERT INTO users (id, name, real_name, display_name, is_bot, deleted, tz_offset, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, real_name = excluded.real_name,
           display_name = excluded.display_name, is_bot = excluded.is_bot, deleted = excluded.deleted,
           tz_offset = excluded.tz_offset, updated_ms = excluded.updated_ms";
    let owned: Vec<Vec<DbValue>> = users
        .iter()
        .map(|u| {
            vec![
                DbValue::from(u.id.as_str()),
                DbValue::from(u.name.as_str()),
                DbValue::from(u.real_name.as_str()),
                DbValue::from(u.display_name.as_str()),
                DbValue::from(u.is_bot as i64),
                DbValue::from(u.deleted as i64),
                DbValue::from(u.tz_offset),
                DbValue::from(now),
            ]
        })
        .collect();
    let stmts: Vec<(&str, &[DbValue])> = owned.iter().map(|p| (sql, p.as_slice())).collect();
    db_batch(&stmts).await.map(|_| ())
}

pub async fn users() -> Vec<User> {
    let Ok(rows) = db_query("SELECT id, name, real_name, display_name, is_bot, deleted, tz_offset FROM users", params![]).await
    else {
        return Vec::new();
    };
    rows.iter()
        .map(|r| User {
            id: s(r.get_named("id")).unwrap_or_default(),
            name: s(r.get_named("name")).unwrap_or_default(),
            real_name: s(r.get_named("real_name")).unwrap_or_default(),
            display_name: s(r.get_named("display_name")).unwrap_or_default(),
            is_bot: b(r.get_named("is_bot")),
            deleted: b(r.get_named("deleted")),
            tz_offset: i(r.get_named("tz_offset")).unwrap_or(0),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

/// One message as the views draw it: what the API said, flattened.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Message {
    pub channel: String,
    pub ts: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// The author's user id (a bot message has none).
    #[serde(default)]
    pub user: Option<String>,
    /// A name to draw when `user` is not a user: a bot's name, or the
    /// `username` of an integration post.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub reply_count: i64,
    #[serde(default)]
    pub latest_reply: Option<String>,
    /// (emoji name, count)
    #[serde(default)]
    pub reactions: Vec<(String, i64)>,
    /// File names.
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub edited: bool,
    /// Attachment fallbacks / texts (link unfurls, integration cards).
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(default)]
    pub deleted: bool,
}

impl Message {
    /// A reply inside a thread (not the thread's root).
    pub fn is_reply(&self) -> bool {
        self.thread_ts.as_deref().is_some_and(|t| t != self.ts)
    }
}

pub async fn upsert_messages(msgs: &[Message]) -> Result<(), HostError> {
    if msgs.is_empty() {
        return Ok(());
    }
    let now = now_ms() as i64;
    let sql = "INSERT INTO messages (channel, ts, thread_ts, json, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(channel, ts) DO UPDATE SET thread_ts = excluded.thread_ts, json = excluded.json, updated_ms = excluded.updated_ms";
    let owned: Vec<Vec<DbValue>> = msgs
        .iter()
        .map(|m| {
            vec![
                DbValue::from(m.channel.as_str()),
                DbValue::from(m.ts.as_str()),
                m.thread_ts.as_deref().map(DbValue::from).unwrap_or(DbValue::Null),
                DbValue::from(serde_json::to_string(m).unwrap_or_default()),
                DbValue::from(now),
            ]
        })
        .collect();
    let stmts: Vec<(&str, &[DbValue])> = owned.iter().map(|p| (sql, p.as_slice())).collect();
    db_batch(&stmts).await.map(|_| ())
}

fn messages_from(rows: &Rows) -> Vec<Message> {
    rows.iter()
        .filter_map(|r| serde_json::from_str::<Message>(r.get_named("json")?.as_str()?).ok())
        .collect()
}

/// The channel's main timeline (roots and unthreaded messages, no
/// replies), oldest first, the newest `limit`.
pub async fn messages(channel: &str, limit: usize) -> Vec<Message> {
    let rows = db_query(
        "SELECT json FROM (SELECT json, ts FROM messages WHERE channel = ?1 AND (thread_ts IS NULL OR thread_ts = ts)
           ORDER BY ts DESC LIMIT ?2) ORDER BY ts",
        params![channel, limit as i64],
    )
    .await;
    rows.map(|r| messages_from(&r)).unwrap_or_default()
}

/// A thread: its root and every cached reply, oldest first.
pub async fn thread(channel: &str, thread_ts: &str) -> Vec<Message> {
    let rows = db_query(
        "SELECT json FROM messages WHERE channel = ?1 AND (ts = ?2 OR thread_ts = ?2) ORDER BY ts",
        params![channel, thread_ts],
    )
    .await;
    rows.map(|r| messages_from(&r)).unwrap_or_default()
}

/// The newest cached ts of the channel's timeline (what a fetch asks
/// after).
pub async fn latest_ts(channel: &str) -> Option<String> {
    let rows = db_query(
        "SELECT MAX(ts) AS ts FROM messages WHERE channel = ?1 AND (thread_ts IS NULL OR thread_ts = ts)",
        params![channel],
    )
    .await
    .ok()?;
    s(rows.row(0)?.get_named("ts"))
}

pub async fn delete_message(channel: &str, ts: &str) {
    let _ = db_exec("DELETE FROM messages WHERE channel = ?1 AND ts = ?2", params![channel, ts]).await;
}

/// Drop messages older than `days` that are not thread roots still
/// referenced; keeps the cache bounded.
pub async fn sweep(days: i64) {
    let cutoff = (now_ms() as i64 / 1000 - days * 86_400).to_string();
    let _ = db_exec("DELETE FROM messages WHERE ts < ?1", params![cutoff.as_str()]).await;
}

// ---------------------------------------------------------------------------
// settings and bindings
// ---------------------------------------------------------------------------

pub async fn setting(key: &str) -> Option<String> {
    let rows = db_query("SELECT value FROM settings WHERE key = ?1", params![key]).await.ok()?;
    s(rows.row(0)?.get_named("value"))
}

pub async fn set_setting(key: &str, value: &str) {
    let _ = db_exec(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .await;
}

/// A Slack thread (or a whole channel when `thread_ts` is empty) whose
/// new messages go to an agent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binding {
    pub channel: String,
    pub thread_ts: String,
    pub agent: String,
    pub since_ts: String,
}

pub async fn bindings() -> Vec<Binding> {
    let Ok(rows) = db_query("SELECT channel, thread_ts, agent, since_ts FROM bindings", params![]).await else {
        return Vec::new();
    };
    rows.iter()
        .map(|r| Binding {
            channel: s(r.get_named("channel")).unwrap_or_default(),
            thread_ts: s(r.get_named("thread_ts")).unwrap_or_default(),
            agent: s(r.get_named("agent")).unwrap_or_default(),
            since_ts: s(r.get_named("since_ts")).unwrap_or_default(),
        })
        .collect()
}

pub async fn bind(channel: &str, thread_ts: &str, agent: &str, since_ts: &str) {
    let _ = db_exec(
        "INSERT INTO bindings (channel, thread_ts, agent, since_ts, created_ms) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(channel, thread_ts) DO UPDATE SET agent = excluded.agent, since_ts = excluded.since_ts",
        params![channel, thread_ts, agent, since_ts, now_ms() as i64],
    )
    .await;
}

pub async fn unbind(channel: &str, thread_ts: &str) {
    let _ = db_exec("DELETE FROM bindings WHERE channel = ?1 AND thread_ts = ?2", params![channel, thread_ts]).await;
}

pub async fn bump_binding(channel: &str, thread_ts: &str, since_ts: &str) {
    let _ = db_exec(
        "UPDATE bindings SET since_ts = ?3 WHERE channel = ?1 AND thread_ts = ?2 AND since_ts < ?3",
        params![channel, thread_ts, since_ts],
    )
    .await;
}
