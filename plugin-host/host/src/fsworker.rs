//! The async fs worker: one background thread doing blocking file I/O so
//! the tmux event loop never stalls on a filesystem.
//!
//! Architecture (see ABI.md): submission = a crossbeam MPMC channel (a
//! future pool is `for _ in 0..N` around the spawn — nothing else
//! changes); completion = a crossbeam channel back to the main thread
//! plus an **eventfd doorbell**, because the main thread sleeps in epoll
//! inside libevent and a queue alone cannot wake it. The doorbell is
//! coalesced with an atomic armed flag (ring on empty→non-empty); the
//! main-thread drain clears the flag and reads the eventfd BEFORE
//! draining the queue (the lost-wakeup rule).
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
use std::os::fd::RawFd;
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
    submit: crossbeam_channel::Sender<FsJob>,
    completions: crossbeam_channel::Receiver<FsCompletion>,
    doorbell_write: RawFd,
    doorbell_read: RawFd,
    worker: Mutex<Option<JoinHandle<()>>>,
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
    let (submit_tx, submit_rx) = crossbeam_channel::unbounded::<FsJob>();
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<FsCompletion>();
    let inflight = inflight();
    let worker = std::thread::Builder::new()
        .name("tmux-plugin-fs".into())
        .spawn(move || worker_main(submit_rx, done_tx, write_fd, inflight))
        .map_err(|e| format!("spawn fs worker: {e}"))?;
    let s = std::sync::Arc::new(State {
        submit: submit_tx,
        completions: done_rx,
        doorbell_write: write_fd,
        doorbell_read: read_fd,
        worker: Mutex::new(Some(worker)),
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
        | FsJob::List { key, .. } => key.clone(),
    };
    {
        let mut counts = inflight().counts.lock().unwrap();
        *counts.entry(key).or_insert(0) += 1;
    }
    s.submit
        .send(job)
        .map_err(|_| "fs worker is gone".to_string())
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
    let handle = s.worker.lock().unwrap().take();
    let (rfd, wfd) = (s.doorbell_read, s.doorbell_write);
    // Dropping the last Arc drops the submit sender; the worker's recv
    // disconnects and it exits.
    drop(s);
    if let Some(handle) = handle {
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

fn worker_main(
    jobs: crossbeam_channel::Receiver<FsJob>,
    done: crossbeam_channel::Sender<FsCompletion>,
    doorbell: RawFd,
    inflight: &'static Inflight,
) {
    while let Ok(job) = jobs.recv() {
        match job {
            FsJob::Write { token, key, root, rel, append, reach, data } => {
                let completion =
                    do_write(token, &root, &rel, append, reach, &data);
                finish(&done, doorbell, inflight, key, completion);
            }
            FsJob::Read { token, key, root, rel, offset, reach, out } => {
                let completion =
                    do_read(token, &root, &rel, offset, reach, &out);
                finish(&done, doorbell, inflight, key, completion);
            }
            FsJob::List { token, key, root, rel, reach, flags, out } => {
                let completion = do_list(token, &root, &rel, reach, flags, &out);
                finish(&done, doorbell, inflight, key, completion);
            }
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
fn do_list(
    token: u64,
    root: &Root,
    rel: &str,
    reach: Reach,
    flags: u32,
    out: &GuestSliceMut,
) -> FsCompletion {
    let dir = match crate::fsbox::open_dir(root, rel, reach) {
        Ok(d) => d,
        Err(e) => return open_failed(token, e),
    };
    let dst = unsafe { std::slice::from_raw_parts_mut(out.ptr, out.cap) };
    let mut off = 0usize;
    let mut total = 0u64;

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

        // The only syscall per entry, and only when asked. metadata()
        // is fstatat against the directory's own descriptor, so no path
        // is rebuilt.
        let mtime = if (flags & LIST_MTIME) != 0 {
            entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0i64, |d| d.as_secs() as i64)
        } else {
            0
        };

        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.len() > u16::MAX as usize {
            continue;
        }
        let need = LIST_REC_HEADER + bytes.len();
        if off + need > dst.len() {
            continue; // full, but keep counting for an honest total
        }
        dst[off..off + 2].copy_from_slice(&(bytes.len() as u16).to_le_bytes());
        dst[off + 2] = kind;
        dst[off + 3] = 0;
        dst[off + 4..off + 12].copy_from_slice(&mtime.to_le_bytes());
        dst[off + LIST_REC_HEADER..off + need].copy_from_slice(bytes);
        off += need;
    }

    FsCompletion {
        token,
        err: 0,
        v0: off as i64,
        v1: total as i64,
        data: Vec::new(),
    }
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
