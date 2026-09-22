//! The async fs jobs: blocking file I/O run on the shared worker pool
//! (see `worker`), so the tmux event loop never stalls on a filesystem.
//!
//! Zero-copy: write jobs read the guest's pinned buffer directly and
//! read jobs write into the guest's pinned out-buffer directly. This is
//! sound because (a) the engine pins linear memories (growth extends,
//! never moves), (b) the SDK future owns the buffer until the completion
//! arrives, and (c) instance teardown calls `worker::wait_for_instance`
//! before the Store (and its memory mapping) drops. Every fs job is
//! therefore tracked as PINNED work.
//!
//! Ordering contract: awaited ops are fully ordered on any worker count —
//! a completion means the worker's write() returned before the next
//! submit exists. Only multiple in-flight ops to the SAME file are
//! unordered; callers must not overlap those.
//!
//! Thread discipline: a job touches no tmux or host state — only
//! syscalls under the plugin's sandbox root descriptor, on pre-validated
//! buffers. All host bookkeeping happens on the main thread. The open
//! itself runs here, so a slow filesystem never stalls the event loop;
//! containment still holds, because resolution is confined beneath the
//! root descriptor the main thread resolved (see fsbox).

use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use tmux_plugin_abi::ErrorCode;

use crate::fsbox::{FsError, Reach, Root};
use crate::worker::{self, Completion, InstKey};

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
    /// Unlink `rel`, which must name a file (not a directory) that stays
    /// inside the sandbox. Idempotent is the caller's job: a missing file
    /// returns NotFound.
    Remove {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel: String,
        reach: Reach,
    },
    /// The lines of a file from an offset whose head holds a needle,
    /// packed into the guest's pinned buffer. See [`do_read_lines`].
    ReadLines {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel: String,
        offset: u64,
        reach: Reach,
        needles: Vec<Needle>,
        head: usize,
        max_line: usize,
        out: GuestSliceMut,
    },
    /// The conversation in an agent harness's transcript from `offset`,
    /// as a block of turns in the completion's data. See
    /// [`do_extract`] and `transcript.rs`.
    Extract {
        token: u64,
        key: InstKey,
        root: Arc<Root>,
        rel: String,
        offset: u64,
        reach: Reach,
        harness: crate::transcript::Harness,
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

/// Submit a job. Main thread only. The instance's pinned in-flight count
/// is up before the task exists, and comes down only after the
/// completion is posted.
pub fn submit(job: FsJob) -> Result<(), String> {
    let key = match &job {
        FsJob::Write { key, .. }
        | FsJob::Read { key, .. }
        | FsJob::ReadLines { key, .. }
        | FsJob::Extract { key, .. }
        | FsJob::List { key, .. }
        | FsJob::Rename { key, .. }
        | FsJob::Remove { key, .. } => key.clone(),
    };
    let guard = worker::track(key, true)?;
    worker::spawn(async move {
        let completion = run_job(job).await;
        worker::post(completion);
        // Decrement AFTER the completion is pushed: wait_for_instance
        // returning guarantees no job still touches guest memory.
        drop(guard);
    });
    Ok(())
}

fn err_completion(token: u64, code: ErrorCode, msg: String) -> Completion {
    worker::err_completion(token, code, msg)
}

async fn run_job(job: FsJob) -> Completion {
    match job {
        FsJob::Write { token, key: _, root, rel, append, reach, data } => {
            do_write(token, &root, &rel, append, reach, &data)
        }
        FsJob::Read { token, key: _, root, rel, offset, reach, out } => {
            do_read(token, &root, &rel, offset, reach, &out)
        }
        FsJob::ReadLines { token, key: _, root, rel, offset, reach, needles, head, max_line, out } => {
            do_read_lines(token, &root, &rel, offset, reach, &needles, head, max_line, &out)
        }
        FsJob::Extract { token, key: _, root, rel, offset, reach, harness } => {
            do_extract(token, &root, &rel, offset, reach, harness)
        }
        FsJob::List { token, key: _, root, rel, reach, flags, out } => {
            do_list(token, &root, &rel, reach, flags, out).await
        }
        FsJob::Rename { token, key: _, root, rel_from, rel_to, flags, reach } => {
            do_rename(token, &root, &rel_from, &rel_to, flags, reach)
        }
        FsJob::Remove { token, key: _, root, rel, reach } => {
            do_remove(token, &root, &rel, reach)
        }
    }
}

/// Turn a sandbox or open failure into a completion the guest can read.
fn open_failed(token: u64, e: FsError) -> Completion {
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
) -> Completion {
    let mut file = match crate::fsbox::open_write(root, rel, append, reach) {
        Ok(f) => f,
        Err(e) => return open_failed(token, e),
    };
    let bytes = unsafe { std::slice::from_raw_parts(data.ptr, data.len) };
    match file.write_all(bytes) {
        Ok(()) => Completion {
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
) -> Completion {
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
    Completion { token, err: 0, v0: 0, v1: 0, data: Vec::new() }
}

fn do_remove(
    token: u64,
    root: &Root,
    rel: &str,
    reach: Reach,
) -> Completion {
    let path = match crate::fsbox::resolve_entry(root, rel, reach) {
        Ok(p) => p,
        Err(e) => return open_failed(token, e),
    };
    if let Err(e) = std::fs::remove_file(&path) {
        return open_failed(token, crate::fsbox::io_err(rel, &e));
    }
    // Persist the directory entry's removal, so the file does not come
    // back after a crash right after the unlink.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Completion { token, err: 0, v0: 0, v1: 0, data: Vec::new() }
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
) -> Completion {
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

    Completion {
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
    worker::executor().spawn(async move {
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

// ---------------------------------------------------------------------------
// fs_read_lines: the lines of a file whose head holds a needle
// ---------------------------------------------------------------------------

/// One needle of a line filter: keep a line whose head holds a keep
/// needle, unless it also holds a reject needle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Needle {
    pub bytes: Vec<u8>,
    pub reject: bool,
}

/// The 16-byte header at the start of the out buffer:
/// `u64 cursor | u32 need | u8 eof | u8[3] pad`, little-endian.
pub const LINES_HEADER: usize = 16;
/// Each kept line: `u64 offset | u32 len | u8 line[len]` (newline included).
pub const LINES_REC_HEADER: usize = 12;
/// Bytes consumed per call at most (at a line boundary): bounds one
/// worker task, and lets the guest see progress on a huge file.
pub const LINES_SCAN_MAX: u64 = 8 * 1024 * 1024;
/// The read block.
const LINES_BLOCK: usize = 256 * 1024;

/// Where a scan stopped and why. `cursor` is the offset after the last
/// line consumed (kept or skipped); `need` is the record size of a kept
/// line that did not fit, or 0; `eof` says the file end was reached.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanEnd {
    pub cursor: u64,
    pub need: usize,
    pub eof: bool,
}

/// Scan `src` from `offset` line by line. A line is judged on its first
/// `head` bytes: kept if any keep needle is in them and no reject needle
/// is, else skipped - and a skipped line is never buffered past `head`,
/// however long it is. A kept line longer than `max_line` is skipped. Each
/// kept line goes to `sink` (offset, bytes with newline); a sink that
/// returns false has no room, and the scan stops before that line with
/// `need` set. A trailing line with no newline is not consumed. Stops at
/// `LINES_SCAN_MAX` consumed bytes.
pub fn scan_lines(
    src: &mut dyn std::io::Read,
    offset: u64,
    needles: &[Needle],
    head: usize,
    max_line: usize,
    sink: &mut dyn FnMut(u64, &[u8]) -> bool,
) -> std::io::Result<ScanEnd> {
    let keep: Vec<memchr::memmem::Finder<'_>> = needles
        .iter()
        .filter(|n| !n.reject)
        .map(|n| memchr::memmem::Finder::new(&n.bytes))
        .collect();
    let reject: Vec<memchr::memmem::Finder<'_>> = needles
        .iter()
        .filter(|n| n.reject)
        .map(|n| memchr::memmem::Finder::new(&n.bytes))
        .collect();
    let judge = |h: &[u8]| -> bool {
        keep.iter().any(|f| f.find(h).is_some()) && !reject.iter().any(|f| f.find(h).is_some())
    };
    let mut block = vec![0u8; LINES_BLOCK];
    // The current line: its start offset, the bytes held for it (all of
    // them while it is unjudged or a candidate, the head only once it is
    // known to be skipped), its length so far, and the verdict.
    let mut line_start = offset;
    let mut line: Vec<u8> = Vec::new();
    let mut line_len: usize = 0;
    let mut verdict: Option<bool> = None;
    let mut end = ScanEnd { cursor: offset, need: 0, eof: false };
    'outer: loop {
        let n = src.read(&mut block)?;
        if n == 0 {
            end.eof = true;
            break;
        }
        let mut seg_start = 0usize;
        for nl in memchr::memchr_iter(b'\n', &block[..n]) {
            let seg = &block[seg_start..nl];
            seg_start = nl + 1;
            let complete_len = line_len + seg.len() + 1;
            let kept = match verdict {
                Some(v) => v,
                None => {
                    // Unjudged: `line` holds every byte so far. Judge on
                    // the head, which is what we hold plus this segment.
                    let take = head.saturating_sub(line.len()).min(seg.len());
                    let mut h: Vec<u8> = Vec::with_capacity(head.min(line.len() + seg.len()));
                    h.extend_from_slice(&line);
                    h.extend_from_slice(&seg[..take]);
                    judge(&h)
                }
            };
            if kept {
                line.extend_from_slice(seg);
                line.push(b'\n');
                if line.len() <= max_line && !sink(line_start, &line) {
                    end.need = LINES_REC_HEADER + line.len();
                    break 'outer;
                }
            }
            line_start += complete_len as u64;
            end.cursor = line_start;
            line.clear();
            line_len = 0;
            verdict = None;
            if end.cursor - offset >= LINES_SCAN_MAX {
                break 'outer;
            }
        }
        // The tail of the block: part of a line still open.
        let tail = &block[seg_start..n];
        if !tail.is_empty() {
            match verdict {
                None => {
                    // Hold everything until the head is complete, then
                    // judge; a skipped line keeps nothing from here on.
                    line.extend_from_slice(tail);
                    line_len += tail.len();
                    if line.len() >= head {
                        let v = judge(&line[..head]);
                        verdict = Some(v);
                        if !v {
                            line.clear();
                        }
                    }
                }
                Some(true) => {
                    line.extend_from_slice(tail);
                    line_len += tail.len();
                    if line.len() > max_line {
                        // Too long to keep: from here on it is skipped.
                        verdict = Some(false);
                        line.clear();
                    }
                }
                Some(false) => {
                    line_len += tail.len();
                }
            }
        }
    }
    Ok(end)
}

/// `fs_read_lines`: scan the file from `offset` and pack the lines that
/// pass the needle filter into the guest's buffer, after a 16-byte
/// header. v0 = bytes written, v1 = lines kept.
#[allow(clippy::too_many_arguments)]
fn do_read_lines(
    token: u64,
    root: &Root,
    rel: &str,
    offset: u64,
    reach: Reach,
    needles: &[Needle],
    head: usize,
    max_line: usize,
    out: &GuestSliceMut,
) -> Completion {
    let mut file = match crate::fsbox::open_read(root, rel, reach) {
        Ok(f) => f,
        Err(e) => return open_failed(token, e),
    };
    if out.cap < LINES_HEADER {
        return err_completion(token, ErrorCode::BadRequest, "fs_read_lines: buffer too small".into());
    }
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)) {
        return err_completion(token, ErrorCode::Host, format!("{rel}: {e}"));
    }
    let dst = unsafe { std::slice::from_raw_parts_mut(out.ptr, out.cap) };
    let mut used = LINES_HEADER;
    let mut count: u32 = 0;
    let mut sink = |off: u64, line: &[u8]| -> bool {
        let rec = LINES_REC_HEADER + line.len();
        if used + rec > dst.len() {
            return false;
        }
        dst[used..used + 8].copy_from_slice(&off.to_le_bytes());
        dst[used + 8..used + 12].copy_from_slice(&(line.len() as u32).to_le_bytes());
        dst[used + 12..used + rec].copy_from_slice(line);
        used += rec;
        count += 1;
        true
    };
    let end = match scan_lines(&mut file, offset, needles, head, max_line, &mut sink) {
        Ok(e) => e,
        Err(e) => return err_completion(token, ErrorCode::Host, format!("{rel}: {e}")),
    };
    dst[0..8].copy_from_slice(&end.cursor.to_le_bytes());
    dst[8..12].copy_from_slice(&(end.need as u32).to_le_bytes());
    dst[12] = u8::from(end.eof);
    dst[13..16].copy_from_slice(&[0, 0, 0]);
    Completion { token, err: 0, v0: used as i64, v1: i64::from(count), data: Vec::new() }
}

/// `transcript_extract`: scan the transcript from `offset` with the
/// harness's needles, parse the records that pass, and return their
/// turns as one block (see `transcript.rs` for the format). v0 = the
/// cursor to resume from, v1 = eof. A line's turns are all in or all
/// out; the block stops growing past `BLOCK_MAX`, except that the first
/// line always fits, so a huge record cannot stall the cursor.
fn do_extract(
    token: u64,
    root: &Root,
    rel: &str,
    offset: u64,
    reach: Reach,
    harness: crate::transcript::Harness,
) -> Completion {
    use crate::transcript as tx;
    let mut file = match crate::fsbox::open_read(root, rel, reach) {
        Ok(f) => f,
        Err(e) => return open_failed(token, e),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)) {
        return err_completion(token, ErrorCode::Host, format!("{rel}: {e}"));
    }
    let needles = harness.needles();
    let mut body: Vec<u8> = Vec::new();
    let mut version: Option<String> = None;
    let mut lines_kept = 0usize;
    let mut sink = |off: u64, line: &[u8]| -> bool {
        let mut fed = tx::Fed::default();
        tx::extract_line(harness, line, off, &mut fed);
        if version.is_none() {
            version = fed.version.take();
        }
        let size: usize = fed.turns.iter().map(tx::turn_size).sum();
        if lines_kept > 0 && body.len() + size > tx::BLOCK_MAX {
            return false;
        }
        for t in &fed.turns {
            tx::put_turn(&mut body, t);
        }
        lines_kept += 1;
        true
    };
    let end = match scan_lines(&mut file, offset, &needles, tx::HEAD, tx::MAX_LINE, &mut sink) {
        Ok(e) => e,
        Err(e) => return err_completion(token, ErrorCode::Host, format!("{rel}: {e}")),
    };
    let mut data = Vec::with_capacity(body.len() + 64);
    tx::block_header(&mut data, version.as_deref());
    data.extend_from_slice(&body);
    Completion { token, err: 0, v0: end.cursor as i64, v1: i64::from(end.eof), data }
}

#[cfg(test)]
mod lines_tests {
    use super::*;

    fn n(b: &str, reject: bool) -> Needle {
        Needle { bytes: b.as_bytes().to_vec(), reject }
    }

    fn run(
        data: &[u8],
        offset: u64,
        needles: &[Needle],
        head: usize,
        max_line: usize,
        room: usize,
    ) -> (Vec<(u64, Vec<u8>)>, ScanEnd) {
        let mut src = std::io::Cursor::new(data.to_vec());
        src.set_position(offset);
        let mut got = Vec::new();
        let mut used = 0usize;
        let mut sink = |off: u64, line: &[u8]| -> bool {
            if used + LINES_REC_HEADER + line.len() > room {
                return false;
            }
            used += LINES_REC_HEADER + line.len();
            got.push((off, line.to_vec()));
            true
        };
        let end = scan_lines(&mut src, offset, needles, head, max_line, &mut sink).unwrap();
        (got, end)
    }

    #[test]
    fn keeps_matching_lines_with_offsets() {
        let data = b"{\"type\":\"user\",\"x\":1}\n{\"type\":\"snapshot\"}\n{\"type\":\"assistant\"}\n";
        let needles = [n("\"type\":\"user\"", false), n("\"type\":\"assistant\"", false)];
        let (got, end) = run(data, 0, &needles, 512, 1 << 20, 1 << 20);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (0, b"{\"type\":\"user\",\"x\":1}\n".to_vec()));
        assert_eq!(got[1].0, 22 + 20);
        assert_eq!(end, ScanEnd { cursor: data.len() as u64, need: 0, eof: true });
    }

    #[test]
    fn reject_needle_and_partial_tail() {
        let data = b"{\"type\":\"user\",\"c\":[{\"type\":\"tool_result\"}]}\n{\"type\":\"user\",\"c\":\"hi\"}\n{\"type\":\"user\",\"c\":\"half";
        let needles = [n("\"type\":\"user\"", false), n("tool_result", true)];
        let (got, end) = run(data, 0, &needles, 512, 1 << 20, 1 << 20);
        assert_eq!(got.len(), 1);
        assert!(got[0].1.starts_with(b"{\"type\":\"user\",\"c\":\"hi\""));
        // The half line is not consumed: the cursor stops before it.
        let second_end = data.iter().rposition(|&b| b == b'\n').unwrap() as u64 + 1;
        assert_eq!(end.cursor, second_end);
        assert!(end.eof);
    }

    #[test]
    fn long_skipped_line_is_not_buffered_and_long_kept_line_is_dropped() {
        // A 3 MB non-candidate line, then a candidate, then a 1 MB
        // candidate over max_line.
        let mut data = Vec::new();
        data.extend_from_slice(b"{\"type\":\"other\",\"blob\":\"");
        data.extend(std::iter::repeat(b'x').take(3 * 1024 * 1024));
        data.extend_from_slice(b"\"}\n{\"type\":\"user\",\"c\":1}\n{\"type\":\"user\",\"c\":\"");
        data.extend(std::iter::repeat(b'y').take(1024 * 1024));
        data.extend_from_slice(b"\"}\n");
        let needles = [n("\"type\":\"user\"", false)];
        let (got, end) = run(&data, 0, &needles, 512, 64 * 1024, 1 << 24);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"{\"type\":\"user\",\"c\":1}\n".to_vec());
        assert_eq!(end.cursor, data.len() as u64);
    }

    #[test]
    fn stops_when_the_buffer_is_full_and_reports_need() {
        let data = b"{\"type\":\"user\",\"c\":1}\n{\"type\":\"user\",\"c\":2}\n{\"type\":\"user\",\"c\":3}\n";
        let needles = [n("\"type\":\"user\"", false)];
        let line = 22usize;
        let (got, end) = run(data, 0, &needles, 512, 1 << 20, 2 * (LINES_REC_HEADER + line) + 5);
        assert_eq!(got.len(), 2);
        assert_eq!(end.cursor, (2 * line) as u64);
        assert_eq!(end.need, LINES_REC_HEADER + line);
        assert!(!end.eof);
        // Resume from the cursor: the third line comes.
        let (got2, end2) = run(data, end.cursor, &needles, 512, 1 << 20, 1 << 20);
        assert_eq!(got2.len(), 1);
        assert_eq!(got2[0].0, (2 * line) as u64);
        assert!(end2.eof);
    }

    #[test]
    fn candidate_spanning_blocks_is_kept_whole() {
        // A candidate line longer than the read block.
        let mut data = Vec::new();
        data.extend_from_slice(b"{\"type\":\"user\",\"c\":\"");
        data.extend(std::iter::repeat(b'z').take(LINES_BLOCK + 1000));
        data.extend_from_slice(b"\"}\n");
        let needles = [n("\"type\":\"user\"", false)];
        let (got, end) = run(&data, 0, &needles, 512, 1 << 24, 1 << 24);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, data);
        assert!(end.eof);
    }
}

fn do_read(
    token: u64,
    root: &Root,
    rel: &str,
    offset: u64,
    reach: Reach,
    out: &GuestSliceMut,
) -> Completion {
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
        Ok((read, eof)) => Completion {
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
