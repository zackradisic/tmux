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
