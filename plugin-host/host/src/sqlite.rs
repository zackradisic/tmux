//! Per-plugin SQLite database: the host side of the `db_*` imports.
//!
//! Every plugin gets one file, `<data dir>/store.db` (the same directory
//! the fs sandbox roots at), opened in WAL mode. The plugin owns the
//! schema and migrates it with `PRAGMA user_version`. The guest never
//! contains SQLite: it sends SQL text plus bound parameters (see
//! `tmux_plugin_abi::db` for the wire format) and gets back an exec
//! result or a rows block.
//!
//! Two connections per plugin:
//!
//! - the **async** connection, used by `db_exec`/`db_query`/`db_batch`.
//!   Each call spawns one task on the worker pool ([`submit`]); the task
//!   awaits the connection's `async_lock::Mutex`, runs its one statement
//!   (blocking that thread for real work, as an fs read does), releases
//!   the lock and posts the completion. A task waiting for the lock is
//!   parked in the executor, not on a thread. One plugin's statements
//!   therefore run one at a time; different plugins run in parallel.
//! - the **sync** connection, used by `db_exec_sync`/`db_query_sync` on
//!   the main thread (`init` migrations, tiny reads). WAL lets the two
//!   coexist; a sync call never waits behind a runner-thread query
//!   beyond SQLite's own write lock (bounded by the busy timeout).
//!
//! Ordering is the fs contract: awaited async ops are ordered, concurrent
//! un-awaited ones from one plugin are not (the mutex is fair against
//! starvation, not against arrival order). A completion the guest has
//! seen is visible to a later sync read.
//!
//! Fairness against fs work: a global semaphore with `runner_threads() -
//! 1` permits is acquired before the connection lock, so SQL can never
//! occupy every runner thread.
//!
//! SQL sandbox: SQLite can reach the filesystem from SQL (`ATTACH
//! DATABASE '/any/path'`, `VACUUM INTO '/any/path'`), which would let a
//! `db`-only plugin read or write arbitrary files. Every connection
//! installs an authorizer denying ATTACH/DETACH, sets
//! `SQLITE_LIMIT_ATTACHED` to 0, enables `SQLITE_DBCONFIG_DEFENSIVE`, and
//! a first-keyword check rejects `VACUUM`. Extension loading is compiled
//! out (feature not enabled).
//!
//! Time caps: a progress handler interrupts a statement past its
//! deadline - 500 ms on the sync connection (inside the 2 s guest CPU
//! budget), 30 s on the async one. Size caps: `MAX_DB_REQUEST_BYTES` on
//! the way in, `MAX_DB_ROWS_BYTES` on the way out.
//!
//! This module is the only user of `rusqlite` in the crate, the way
//! `engine.rs` is the only user of wasmtime.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use tmux_plugin_abi::db::{BatchStmt, DbValue, ExecResult, RowsWriter};
use tmux_plugin_abi::{ErrorCode, MAX_DB_ROWS_BYTES};

use crate::abi::{err, HostError};
use crate::fsbox::Root;
use crate::worker::{self, Completion, InFlight};

/// The database file, inside the plugin's data directory.
pub const DB_FILE: &str = "store.db";

/// Busy timeouts: how long a statement waits for another connection's
/// write lock before failing with `E_HOST` "database is busy".
const ASYNC_BUSY: Duration = Duration::from_secs(5);
const SYNC_BUSY: Duration = Duration::from_millis(250);

/// Statement wall-clock caps, enforced by the progress handler.
pub const ASYNC_STATEMENT_MS: u64 = 30_000;
pub const SYNC_STATEMENT_MS: u64 = 500;

/// Progress-handler granularity, in virtual-machine instructions.
const PROGRESS_OPS: i32 = 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The database file for a plugin's sandbox root.
pub fn path_for(root: &Root) -> PathBuf {
    root.path().join(DB_FILE)
}

// ---------------------------------------------------------------------------
// A connection with its sandbox and time cap installed.
// ---------------------------------------------------------------------------

pub struct Conn {
    conn: Connection,
    /// Unix ms after which the progress handler interrupts; 0 = never.
    deadline: Arc<AtomicU64>,
}

impl Conn {
    pub fn open(path: &Path, busy: Duration) -> Result<Conn, HostError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| {
            err(ErrorCode::Host, format!("open {}: {e}", path.display()))
        })?;
        // Sandbox first, before any SQL runs.
        conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
            .map_err(map_err)?;
        conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0);
        conn.authorizer(Some(
            |ctx: rusqlite::hooks::AuthContext<'_>| -> rusqlite::hooks::Authorization {
                use rusqlite::hooks::{AuthAction, Authorization};
                match ctx.action {
                    AuthAction::Attach { .. } | AuthAction::Detach { .. } => {
                        Authorization::Deny
                    }
                    _ => Authorization::Allow,
                }
            },
        ));
        let deadline = Arc::new(AtomicU64::new(0));
        let d = deadline.clone();
        conn.progress_handler(
            PROGRESS_OPS,
            Some(move || {
                let at = d.load(Ordering::Relaxed);
                at != 0 && now_ms() > at
            }),
        );
        conn.busy_timeout(busy).map_err(map_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(map_err)?;
        Ok(Conn { conn, deadline })
    }

    /// Arm the time cap for the next statement(s): `ms` from now.
    pub fn arm(&self, ms: u64) {
        self.deadline.store(now_ms().saturating_add(ms), Ordering::Relaxed);
    }

    /// Disarm the time cap (after a statement, so a later WAL checkpoint
    /// on close is never interrupted).
    fn disarm(&self) {
        self.deadline.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// The sync side: one connection per plugin, main thread only.
// ---------------------------------------------------------------------------

thread_local! {
    static SYNC: RefCell<HashMap<String, Conn>> = RefCell::new(HashMap::new());
}

/// Run `f` on the plugin's sync connection (opened on first use). Main
/// thread only. The connection borrow ends when `f` returns, so the
/// caller can re-enter the guest (`give_owned`) afterwards.
pub fn with_sync<T>(
    plugin: &str,
    root: &Root,
    f: impl FnOnce(&mut Conn) -> Result<T, HostError>,
) -> Result<T, HostError> {
    SYNC.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(plugin) {
            let conn = Conn::open(&path_for(root), SYNC_BUSY)?;
            map.insert(plugin.to_string(), conn);
        }
        let conn = map.get_mut(plugin).expect("just inserted");
        conn.arm(SYNC_STATEMENT_MS);
        let r = f(conn);
        conn.disarm();
        r
    })
}

// ---------------------------------------------------------------------------
// The async side: one task per statement, the connection behind an
// async mutex.
// ---------------------------------------------------------------------------

pub enum DbJob {
    Exec { sql: String, params: Vec<DbValue> },
    Query { sql: String, params: Vec<DbValue> },
    Batch { stmts: Vec<BatchStmt> },
}

pub struct DbRequest {
    pub token: u64,
    /// Detached in-flight accounting: counted by `worker::shutdown`, not
    /// by `wait_for_instance` (the job touches no guest memory).
    pub guard: InFlight,
    pub job: DbJob,
}

/// A plugin's async connection. Shared between the main thread (which
/// looks it up per call) and the statement tasks. Opened lazily by the
/// first task, on a runner thread.
pub struct DbHandle {
    path: PathBuf,
    conn: async_lock::Mutex<Option<Conn>>,
}

thread_local! {
    static HANDLES: RefCell<HashMap<String, Arc<DbHandle>>> =
        RefCell::new(HashMap::new());
}

/// The plugin's async handle, created on first use and cached (mirrors
/// `fsbox::root_for`). Main thread only.
pub fn handle_for(plugin: &str, root: &Root) -> Arc<DbHandle> {
    HANDLES.with(|c| {
        if let Some(h) = c.borrow().get(plugin) {
            return Arc::clone(h);
        }
        let h = Arc::new(DbHandle {
            path: path_for(root),
            conn: async_lock::Mutex::new(None),
        });
        c.borrow_mut().insert(plugin.to_string(), Arc::clone(&h));
        h
    })
}

/// Drop a plugin's cached connections (plugin unloaded). A statement
/// task still in flight holds its own `Arc` to the async handle, so this
/// never disturbs work in progress; the connection closes when the last
/// reference drops.
pub fn forget(plugin: &str) {
    HANDLES.with(|c| {
        c.borrow_mut().remove(plugin);
    });
    SYNC.with(|c| {
        c.borrow_mut().remove(plugin);
    });
}

/// Drop every cached connection (host shutdown). Called after
/// `worker::shutdown`, so no statement task runs; the last close of each
/// file checkpoints its WAL and removes the -wal/-shm files.
pub fn forget_all() {
    HANDLES.with(|c| c.borrow_mut().clear());
    SYNC.with(|c| c.borrow_mut().clear());
}

/// Permits for statement tasks actually running SQL: one fewer than the
/// runner threads, so a burst of statements can never starve fs jobs.
static RUNNING: OnceLock<async_lock::Semaphore> = OnceLock::new();

fn running() -> &'static async_lock::Semaphore {
    RUNNING.get_or_init(|| {
        async_lock::Semaphore::new(worker::runner_threads().saturating_sub(1).max(1))
    })
}

/// Spawn one statement task. Main thread only; the caller has already
/// copied the request out of guest memory and taken the in-flight guard.
pub fn submit(h: &Arc<DbHandle>, req: DbRequest) {
    let h = Arc::clone(h);
    worker::spawn(async move {
        let DbRequest { token, guard, job } = req;
        // Parked, not blocking, while the pool is full of SQL.
        let _permit = running().acquire().await;
        // Parked in the executor while another statement of this plugin
        // runs.
        let mut slot = h.conn.lock().await;
        let result = match &mut *slot {
            Some(conn) => run(conn, job),
            None => match Conn::open(&h.path, ASYNC_BUSY) {
                Ok(conn) => {
                    let conn = slot.insert(conn);
                    run(conn, job)
                }
                Err(e) => Err(e),
            },
        };
        drop(slot);
        worker::post(match result {
            Ok((v0, v1, data)) => Completion { token, err: 0, v0, v1, data },
            Err(e) => worker::err_completion(token, e.code, e.message),
        });
        // After the post: shutdown() waits on this guard, so it never
        // closes a connection mid-statement.
        drop(guard);
    });
}

/// Run one job on an armed connection. Returns (v0, v1, data).
fn run(conn: &mut Conn, job: DbJob) -> Result<(i64, i64, Vec<u8>), HostError> {
    conn.arm(ASYNC_STATEMENT_MS);
    let r = match job {
        DbJob::Exec { sql, params } => {
            exec(conn, &sql, &params).map(|r| (r.changes, r.last_insert_rowid, Vec::new()))
        }
        DbJob::Query { sql, params } => {
            query(conn, &sql, &params).map(|(data, nrows, ncols)| {
                (i64::from(nrows), i64::from(ncols), data)
            })
        }
        DbJob::Batch { stmts } => {
            batch(conn, &stmts).map(|r| (r.changes, r.last_insert_rowid, Vec::new()))
        }
    };
    conn.disarm();
    r
}

// ---------------------------------------------------------------------------
// Statement execution, shared by both sides.
// ---------------------------------------------------------------------------

fn bind(params: &[DbValue]) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;
    params
        .iter()
        .map(|p| match p {
            DbValue::Null => Value::Null,
            DbValue::Integer(i) => Value::Integer(*i),
            DbValue::Real(f) => Value::Real(*f),
            DbValue::Text(s) => Value::Text(s.clone()),
            DbValue::Blob(b) => Value::Blob(b.clone()),
        })
        .collect()
}

/// The first SQL keyword, past whitespace and comments, upper-cased.
fn first_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Reject statements the sandbox cannot otherwise stop. `VACUUM INTO
/// 'path'` writes a copy of the database anywhere on disk and does not
/// pass through the authorizer, so all of VACUUM is refused; plain
/// `VACUUM` is a maintenance operation a plugin has no business running
/// on a live tmux server anyway.
fn check_statement(sql: &str) -> Result<(), HostError> {
    if first_keyword(sql) == "VACUUM" {
        return Err(err(ErrorCode::BadRequest, "VACUUM is not allowed"));
    }
    Ok(())
}

/// Prepare exactly one statement. A second statement in the text is a
/// `BadRequest`: bound parameters would be ambiguous, and nothing has
/// run yet, so the refusal has no side effects.
fn prepare_single<'c>(
    conn: &'c Connection,
    sql: &str,
) -> Result<rusqlite::Statement<'c>, HostError> {
    let mut b = rusqlite::Batch::new(conn, sql);
    let first = b
        .next()
        .map_err(map_err)?
        .ok_or_else(|| err(ErrorCode::BadRequest, "empty SQL statement"))?;
    if b.next().map_err(map_err)?.is_some() {
        return Err(err(
            ErrorCode::BadRequest,
            "multiple statements: use db_batch, or a script without parameters",
        ));
    }
    Ok(first)
}

fn check_param_count(stmt: &rusqlite::Statement<'_>, given: usize) -> Result<(), HostError> {
    let want = stmt.parameter_count();
    if want != given {
        return Err(err(
            ErrorCode::BadRequest,
            format!("statement has {want} parameters, {given} given"),
        ));
    }
    Ok(())
}

/// Run a statement (or, with no parameters, a whole script) that returns
/// no rows. Rows a script statement does produce are stepped through
/// and discarded, so `PRAGMA journal_mode` style statements work.
pub fn exec(conn: &mut Conn, sql: &str, params: &[DbValue]) -> Result<ExecResult, HostError> {
    check_statement(sql)?;
    let c = &conn.conn;
    if params.is_empty() {
        let before = c.total_changes();
        let mut b = rusqlite::Batch::new(c, sql);
        let mut any = false;
        while let Some(mut stmt) = b.next().map_err(map_err)? {
            any = true;
            if stmt.parameter_count() != 0 {
                return Err(err(
                    ErrorCode::BadRequest,
                    "script statements cannot take parameters",
                ));
            }
            let mut rows = stmt.query([]).map_err(map_err)?;
            while rows.next().map_err(map_err)?.is_some() {}
        }
        if !any {
            return Err(err(ErrorCode::BadRequest, "empty SQL statement"));
        }
        let changes = c.total_changes().saturating_sub(before);
        return Ok(ExecResult {
            changes: changes as i64,
            last_insert_rowid: c.last_insert_rowid(),
        });
    }
    let mut stmt = prepare_single(c, sql)?;
    check_param_count(&stmt, params.len())?;
    let changes = stmt
        .execute(rusqlite::params_from_iter(bind(params)))
        .map_err(map_err)?;
    Ok(ExecResult {
        changes: changes as i64,
        last_insert_rowid: c.last_insert_rowid(),
    })
}

/// Run one statement and encode its result set. Returns the rows block,
/// the row count and the column count.
pub fn query(
    conn: &mut Conn,
    sql: &str,
    params: &[DbValue],
) -> Result<(Vec<u8>, u32, u16), HostError> {
    check_statement(sql)?;
    let c = &conn.conn;
    let mut stmt = prepare_single(c, sql)?;
    check_param_count(&stmt, params.len())?;
    // Column names must be read before the first step: SQLite may
    // change them once the statement is running (after a schema change).
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    if names.len() > usize::from(u16::MAX) {
        return Err(err(ErrorCode::Limit, "too many result columns"));
    }
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut w = RowsWriter::new(&name_refs);
    let ncols = names.len();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bind(params)))
        .map_err(map_err)?;
    while let Some(row) = rows.next().map_err(map_err)? {
        for i in 0..ncols {
            match row.get_ref(i).map_err(map_err)? {
                ValueRef::Null => w.null(),
                ValueRef::Integer(v) => w.integer(v),
                ValueRef::Real(v) => w.real(v),
                ValueRef::Text(b) => match std::str::from_utf8(b) {
                    Ok(s) => w.text(s),
                    // Invalid UTF-8 cannot be a wire `str`; hand the bytes
                    // over as they are.
                    Err(_) => w.blob(b),
                },
                ValueRef::Blob(b) => w.blob(b),
            }
        }
        w.end_row();
        if w.len() > MAX_DB_ROWS_BYTES {
            return Err(err(
                ErrorCode::Limit,
                format!(
                    "result set exceeds {} bytes; page with LIMIT/OFFSET",
                    MAX_DB_ROWS_BYTES
                ),
            ));
        }
    }
    let nrows = w.nrows();
    Ok((w.finish(), nrows, ncols as u16))
}

/// Run every statement in one transaction. The first failure rolls the
/// whole batch back and reports `statement #i: <message>`.
pub fn batch(conn: &mut Conn, stmts: &[BatchStmt]) -> Result<ExecResult, HostError> {
    if stmts.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty batch"));
    }
    for s in stmts {
        check_statement(&s.sql)?;
    }
    let c = &mut conn.conn;
    let before = c.total_changes();
    let tx = c.transaction().map_err(map_err)?;
    for (i, s) in stmts.iter().enumerate() {
        let prefix = |mut e: HostError| {
            e.message = format!("statement #{}: {}", i + 1, e.message);
            e
        };
        let mut stmt = prepare_single(&tx, &s.sql).map_err(prefix)?;
        check_param_count(&stmt, s.params.len()).map_err(prefix)?;
        // Step through rows too, so a RETURNING clause or a PRAGMA inside
        // a batch does not fail with "returned results".
        let mut rows = stmt
            .query(rusqlite::params_from_iter(bind(&s.params)))
            .map_err(map_err)
            .map_err(prefix)?;
        while rows.next().map_err(map_err).map_err(prefix)?.is_some() {}
    }
    tx.commit().map_err(map_err)?;
    let changes = conn.conn.total_changes().saturating_sub(before);
    Ok(ExecResult {
        changes: changes as i64,
        last_insert_rowid: conn.conn.last_insert_rowid(),
    })
}

/// Map a rusqlite error onto the ABI's error codes. The guest's fault
/// (syntax, constraints, missing tables, type or parameter mismatches)
/// is `BadRequest`; our caps and SQLite's own size and interrupt limits
/// are `Limit`; everything else (busy, I/O, corruption) is `Host`.
pub fn map_err(e: rusqlite::Error) -> HostError {
    use rusqlite::ffi::ErrorCode as Sq;
    use rusqlite::Error as E;
    let message = e.to_string();
    let code = match &e {
        E::SqlInputError { .. }
        | E::InvalidParameterCount(..)
        | E::MultipleStatement
        | E::InvalidQuery
        | E::ExecuteReturnedResults
        | E::QueryReturnedNoRows
        | E::InvalidColumnIndex(_)
        | E::InvalidColumnName(_)
        | E::InvalidColumnType(..)
        | E::ToSqlConversionFailure(_)
        | E::InvalidParameterName(_)
        | E::StatementChangedRows(_) => ErrorCode::BadRequest,
        E::SqliteFailure(f, _) => match f.code {
            Sq::ConstraintViolation
            | Sq::TypeMismatch
            | Sq::ParameterOutOfRange
            | Sq::AuthorizationForStatementDenied
            | Sq::Unknown => ErrorCode::BadRequest,
            Sq::TooBig | Sq::OperationInterrupted => ErrorCode::Limit,
            _ => ErrorCode::Host,
        },
        _ => ErrorCode::Host,
    };
    let message = match &e {
        E::SqliteFailure(f, _) if f.code == Sq::OperationInterrupted => {
            "statement exceeded its time limit".to_string()
        }
        E::SqliteFailure(f, _)
            if f.code == Sq::DatabaseBusy || f.code == Sq::DatabaseLocked =>
        {
            format!("database is busy: {message}")
        }
        _ => message,
    };
    HostError { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_conn(tag: &str, busy: Duration) -> (Conn, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("pgh-sqlite-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DB_FILE);
        (Conn::open(&path, busy).unwrap(), dir)
    }

    fn q(conn: &mut Conn, sql: &str, params: &[DbValue]) -> tmux_plugin_abi::db::Rows {
        let (data, _, _) = query(conn, sql, params).unwrap();
        tmux_plugin_abi::db::Rows::decode(&data).unwrap()
    }

    #[test]
    fn exec_and_query_all_types() {
        let (mut c, dir) = temp_conn("types", SYNC_BUSY);
        c.arm(5_000);
        exec(&mut c, "CREATE TABLE t (i, r, s, b, n)", &[]).unwrap();
        let r = exec(
            &mut c,
            "INSERT INTO t VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                DbValue::Integer(-7),
                DbValue::Real(1.5),
                DbValue::Text("hé".into()),
                DbValue::Blob(vec![1, 2, 3]),
                DbValue::Null,
            ],
        )
        .unwrap();
        assert_eq!(r.changes, 1);
        assert_eq!(r.last_insert_rowid, 1);
        let rows = q(&mut c, "SELECT i, r, s, b, n FROM t", &[]);
        assert_eq!(rows.columns, vec!["i", "r", "s", "b", "n"]);
        assert_eq!(rows.len(), 1);
        let row = rows.row(0).unwrap();
        assert_eq!(row.get(0), Some(&DbValue::Integer(-7)));
        assert_eq!(row.get(1), Some(&DbValue::Real(1.5)));
        assert_eq!(row.get(2), Some(&DbValue::Text("hé".into())));
        assert_eq!(row.get(3), Some(&DbValue::Blob(vec![1, 2, 3])));
        assert_eq!(row.get(4), Some(&DbValue::Null));
        // Zero rows keep the names.
        let rows = q(&mut c, "SELECT i FROM t WHERE i = 99", &[]);
        assert_eq!(rows.columns, vec!["i"]);
        assert!(rows.is_empty());
        // WAL is on.
        let rows = q(&mut c, "PRAGMA journal_mode", &[]);
        assert_eq!(rows.scalar().and_then(DbValue::as_str), Some("wal"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn script_mode_and_its_rules() {
        let (mut c, dir) = temp_conn("script", SYNC_BUSY);
        c.arm(5_000);
        let r = exec(
            &mut c,
            "CREATE TABLE kv (k TEXT PRIMARY KEY, v);
             INSERT INTO kv VALUES ('a', 1);
             INSERT INTO kv VALUES ('b', 2);
             PRAGMA user_version = 3;",
            &[],
        )
        .unwrap();
        assert_eq!(r.changes, 2);
        let rows = q(&mut c, "PRAGMA user_version", &[]);
        assert_eq!(rows.scalar().and_then(DbValue::as_i64), Some(3));

        // Params with a script: refused before anything runs.
        let e = exec(
            &mut c,
            "INSERT INTO kv VALUES ('c', ?1); INSERT INTO kv VALUES ('d', 4)",
            &[DbValue::Integer(3)],
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest, "{}", e.message);
        let rows = q(&mut c, "SELECT count(*) FROM kv", &[]);
        assert_eq!(rows.scalar().and_then(DbValue::as_i64), Some(2));

        // A script whose statement wants a parameter.
        let e = exec(&mut c, "INSERT INTO kv VALUES ('c', ?1)", &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);

        // Empty and comment-only SQL.
        assert_eq!(exec(&mut c, "", &[]).unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(
            exec(&mut c, "-- nothing\n", &[]).unwrap_err().code,
            ErrorCode::BadRequest
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn guest_errors_are_bad_request() {
        let (mut c, dir) = temp_conn("errors", SYNC_BUSY);
        c.arm(5_000);
        exec(&mut c, "CREATE TABLE u (k TEXT UNIQUE)", &[]).unwrap();
        exec(&mut c, "INSERT INTO u VALUES (?1)", &[DbValue::from("x")]).unwrap();

        let e = exec(&mut c, "INSERT INTO u VALUES (?1)", &[DbValue::from("x")]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.contains("UNIQUE"), "{}", e.message);

        let e = exec(&mut c, "INSERT INTO u VALUES (?1, ?2)", &[DbValue::from("y")]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest, "{}", e.message);

        let e = exec(&mut c, "SELEC 1", &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.contains("syntax"), "{}", e.message);

        let e = query(&mut c, "SELECT * FROM nope", &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.contains("no such table"), "{}", e.message);

        let e = query(&mut c, "SELECT 1; SELECT 2", &[DbValue::Integer(1)]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.contains("multiple"), "{}", e.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn batch_is_one_transaction() {
        let (mut c, dir) = temp_conn("batch", SYNC_BUSY);
        c.arm(5_000);
        exec(&mut c, "CREATE TABLE t (k TEXT UNIQUE)", &[]).unwrap();
        let stmt = |sql: &str, p: Vec<DbValue>| BatchStmt { sql: sql.into(), params: p };

        let r = batch(
            &mut c,
            &[
                stmt("INSERT INTO t VALUES (?1)", vec!["a".into()]),
                stmt("INSERT INTO t VALUES (?1)", vec!["b".into()]),
            ],
        )
        .unwrap();
        assert_eq!(r.changes, 2);
        assert_eq!(r.last_insert_rowid, 2);

        // Statement #2 violates UNIQUE: #1 must roll back too.
        let e = batch(
            &mut c,
            &[
                stmt("INSERT INTO t VALUES (?1)", vec!["c".into()]),
                stmt("INSERT INTO t VALUES (?1)", vec!["a".into()]),
            ],
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.starts_with("statement #2: "), "{}", e.message);
        let rows = q(&mut c, "SELECT count(*) FROM t", &[]);
        assert_eq!(rows.scalar().and_then(DbValue::as_i64), Some(2));

        // The connection is usable after the rollback.
        batch(&mut c, &[stmt("DELETE FROM t", vec![])]).unwrap();
        assert_eq!(batch(&mut c, &[]).unwrap_err().code, ErrorCode::BadRequest);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rows_cap_is_a_limit() {
        let (mut c, dir) = temp_conn("cap", SYNC_BUSY);
        c.arm(10_000);
        let e = query(&mut c, "SELECT zeroblob(9 * 1024 * 1024)", &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::Limit);
        assert!(e.message.contains("LIMIT/OFFSET"), "{}", e.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sandbox_denies_attach_and_vacuum() {
        let (mut c, dir) = temp_conn("sandbox", SYNC_BUSY);
        c.arm(5_000);
        let other = dir.join("other.db");
        let sql = format!("ATTACH DATABASE '{}' AS o", other.display());
        let e = exec(&mut c, &sql, &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest, "{}", e.message);
        assert!(!other.exists());

        let sql = format!("VACUUM INTO '{}'", dir.join("copy.db").display());
        let e = exec(&mut c, &sql, &[]).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(e.message.contains("VACUUM"), "{}", e.message);
        assert!(!dir.join("copy.db").exists());
        // Comments before the keyword do not hide it.
        let e = exec(&mut c, "/* x */ -- y\n vacuum", &[]).unwrap_err();
        assert!(e.message.contains("VACUUM"), "{}", e.message);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn time_cap_interrupts() {
        let (mut c, dir) = temp_conn("time", SYNC_BUSY);
        c.arm(50);
        let e = query(
            &mut c,
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r)
             SELECT count(*) FROM r",
            &[],
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::Limit, "{}", e.message);
        // And the connection still works afterwards.
        c.arm(5_000);
        let rows = q(&mut c, "SELECT 1", &[]);
        assert_eq!(rows.scalar().and_then(DbValue::as_i64), Some(1));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn first_keyword_skips_comments() {
        assert_eq!(first_keyword("  select 1"), "SELECT");
        assert_eq!(first_keyword("-- c\n/* d */ Vacuum"), "VACUUM");
        assert_eq!(first_keyword(""), "");
        assert_eq!(first_keyword("-- only"), "");
    }
}
