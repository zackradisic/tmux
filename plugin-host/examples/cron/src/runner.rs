//! Running one job occurrence: execute the action, cap the output,
//! decide on a retry, write the finished row, and wake the scheduler.

use tmux_plugin_sdk::prelude::*;

use crate::scheduler::{self, Ctl};
use crate::store::{self, Finished, Job, Kind};
use crate::{picker, schedule};

/// Backoff doubling stops here (2^10 * base).
const MAX_BACKOFF_SHIFT: u32 = 10;

/// Keep the last `max` bytes of `output`, cut at a char boundary with the
/// first partial line dropped, and mark the cut. Returns (tail, true
/// length).
pub fn tail_cap(output: &str, max: usize) -> (String, i64) {
    let len = output.len() as i64;
    if output.len() <= max {
        return (output.to_string(), len);
    }
    let mut start = output.len() - max;
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let mut tail = &output[start..];
    if let Some(nl) = tail.find('\n') {
        tail = &tail[nl + 1..];
    }
    (format!("…\n{tail}"), len)
}

struct Outcome {
    ok: bool,
    exit_code: Option<i64>,
    signalled: bool,
    output: String,
    error: Option<String>,
}

async fn run_action(job: &Job) -> Outcome {
    match job.kind {
        Kind::Shell => match run_job(job.command.as_str(), job.cwd.as_deref()).await {
            Ok(o) => Outcome {
                ok: !o.signalled && o.status == 0,
                exit_code: Some(i64::from(o.status)),
                signalled: o.signalled,
                output: o.output,
                error: None,
            },
            Err(e) => Outcome {
                ok: false,
                exit_code: None,
                signalled: false,
                output: String::new(),
                error: Some(e.message),
            },
        },
        // Only a parse error fails here: a tmux command that runs and
        // errors still completes Ok (see the module doc in lib.rs).
        Kind::Tmux => match run_command(job.command.as_str()).await {
            Ok(()) => Outcome {
                ok: true,
                exit_code: Some(0),
                signalled: false,
                output: String::new(),
                error: None,
            },
            Err(e) => Outcome {
                ok: false,
                exit_code: None,
                signalled: false,
                output: String::new(),
                error: Some(e.message),
            },
        },
    }
}

/// Run one `running` row to its finished state. Returns whether it
/// succeeded.
async fn execute_one(sh: &Ctl, job: &Job, run_id: i64, attempt: i64) -> bool {
    let started = now_ms() as i64;
    let out = run_action(job).await;
    let finished = now_ms() as i64;
    let (tail, bytes) = tail_cap(&out.output, sh.cfg.max_output_bytes);
    let retry = if !out.ok && attempt <= sh.cfg.retry_max {
        let shift = (attempt - 1).clamp(0, i64::from(MAX_BACKOFF_SHIFT)) as u32;
        let wait = sh.cfg.retry_backoff_ms.saturating_mul(1i64 << shift);
        Some((attempt + 1, finished + wait))
    } else {
        None
    };
    let f = Finished {
        run_id,
        job_id: job.id,
        state: if out.ok { "ok" } else { "failed" },
        finished_ms: finished,
        duration_ms: finished - started,
        exit_code: out.exit_code,
        signalled: out.signalled,
        output: if tail.is_empty() { None } else { Some(tail) },
        output_bytes: bytes,
        error: out.error.clone(),
        retry,
        keep_runs: sh.cfg.keep_runs,
        keep_before_ms: finished - sh.cfg.keep_days * 86_400_000,
    };
    if let Err(e) = store::finalize(&f).await {
        log(&format!("cron: finalize run {run_id}: {e}"));
    }
    if !out.ok && sh.cfg.notify_failures {
        let why = match (&out.error, out.exit_code) {
            (Some(e), _) => e.clone(),
            (None, Some(c)) if out.signalled => format!("signal {c}"),
            (None, Some(c)) => format!("exit {c}"),
            (None, None) => "failed".into(),
        };
        let next = match retry {
            Some((_, at)) => format!("; retry {}", schedule::describe_next(at, finished)),
            None if attempt > 1 => "; no more retries".to_string(),
            None => String::new(),
        };
        let _ = display_message(&format!(
            "cron: {} failed ({why}){next}",
            job.label()
        ));
    }
    out.ok
}

/// Run one occurrence, then release the job and wake the scheduler.
pub async fn execute(sh: Ctl, job: Job, run_id: i64, attempt: i64) {
    execute_one(&sh, &job, run_id, attempt).await;
    sh.running.borrow_mut().remove(&job.id);
    picker::refresh(&sh);
    scheduler::kick(&sh);
}

/// `catchup = each`: the first occurrence is already claimed as
/// `first_run_id`; run it, then each of `rest` in order, each with its
/// own `catchup` row. The job stays in `running` throughout.
pub async fn execute_each(sh: Ctl, job: Job, first_run_id: i64, rest: Vec<i64>) {
    execute_one(&sh, &job, first_run_id, 1).await;
    for scheduled in rest {
        let now = now_ms() as i64;
        match store::insert_run(job.id, "catchup", scheduled, now).await {
            Ok(id) => {
                execute_one(&sh, &job, id, 1).await;
            }
            Err(e) => {
                log(&format!("cron: catch-up row for {}: {e}", job.label()));
                break;
            }
        }
    }
    sh.running.borrow_mut().remove(&job.id);
    picker::refresh(&sh);
    scheduler::kick(&sh);
}

#[cfg(test)]
mod tests {
    use super::tail_cap;

    #[test]
    fn tail_cap_keeps_the_end() {
        let (t, n) = tail_cap("abc", 10);
        assert_eq!((t.as_str(), n), ("abc", 3));
        let (t, n) = tail_cap("line1\nline2\nline3\n", 9);
        assert_eq!(n, 18);
        assert_eq!(t, "…\nline3\n");
        // A cut inside a multi-byte char moves forward to a boundary.
        let (t, _) = tail_cap("ééé\nxx", 6);
        assert!(t.ends_with("xx"));
    }
}
