//! The database: schema and every SQL statement, each a named function.
//! Nothing outside this module writes SQL. The DB is the source of truth
//! for the roster; the plugin keeps only a live pane->id map in memory.
//!
//! A row carries two kinds of fact, from two kinds of source:
//!
//!   * OBSERVED (pane, session, window, life, first_seen_ms,
//!     last_status_ms, ended_ms): what tmux itself sees. Membership and
//!     liveness live here, so a killed CLI always self-corrects.
//!   * RESOLVED (name, status, started_ms, last_active_ms, source_path):
//!     what the harness's own session file says, read at render time. A
//!     resolver enriches these; when it has not run yet they are NULL and
//!     the observed fallbacks (first_seen_ms / last_status_ms) stand in.
//!
//! Identity is two-phase. A new agent enters under a provisional,
//! pane-bound id (`prov-<kind>-<pane>`); when a resolver learns the
//! durable harness id, [`rename_id`] / [`merge_id`] migrates the row, so
//! a resumed session re-links to its own history.

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

pub const USER_VERSION: i64 = 9;

/// The server a row belongs to. A provider only ever writes rows for its
/// own server, so every stored row says "local"; the view stamps the link
/// name on rows it fetched from a remote provider.
pub const LOCAL: &str = "local";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agents (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN
    ('working','needs_input','waiting','done')),
  life TEXT NOT NULL CHECK (life IN ('active','stale','archived')),
  pane INTEGER,
  session TEXT,
  window TEXT,
  task TEXT,
  note TEXT,
  name TEXT,
  name_ms INTEGER,
  user_name TEXT,
  user_name_ms INTEGER,
  waiting_ms INTEGER,
  acked_ms INTEGER,
  first_seen_ms INTEGER NOT NULL,
  last_status_ms INTEGER NOT NULL,
  started_ms INTEGER,
  last_active_ms INTEGER,
  source_path TEXT,
  ended_ms INTEGER,
  reason TEXT,
  server TEXT NOT NULL DEFAULT 'local',
  transcript_path TEXT,
  transcript_cursor INTEGER NOT NULL DEFAULT 0,
  harness_version TEXT);
CREATE INDEX IF NOT EXISTS agents_live ON agents(ended_ms, last_active_ms);
-- At most one live agent per pane per server; NULL panes (ended) do not
-- collide.
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_live_pane
  ON agents(server, pane) WHERE ended_ms IS NULL AND pane IS NOT NULL;
-- A capture follows its agent through an id migration. ON DELETE
-- CASCADE alone made the provisional -> durable rename fail the key for
-- any agent whose text had already been saved.
CREATE TABLE IF NOT EXISTS captures (
  id TEXT PRIMARY KEY,
  text TEXT,
  FOREIGN KEY(id) REFERENCES agents(id)
    ON DELETE CASCADE ON UPDATE CASCADE);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT);
-- The conversation, one row per turn (a prompt, an assistant message, a
-- condensed tool call), extracted from the harness's transcript. `text`
-- is TEXT for a short turn and a zstd BLOB for a long one. `offset`/`len`
-- locate the record in the transcript for what is not kept here.
CREATE TABLE IF NOT EXISTS turns (
  id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('user','assistant','tool')),
  ts_ms INTEGER,
  text,
  path TEXT,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL,
  PRIMARY KEY (id, seq),
  FOREIGN KEY(id) REFERENCES agents(id)
    ON DELETE CASCADE ON UPDATE CASCADE);
-- The search index snapshot (see index.rs): one row.
CREATE TABLE IF NOT EXISTS search_index (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  blob BLOB NOT NULL,
  max_rowid INTEGER NOT NULL);
PRAGMA user_version = 9;";

/// The v1 -> v2 upgrade: the resolved columns did not exist in v1.
const MIGRATE_V2: &str = "
ALTER TABLE agents ADD COLUMN name TEXT;
ALTER TABLE agents ADD COLUMN started_ms INTEGER;
ALTER TABLE agents ADD COLUMN last_active_ms INTEGER;
ALTER TABLE agents ADD COLUMN source_path TEXT;
PRAGMA user_version = 2;";

/// The v2 -> v3 upgrade: a key/value settings table (remembered UI size).
const MIGRATE_V3: &str = "
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT);
PRAGMA user_version = 3;";

/// The v3 -> v4 upgrade: a user-set name that competes with the harness
/// name by recency (see `display` name resolution and `rename_by_user`).
const MIGRATE_V4: &str = "
ALTER TABLE agents ADD COLUMN name_ms INTEGER;
ALTER TABLE agents ADD COLUMN user_name TEXT;
ALTER TABLE agents ADD COLUMN user_name_ms INTEGER;
PRAGMA user_version = 4;";

/// The v4 -> v5 upgrade: unread tracking. `waiting_ms` stamps the moment
/// an agent enters `waiting`; `acked_ms` stamps when the user last got to
/// it (a jump, or the picker cursor landing on the row). An agent is
/// unread while it waits and `acked_ms` is older than `waiting_ms`.
const MIGRATE_V5: &str = "
ALTER TABLE agents ADD COLUMN waiting_ms INTEGER;
ALTER TABLE agents ADD COLUMN acked_ms INTEGER;
PRAGMA user_version = 5;";

/// The v5 -> v6 upgrade: rows carry the server they were observed on, and
/// the one-live-agent-per-pane rule is per server.
const MIGRATE_V6: &str = "
ALTER TABLE agents ADD COLUMN server TEXT NOT NULL DEFAULT 'local';
DROP INDEX IF EXISTS agents_one_live_pane;
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_live_pane
  ON agents(server, pane) WHERE ended_ms IS NULL AND pane IS NOT NULL;
PRAGMA user_version = 6;";

/// The v6 -> v7 upgrade: a capture follows its agent through an id
/// migration. Until now `captures` referenced `agents(id)` with ON DELETE
/// CASCADE only, so [`rename_id`] - the provisional -> durable move every
/// resolved agent makes - failed the foreign key for any agent whose text
/// had already been saved under the old id. The error went nowhere: the
/// row kept its provisional id in the db while the running plugin moved on
/// to the durable one, and every later write (name, status, ack, rename)
/// addressed a row that did not exist. SQLite cannot alter a constraint,
/// so the table is rebuilt. A capture whose agent is already gone - an
/// orphan from before the key was enforced - is dropped rather than
/// failing the upgrade, and the leading DROP lets a half-finished run be
/// retried.
const MIGRATE_V7: &str = "
DROP TABLE IF EXISTS captures_v7;
CREATE TABLE captures_v7 (
  id TEXT PRIMARY KEY,
  text TEXT,
  FOREIGN KEY(id) REFERENCES agents(id)
    ON DELETE CASCADE ON UPDATE CASCADE);
INSERT INTO captures_v7 (id, text)
  SELECT c.id, c.text FROM captures c JOIN agents a ON a.id = c.id;
DROP TABLE captures;
ALTER TABLE captures_v7 RENAME TO captures;
PRAGMA user_version = 7;";

/// The v7 -> v8 upgrade: `note` - why an agent wants the user. A shim
/// reporting `needs_input` may carry the harness's own words with it (the
/// Claude `Notification` message, a permission prompt's subject), and the
/// roster shows them on the row so the band says what each agent is
/// blocked on without a jump. It is deliberately NOT `task`: a task
/// describes what the agent is doing and outlives the turn, while a note
/// is only true while the agent is in `needs_input` and is cleared by the
/// next status report.
const MIGRATE_V8: &str = "
ALTER TABLE agents ADD COLUMN note TEXT;
PRAGMA user_version = 8;";

/// The v8 -> v9 upgrade: the transcript. Where the harness's own
/// transcript file is, how far into it the extractor has read, and which
/// harness version wrote it; the `turns` table the conversation lands in;
/// and the search index snapshot. See `transcript.rs` and `index.rs`.
const MIGRATE_V9: &str = "
ALTER TABLE agents ADD COLUMN transcript_path TEXT;
ALTER TABLE agents ADD COLUMN transcript_cursor INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN harness_version TEXT;
CREATE TABLE IF NOT EXISTS turns (
  id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('user','assistant','tool')),
  ts_ms INTEGER,
  text,
  path TEXT,
  offset INTEGER NOT NULL,
  len INTEGER NOT NULL,
  PRIMARY KEY (id, seq),
  FOREIGN KEY(id) REFERENCES agents(id)
    ON DELETE CASCADE ON UPDATE CASCADE);
CREATE TABLE IF NOT EXISTS search_index (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  blob BLOB NOT NULL,
  max_rowid INTEGER NOT NULL);
PRAGMA user_version = 9;";

const COLS: &str = "id, kind, status, life, pane, session, window, task, \
                    note, name, name_ms, user_name, user_name_ms, \
                    waiting_ms, acked_ms, first_seen_ms, \
                    last_status_ms, started_ms, last_active_ms, source_path, \
                    ended_ms, reason, server, transcript_path, \
                    transcript_cursor, harness_version";

/// One agent. Serialized as JSON when a provider ships its rows to a view
/// on another server, so every field is plain data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    /// "local" in the store; the link name on a row fetched from a remote
    /// provider.
    #[serde(default = "default_server")]
    pub server: String,
    pub id: String,
    pub kind: String,
    pub status: String,
    pub life: String,
    pub pane: Option<i64>,
    pub session: Option<String>,
    pub window: Option<String>,
    pub task: Option<String>,
    /// Why the agent wants the user, in the harness's own words. Set by a
    /// `needs_input` report that carries text; cleared by any later
    /// status. See [`MIGRATE_V8`].
    #[serde(default)]
    pub note: Option<String>,
    pub name: Option<String>,
    pub name_ms: Option<i64>,
    pub user_name: Option<String>,
    pub user_name_ms: Option<i64>,
    pub waiting_ms: Option<i64>,
    pub acked_ms: Option<i64>,
    pub first_seen_ms: i64,
    pub last_status_ms: i64,
    pub started_ms: Option<i64>,
    pub last_active_ms: Option<i64>,
    pub source_path: Option<String>,
    pub ended_ms: Option<i64>,
    pub reason: Option<String>,
    /// The harness's transcript file, once a resolver has derived it.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// How many bytes of the transcript the extractor has consumed.
    #[serde(default)]
    pub transcript_cursor: i64,
    /// The harness version that wrote the transcript, from its records.
    #[serde(default)]
    pub harness_version: Option<String>,
}

fn default_server() -> String {
    LOCAL.to_string()
}

/// [`Agent::key`] for a row named by server and id alone (a search hit
/// that came back from a remote provider, say).
pub fn row_key(server: &str, id: &str) -> String {
    format!("{server}\u{1}{id}")
}

impl Agent {
    pub fn live(&self) -> bool {
        self.ended_ms.is_none()
    }

    /// Is this a row of the local server?
    pub fn is_local(&self) -> bool {
        self.server == LOCAL
    }

    /// The key a view uses for marks and ranks: an id is unique per
    /// server only.
    pub fn key(&self) -> String {
        row_key(&self.server, &self.id)
    }

    /// When the agent was last active: the resolved harness time, else the
    /// observed status time. Drives ordering and the age column.
    pub fn active_ms(&self) -> i64 {
        self.last_active_ms.unwrap_or(self.last_status_ms)
    }

    /// When the agent started: the resolved harness time, else first seen.
    pub fn started(&self) -> i64 {
        self.started_ms.unwrap_or(self.first_seen_ms)
    }

    /// An agent that stopped for the user and has not been gotten to yet:
    /// it entered `waiting` or `needs_input` more recently than the last
    /// acknowledgement (a jump to its pane, typing into it, or `r`). A
    /// live stopped row with no ack is unread. The cursor landing on a
    /// row is not an acknowledgement: scrolling past is not reading.
    pub fn unread(&self) -> bool {
        if !self.live() || !matches!(self.status.as_str(), "waiting" | "needs_input") {
            return false;
        }
        match self.waiting_ms {
            None => false,
            Some(w) => self.acked_ms.map_or(true, |a| a < w),
        }
    }
}

fn s(v: Option<&DbValue>) -> Option<String> {
    v.and_then(DbValue::as_str).map(str::to_owned)
}

fn i(v: Option<&DbValue>) -> Option<i64> {
    v.and_then(DbValue::as_i64)
}

fn agents_from(rows: &Rows) -> Vec<Agent> {
    rows.iter()
        .map(|row| Agent {
            server: s(row.get_named("server")).unwrap_or_else(default_server),
            id: s(row.get_named("id")).unwrap_or_default(),
            kind: s(row.get_named("kind")).unwrap_or_default(),
            status: s(row.get_named("status")).unwrap_or_default(),
            life: s(row.get_named("life")).unwrap_or_default(),
            pane: i(row.get_named("pane")),
            session: s(row.get_named("session")),
            window: s(row.get_named("window")),
            task: s(row.get_named("task")),
            note: s(row.get_named("note")),
            name: s(row.get_named("name")),
            name_ms: i(row.get_named("name_ms")),
            user_name: s(row.get_named("user_name")),
            user_name_ms: i(row.get_named("user_name_ms")),
            waiting_ms: i(row.get_named("waiting_ms")),
            acked_ms: i(row.get_named("acked_ms")),
            first_seen_ms: i(row.get_named("first_seen_ms")).unwrap_or(0),
            last_status_ms: i(row.get_named("last_status_ms")).unwrap_or(0),
            started_ms: i(row.get_named("started_ms")),
            last_active_ms: i(row.get_named("last_active_ms")),
            source_path: s(row.get_named("source_path")),
            ended_ms: i(row.get_named("ended_ms")),
            reason: s(row.get_named("reason")),
            transcript_path: s(row.get_named("transcript_path")),
            transcript_cursor: i(row.get_named("transcript_cursor")).unwrap_or(0),
            harness_version: s(row.get_named("harness_version")),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// init (sync, main thread)
// ---------------------------------------------------------------------------

/// Create or upgrade the schema. Runs before any async DB use.
pub fn migrate_sync() -> Result<(), String> {
    let v = db_query_sync("PRAGMA user_version", params![])
        .map_err(|e| format!("db: {e}"))?;
    let version = v.scalar().and_then(DbValue::as_i64).unwrap_or(0);
    if version > USER_VERSION {
        return Err(format!(
            "store.db is version {version}, this plugin understands {USER_VERSION}"
        ));
    }
    // A fresh db jumps straight to the latest; an existing one upgrades in
    // order.
    if version == 0 {
        db_exec_sync(SCHEMA, params![]).map_err(|e| format!("db: {e}"))?;
    } else {
        if version == 1 {
            db_exec_sync(MIGRATE_V2, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 2 {
            db_exec_sync(MIGRATE_V3, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 3 {
            db_exec_sync(MIGRATE_V4, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 4 {
            db_exec_sync(MIGRATE_V5, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 5 {
            db_exec_sync(MIGRATE_V6, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 6 {
            db_exec_sync(MIGRATE_V7, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 7 {
            db_exec_sync(MIGRATE_V8, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        if version <= 8 {
            db_exec_sync(MIGRATE_V9, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// settings (key/value; remembered UI state)
// ---------------------------------------------------------------------------

pub async fn get_setting(key: &str) -> Result<Option<String>, HostError> {
    let rows = db_query(
        "SELECT value FROM settings WHERE key = ?1 LIMIT 1",
        params![key],
    )
    .await?;
    Ok(rows.scalar().and_then(DbValue::as_str).map(str::to_owned))
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

// ---------------------------------------------------------------------------
// membership + identity (async)
// ---------------------------------------------------------------------------

/// The live agent currently bound to a pane, if any.
pub async fn live_by_pane(pane: i64) -> Result<Option<Agent>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {COLS} FROM agents WHERE pane = ?1 AND ended_ms IS NULL \
             LIMIT 1"
        ),
        params![pane],
    )
    .await?;
    Ok(agents_from(&rows).into_iter().next())
}

/// Whether a durable id already has a row (live or archived).
pub async fn id_exists(id: &str) -> Result<bool, HostError> {
    let rows =
        db_query("SELECT 1 FROM agents WHERE id = ?1 LIMIT 1", params![id])
            .await?;
    Ok(!rows.is_empty())
}

/// Create, revive, or rebind a live agent by its durable id, in one
/// upsert. A new id inserts; a known id re-points at the pane, refreshes
/// its labels, clears any ended state, and (only if it had ended) resets
/// the turn status. Keeps first_seen, task, a resolved name, and the life
/// of a still-live agent - a plain re-sighting NEVER clears an archive
/// (that would resurrect it on every restart). A genuinely new session id
/// clears the archive through `rename_id`; a new `working` report clears it
/// through `unarchive_by_pane`.
#[allow(clippy::too_many_arguments)]
pub async fn activate(
    id: &str,
    kind: &str,
    pane: i64,
    session: Option<&str>,
    window: Option<&str>,
    name: Option<&str>,
    now_ms: i64,
) -> Result<(), HostError> {
    db_exec(
        "INSERT INTO agents \
           (id, kind, status, life, pane, session, window, task, name, \
            name_ms, first_seen_ms, last_status_ms, last_active_ms) \
         VALUES (?1, ?2, 'working', 'active', ?3, ?4, ?5, NULL, ?6, \
            CASE WHEN ?6 IS NOT NULL THEN ?7 ELSE NULL END, ?7, ?7, ?7) \
         ON CONFLICT(id) DO UPDATE SET \
            kind = excluded.kind, \
            pane = excluded.pane, \
            session = excluded.session, \
            window = excluded.window, \
            name_ms = CASE \
                WHEN agents.name IS NULL AND excluded.name IS NOT NULL \
                THEN CASE WHEN agents.user_name IS NOT NULL THEN 0 ELSE ?7 END \
                ELSE agents.name_ms END, \
            name = COALESCE(agents.name, excluded.name), \
            ended_ms = NULL, \
            reason = NULL, \
            status = CASE WHEN agents.ended_ms IS NOT NULL \
                          THEN 'working' ELSE agents.status END, \
            last_status_ms = CASE WHEN agents.ended_ms IS NOT NULL \
                                  THEN ?7 ELSE agents.last_status_ms END, \
            last_active_ms = CASE WHEN agents.ended_ms IS NOT NULL \
                                  THEN ?7 ELSE agents.last_active_ms END",
        params![id, kind, pane, session, window, name, now_ms],
    )
    .await?;
    Ok(())
}

/// Rename a provisional id to the durable one, when the durable id has no
/// row yet. The row keeps every observed fact, but a new durable id means
/// a genuinely new session took over the row, so clear any archive: the
/// user archived the OLD session, not this one.
pub async fn rename_id(old: &str, new: &str) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET id = ?2, \
            life = CASE WHEN life = 'archived' THEN 'active' ELSE life END \
         WHERE id = ?1",
        params![old, new],
    )
    .await?;
    Ok(())
}

/// Merge a live provisional row into an existing durable row (a resumed
/// session): free the pane from the provisional row, drop it, then point
/// the durable row at the live pane and revive it. Atomic.
#[allow(clippy::too_many_arguments)]
pub async fn merge_id(
    old: &str,
    new: &str,
    pane: i64,
    session: Option<&str>,
    window: Option<&str>,
    now_ms: i64,
) -> Result<(), HostError> {
    db_batch(&[
        ("DELETE FROM agents WHERE id = ?1", params![old]),
        (
            "UPDATE agents SET pane = ?2, session = ?3, window = ?4, \
               status = CASE WHEN ended_ms IS NOT NULL \
                             THEN 'working' ELSE status END, \
               life = CASE WHEN life = 'archived' THEN 'active' ELSE life END, \
               ended_ms = NULL, reason = NULL, \
               last_status_ms = ?5, last_active_ms = ?5 \
             WHERE id = ?1",
            params![new, pane, session, window, now_ms],
        ),
    ])
    .await?;
    Ok(())
}

/// Fold resolved facts onto a live row. Every field is COALESCE'd on the
/// argument, so a `None` leaves the stored value untouched; a resolved
/// status overrides. Never moves an ended row.
pub async fn enrich(
    id: &str,
    name: Option<&str>,
    status: Option<&str>,
    started_ms: Option<i64>,
    last_active_ms: Option<i64>,
    source_path: Option<&str>,
    now_ms: i64,
) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET \
            name_ms = CASE \
                WHEN ?2 IS NOT NULL AND ?2 <> COALESCE(name, '') THEN \
                    CASE WHEN COALESCE(name, '') = '' \
                              AND user_name IS NOT NULL \
                         THEN 0 ELSE ?7 END \
                ELSE name_ms END, \
            name = COALESCE(?2, name), \
            status = COALESCE(?3, status), \
            started_ms = COALESCE(?4, started_ms), \
            last_active_ms = COALESCE(?5, last_active_ms), \
            source_path = COALESCE(?6, source_path) \
         WHERE id = ?1 AND ended_ms IS NULL",
        params![id, name, status, started_ms, last_active_ms, source_path,
                now_ms],
    )
    .await?;
    Ok(())
}

/// The user's chosen name for an agent. Empty clears it (revert to the
/// harness name). The name competes with the harness name by recency; see
/// the display-name resolution.
pub async fn rename_by_user(
    id: &str,
    name: Option<&str>,
    now_ms: i64,
) -> Result<(), HostError> {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    db_exec(
        "UPDATE agents SET \
            user_name = ?2, \
            user_name_ms = CASE WHEN ?2 IS NULL THEN NULL ELSE ?3 END \
         WHERE id = ?1",
        params![id, name, now_ms],
    )
    .await?;
    Ok(())
}

/// Update the turn status (and task, when the report carries one).
///
/// `note` is written on every report, not only when it is `Some`: it says
/// why the agent wants the user *now*, so a report that carries none
/// clears the one the last report left. Without that, the reason an agent
/// was blocked five turns ago would stay on the row forever.
pub async fn set_status(
    pane: i64,
    status: &str,
    task: Option<&str>,
    note: Option<&str>,
    now_ms: i64,
) -> Result<u64, HostError> {
    let r = if task.is_some() {
        db_exec(
            "UPDATE agents SET status = ?2, task = ?3, note = ?4, \
             last_status_ms = ?5, \
             waiting_ms = CASE WHEN status <> ?2 AND ?2 IN ('waiting', 'needs_input') \
                               THEN ?5 ELSE waiting_ms END \
             WHERE pane = ?1 AND ended_ms IS NULL",
            params![pane, status, task, note, now_ms],
        )
        .await?
    } else {
        db_exec(
            "UPDATE agents SET status = ?2, note = ?3, last_status_ms = ?4, \
             waiting_ms = CASE WHEN status <> ?2 AND ?2 IN ('waiting', 'needs_input') \
                               THEN ?4 ELSE waiting_ms END \
             WHERE pane = ?1 AND ended_ms IS NULL",
            params![pane, status, note, now_ms],
        )
        .await?
    };
    Ok(r.changes as u64)
}

/// Set the turn status by hand, from the picker - the user moving a row
/// between the attention band and `waiting`. Addressed by id, not pane,
/// so it reaches a row whose pane this server does not own (a remote row
/// acts through its provider) and one whose pane has gone.
///
/// Three differences from a shim's [`set_status`]:
///
///   * the note goes: whatever the agent said it was blocked on, the user
///     has now judged the row, and a stale reason under a hand-set status
///     reads as if the agent still wants something;
///   * `last_status_ms` is stamped, which is what makes the choice stick -
///     `keeps_status` and the enrichers both compare against it, so the
///     next render will not flatten a hand-set status back;
///   * entering `waiting` acknowledges the row as well as stamping the
///     episode. The user is looking straight at it, so leaving it unread
///     (a bright `envelope`) would be a notification for something they
///     just did.
pub async fn set_status_by_id(
    id: &str,
    status: &str,
    now_ms: i64,
) -> Result<u64, HostError> {
    let r = db_exec(
        "UPDATE agents SET status = ?2, note = NULL, last_status_ms = ?3, \
         waiting_ms = CASE WHEN ?2 = 'waiting' THEN ?3 ELSE waiting_ms END, \
         acked_ms = CASE WHEN ?2 = 'waiting' THEN ?3 ELSE acked_ms END \
         WHERE id = ?1 AND ended_ms IS NULL",
        params![id, status, now_ms],
    )
    .await?;
    Ok(r.changes as u64)
}

/// Mark an agent acknowledged as of `now_ms`: the user got to it (a jump
/// to its pane, typing into it, or the read key). This clears the unread
/// flag for the current episode.
pub async fn acknowledge(id: &str, now_ms: i64) -> Result<u64, HostError> {
    let r = db_exec(
        "UPDATE agents SET acked_ms = ?2 WHERE id = ?1",
        params![id, now_ms],
    )
    .await?;
    Ok(r.changes as u64)
}

/// The opposite: the user wants the row back on the unread pile (the
/// unread key). The ack goes; a row that was never stamped as stopped
/// gets stamped now, so it reads as unread whatever its history.
pub async fn unacknowledge(id: &str, now_ms: i64) -> Result<u64, HostError> {
    let r = db_exec(
        "UPDATE agents SET acked_ms = NULL, waiting_ms = COALESCE(waiting_ms, ?2) \
         WHERE id = ?1",
        params![id, now_ms],
    )
    .await?;
    Ok(r.changes as u64)
}

/// Clear the archive on a pane's live row. A `working` report is a fresh
/// turn - the user sent the agent a new message - so an archived agent
/// comes back into the roster. Returns the rows changed (0 if it was not
/// archived).
pub async fn unarchive_by_pane(pane: i64) -> Result<u64, HostError> {
    let r = db_exec(
        "UPDATE agents SET life = 'active' \
         WHERE pane = ?1 AND ended_ms IS NULL AND life = 'archived'",
        params![pane],
    )
    .await?;
    Ok(r.changes as u64)
}

/// End the live agent on a pane (process gone, or command changed away).
pub async fn end_by_pane(
    pane: i64,
    now_ms: i64,
    reason: &str,
) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET ended_ms = ?2, reason = ?3, \
         status = CASE WHEN status = 'working' THEN 'done' ELSE status END, \
         pane = NULL WHERE pane = ?1 AND ended_ms IS NULL",
        params![pane, now_ms, reason],
    )
    .await?;
    Ok(())
}

/// Mark a turn done and end the agent (a shim's `done`). Keeps the pane
/// column NULL so the unique-pane index frees the pane immediately.
pub async fn finish_by_pane(pane: i64, now_ms: i64) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET status = 'done', ended_ms = ?2, reason = 'exited', \
         last_status_ms = ?2, pane = NULL WHERE pane = ?1 AND ended_ms IS NULL",
        params![pane, now_ms],
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// lifecycle (human, from the picker)
// ---------------------------------------------------------------------------

pub async fn set_life(id: &str, life: &str) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET life = ?2 WHERE id = ?1",
        params![id, life],
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// reads for the picker
// ---------------------------------------------------------------------------

/// Live agents (not ended, not archived). Ordering is decided in Rust
/// (state bands), so the SQL only has to gather the set.
pub async fn live_agents() -> Result<Vec<Agent>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {COLS} FROM agents WHERE ended_ms IS NULL \
             AND life != 'archived'"
        ),
        params![],
    )
    .await?;
    Ok(agents_from(&rows))
}

/// Every agent whose pane is still meant to exist, archived or not: the
/// set a sweep for vanished panes has to cover. [`live_agents`] leaves
/// the archived ones out, and an archived agent whose pane dies must
/// still be ended, or its saved capture never stands in for the pane.
pub async fn unended() -> Result<Vec<Agent>, HostError> {
    let rows = db_query(
        &format!("SELECT {COLS} FROM agents WHERE ended_ms IS NULL"),
        params![],
    )
    .await?;
    Ok(agents_from(&rows))
}

/// Ended or archived agents, most recent first, capped.
pub async fn history(limit: i64) -> Result<Vec<Agent>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {COLS} FROM agents WHERE ended_ms IS NOT NULL \
             OR life = 'archived' \
             ORDER BY COALESCE(ended_ms, last_active_ms, last_status_ms) DESC \
             LIMIT ?1"
        ),
        params![limit],
    )
    .await?;
    Ok(agents_from(&rows))
}

/// Every archived agent, most recent first. Not capped: the archive is
/// the set the user set aside on purpose, and a row must not drop out of
/// it behind a hundred agents that merely finished (which is what
/// [`history`]'s cap does to it).
pub async fn archived() -> Result<Vec<Agent>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {COLS} FROM agents WHERE life = 'archived' \
             ORDER BY COALESCE(ended_ms, last_active_ms, last_status_ms) DESC"
        ),
        params![],
    )
    .await?;
    Ok(agents_from(&rows))
}

/// The saved capture of every archived agent whose pane is gone, by id.
/// Content search greps these for the rows that have no grid left.
pub async fn archived_captures() -> Result<Vec<(String, String)>, HostError> {
    let rows = db_query(
        "SELECT c.id AS id, c.text AS text FROM captures c \
         JOIN agents a ON a.id = c.id \
         WHERE a.life = 'archived' AND a.ended_ms IS NOT NULL",
        params![],
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|r| Some((s(r.get_named("id"))?, s(r.get_named("text"))?)))
        .collect())
}

// ---------------------------------------------------------------------------
// captures (the final preview blob for a finished agent)
// ---------------------------------------------------------------------------

pub async fn save_capture(id: &str, text: &str) -> Result<(), HostError> {
    db_exec(
        "INSERT OR REPLACE INTO captures (id, text) VALUES (?1, ?2)",
        params![id, text],
    )
    .await?;
    Ok(())
}

/// The saved capture of an agent, if any.
pub async fn get_capture(id: &str) -> Result<Option<String>, HostError> {
    let rows = db_query(
        "SELECT text FROM captures WHERE id = ?1 LIMIT 1",
        params![id],
    )
    .await?;
    Ok(rows.scalar().and_then(DbValue::as_str).map(str::to_owned))
}

/// One live or archived agent by id.
pub async fn by_id(id: &str) -> Result<Option<Agent>, HostError> {
    let rows = db_query(
        &format!("SELECT {COLS} FROM agents WHERE id = ?1 LIMIT 1"),
        params![id],
    )
    .await?;
    Ok(agents_from(&rows).into_iter().next())
}

// ---------------------------------------------------------------------------
// retention
// ---------------------------------------------------------------------------

/// Retention. An ended agent with no stored conversation goes after
/// `keep_days`: it was only ever a roster row. One with turns is the
/// searchable history and stays for `history_days`, well past the
/// harness's own cleanup of the transcript it came from. Turns and
/// captures cascade with the row.
pub async fn prune(keep_days: i64, history_days: i64, now_ms: i64) -> Result<u64, HostError> {
    let cutoff = now_ms - keep_days * 86_400_000;
    let r1 = db_exec(
        "DELETE FROM agents WHERE ended_ms IS NOT NULL AND ended_ms < ?1 \
         AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = agents.id)",
        params![cutoff],
    )
    .await?;
    let cutoff = now_ms - history_days * 86_400_000;
    let r2 = db_exec(
        "DELETE FROM agents WHERE ended_ms IS NOT NULL AND ended_ms < ?1",
        params![cutoff],
    )
    .await?;
    Ok((r1.changes + r2.changes) as u64)
}

// ---------------------------------------------------------------------------
// the transcript: where it is, how far it has been read, and the turns
// ---------------------------------------------------------------------------

/// A stored turn. Serialized as JSON when a provider ships turns to a
/// view on another server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRow {
    pub rowid: i64,
    pub id: String,
    pub seq: i64,
    pub kind: String,
    pub ts_ms: Option<i64>,
    pub text: String,
    pub path: Option<String>,
    pub offset: i64,
    pub len: i64,
}

/// A turn's text over this size is stored zstd-compressed (the host
/// compresses a `zstd_ref` parameter on the way in); below it the frame
/// would be larger than the text.
const ZSTD_MIN: usize = 512;

fn turn_text(v: Option<&DbValue>) -> String {
    match v {
        Some(DbValue::Text(t)) => t.clone(),
        Some(DbValue::Blob(b)) => db_decompress(b)
            .ok()
            .map(|o| String::from_utf8_lossy(&o).into_owned())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn turns_from(rows: &Rows) -> Vec<TurnRow> {
    rows.iter()
        .map(|row| TurnRow {
            rowid: i(row.get_named("rowid")).unwrap_or(0),
            id: s(row.get_named("id")).unwrap_or_default(),
            seq: i(row.get_named("seq")).unwrap_or(0),
            kind: s(row.get_named("kind")).unwrap_or_default(),
            ts_ms: i(row.get_named("ts_ms")),
            text: turn_text(row.get_named("text")),
            path: s(row.get_named("path")),
            offset: i(row.get_named("offset")).unwrap_or(0),
            len: i(row.get_named("len")).unwrap_or(0),
        })
        .collect()
}

const TURN_COLS: &str = "rowid, id, seq, kind, ts_ms, text, path, offset, len";

/// Record where an agent's transcript lives. A different path resets the
/// cursor: the extractor starts over on the new file. The same path is a
/// no-op, so a render never disturbs an ingest in progress.
pub async fn set_transcript(id: &str, path: &str) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET transcript_path = ?2, \
            transcript_cursor = CASE WHEN transcript_path IS ?2 \
                                     THEN transcript_cursor ELSE 0 END \
         WHERE id = ?1",
        params![id, path],
    )
    .await?;
    Ok(())
}

/// The agents whose transcript may have grown: every live one with a
/// transcript, and the ones that ended within `recent_ms` (an agent that
/// died while the server was down still has a tail to read). An agent
/// that ended long ago has a transcript that stopped with it.
pub async fn ingest_targets(now_ms: i64, recent_ms: i64) -> Result<Vec<Agent>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {COLS} FROM agents WHERE transcript_path IS NOT NULL \
             AND (ended_ms IS NULL OR ended_ms > ?1)"
        ),
        params![now_ms - recent_ms],
    )
    .await?;
    Ok(agents_from(&rows))
}

/// The next `seq` for an agent's turns.
pub async fn next_seq(id: &str) -> Result<i64, HostError> {
    let rows = db_query(
        "SELECT COALESCE(MAX(seq), -1) + 1 AS n FROM turns WHERE id = ?1",
        params![id],
    )
    .await?;
    Ok(rows.scalar().and_then(DbValue::as_i64).unwrap_or(0))
}

/// A turn about to be stored. `text` is borrowed so a long one can be
/// bound as a `zstd_ref` straight out of the extractor's buffer.
pub struct NewTurn<'a> {
    pub seq: i64,
    pub kind: &'a str,
    pub ts_ms: Option<i64>,
    pub text: &'a str,
    pub path: Option<&'a str>,
    pub offset: i64,
    pub len: i64,
}

/// Store a batch of turns and advance the transcript cursor, atomically:
/// a crash between the two would otherwise re-read (duplicate) or skip
/// (lose) the batch. `version` is recorded when the batch learned one.
/// Returns the rowid of the last turn inserted (for the index).
pub async fn insert_turns(
    id: &str,
    turns: &[NewTurn<'_>],
    cursor: i64,
    version: Option<&str>,
) -> Result<i64, HostError> {
    const INSERT: &str = "INSERT OR REPLACE INTO turns \
        (id, seq, kind, ts_ms, text, path, offset, len) \
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
    let mut owned: Vec<Vec<DbValue>> = Vec::with_capacity(turns.len() + 1);
    for t in turns {
        let text = if t.text.len() >= ZSTD_MIN {
            zstd_ref(t.text.as_bytes())
        } else {
            DbValue::Text(t.text.to_string())
        };
        owned.push(vec![
            DbValue::from(id),
            DbValue::from(t.seq),
            DbValue::from(t.kind),
            DbValue::from(t.ts_ms),
            text,
            DbValue::from(t.path),
            DbValue::from(t.offset),
            DbValue::from(t.len),
        ]);
    }
    owned.push(vec![DbValue::from(id), DbValue::from(cursor), DbValue::from(version)]);
    let mut stmts: Vec<(&str, &[DbValue])> = owned[..turns.len()]
        .iter()
        .map(|p| (INSERT, p.as_slice()))
        .collect();
    stmts.push((
        "UPDATE agents SET transcript_cursor = ?2, \
            harness_version = COALESCE(?3, harness_version) WHERE id = ?1",
        owned[turns.len()].as_slice(),
    ));
    db_batch(&stmts).await?;
    let rows = db_query("SELECT COALESCE(MAX(rowid), 0) AS m FROM turns", params![]).await?;
    Ok(rows.scalar().and_then(DbValue::as_i64).unwrap_or(0))
}

/// One agent's turns in `[from, to)` by seq, in order.
pub async fn turns_range(id: &str, from: i64, to: i64) -> Result<Vec<TurnRow>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {TURN_COLS} FROM turns WHERE id = ?1 AND seq >= ?2 AND seq < ?3 \
             ORDER BY seq"
        ),
        params![id, from, to],
    )
    .await?;
    Ok(turns_from(&rows))
}

/// Turns stored after `rowid`, oldest first, at most `limit`: what the
/// index has to catch up on after loading a snapshot.
pub async fn turns_after(rowid: i64, limit: i64) -> Result<Vec<TurnRow>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {TURN_COLS} FROM turns WHERE rowid > ?1 ORDER BY rowid LIMIT ?2"
        ),
        params![rowid, limit],
    )
    .await?;
    Ok(turns_from(&rows))
}

/// The turns inside several windows of `[from, to)` by seq, one window
/// per agent, for the snippets of a result list. One statement, however
/// many windows; ordered by agent then seq.
pub async fn turns_windows(windows: &[(String, i64, i64)]) -> Result<Vec<TurnRow>, HostError> {
    if windows.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = format!("SELECT {TURN_COLS} FROM turns WHERE ");
    let mut params: Vec<DbValue> = Vec::with_capacity(windows.len() * 3);
    for (i, (id, from, to)) in windows.iter().enumerate() {
        if i > 0 {
            sql.push_str(" OR ");
        }
        let n = i * 3;
        sql.push_str(&format!("(id = ?{} AND seq >= ?{} AND seq < ?{})", n + 1, n + 2, n + 3));
        params.push(DbValue::from(id.as_str()));
        params.push(DbValue::from(*from));
        params.push(DbValue::from(*to));
    }
    sql.push_str(" ORDER BY id, seq");
    let rows = db_query(&sql, &params).await?;
    Ok(turns_from(&rows))
}

/// The agents with these ids, in no particular order.
pub async fn by_ids(ids: &[String]) -> Result<Vec<Agent>, HostError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
    let sql = format!("SELECT {COLS} FROM agents WHERE id IN ({})", marks.join(", "));
    let params: Vec<DbValue> = ids.iter().map(|s| DbValue::from(s.as_str())).collect();
    let rows = db_query(&sql, &params).await?;
    Ok(agents_from(&rows))
}

/// Store the index snapshot (compressed by the host).
pub async fn save_index(blob: &[u8], max_rowid: i64) -> Result<(), HostError> {
    db_exec(
        "INSERT INTO search_index (id, blob, max_rowid) VALUES (1, ?1, ?2) \
         ON CONFLICT(id) DO UPDATE SET blob = excluded.blob, \
            max_rowid = excluded.max_rowid",
        &[zstd_ref(blob), DbValue::from(max_rowid)],
    )
    .await?;
    Ok(())
}

/// The stored index snapshot, inflated, if there is one.
pub async fn load_index() -> Result<Option<Vec<u8>>, HostError> {
    let rows = db_query("SELECT blob FROM search_index WHERE id = 1", params![]).await?;
    match rows.scalar() {
        Some(DbValue::Blob(b)) => Ok(db_decompress(b).ok().map(|o| o.to_vec())),
        _ => Ok(None),
    }
}
