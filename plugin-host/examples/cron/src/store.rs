//! The database: schema, row types, and every SQL statement the plugin
//! runs, each as a named function. Nothing outside this module writes
//! SQL. The DB is the source of truth; the plugin keeps no job state in
//! memory beyond "which jobs are running right now".

use tmux_plugin_sdk::prelude::*;

pub const USER_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
  id INTEGER PRIMARY KEY,
  name TEXT UNIQUE,
  schedule TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('shell','tmux')),
  command TEXT NOT NULL,
  cwd TEXT,
  catchup TEXT NOT NULL DEFAULT 'once' CHECK (catchup IN ('skip','once','each')),
  enabled INTEGER NOT NULL DEFAULT 1,
  next_run_ms INTEGER,
  tz_offset_min INTEGER NOT NULL DEFAULT 0,
  created_ms INTEGER NOT NULL,
  updated_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS jobs_due ON jobs(enabled, next_run_ms);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY,
  job_id INTEGER NOT NULL,
  attempt INTEGER NOT NULL DEFAULT 1,
  reason TEXT NOT NULL CHECK (reason IN ('schedule','catchup','manual','retry')),
  state TEXT NOT NULL CHECK (state IN ('pending','running','ok','failed','interrupted')),
  scheduled_ms INTEGER NOT NULL,
  started_ms INTEGER,
  finished_ms INTEGER,
  duration_ms INTEGER,
  exit_code INTEGER,
  signalled INTEGER NOT NULL DEFAULT 0,
  output TEXT,
  output_bytes INTEGER,
  error TEXT);
CREATE INDEX IF NOT EXISTS runs_job ON runs(job_id, id);
CREATE INDEX IF NOT EXISTS runs_pending ON runs(state, scheduled_ms);
PRAGMA user_version = 1;";

const JOB_COLUMNS: &str = "id, name, schedule, kind, command, cwd, catchup, enabled, \
                           next_run_ms, tz_offset_min, created_ms, updated_ms";
const RUN_COLUMNS: &str = "id, job_id, attempt, reason, state, scheduled_ms, started_ms, \
                           finished_ms, duration_ms, exit_code, signalled, output, \
                           output_bytes, error";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Shell,
    Tmux,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Shell => "shell",
            Kind::Tmux => "tmux",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "shell" => Some(Kind::Shell),
            "tmux" => Some(Kind::Tmux),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catchup {
    Skip,
    Once,
    Each,
}

impl Catchup {
    pub fn as_str(self) -> &'static str {
        match self {
            Catchup::Skip => "skip",
            Catchup::Once => "once",
            Catchup::Each => "each",
        }
    }

    pub fn parse(s: &str) -> Option<Catchup> {
        match s {
            "skip" => Some(Catchup::Skip),
            "once" => Some(Catchup::Once),
            "each" => Some(Catchup::Each),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub name: Option<String>,
    pub schedule: String,
    pub kind: Kind,
    pub command: String,
    pub cwd: Option<String>,
    pub catchup: Catchup,
    pub enabled: bool,
    pub next_run_ms: Option<i64>,
    pub tz_offset_min: i32,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Job {
    /// `#3 "nightly"` or `#3`.
    pub fn label(&self) -> String {
        match &self.name {
            Some(n) => format!("#{} \"{}\"", self.id, n),
            None => format!("#{}", self.id),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunRow {
    pub id: i64,
    pub job_id: i64,
    pub attempt: i64,
    pub reason: String,
    pub state: String,
    pub scheduled_ms: i64,
    pub started_ms: Option<i64>,
    pub finished_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub exit_code: Option<i64>,
    pub signalled: bool,
    pub output: Option<String>,
    pub output_bytes: Option<i64>,
    pub error: Option<String>,
}

/// One row of `ls` / the picker: the job plus its newest finished run.
#[derive(Debug, Clone)]
pub struct JobListing {
    pub job: Job,
    pub last_state: Option<String>,
    pub last_finished_ms: Option<i64>,
    pub last_exit: Option<i64>,
    pub running: bool,
}

pub struct NewJob {
    pub name: Option<String>,
    pub schedule: String,
    pub kind: Kind,
    pub command: String,
    pub cwd: Option<String>,
    pub catchup: Catchup,
    pub next_run_ms: i64,
    pub tz_offset_min: i32,
    pub now_ms: i64,
}

/// Everything `finalize` writes.
pub struct Finished {
    pub run_id: i64,
    pub job_id: i64,
    pub state: &'static str,
    pub finished_ms: i64,
    pub duration_ms: i64,
    pub exit_code: Option<i64>,
    pub signalled: bool,
    pub output: Option<String>,
    pub output_bytes: i64,
    pub error: Option<String>,
    /// A retry to schedule: (attempt number, when).
    pub retry: Option<(i64, i64)>,
    pub keep_runs: i64,
    /// Finished runs older than this are dropped.
    pub keep_before_ms: i64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub jobs: i64,
    pub enabled: i64,
    pub running: i64,
    pub retries_pending: i64,
    pub runs_kept: i64,
}

fn s(v: Option<&DbValue>) -> Option<String> {
    v.and_then(DbValue::as_str).map(str::to_owned)
}

fn i(v: Option<&DbValue>) -> Option<i64> {
    v.and_then(DbValue::as_i64)
}

fn job_from(row: Row<'_>) -> Option<Job> {
    Some(Job {
        id: i(row.get_named("id"))?,
        name: s(row.get_named("name")),
        schedule: s(row.get_named("schedule"))?,
        kind: Kind::parse(&s(row.get_named("kind"))?)?,
        command: s(row.get_named("command"))?,
        cwd: s(row.get_named("cwd")),
        catchup: Catchup::parse(&s(row.get_named("catchup"))?)?,
        enabled: i(row.get_named("enabled")).unwrap_or(0) != 0,
        next_run_ms: i(row.get_named("next_run_ms")),
        tz_offset_min: i(row.get_named("tz_offset_min")).unwrap_or(0) as i32,
        created_ms: i(row.get_named("created_ms")).unwrap_or(0),
        updated_ms: i(row.get_named("updated_ms")).unwrap_or(0),
    })
}

fn run_from(row: Row<'_>) -> Option<RunRow> {
    Some(RunRow {
        id: i(row.get_named("id"))?,
        job_id: i(row.get_named("job_id"))?,
        attempt: i(row.get_named("attempt")).unwrap_or(1),
        reason: s(row.get_named("reason")).unwrap_or_default(),
        state: s(row.get_named("state")).unwrap_or_default(),
        scheduled_ms: i(row.get_named("scheduled_ms")).unwrap_or(0),
        started_ms: i(row.get_named("started_ms")),
        finished_ms: i(row.get_named("finished_ms")),
        duration_ms: i(row.get_named("duration_ms")),
        exit_code: i(row.get_named("exit_code")),
        signalled: i(row.get_named("signalled")).unwrap_or(0) != 0,
        output: s(row.get_named("output")),
        output_bytes: i(row.get_named("output_bytes")),
        error: s(row.get_named("error")),
    })
}

fn jobs_from(rows: &Rows) -> Vec<Job> {
    rows.iter().filter_map(job_from).collect()
}

// ---------------------------------------------------------------------------
// init (sync, main thread)
// ---------------------------------------------------------------------------

/// Create or upgrade the schema.
pub fn migrate_sync() -> Result<(), String> {
    let v = db_query_sync("PRAGMA user_version", params![])
        .map_err(|e| format!("db: {e}"))?;
    let version = v.scalar().and_then(DbValue::as_i64).unwrap_or(0);
    if version == 0 {
        db_exec_sync(SCHEMA, params![]).map_err(|e| format!("db: {e}"))?;
    } else if version > USER_VERSION {
        return Err(format!(
            "store.db is version {version}, this plugin understands {USER_VERSION}"
        ));
    }
    Ok(())
}

/// Runs the previous server left `running` did not finish: the plugin
/// lost track of them. Returns how many.
pub fn mark_interrupted_sync(now_ms: i64) -> Result<i64, String> {
    let r = db_exec_sync(
        "UPDATE runs SET state = 'interrupted', finished_ms = ?1, \
         error = 'server stopped during run' WHERE state = 'running'",
        params![now_ms],
    )
    .map_err(|e| format!("db: {e}"))?;
    Ok(r.changes)
}

// ---------------------------------------------------------------------------
// scheduler queries
// ---------------------------------------------------------------------------

pub async fn due_jobs(now_ms: i64) -> Result<Vec<Job>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE enabled = 1 AND next_run_ms IS NOT NULL \
             AND next_run_ms <= ?1 ORDER BY next_run_ms, id"
        ),
        params![now_ms],
    )
    .await?;
    Ok(jobs_from(&rows))
}

pub async fn enabled_jobs() -> Result<Vec<Job>, HostError> {
    let rows = db_query(
        &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE enabled = 1 ORDER BY id"),
        params![],
    )
    .await?;
    Ok(jobs_from(&rows))
}

pub async fn all_jobs() -> Result<Vec<Job>, HostError> {
    let rows =
        db_query(&format!("SELECT {JOB_COLUMNS} FROM jobs ORDER BY id"), params![]).await?;
    Ok(jobs_from(&rows))
}

pub async fn due_retries(now_ms: i64) -> Result<Vec<RunRow>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE state = 'pending' AND scheduled_ms <= ?1 \
             ORDER BY scheduled_ms, id"
        ),
        params![now_ms],
    )
    .await?;
    Ok(rows.iter().filter_map(run_from).collect())
}

/// The earliest moment anything is due: an enabled job's next run or a
/// pending retry. `None` when nothing is scheduled.
pub async fn next_wake() -> Result<Option<i64>, HostError> {
    let rows = db_query(
        "SELECT min(t) FROM (\
           SELECT min(next_run_ms) AS t FROM jobs WHERE enabled = 1 \
           UNION ALL \
           SELECT min(scheduled_ms) FROM runs WHERE state = 'pending')",
        params![],
    )
    .await?;
    Ok(rows.scalar().and_then(DbValue::as_i64))
}

/// Claim a due job: advance `next_run_ms` (only if it still holds the
/// value we read) and insert its `running` row in one transaction. The
/// INSERT is guarded by `changes() = 1`, so a lost race inserts nothing.
/// Pending retries of the job are dropped: the schedule moved on.
/// Returns the run id when the claim won.
pub async fn claim_job(
    job_id: i64,
    expected_next_ms: Option<i64>,
    new_next_ms: Option<i64>,
    scheduled_ms: i64,
    now_ms: i64,
    reason: &str,
) -> Result<Option<i64>, HostError> {
    let r = db_batch(&[
        (
            "DELETE FROM runs WHERE job_id = ?1 AND state = 'pending' \
             AND (SELECT next_run_ms FROM jobs WHERE id = ?1) IS ?2",
            params![job_id, expected_next_ms],
        ),
        (
            "UPDATE jobs SET next_run_ms = ?2, updated_ms = ?3 \
             WHERE id = ?1 AND next_run_ms IS ?4",
            params![job_id, new_next_ms, now_ms, expected_next_ms],
        ),
        (
            "INSERT INTO runs (job_id, attempt, reason, state, scheduled_ms, started_ms) \
             SELECT ?1, 1, ?2, 'running', ?3, ?4 WHERE changes() = 1",
            params![job_id, reason, scheduled_ms, now_ms],
        ),
    ])
    .await?;
    // DELETE k (only when the claim wins) + UPDATE 1 + INSERT 1.
    Ok(if r.changes >= 2 { Some(r.last_insert_rowid) } else { None })
}

/// Take a pending retry into `running`. False if it is gone.
pub async fn claim_retry(run_id: i64, now_ms: i64) -> Result<bool, HostError> {
    let r = db_exec(
        "UPDATE runs SET state = 'running', started_ms = ?2 WHERE id = ?1 AND state = 'pending'",
        params![run_id, now_ms],
    )
    .await?;
    Ok(r.changes == 1)
}

/// Insert a `running` row outside the schedule (`manual`, or the later
/// occurrences of a `catchup each` pass). Returns the run id.
pub async fn insert_run(
    job_id: i64,
    reason: &str,
    scheduled_ms: i64,
    now_ms: i64,
) -> Result<i64, HostError> {
    let r = db_exec(
        "INSERT INTO runs (job_id, attempt, reason, state, scheduled_ms, started_ms) \
         VALUES (?1, 1, ?2, 'running', ?3, ?4)",
        params![job_id, reason, scheduled_ms, now_ms],
    )
    .await?;
    Ok(r.last_insert_rowid)
}

/// Record a finished run, schedule its retry, and apply retention, in
/// one transaction.
pub async fn finalize(f: &Finished) -> Result<(), HostError> {
    let update: Vec<DbValue> = vec![
        f.run_id.into(),
        f.state.into(),
        f.finished_ms.into(),
        f.duration_ms.into(),
        f.exit_code.into(),
        f.signalled.into(),
        f.output.clone().into(),
        f.output_bytes.into(),
        f.error.clone().into(),
    ];
    let retry: Vec<DbValue> = match f.retry {
        Some((attempt, at)) => vec![f.job_id.into(), attempt.into(), at.into()],
        None => Vec::new(),
    };
    let keep: Vec<DbValue> = vec![f.job_id.into(), f.keep_runs.into()];
    let age: Vec<DbValue> = vec![f.job_id.into(), f.keep_before_ms.into()];

    let mut stmts: Vec<(&str, &[DbValue])> = vec![(
        "UPDATE runs SET state = ?2, finished_ms = ?3, duration_ms = ?4, exit_code = ?5, \
         signalled = ?6, output = ?7, output_bytes = ?8, error = ?9 WHERE id = ?1",
        &update,
    )];
    if f.retry.is_some() {
        stmts.push((
            "INSERT INTO runs (job_id, attempt, reason, state, scheduled_ms) \
             VALUES (?1, ?2, 'retry', 'pending', ?3)",
            &retry,
        ));
    }
    stmts.push((
        "DELETE FROM runs WHERE job_id = ?1 AND state IN ('ok','failed','interrupted') \
         AND id NOT IN (SELECT id FROM runs WHERE job_id = ?1 \
                        AND state IN ('ok','failed','interrupted') \
                        ORDER BY id DESC LIMIT ?2)",
        &keep,
    ));
    stmts.push((
        "DELETE FROM runs WHERE job_id = ?1 AND finished_ms IS NOT NULL AND finished_ms < ?2",
        &age,
    ));
    db_batch(&stmts).await.map(|_| ())
}

/// Move a job's next run (catch-up skip, DST fix-up, re-enable).
pub async fn set_next_run(
    job_id: i64,
    next_run_ms: Option<i64>,
    tz_offset_min: i32,
    now_ms: i64,
) -> Result<(), HostError> {
    db_exec(
        "UPDATE jobs SET next_run_ms = ?2, tz_offset_min = ?3, updated_ms = ?4 WHERE id = ?1",
        params![job_id, next_run_ms, tz_offset_min, now_ms],
    )
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

pub async fn insert_job(j: &NewJob) -> Result<i64, HostError> {
    let r = db_exec(
        "INSERT INTO jobs (name, schedule, kind, command, cwd, catchup, enabled, next_run_ms, \
         tz_offset_min, created_ms, updated_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?9)",
        params![
            j.name.clone(),
            j.schedule.as_str(),
            j.kind.as_str(),
            j.command.as_str(),
            j.cwd.clone(),
            j.catchup.as_str(),
            j.next_run_ms,
            j.tz_offset_min,
            j.now_ms
        ],
    )
    .await?;
    Ok(r.last_insert_rowid)
}

/// A job by `#id`, bare id, or name.
pub async fn find_job(ident: &str) -> Result<Option<Job>, HostError> {
    let ident = ident.trim();
    let id: Option<i64> = ident.strip_prefix('#').unwrap_or(ident).parse().ok();
    let rows = db_query(
        &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id IS ?1 OR name = ?2 LIMIT 1"),
        params![id, ident],
    )
    .await?;
    Ok(rows.row(0).and_then(job_from))
}

pub async fn find_job_by_id(id: i64) -> Result<Option<Job>, HostError> {
    let rows = db_query(
        &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
        params![id],
    )
    .await?;
    Ok(rows.row(0).and_then(job_from))
}

/// Delete a job and its runs. Returns the number of runs dropped.
pub async fn delete_job(job_id: i64) -> Result<i64, HostError> {
    let r = db_batch(&[
        ("DELETE FROM runs WHERE job_id = ?1", params![job_id]),
        ("DELETE FROM jobs WHERE id = ?1", params![job_id]),
    ])
    .await?;
    Ok((r.changes - 1).max(0))
}

pub async fn set_enabled(
    job_id: i64,
    enabled: bool,
    next_run_ms: Option<i64>,
    now_ms: i64,
) -> Result<(), HostError> {
    db_batch(&[
        (
            "UPDATE jobs SET enabled = ?2, next_run_ms = ?3, updated_ms = ?4 WHERE id = ?1",
            params![job_id, enabled, next_run_ms, now_ms],
        ),
        // A disabled job keeps no pending retry.
        (
            "DELETE FROM runs WHERE job_id = ?1 AND state = 'pending' AND ?2 = 0",
            params![job_id, enabled],
        ),
    ])
    .await
    .map(|_| ())
}

/// Every job with its newest finished run and whether it runs now.
pub async fn list_jobs() -> Result<Vec<JobListing>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {cols}, \
             (SELECT r.state FROM runs r WHERE r.job_id = j.id \
                AND r.state IN ('ok','failed','interrupted') ORDER BY r.id DESC LIMIT 1) \
                AS last_state, \
             (SELECT r.finished_ms FROM runs r WHERE r.job_id = j.id \
                AND r.state IN ('ok','failed','interrupted') ORDER BY r.id DESC LIMIT 1) \
                AS last_finished_ms, \
             (SELECT r.exit_code FROM runs r WHERE r.job_id = j.id \
                AND r.state IN ('ok','failed','interrupted') ORDER BY r.id DESC LIMIT 1) \
                AS last_exit, \
             (SELECT count(*) FROM runs r WHERE r.job_id = j.id AND r.state = 'running') \
                AS running \
             FROM jobs j ORDER BY j.id",
            cols = JOB_COLUMNS
                .split(", ")
                .map(|c| format!("j.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        params![],
    )
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some(JobListing {
                job: job_from(row)?,
                last_state: s(row.get_named("last_state")),
                last_finished_ms: i(row.get_named("last_finished_ms")),
                last_exit: i(row.get_named("last_exit")),
                running: i(row.get_named("running")).unwrap_or(0) > 0,
            })
        })
        .collect())
}

/// The newest finished run of a job.
pub async fn last_run(job_id: i64) -> Result<Option<RunRow>, HostError> {
    let rows = db_query(
        &format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE job_id = ?1 \
             AND state IN ('ok','failed','interrupted') ORDER BY id DESC LIMIT 1"
        ),
        params![job_id],
    )
    .await?;
    Ok(rows.row(0).and_then(run_from))
}

pub async fn status_counts() -> Result<Status, HostError> {
    let rows = db_query(
        "SELECT (SELECT count(*) FROM jobs) AS jobs, \
                (SELECT count(*) FROM jobs WHERE enabled = 1) AS enabled, \
                (SELECT count(*) FROM runs WHERE state = 'running') AS running, \
                (SELECT count(*) FROM runs WHERE state = 'pending') AS retries, \
                (SELECT count(*) FROM runs WHERE state IN ('ok','failed','interrupted')) AS kept",
        params![],
    )
    .await?;
    let Some(row) = rows.row(0) else { return Ok(Status::default()) };
    Ok(Status {
        jobs: i(row.get_named("jobs")).unwrap_or(0),
        enabled: i(row.get_named("enabled")).unwrap_or(0),
        running: i(row.get_named("running")).unwrap_or(0),
        retries_pending: i(row.get_named("retries")).unwrap_or(0),
        runs_kept: i(row.get_named("kept")).unwrap_or(0),
    })
}
