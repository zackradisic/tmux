//! The single scheduler loop: sleep until the nearest due moment, claim
//! what is due, spawn one task per run, repeat. Everything is re-read
//! from the database on every pass, so the loop holds no state between
//! iterations and can be cancelled while it sleeps and started again.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use tmux_plugin_sdk::executor::{self, TaskId};
use tmux_plugin_sdk::prelude::*;

use crate::picker::Picker;
use crate::schedule::{self, Schedule};
use crate::store::{self, Catchup, Job};
use crate::{runner, Config};

/// Missed occurrences a `catchup = each` job replays at most.
const CATCHUP_CAP: usize = 50;

pub struct Shared {
    pub cfg: Config,
    /// Jobs with a run in flight (one at a time per job).
    pub running: RefCell<HashSet<i64>>,
    pub loop_task: Cell<Option<TaskId>>,
    /// True while the loop is parked in `sleep_ms`: the only moment it
    /// is safe to cancel and respawn it.
    pub sleeping: Cell<bool>,
    /// Set when something changed while the loop was busy: skip the
    /// next sleep.
    pub wake: Cell<bool>,
    pub tz_offset_min: Cell<i32>,
    pub picker: RefCell<Option<Picker>>,
}

pub type Ctl = Rc<Shared>;

pub fn start(sh: &Ctl) {
    let id = executor::spawn(run_loop(Rc::clone(sh), true));
    sh.loop_task.set(Some(id));
}

/// Something changed (a job added, a run finished): make the loop
/// recompute its wake time now.
pub fn kick(sh: &Ctl) {
    if sh.sleeping.get() {
        if let Some(id) = sh.loop_task.take() {
            executor::cancel(id);
        }
        sh.sleeping.set(false);
        let id = executor::spawn(run_loop(Rc::clone(sh), false));
        sh.loop_task.set(Some(id));
    } else {
        sh.wake.set(true);
    }
}

/// The local UTC offset in minutes, from tmux's own clock formatting.
pub fn tz_offset_min(sh: &Ctl) -> i32 {
    if sh.cfg.utc {
        return 0;
    }
    format_expand(OptionTarget::Server, "#{t/f/%z:start_time}")
        .ok()
        .and_then(|s| schedule::parse_tz_offset(&s))
        .unwrap_or(0)
}

fn parse_or_log(job: &Job) -> Option<Schedule> {
    match schedule::parse(&job.schedule) {
        Ok(s) => Some(s),
        Err(e) => {
            log(&format!("cron: {} has a bad schedule: {e}", job.label()));
            None
        }
    }
}

async fn run_loop(sh: Ctl, mut first: bool) {
    let mut idle_spins = 0u32;
    loop {
        let now = now_ms() as i64;
        let off = tz_offset_min(&sh);
        sh.tz_offset_min.set(off);
        if first {
            catch_up(&sh, now, off).await;
            first = false;
        }
        let claimed = pass(&sh, now, off).await;

        let next = store::next_wake().await.ok().flatten();
        let mut sleep = match next {
            Some(t) => (t - now).clamp(0, sh.cfg.max_sleep_ms),
            None => sh.cfg.max_sleep_ms,
        };
        // Due but not claimable (the job is still running): do not spin.
        if claimed == 0 && sleep == 0 {
            idle_spins += 1;
            if idle_spins >= 2 {
                sleep = 1000;
            }
        } else {
            idle_spins = 0;
        }
        if sh.wake.replace(false) {
            continue;
        }
        sh.sleeping.set(true);
        let r = sleep_ms(sleep as u64).await;
        sh.sleeping.set(false);
        if r.is_err() {
            return; // instance torn down
        }
    }
}

/// Claim a due occurrence and spawn its run. Returns true when claimed.
async fn claim_and_run(
    sh: &Ctl,
    job: &Job,
    sched: &Schedule,
    scheduled_ms: i64,
    now: i64,
    off: i32,
    reason: &str,
) -> bool {
    let anchor = job.next_run_ms.unwrap_or(job.created_ms);
    let next = schedule::next_after(sched, anchor, now, off);
    match store::claim_job(job.id, job.next_run_ms, next, scheduled_ms, now, reason).await {
        Ok(Some(run_id)) => {
            sh.running.borrow_mut().insert(job.id);
            executor::spawn(runner::execute(Rc::clone(sh), job.clone(), run_id, 1));
            true
        }
        Ok(None) => false,
        Err(e) => {
            log(&format!("cron: claim {}: {e}", job.label()));
            false
        }
    }
}

/// The first pass after init: overdue jobs by their catch-up policy.
async fn catch_up(sh: &Ctl, now: i64, off: i32) {
    let jobs = match store::enabled_jobs().await {
        Ok(j) => j,
        Err(e) => {
            log(&format!("cron: catch-up query: {e}"));
            return;
        }
    };
    for job in jobs {
        let Some(due) = job.next_run_ms else { continue };
        if due > now || sh.running.borrow().contains(&job.id) {
            continue;
        }
        let Some(sched) = parse_or_log(&job) else { continue };
        match job.catchup {
            Catchup::Skip => {
                let next = schedule::next_after(&sched, due, now, off);
                if let Err(e) = store::set_next_run(job.id, next, off, now).await {
                    log(&format!("cron: skip {}: {e}", job.label()));
                }
            }
            Catchup::Once => {
                let missed = schedule::missed_between(&sched, due, now, off, 100_000);
                let latest = missed.last().copied().unwrap_or(due);
                claim_and_run(sh, &job, &sched, latest, now, off, "catchup").await;
            }
            Catchup::Each => {
                let mut missed = schedule::missed_between(&sched, due, now, off, CATCHUP_CAP);
                if missed.is_empty() {
                    continue;
                }
                let first = missed.remove(0);
                let next = schedule::next_after(&sched, due, now, off);
                match store::claim_job(job.id, job.next_run_ms, next, first, now, "catchup")
                    .await
                {
                    Ok(Some(run_id)) => {
                        sh.running.borrow_mut().insert(job.id);
                        executor::spawn(runner::execute_each(
                            Rc::clone(sh),
                            job.clone(),
                            run_id,
                            missed,
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => log(&format!("cron: catch-up {}: {e}", job.label())),
                }
            }
        }
    }
}

/// One normal pass: due jobs and due retries. Also re-anchors cron-kind
/// jobs whose stored offset differs from the current one (DST moved).
async fn pass(sh: &Ctl, now: i64, off: i32) -> usize {
    let mut claimed = 0;
    match store::due_jobs(now).await {
        Ok(jobs) => {
            for job in jobs {
                if sh.running.borrow().contains(&job.id) {
                    continue;
                }
                let Some(sched) = parse_or_log(&job) else { continue };
                let due = job.next_run_ms.unwrap_or(now);
                if claim_and_run(sh, &job, &sched, due, now, off, "schedule").await {
                    claimed += 1;
                }
            }
        }
        Err(e) => log(&format!("cron: due query: {e}")),
    }
    match store::due_retries(now).await {
        Ok(retries) => {
            for r in retries {
                if sh.running.borrow().contains(&r.job_id) {
                    continue;
                }
                let job = match store::find_job_by_id(r.job_id).await {
                    Ok(Some(j)) if j.enabled => j,
                    _ => continue,
                };
                match store::claim_retry(r.id, now).await {
                    Ok(true) => {
                        sh.running.borrow_mut().insert(job.id);
                        executor::spawn(runner::execute(Rc::clone(sh), job, r.id, r.attempt));
                        claimed += 1;
                    }
                    Ok(false) => {}
                    Err(e) => log(&format!("cron: claim retry {}: {e}", r.id)),
                }
            }
        }
        Err(e) => log(&format!("cron: retry query: {e}")),
    }
    // DST fix-up: a cron job computed under another offset gets its next
    // run recomputed once, within one wake of the change.
    if let Ok(jobs) = store::enabled_jobs().await {
        for job in jobs {
            if job.tz_offset_min == off || job.schedule.starts_with("every") {
                continue;
            }
            let Some(sched) = parse_or_log(&job) else { continue };
            let next = schedule::next_after(&sched, now, now, off);
            if let Err(e) = store::set_next_run(job.id, next, off, now).await {
                log(&format!("cron: tz fix-up {}: {e}", job.label()));
            }
        }
    }
    claimed
}
