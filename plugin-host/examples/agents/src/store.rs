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

use tmux_plugin_sdk::prelude::*;

pub const USER_VERSION: i64 = 2;

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
  name TEXT,
  first_seen_ms INTEGER NOT NULL,
  last_status_ms INTEGER NOT NULL,
  started_ms INTEGER,
  last_active_ms INTEGER,
  source_path TEXT,
  ended_ms INTEGER,
  reason TEXT);
CREATE INDEX IF NOT EXISTS agents_live ON agents(ended_ms, last_active_ms);
-- At most one live agent per pane; NULL panes (ended) do not collide.
CREATE UNIQUE INDEX IF NOT EXISTS agents_one_live_pane
  ON agents(pane) WHERE ended_ms IS NULL AND pane IS NOT NULL;
CREATE TABLE IF NOT EXISTS captures (
  id TEXT PRIMARY KEY,
  text TEXT,
  FOREIGN KEY(id) REFERENCES agents(id) ON DELETE CASCADE);
PRAGMA user_version = 2;";

/// The v1 -> v2 upgrade: the resolved columns did not exist in v1.
const MIGRATE_V2: &str = "
ALTER TABLE agents ADD COLUMN name TEXT;
ALTER TABLE agents ADD COLUMN started_ms INTEGER;
ALTER TABLE agents ADD COLUMN last_active_ms INTEGER;
ALTER TABLE agents ADD COLUMN source_path TEXT;
PRAGMA user_version = 2;";

const COLS: &str = "id, kind, status, life, pane, session, window, task, name, \
                    first_seen_ms, last_status_ms, started_ms, last_active_ms, \
                    source_path, ended_ms, reason";

#[derive(Debug, Clone)]
pub struct Agent {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub life: String,
    pub pane: Option<i64>,
    pub session: Option<String>,
    pub window: Option<String>,
    pub task: Option<String>,
    pub name: Option<String>,
    pub first_seen_ms: i64,
    pub last_status_ms: i64,
    pub started_ms: Option<i64>,
    pub last_active_ms: Option<i64>,
    pub source_path: Option<String>,
    pub ended_ms: Option<i64>,
    pub reason: Option<String>,
}

impl Agent {
    pub fn live(&self) -> bool {
        self.ended_ms.is_none()
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
            id: s(row.get_named("id")).unwrap_or_default(),
            kind: s(row.get_named("kind")).unwrap_or_default(),
            status: s(row.get_named("status")).unwrap_or_default(),
            life: s(row.get_named("life")).unwrap_or_default(),
            pane: i(row.get_named("pane")),
            session: s(row.get_named("session")),
            window: s(row.get_named("window")),
            task: s(row.get_named("task")),
            name: s(row.get_named("name")),
            first_seen_ms: i(row.get_named("first_seen_ms")).unwrap_or(0),
            last_status_ms: i(row.get_named("last_status_ms")).unwrap_or(0),
            started_ms: i(row.get_named("started_ms")),
            last_active_ms: i(row.get_named("last_active_ms")),
            source_path: s(row.get_named("source_path")),
            ended_ms: i(row.get_named("ended_ms")),
            reason: s(row.get_named("reason")),
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
    match version {
        0 => {
            db_exec_sync(SCHEMA, params![]).map_err(|e| format!("db: {e}"))?;
        }
        1 => {
            db_exec_sync(MIGRATE_V2, params![])
                .map_err(|e| format!("db: {e}"))?;
        }
        v if v > USER_VERSION => {
            return Err(format!(
                "store.db is version {v}, this plugin understands {USER_VERSION}"
            ));
        }
        _ => {}
    }
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
            first_seen_ms, last_status_ms, last_active_ms) \
         VALUES (?1, ?2, 'working', 'active', ?3, ?4, ?5, NULL, ?6, ?7, ?7, ?7) \
         ON CONFLICT(id) DO UPDATE SET \
            kind = excluded.kind, \
            pane = excluded.pane, \
            session = excluded.session, \
            window = excluded.window, \
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
) -> Result<(), HostError> {
    db_exec(
        "UPDATE agents SET \
            name = COALESCE(?2, name), \
            status = COALESCE(?3, status), \
            started_ms = COALESCE(?4, started_ms), \
            last_active_ms = COALESCE(?5, last_active_ms), \
            source_path = COALESCE(?6, source_path) \
         WHERE id = ?1 AND ended_ms IS NULL",
        params![id, name, status, started_ms, last_active_ms, source_path],
    )
    .await?;
    Ok(())
}

/// Update the turn status (and task, when the report carries one).
pub async fn set_status(
    pane: i64,
    status: &str,
    task: Option<&str>,
    now_ms: i64,
) -> Result<u64, HostError> {
    let r = if task.is_some() {
        db_exec(
            "UPDATE agents SET status = ?2, task = ?3, last_status_ms = ?4 \
             WHERE pane = ?1 AND ended_ms IS NULL",
            params![pane, status, task, now_ms],
        )
        .await?
    } else {
        db_exec(
            "UPDATE agents SET status = ?2, last_status_ms = ?3 \
             WHERE pane = ?1 AND ended_ms IS NULL",
            params![pane, status, now_ms],
        )
        .await?
    };
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

// ---------------------------------------------------------------------------
// retention
// ---------------------------------------------------------------------------

/// Drop ended agents older than keep_days. Their captures cascade.
pub async fn prune(keep_days: i64, now_ms: i64) -> Result<u64, HostError> {
    let cutoff = now_ms - keep_days * 86_400_000;
    let r = db_exec(
        "DELETE FROM agents WHERE ended_ms IS NOT NULL AND ended_ms < ?1",
        params![cutoff],
    )
    .await?;
    Ok(r.changes as u64)
}
