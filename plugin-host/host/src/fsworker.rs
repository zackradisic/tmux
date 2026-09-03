//! The async fs side: a small executor doing blocking file I/O off the
//! tmux event loop, so the loop never stalls on a filesystem.
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
//! a thread on another task. Blocking on real work (a read, a statx) is
//! exactly what these threads are for.
//!
//! Zero-copy: write jobs read the guest's pinned buffer directly and
//! read jobs write into the guest's pinned out-buffer directly. This is
//! sound because (a) the engine pins linear memories (growth extends,
//! never moves), (b) the SDK future owns the buffer until the completion
//! arrives, and (c) instance teardown calls [`wait_for_instance`] before
//! the Store (and its memory mapping) drops.
//!
//! Ordering contract: awaited ops are fully ordered on any worker count —
//! a completion means the worker's write() returned before the next
//! submit exists. Only multiple in-flight ops to the SAME file are
//! unordered; callers must not overlap those.
//!
//! Thread discipline: the worker touches no tmux or host state — only
//! syscalls under the plugin's sandbox root descriptor, on pre-validated
//! buffers. All host bookkeeping happens on the main thread. The open
//! itself runs here, so a slow filesystem never stalls the event loop;
//! containment still holds, because resolution is confined beneath the
//! root descriptor the main thread resolved (see fsbox).

use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

use tmux_plugin_abi::ErrorCode;

use crate::fsbox::{FsError, Reach, Root};
use crate::registry::ScopeId;

/// The instance a job belongs to, for teardown coordination.
pub type InstKey = (String, ScopeId, u64);

/// A pinned guest input buffer (fs_write data).
pub struct GuestSlice {
    pub ptr: *const u8,
    pub len: usize,
}
// The pointer targets pinned wasm linear memory kept alive until
// wait_for_instance returns; see the module docs.
unsafe impl Send for GuestSlice {}

/// A pinned guest output buffer (fs_read destination).
pub struct GuestSliceMut {
    pub ptr: *mut u8,
    pub cap: usize,
}
unsafe impl Send for GuestSliceMut {}

pub enum FsJob {
    Write {
        token: u64,
        key: InstKey,
        /// The plugin's sandbox root. The worker opens through it, so the
        /// path walk happens here rather than on the event loop, and the
        /// kernel still confines resolution beneath this descriptor.
        root: Arc<Root>,
        rel: String,
        append: bool,
        reach: Reach,
        data: GuestSlice,
    },
    Read {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel: String,
        offset: u64,
        reach: Reach,
        out: GuestSliceMut,
    },
    /// Rename `rel_from` to `rel_to`, both under the same root. The
    /// worker syncs the source's data first, so the name never moves
    /// ahead of the bytes it publishes.
    Rename {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel_from: String,
        rel_to: String,
        /// [`RENAME_REPLACE`], [`RENAME_NOREPLACE`] or [`RENAME_EXCHANGE`].
        flags: u32,
        reach: Reach,
    },
    /// List a directory, packing the entries straight into the guest's
    /// pinned buffer. See [`do_list`] for the record format.
    List {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel: String,
        reach: Reach,
        flags: u32,
        out: GuestSliceMut,
    },
}

/// A finished job, handed to the main thread's drain. Same shape as
/// pgh_async_complete.
pub struct FsCompletion {
    pub token: u64,
    pub err: i32,
    pub v0: i64,
    pub v1: i64,
    pub data: Vec<u8>,
}

struct Inflight {
    counts: Mutex<HashMap<InstKey, usize>>,
    cv: Condvar,
}

struct State {
    /// Handed to every task so it can post its completion.
    done: crossbeam_channel::Sender<FsCompletion>,
    completions: crossbeam_channel::Receiver<FsCompletion>,
    doorbell_write: RawFd,
    doorbell_read: RawFd,
    /// Closing this ends every runner's `run` future.
    stop: async_channel::Sender<()>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// The executor every fs task runs on. A `static` rather than an `Arc`
/// so a task can hold `&'static Executor` and spawn children without a
/// reference cycle back into the executor that owns it.
static EXECUTOR: OnceLock<async_executor::Executor<'static>> =
    OnceLock::new();

fn executor() -> &'static async_executor::Executor<'static> {
    EXECUTOR.get_or_init(async_executor::Executor::new)
}

/// Runner threads. Six is where the measured curve flattens: a
/// ten-thousand-entry listing with times takes 4.4ms on four threads,
/// 3.27ms on six, and no better on eight or twelve. They park on the
/// executor when there is nothing to do, which in a tmux server is
/// almost always.
fn runner_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 6)
}

/// Doorbell coalescing (the kprotty/Bun "notified" bit): workers
/// fetch_or(true) and ring only on the false→true edge.
static ARMED: AtomicBool = AtomicBool::new(false);
static INFLIGHT: OnceLock<Inflight> = OnceLock::new();
static STATE: Mutex<Option<std::sync::Arc<State>>> = Mutex::new(None);

fn inflight() -> &'static Inflight {
    INFLIGHT.get_or_init(|| Inflight {
        counts: Mutex::new(HashMap::new()),
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
fn state() -> Result<std::sync::Arc<State>, String> {
    let mut guard = STATE.lock().unwrap();
    if let Some(s) = guard.as_ref() {
        return Ok(s.clone());
    }
    let (read_fd, write_fd) = make_doorbell()?;
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<FsCompletion>();
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let mut threads = Vec::new();
    for i in 0..runner_threads() {
        let stop = stop_rx.clone();
        let h = std::thread::Builder::new()
            .name(format!("tmux-plugin-fs{i}"))
            .spawn(move || runner_main(stop))
            .map_err(|e| format!("spawn fs runner: {e}"))?;
        threads.push(h);
    }
    let s = std::sync::Arc::new(State {
        done: done_tx,
        completions: done_rx,
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
            crate::hostlog::error("host", &format!("fs worker: {e}"));
            -1
        }
    }
}

/// Submit a job. Main thread only. Increments the instance's in-flight
/// count before handing the job over.
pub fn submit(job: FsJob) -> Result<(), String> {
    let s = state()?;
    let key = match &job {
        FsJob::Write { key, .. }
        | FsJob::Read { key, .. }
        | FsJob::List { key, .. }
        | FsJob::Rename { key, .. } => key.clone(),
    };
    {
        let mut counts = inflight().counts.lock().unwrap();
        *counts.entry(key.clone()).or_insert(0) += 1;
    }
    let done = s.done.clone();
    let doorbell = s.doorbell_write;
    let inflight = inflight();
    // The count is already up, so wait_for_instance cannot slip past
    // between here and the task's first poll.
    executor()
        .spawn(async move {
            let completion = run_job(job).await;
            finish(&done, doorbell, inflight, key, completion);
        })
        .detach();
    Ok(())
}

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

/// Block until every in-flight job of one instance has finished (its
/// pinned guest buffers are about to go away). Local file I/O, so this
/// is bounded; completions for the dead instance are dropped later by
/// the token generation check.
pub fn wait_for_instance(plugin: &str, scope: ScopeId, generation: u64) {
    let inflight = match INFLIGHT.get() {
        Some(i) => i,
        None => return,
    };
    let key: InstKey = (plugin.to_string(), scope, generation);
    let mut counts = inflight.counts.lock().unwrap();
    while counts.get(&key).copied().unwrap_or(0) > 0 {
        counts = inflight.cv.wait(counts).unwrap();
    }
    counts.remove(&key);
}

/// Join the worker (server shutdown). Called BEFORE instances drop so
/// in-flight jobs still see live guest memory; pending jobs finish first
/// (channel FIFO), their completions are simply never delivered.
pub fn shutdown() {
    let s = {
        let mut guard = STATE.lock().unwrap();
        guard.take()
    };
    let Some(s) = s else { return };
    // Wait until every in-flight job has completed (guest memory is
    // still mapped at this point).
    if let Some(inflight) = INFLIGHT.get() {
        let mut counts = inflight.counts.lock().unwrap();
        while counts.values().any(|&v| v > 0) {
            counts = inflight.cv.wait(counts).unwrap();
        }
        counts.clear();
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

fn finish(
    done: &crossbeam_channel::Sender<FsCompletion>,
    doorbell: RawFd,
    inflight: &Inflight,
    key: InstKey,
    completion: FsCompletion,
) {
    let _ = done.send(completion);
    // Ring on the empty→non-empty edge only (coalescing).
    if !ARMED.swap(true, Ordering::SeqCst) {
        ring(doorbell);
    }
    // Decrement AFTER the completion is pushed: wait_for_instance
    // returning guarantees no job still touches guest memory.
    let mut counts = inflight.counts.lock().unwrap();
    if let Some(n) = counts.get_mut(&key) {
        *n = n.saturating_sub(1);
    }
    inflight.cv.notify_all();
}

fn err_completion(token: u64, code: ErrorCode, msg: String) -> FsCompletion {
    FsCompletion {
        token,
        err: code.as_num(),
        v0: 0,
        v1: 0,
        data: msg.into_bytes(),
    }
}

/// One runner thread: drive the executor until the stop channel closes.
fn runner_main(stop: async_channel::Receiver<()>) {
    futures_lite::future::block_on(executor().run(async move {
        let _ = stop.recv().await;
    }));
}

async fn run_job(job: FsJob) -> FsCompletion {
    match job {
        FsJob::Write { token, key: _, root, rel, append, reach, data } => {
            do_write(token, &root, &rel, append, reach, &data)
        }
        FsJob::Read { token, key: _, root, rel, offset, reach, out } => {
            do_read(token, &root, &rel, offset, reach, &out)
        }
        FsJob::List { token, key: _, root, rel, reach, flags, out } => {
            do_list(token, &root, &rel, reach, flags, out).await
        }
        FsJob::Rename { token, key: _, root, rel_from, rel_to, flags, reach } => {
            do_rename(token, &root, &rel_from, &rel_to, flags, reach)
        }
    }
}

/// Turn a sandbox or open failure into a completion the guest can read.
fn open_failed(token: u64, e: FsError) -> FsCompletion {
    let code = e.code();
    err_completion(token, code, e.message())
}

fn do_write(
    token: u64,
    root: &Root,
    rel: &str,
    append: bool,
    reach: Reach,
    data: &GuestSlice,
) -> FsCompletion {
    let mut file = match crate::fsbox::open_write(root, rel, append, reach) {
        Ok(f) => f,
        Err(e) => return open_failed(token, e),
    };
    let bytes = unsafe { std::slice::from_raw_parts(data.ptr, data.len) };
    match file.write_all(bytes) {
        Ok(()) => FsCompletion {
            token,
            err: 0,
            v0: data.len as i64,
            v1: 0,
            data: Vec::new(),
        },
        Err(e) => {
            err_completion(token, ErrorCode::Host, format!("{rel}: {e}"))
        }
    }
}

/// `flags` wire values for [`do_rename`], renumbered so the wire form
/// does not depend on libc. `RENAME_EXCHANGE` swaps two existing names
/// atomically; plain replace is already atomic and is the right tool for
/// the publish-a-temp-file pattern.
pub const RENAME_REPLACE: u32 = 0;
pub const RENAME_NOREPLACE: u32 = 1;
pub const RENAME_EXCHANGE: u32 = 2;

fn do_rename(
    token: u64,
    root: &Root,
    rel_from: &str,
    rel_to: &str,
    flags: u32,
    reach: Reach,
) -> FsCompletion {
    let from = match crate::fsbox::resolve_entry(root, rel_from, reach) {
        Ok(p) => p,
        Err(e) => return open_failed(token, e),
    };
    let to = match crate::fsbox::resolve_entry(root, rel_to, reach) {
        Ok(p) => p,
        Err(e) => return open_failed(token, e),
    };
    // Durability before visibility: flush the source's data so a crash
    // right after the rename cannot publish a name whose bytes never
    // reached the disk. rename orders metadata, not data.
    match std::fs::File::open(&from) {
        Ok(f) => {
            if let Err(e) = f.sync_data() {
                return open_failed(token, crate::fsbox::io_err(rel_from, &e));
            }
        }
        Err(e) => {
            return open_failed(token, crate::fsbox::io_err(rel_from, &e))
        }
    }
    if let Err(e) = rename_syscall(&from, &to, flags) {
        return open_failed(
            token,
            crate::fsbox::io_err(&format!("{rel_from} -> {rel_to}"), &e),
        );
    }
    // Persist the directory entry too, so the new name survives a crash.
    if let Some(dir) = to.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    FsCompletion { token, err: 0, v0: 0, v1: 0, data: Vec::new() }
}

#[cfg(target_os = "linux")]
fn rename_syscall(
    from: &std::path::Path,
    to: &std::path::Path,
    flags: u32,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let raw = match flags {
        RENAME_NOREPLACE => libc::RENAME_NOREPLACE,
        RENAME_EXCHANGE => libc::RENAME_EXCHANGE,
        _ => 0,
    };
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            raw,
        )
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

#[cfg(not(target_os = "linux"))]
fn rename_syscall(
    from: &std::path::Path,
    to: &std::path::Path,
    flags: u32,
) -> std::io::Result<()> {
    if flags != RENAME_REPLACE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "rename flags need renameat2",
        ));
    }
    std::fs::rename(from, to)
}

/// Directory entry kinds, as they cross the ABI. These are `d_type`
/// values renumbered so the wire form does not depend on the platform.
const KIND_UNKNOWN: u8 = 0;
const KIND_DIR: u8 = 1;
const KIND_FILE: u8 = 2;
const KIND_SYMLINK: u8 = 3;
const KIND_OTHER: u8 = 4;

/// Bytes of record header before the name.
const LIST_REC_HEADER: usize = 12;

/// `flags` bits for [`do_list`].
pub const LIST_MTIME: u32 = 1 << 0;
pub const LIST_DIRS_ONLY: u32 = 1 << 1;

/// List a directory into the guest's pinned buffer.
///
/// Wire format, little-endian, no padding between records:
///
/// ```text
///   record: u16 namelen | u8 kind | u8 reserved | i64 mtime | u8 name[namelen]
/// ```
///
/// There is no buffer header: the counts ride back on the completion
/// (`v0` = bytes written, `v1` = entries the directory holds). A guest
/// that sees fewer entries than `v1` was truncated and may retry with a
/// bigger buffer.
///
/// The entries go straight from `readdir` into wasm linear memory - the
/// host keeps no copy of its own, and never allocates per entry. Reading
/// continues after the buffer fills so `v1` is the true total.
///
/// `mtime` is the seconds part of the modification time, and is zero
/// unless `LIST_MTIME` was asked for. It is the one field that is not
/// free: `d_type` rides along with the directory entry, but a time needs
/// an `fstatat` per name, measured here at 0.8us. Hence the flag - a
/// caller that only wants names never pays it. `LIST_DIRS_ONLY` narrows
/// the walk before that cost is spent, which matters in a directory of
/// ten thousand files and five subdirectories.
///
/// (A platform with a bulk metadata call - `getattrlistbulk` on macOS,
/// `NtQueryDirectoryFile` on Windows - can return names and times in one
/// syscall and should get its own backend here. Linux has no such call:
/// `getdents64` carries no timestamp, so per-entry `statx` is the floor.)
///
/// Order is whatever the filesystem returns, which on a hashed directory
/// index is neither creation nor name order. Sorting belongs to the
/// guest, which is going to rank and filter anyway.
/// How many entries one stat task takes. Small enough that a slow chunk
/// cannot stall the join for long, large enough that the dispatch is
/// noise: 40 tasks for a ten-thousand-entry directory costs about 80us
/// against 2.1ms of stats. Per-entry tasks would cost 970ns each, which
/// is more than the 552ns of work they carry - the mistake io_uring's
/// one-SQE-per-op interface forces on you, and the reason this is
/// chunked rather than mapped.
const STAT_CHUNK: usize = 250;

/// A pointer into the guest's pinned out-buffer, for a stat task.
///
/// Sound for the same reason `GuestSliceMut` is, plus one: the parent
/// awaits every task it spawns before it returns, and the completion
/// that decrements the in-flight count is posted after that. So the
/// buffer outlives the tasks, and `wait_for_instance` cannot return
/// while one is still writing.
struct StatBuf(*mut u8);
unsafe impl Send for StatBuf {}

/// The seconds part of a name's mtime, resolved against an open
/// directory. 0 if it cannot be read - a racing unlink must not fail a
/// listing.
unsafe fn stat_mtime(dirfd: RawFd, name: &[u8]) -> i64 {
    // NUL-terminate without allocating: directory names are short, and
    // this runs once per entry.
    let mut stack = [0u8; 256];
    let mut heap: Vec<u8> = Vec::new();
    let cname: *const libc::c_char = if name.len() < stack.len() {
        stack[..name.len()].copy_from_slice(name);
        stack[name.len()] = 0;
        stack.as_ptr() as *const libc::c_char
    } else {
        heap.reserve_exact(name.len() + 1);
        heap.extend_from_slice(name);
        heap.push(0);
        heap.as_ptr() as *const libc::c_char
    };

    #[cfg(target_os = "linux")]
    {
        // Ask for the one field we want, and tell the kernel not to sync
        // a network filesystem to answer it. Against DirEntry::metadata,
        // which fetches a full stat, this is worth about 9%.
        let mut st: libc::statx = std::mem::zeroed();
        let r = libc::statx(
            dirfd,
            cname,
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC,
            libc::STATX_MTIME,
            &mut st,
        );
        if r == 0 {
            st.stx_mtime.tv_sec
        } else {
            0
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut st: libc::stat = std::mem::zeroed();
        let r = libc::fstatat(dirfd, cname, &mut st, libc::AT_SYMLINK_NOFOLLOW);
        if r == 0 {
            st.st_mtime as i64
        } else {
            0
        }
    }
}

/// Fill in the mtime of every record named by `offs`, in place.
///
/// The names are already in the guest buffer, so nothing is copied to
/// get here and nothing is allocated per entry. Two tasks never name the
/// same record, so the 8-byte writes never overlap.
unsafe fn stat_range(dirfd: RawFd, base: *mut u8, offs: &[u32]) {
    for &o in offs {
        let rec = base.add(o as usize);
        let namelen = u16::from_le_bytes([*rec, *rec.add(1)]) as usize;
        let name = std::slice::from_raw_parts(rec.add(LIST_REC_HEADER), namelen);
        let mtime = stat_mtime(dirfd, name).to_le_bytes();
        std::ptr::copy_nonoverlapping(mtime.as_ptr(), rec.add(4), 8);
    }
}

/// Write one record's fixed part at `off`, with a zero mtime.
///
/// Raw rather than through a `&mut [u8]`: while this runs, stat tasks
/// hold pointers into records already written, and a live `&mut` slice
/// over the whole buffer would alias them.
///
/// The caller has checked `off + LIST_REC_HEADER + name.len() <= cap`.
unsafe fn write_record(base: *mut u8, off: usize, name: &[u8], kind: u8) {
    let rec = base.add(off);
    let nl = (name.len() as u16).to_le_bytes();
    std::ptr::copy_nonoverlapping(nl.as_ptr(), rec, 2);
    *rec.add(2) = kind;
    *rec.add(3) = 0;
    std::ptr::write_bytes(rec.add(4), 0, 8);
    std::ptr::copy_nonoverlapping(
        name.as_ptr(),
        rec.add(LIST_REC_HEADER),
        name.len(),
    );
}

async fn do_list(
    token: u64,
    root: &Root,
    rel: &str,
    reach: Reach,
    flags: u32,
    out: GuestSliceMut,
) -> FsCompletion {
    let (dir, dirfile) = match crate::fsbox::open_dir_full(root, rel, reach) {
        Ok(d) => d,
        Err(e) => return open_failed(token, e),
    };
    let want_mtime = (flags & LIST_MTIME) != 0;
    let fd = dirfile.as_raw_fd();
    let base = out.ptr;
    let cap = out.cap;
    let mut off = 0usize;
    let mut total = 0u64;
    // Records written but not yet timed. One allocation per batch, not
    // one per entry.
    let mut offsets: Vec<u32> = Vec::with_capacity(STAT_CHUNK);
    let mut tasks = Vec::new();

    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            // A racing unlink must not abort a listing.
            Err(_) => continue,
        };
        // file_type() reads d_type out of the entry; it only falls back
        // to a stat when the filesystem answered DT_UNKNOWN.
        let kind = match entry.file_type() {
            Ok(t) if t.is_dir() => KIND_DIR,
            Ok(t) if t.is_file() => KIND_FILE,
            Ok(t) if t.is_symlink() => KIND_SYMLINK,
            Ok(_) => KIND_OTHER,
            Err(_) => KIND_UNKNOWN,
        };
        if (flags & LIST_DIRS_ONLY) != 0 && kind != KIND_DIR {
            continue; // filtered before any stat is paid for
        }
        total += 1;

        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.len() > u16::MAX as usize {
            continue;
        }
        let need = LIST_REC_HEADER + bytes.len();
        if off + need > cap {
            continue; // full, but keep counting for an honest total
        }
        unsafe { write_record(base, off, bytes, kind) };
        if want_mtime {
            offsets.push(off as u32);
            // Hand this batch off the moment it is full and carry on
            // walking. The times for entries already read are then
            // fetched while the rest of the directory is still being
            // read, instead of after it: 4.79ms against 3.87ms over ten
            // thousand entries, for the same syscalls and the same
            // tasks, just started earlier.
            //
            // Safe against the walk because the regions are disjoint: a
            // task only touches records this loop has finished with,
            // and the loop only writes past them.
            if offsets.len() == STAT_CHUNK {
                tasks.push(spawn_stats(fd, base, &mut offsets));
            }
        }
        off += need;
    }

    if want_mtime && !offsets.is_empty() {
        if tasks.is_empty() {
            // The whole directory fits one batch. Do it here: a small
            // directory should never pay for a hand-off it cannot
            // amortise (200 entries cost 0.10ms across threads and
            // 0.22ms once the dispatch is counted).
            unsafe { stat_range(fd, base, &offsets) };
        } else {
            tasks.push(spawn_stats(fd, base, &mut offsets));
        }
    }
    // Awaited, not blocked on: this thread goes back to the executor and
    // runs whatever is ready, which is usually one of these very chunks.
    // Blocking here instead would let every runner fall asleep in a join
    // holding the thread its own chunks need.
    //
    // Every task is awaited, and nothing returns early in between.
    // Dropping a Task cancels it, and a chunk cancelled mid-poll would
    // still be writing into a buffer that `finish` is about to declare
    // free.
    for t in tasks {
        t.await;
    }

    FsCompletion {
        token,
        err: 0,
        v0: off as i64,
        v1: total as i64,
        data: Vec::new(),
    }
}

/// Spawn a stat task for the batch in `offsets`, leaving it empty.
fn spawn_stats(
    fd: RawFd,
    base: *mut u8,
    offsets: &mut Vec<u32>,
) -> async_executor::Task<()> {
    let offs = std::mem::replace(offsets, Vec::with_capacity(STAT_CHUNK));
    let buf = StatBuf(base);
    executor().spawn(async move {
        let buf = buf;
        // Nothing in stat_range can panic today. Contained anyway,
        // because the alternative is worse than the bug: a panic would
        // travel up the await in do_list, drop the sibling handles, and
        // cancel tasks that are still writing into guest memory.
        let _ = std::panic::catch_unwind(|| unsafe {
            stat_range(fd, buf.0, &offs)
        });
    })
}

fn do_read(
    token: u64,
    root: &Root,
    rel: &str,
    offset: u64,
    reach: Reach,
    out: &GuestSliceMut,
) -> FsCompletion {
    let mut file = match crate::fsbox::open_read(root, rel, reach) {
        Ok(f) => f,
        Err(e) => return open_failed(token, e),
    };
    let dst = unsafe { std::slice::from_raw_parts_mut(out.ptr, out.cap) };
    let result = (|| -> std::io::Result<(usize, bool)> {
        let size = file.metadata()?.len();
        file.seek(std::io::SeekFrom::Start(offset))?;
        let mut read = 0;
        while read < dst.len() {
            let n = file.read(&mut dst[read..])?;
            if n == 0 {
                break;
            }
            read += n;
        }
        let eof = offset.saturating_add(read as u64) >= size;
        Ok((read, eof))
    })();
    match result {
        Ok((read, eof)) => FsCompletion {
            token,
            err: 0,
            v0: read as i64,
            v1: i64::from(eof),
            data: Vec::new(),
        },
        Err(e) => {
            err_completion(token, ErrorCode::Host, format!("{rel}: {e}"))
        }
    }
}
