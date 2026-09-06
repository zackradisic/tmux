//! The blocking-work executor: a small pool of threads that runs work
//! the tmux event loop must never wait on (file I/O, SQLite), and the
//! doorbell that brings the results back.
//!
//! Architecture (see ABI.md): submission = the main thread spawns a task
//! straight onto the executor, which a fixed set of threads runs;
//! completion = a crossbeam channel back to the main thread plus an
//! **eventfd doorbell**, because the main thread sleeps in epoll inside
//! libevent and a queue alone cannot wake it. The doorbell is coalesced
//! with an atomic armed flag (ring on empty→non-empty); the main-thread
//! drain clears the flag and reads the eventfd BEFORE draining the queue
//! (the lost-wakeup rule).
//!
//! Why an executor and not a thread per job: a listing wants its stats
//! run in parallel, and the obvious way to do that - push chunks to the
//! same pool and block on a join - deadlocks. Every thread can end up
//! asleep in a join, holding the thread its own chunks need. The
//! dependency graph is a DAG; the cycle is in the threads. Awaiting
//! instead of blocking removes it: a task that waits gives its thread
//! back, and that thread runs whatever is ready - including the chunks
//! it is waiting for. The rule that keeps it true is narrow: never block
//! a thread on another task. Blocking on real work (a read, a statx, a
//! SQL statement) is exactly what these threads are for.
//!
//! Two kinds of in-flight work are tracked. **Pinned** work touches
//! guest memory directly (the fs jobs: zero-copy reads and writes of the
//! guest's buffers); it is counted per instance, and instance teardown
//! calls [`wait_for_instance`] before the Store drops. **Detached** work
//! (SQLite) copies its inputs at call time and returns its results as
//! completion data; it is counted only so [`shutdown`] can wait for it.
//! Both are held as an RAII [`InFlight`] guard, dropped AFTER the
//! completion is posted, so a returned wait means the work is finished.
//!
//! Thread discipline: the workers touch no tmux or host state - only
//! syscalls on pre-validated inputs. All host bookkeeping happens on the
//! main thread.
//!
//! Consumers: `fsworker` (the fs jobs) and `sqlite` (statements).

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use tmux_plugin_abi::ErrorCode;

use crate::registry::ScopeId;

/// The instance a job belongs to, for teardown coordination.
pub type InstKey = (String, ScopeId, u64);

/// A finished job, handed to the main thread's drain. Same shape as
/// pgh_async_complete.
pub struct Completion {
    pub token: u64,
    pub err: i32,
    pub v0: i64,
    pub v1: i64,
    pub data: Vec<u8>,
}

/// In-flight counts: pinned work per instance, detached work in total.
#[derive(Default)]
struct Counts {
    pinned: HashMap<InstKey, usize>,
    detached: usize,
}

struct Inflight {
    counts: Mutex<Counts>,
    cv: Condvar,
}

struct State {
    /// Every task posts its completion here.
    done: crossbeam_channel::Sender<Completion>,
    completions: crossbeam_channel::Receiver<Completion>,
    /// Worker-thread log lines, drained onto the main-thread host log
    /// (which is thread-local, so a worker cannot write it directly).
    log_tx: crossbeam_channel::Sender<String>,
    log_rx: crossbeam_channel::Receiver<String>,
    doorbell_write: RawFd,
    doorbell_read: RawFd,
    /// Closing this ends every runner's `run` future.
    stop: async_channel::Sender<()>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// The executor every task runs on. A `static` rather than an `Arc` so
/// a task can hold `&'static Executor` and spawn children without a
/// reference cycle back into the executor that owns it.
static EXECUTOR: OnceLock<async_executor::Executor<'static>> =
    OnceLock::new();

pub(crate) fn executor() -> &'static async_executor::Executor<'static> {
    EXECUTOR.get_or_init(async_executor::Executor::new)
}

/// Runner threads. Six is where the measured curve flattens: a
/// ten-thousand-entry listing with times takes 4.4ms on four threads,
/// 3.27ms on six, and no better on eight or twelve. They park on the
/// executor when there is nothing to do, which in a tmux server is
/// almost always.
pub(crate) fn runner_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 6)
}

/// Doorbell coalescing (the kprotty/Bun "notified" bit): workers
/// fetch_or(true) and ring only on the false→true edge.
static ARMED: AtomicBool = AtomicBool::new(false);
static INFLIGHT: OnceLock<Inflight> = OnceLock::new();
static STATE: Mutex<Option<Arc<State>>> = Mutex::new(None);

fn inflight() -> &'static Inflight {
    INFLIGHT.get_or_init(|| Inflight {
        counts: Mutex::new(Counts::default()),
        cv: Condvar::new(),
    })
}

/// Create the doorbell: eventfd on Linux, a pipe elsewhere. Returns
/// (read_fd, write_fd) — the same fd twice for eventfd.
fn make_doorbell() -> Result<(RawFd, RawFd), String> {
    #[cfg(target_os = "linux")]
    {
        let fd = unsafe {
            libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)
        };
        if fd < 0 {
            return Err("eventfd failed".into());
        }
        Ok((fd, fd))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err("pipe failed".into());
        }
        for fd in fds {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        Ok((fds[0], fds[1]))
    }
}

fn ring(fd: RawFd) {
    let one: u64 = 1;
    unsafe {
        libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
    }
}

fn drain_doorbell(fd: RawFd) {
    let mut buf = [0u8; 8];
    unsafe {
        while libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 8) == 8 {}
    }
}

/// Get (initializing on first use) the worker state. Main thread only.
fn state() -> Result<Arc<State>, String> {
    let mut guard = STATE.lock().unwrap();
    if let Some(s) = guard.as_ref() {
        return Ok(s.clone());
    }
    let (read_fd, write_fd) = make_doorbell()?;
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Completion>();
    let (log_tx, log_rx) = crossbeam_channel::unbounded::<String>();
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let mut threads = Vec::new();
    for i in 0..runner_threads() {
        let stop = stop_rx.clone();
        let h = std::thread::Builder::new()
            .name(format!("tmux-plugin-w{i}"))
            .spawn(move || runner_main(stop))
            .map_err(|e| format!("spawn worker runner: {e}"))?;
        threads.push(h);
    }
    let s = Arc::new(State {
        done: done_tx,
        completions: done_rx,
        log_tx,
        log_rx,
        doorbell_write: write_fd,
        doorbell_read: read_fd,
        stop: stop_tx,
        threads: Mutex::new(threads),
    });
    *guard = Some(s.clone());
    Ok(s)
}

/// The pollable doorbell fd for the C side to wire into libevent
/// (creates the worker machinery on first call). -1 on failure.
pub fn notify_fd() -> RawFd {
    match state() {
        Ok(s) => s.doorbell_read,
        Err(e) => {
            crate::hostlog::error("host", &format!("worker: {e}"));
            -1
        }
    }
}

// ---------------------------------------------------------------------------
// Submission API.
// ---------------------------------------------------------------------------

/// An in-flight job's accounting handle. Dropping it decrements the
/// count and wakes anyone waiting in [`wait_for_instance`] or
/// [`shutdown`]. Drop it AFTER posting the completion, never before.
pub struct InFlight {
    key: InstKey,
    pinned: bool,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let inflight = inflight();
        let mut counts = inflight.counts.lock().unwrap();
        if self.pinned {
            if let Some(n) = counts.pinned.get_mut(&self.key) {
                *n = n.saturating_sub(1);
            }
        } else {
            counts.detached = counts.detached.saturating_sub(1);
        }
        inflight.cv.notify_all();
    }
}

/// Register one job before spawning it. Main thread only; creates the
/// worker machinery on first use. `pinned` means the job touches guest
/// memory: it is counted per instance and awaited by
/// [`wait_for_instance`]. A detached job (`pinned = false`) is counted
/// only by [`shutdown`].
///
/// The count is up before the task's first poll, so a wait cannot slip
/// past between the guest call and the spawn.
pub fn track(key: InstKey, pinned: bool) -> Result<InFlight, String> {
    state()?;
    let mut counts = inflight().counts.lock().unwrap();
    if pinned {
        *counts.pinned.entry(key.clone()).or_insert(0) += 1;
    } else {
        counts.detached += 1;
    }
    Ok(InFlight { key, pinned })
}

/// Spawn a task on the pool. The caller holds an [`InFlight`] guard for
/// it (moved into the future), or the task is invisible to shutdown.
pub fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    executor().spawn(fut).detach();
}

/// Post a completion to the main thread and ring the doorbell on the
/// empty→non-empty edge only (coalescing). Callable from any thread.
/// After [`shutdown`] has begun the completion is dropped: nothing will
/// deliver it anyway.
pub fn post(c: Completion) {
    let s = { STATE.lock().unwrap().clone() };
    let Some(s) = s else { return };
    let _ = s.done.send(c);
    if !ARMED.swap(true, Ordering::SeqCst) {
        ring(s.doorbell_write);
    }
}

/// Log one line from a worker thread. It reaches the main-thread host log
/// (readable with `plugin-log`) on the next drain. Rings the doorbell on
/// the empty->non-empty edge so a log with no completion still wakes the
/// main thread.
pub fn worker_log(msg: String) {
    let s = { STATE.lock().unwrap().clone() };
    let Some(s) = s else { return };
    let _ = s.log_tx.send(msg);
    if !ARMED.swap(true, Ordering::SeqCst) {
        ring(s.doorbell_write);
    }
}

pub fn err_completion(token: u64, code: ErrorCode, msg: String) -> Completion {
    Completion {
        token,
        err: code.as_num(),
        v0: 0,
        v1: 0,
        data: msg.into_bytes(),
    }
}

// ---------------------------------------------------------------------------
// Main-thread side.
// ---------------------------------------------------------------------------

/// Main-thread drain (pgh_fs_drain): clear the armed flag and read the
/// doorbell BEFORE draining the queue, so a worker push during the drain
/// re-arms and re-rings; then move every completion onto the plugin
/// delivery queue (same path as pgh_async_complete).
pub fn drain() {
    let s = {
        let guard = STATE.lock().unwrap();
        match guard.as_ref() {
            Some(s) => s.clone(),
            None => return,
        }
    };
    ARMED.store(false, Ordering::SeqCst);
    drain_doorbell(s.doorbell_read);
    // Surface any worker-thread log lines on the main-thread host log.
    while let Ok(line) = s.log_rx.try_recv() {
        crate::hostlog::error("host", &line);
    }
    while let Ok(c) = s.completions.try_recv() {
        crate::state::EVENTS.with(|e| {
            e.borrow_mut().deliveries.push_back(
                crate::state::Delivery::AsyncComplete {
                    token: c.token,
                    err: c.err,
                    v0: c.v0,
                    v1: c.v1,
                    data: c.data,
                },
            );
        });
    }
}

/// Block until every pinned job of one instance has finished (its
/// pinned guest buffers are about to go away). Local file I/O, so this
/// is bounded; completions for the dead instance are dropped later by
/// the token generation check. Detached jobs are not waited for: they
/// hold no guest memory.
pub fn wait_for_instance(plugin: &str, scope: ScopeId, generation: u64) {
    let inflight = match INFLIGHT.get() {
        Some(i) => i,
        None => return,
    };
    let key: InstKey = (plugin.to_string(), scope, generation);
    let mut counts = inflight.counts.lock().unwrap();
    while counts.pinned.get(&key).copied().unwrap_or(0) > 0 {
        counts = inflight.cv.wait(counts).unwrap();
    }
    counts.pinned.remove(&key);
}

/// Join the pool (server shutdown). Called BEFORE instances drop so
/// in-flight pinned jobs still see live guest memory; every job, pinned
/// or detached, finishes first (their completions are simply never
/// delivered), then the runner threads exit.
pub fn shutdown() {
    let s = {
        let mut guard = STATE.lock().unwrap();
        guard.take()
    };
    let Some(s) = s else { return };
    // Wait until every in-flight job has completed (guest memory is
    // still mapped at this point, and every SQLite connection a task
    // holds is released by the time its guard drops).
    if let Some(inflight) = INFLIGHT.get() {
        let mut counts = inflight.counts.lock().unwrap();
        while counts.detached > 0 || counts.pinned.values().any(|&v| v > 0) {
            counts = inflight.cv.wait(counts).unwrap();
        }
        counts.pinned.clear();
    }
    let handles: Vec<JoinHandle<()>> =
        s.threads.lock().unwrap().drain(..).collect();
    let (rfd, wfd) = (s.doorbell_read, s.doorbell_write);
    // Every runner's `run` future is awaiting this channel; closing it
    // completes them, and `run` returns once it has no task to poll.
    // Safe here because the wait above already drained every job.
    s.stop.close();
    drop(s);
    for handle in handles {
        let _ = handle.join();
    }
    unsafe {
        libc::close(rfd);
        if wfd != rfd {
            libc::close(wfd);
        }
    }
    ARMED.store(false, Ordering::SeqCst);
}

/// One runner thread: drive the executor until the stop channel closes.
///
/// A task panic unwinds out of `executor().run`. Catch it so the panic
/// does NOT kill the thread: log it and re-enter `run`, keeping the pool
/// alive. A thread that died here was never replaced, so one panicking
/// task used to remove a pool thread for good; enough of them emptied the
/// pool and every later async db/fs call hung with no thread to poll it.
fn runner_main(stop: async_channel::Receiver<()>) {
    loop {
        let stop = stop.clone();
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures_lite::future::block_on(executor().run(async move {
                let _ = stop.recv().await;
            }));
        }));
        match res {
            // The stop channel closed: a clean exit.
            Ok(()) => return,
            // A task panicked; the panic hook already logged the detail.
            // Keep the thread and go back to serving the pool.
            Err(_) => {
                worker_log(
                    "worker task panicked; thread recovered, pool preserved"
                        .to_string(),
                );
            }
        }
    }
}
