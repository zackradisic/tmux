//! Every SQL statement of the plugin lives here. The database is the
//! source of truth: one `snapshot` row per save, one `pane_blob` row per
//! 4 MiB chunk of pane text, compressed by the host (`zstd_ref`) and
//! inflated on the way back (`db_decompress`).

use tmux_plugin_sdk::prelude::*;

pub const USER_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS snapshot (
  id            INTEGER PRIMARY KEY,
  saved_at_ms   INTEGER NOT NULL,
  version       INTEGER NOT NULL,
  reason        TEXT    NOT NULL CHECK (reason IN
                  ('manual', 'autosave', 'kill', 'restart', 'import')),
  label         TEXT,
  pinned        INTEGER NOT NULL DEFAULT 0,
  sessions      INTEGER NOT NULL,
  windows       INTEGER NOT NULL,
  panes         INTEGER NOT NULL,
  raw_bytes     INTEGER NOT NULL,
  content_hash  INTEGER NOT NULL,
  session_names TEXT    NOT NULL,
  meta          TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS snapshot_time ON snapshot (saved_at_ms);
CREATE TABLE IF NOT EXISTS pane_blob (
  snapshot_id   INTEGER NOT NULL REFERENCES snapshot (id) ON DELETE CASCADE,
  pane_id       INTEGER NOT NULL,
  seq           INTEGER NOT NULL,
  raw_len       INTEGER NOT NULL,
  data          BLOB    NOT NULL,
  PRIMARY KEY (snapshot_id, pane_id, seq)
);
PRAGMA user_version = 1;
";

/// Columns of the picker and status queries: everything but `meta`.
const ROW_COLUMNS: &str = "id, saved_at_ms, reason, label, pinned, sessions, \
                           windows, panes, raw_bytes, session_names";

/// Why a snapshot was taken. Stored as text; the picker shows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Manual,
    Autosave,
    Kill,
    Restart,
    Import,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Manual => "manual",
            Reason::Autosave => "autosave",
            Reason::Kill => "kill",
            Reason::Restart => "restart",
            Reason::Import => "import",
        }
    }
}

/// One `snapshot` row without its metadata blob. `label` and
/// `raw_bytes` are stored for scripts and a later picker column.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SnapshotRow {
    pub id: i64,
    pub saved_at_ms: i64,
    pub reason: String,
    pub label: Option<String>,
    pub pinned: bool,
    pub sessions: i64,
    pub windows: i64,
    pub panes: i64,
    pub raw_bytes: i64,
    pub names: Vec<String>,
}

/// What goes into one new row.
pub struct NewSnapshot<'a> {
    pub id: i64,
    pub saved_at_ms: i64,
    pub version: i64,
    pub reason: Reason,
    pub sessions: i64,
    pub windows: i64,
    pub panes: i64,
    pub raw_bytes: i64,
    pub content_hash: i64,
    pub names: &'a [String],
    pub meta: &'a str,
}

/// One chunk of pane text to store: raw bytes, compressed by the host.
pub struct Chunk<'a> {
    pub pane_id: i64,
    pub seq: i64,
    pub data: &'a [u8],
}

/// Retention rules. `keep_snapshots` newest rows always stay; older rows
/// past `keep_days` go too. Pinned rows and the newest row never go.
#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub keep_snapshots: u32,
    pub keep_days: u32,
}

fn db_err(e: HostError) -> String {
    format!("db: {e}")
}

fn row_of(r: Row<'_>) -> Option<SnapshotRow> {
    let names = r.get(9)?.as_str()?;
    Some(SnapshotRow {
        id: r.get(0)?.as_i64()?,
        saved_at_ms: r.get(1)?.as_i64()?,
        reason: r.get(2)?.as_str()?.to_string(),
        label: r.get(3).and_then(|v| v.as_str()).map(str::to_string),
        pinned: r.get(4)?.as_i64()? != 0,
        sessions: r.get(5)?.as_i64()?,
        windows: r.get(6)?.as_i64()?,
        panes: r.get(7)?.as_i64()?,
        raw_bytes: r.get(8)?.as_i64()?,
        names: if names.is_empty() {
            Vec::new()
        } else {
            names.split('\n').map(str::to_string).collect()
        },
    })
}

/// Create or check the schema. Sync: `init` only.
pub fn migrate_sync() -> Result<(), String> {
    let v = db_query_sync("PRAGMA user_version", params![]).map_err(db_err)?;
    let version = v.scalar().and_then(DbValue::as_i64).unwrap_or(0);
    if version == 0 {
        db_exec_sync(SCHEMA, params![]).map_err(db_err)?;
    } else if version > USER_VERSION {
        return Err(format!(
            "store.db is version {version}, this plugin understands {USER_VERSION}"
        ));
    }
    Ok(())
}

/// Every snapshot, newest first.
pub async fn list() -> Result<Vec<SnapshotRow>, String> {
    let rows = db_query(
        &format!("SELECT {ROW_COLUMNS} FROM snapshot ORDER BY saved_at_ms DESC, id DESC"),
        params![],
    )
    .await
    .map_err(db_err)?;
    Ok(rows.iter().filter_map(row_of).collect())
}

/// One snapshot's row, or the newest when `id` is None.
pub async fn find(id: Option<i64>) -> Result<Option<SnapshotRow>, String> {
    let rows = match id {
        Some(id) => {
            db_query(
                &format!("SELECT {ROW_COLUMNS} FROM snapshot WHERE id = ?1"),
                params![id],
            )
            .await
        }
        None => {
            db_query(
                &format!(
                    "SELECT {ROW_COLUMNS} FROM snapshot \
                     ORDER BY saved_at_ms DESC, id DESC LIMIT 1"
                ),
                params![],
            )
            .await
        }
    }
    .map_err(db_err)?;
    Ok(rows.row(0).and_then(row_of))
}

/// Row count and the compressed bytes on disk.
pub async fn size() -> Result<(i64, i64), String> {
    let rows = db_query(
        "SELECT (SELECT count(*) FROM snapshot), \
                (SELECT COALESCE(SUM(LENGTH(data)), 0) FROM pane_blob)",
        params![],
    )
    .await
    .map_err(db_err)?;
    let n = rows.get(0, 0).and_then(DbValue::as_i64).unwrap_or(0);
    let bytes = rows.get(0, 1).and_then(DbValue::as_i64).unwrap_or(0);
    Ok((n, bytes))
}

/// The newest row's content hash, for autosave dedup.
pub async fn newest_hash() -> Result<Option<(i64, i64)>, String> {
    let rows = db_query(
        "SELECT id, content_hash FROM snapshot ORDER BY saved_at_ms DESC, id DESC LIMIT 1",
        params![],
    )
    .await
    .map_err(db_err)?;
    Ok(rows.row(0).and_then(|r| Some((r.get(0)?.as_i64()?, r.get(1)?.as_i64()?))))
}

/// The id the next save takes. The plugin is the only writer and the
/// `busy` flag serializes saves, so reading it ahead is safe.
pub async fn next_id() -> Result<i64, String> {
    let rows = db_query("SELECT COALESCE(MAX(id), 0) + 1 FROM snapshot", params![])
        .await
        .map_err(db_err)?;
    Ok(rows.scalar().and_then(DbValue::as_i64).unwrap_or(1))
}

/// Insert one snapshot with all its chunks in one transaction. The
/// chunk buffers are read in place by the host, so they must outlive
/// the await, which the borrow guarantees.
pub async fn insert(snap: &NewSnapshot<'_>, chunks: &[Chunk<'_>]) -> Result<(), String> {
    let names = snap.names.join("\n");
    let head: Vec<DbValue> = vec![
        snap.id.into(),
        snap.saved_at_ms.into(),
        snap.version.into(),
        snap.reason.as_str().into(),
        (snap.sessions).into(),
        snap.windows.into(),
        snap.panes.into(),
        snap.raw_bytes.into(),
        snap.content_hash.into(),
        names.into(),
        snap.meta.into(),
    ];
    let chunk_params: Vec<Vec<DbValue>> = chunks
        .iter()
        .map(|c| {
            vec![
                snap.id.into(),
                c.pane_id.into(),
                c.seq.into(),
                (c.data.len() as i64).into(),
                zstd_ref(c.data),
            ]
        })
        .collect();
    let mut stmts: Vec<(&str, &[DbValue])> = Vec::with_capacity(1 + chunks.len());
    stmts.push((
        "INSERT INTO snapshot (id, saved_at_ms, version, reason, sessions, windows, \
         panes, raw_bytes, content_hash, session_names, meta) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        &head,
    ));
    for p in &chunk_params {
        stmts.push((
            "INSERT INTO pane_blob (snapshot_id, pane_id, seq, raw_len, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            p,
        ));
    }
    db_batch(&stmts).await.map_err(db_err)?;
    Ok(())
}

/// The JSON metadata of one snapshot.
pub async fn meta(id: i64) -> Result<Option<String>, String> {
    let rows = db_query("SELECT meta FROM snapshot WHERE id = ?1", params![id])
        .await
        .map_err(db_err)?;
    Ok(rows.scalar().and_then(DbValue::as_str).map(str::to_string))
}

/// One chunk of pane text, inflated. `None` past the last chunk. One
/// chunk per query keeps every result under the 8 MiB rows cap.
pub async fn chunk(id: i64, pane_id: i64, seq: i64) -> Result<Option<Vec<u8>>, String> {
    let rows = db_query(
        "SELECT data FROM pane_blob WHERE snapshot_id = ?1 AND pane_id = ?2 AND seq = ?3",
        params![id, pane_id, seq],
    )
    .await
    .map_err(db_err)?;
    let Some(frame) = rows.scalar().and_then(DbValue::as_blob) else {
        return Ok(None);
    };
    let raw = db_decompress(frame).map_err(|e| format!("snapshot {id} pane {pane_id}: {e}"))?;
    Ok(Some(raw.to_vec()))
}

/// Delete one snapshot; the chunks cascade.
pub async fn delete(id: i64) -> Result<(), String> {
    db_exec("DELETE FROM snapshot WHERE id = ?1", params![id])
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn set_pinned(id: i64, pinned: bool) -> Result<(), String> {
    db_exec(
        "UPDATE snapshot SET pinned = ?2 WHERE id = ?1",
        params![id, pinned],
    )
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Apply the retention rules. Returns how many rows went.
pub async fn retain(r: Retention, now_ms: i64) -> Result<i64, String> {
    let keep = i64::from(r.keep_snapshots.max(1));
    let cutoff = now_ms.saturating_sub(i64::from(r.keep_days) * 86_400_000);
    let count_cap = (
        "DELETE FROM snapshot WHERE pinned = 0 AND id NOT IN \
         (SELECT id FROM snapshot ORDER BY saved_at_ms DESC, id DESC LIMIT ?1)",
        vec![DbValue::from(keep)],
    );
    let age_cap = (
        "DELETE FROM snapshot WHERE pinned = 0 AND saved_at_ms < ?1 AND id != \
         (SELECT id FROM snapshot ORDER BY saved_at_ms DESC, id DESC LIMIT 1)",
        vec![DbValue::from(cutoff)],
    );
    let stmts: Vec<(&str, &[DbValue])> = if r.keep_days > 0 {
        vec![(count_cap.0, &count_cap.1), (age_cap.0, &age_cap.1)]
    } else {
        vec![(count_cap.0, &count_cap.1)]
    };
    let r = db_batch(&stmts).await.map_err(db_err)?;
    Ok(r.changes)
}
