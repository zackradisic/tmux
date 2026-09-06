//! Per-plugin filesystem sandbox for the fs_* host calls.
//!
//! Every plugin gets one data directory
//! (`$XDG_DATA_HOME|~/.local/share` + `tmux/plugins/<plugin>/`), created
//! on first use and then cached as a [`Root`]: a directory descriptor plus
//! its canonical path. Paths crossing the ABI are RELATIVE to that root.
//!
//! Containment is enforced twice. First syntactically, on the string:
//! absolute paths and any `..` component are rejected, which costs no
//! syscalls and produces the error the plugin author wants to read. Then
//! by the OS, on the actual open:
//!
//! - **Linux 5.6+**: `openat2` with `RESOLVE_BENEATH` from the root
//!   descriptor. The kernel enforces containment *during* the path walk,
//!   so there is no check-then-use window, and a symlink leading out of
//!   the tree fails the walk instead of being followed.
//! - **Everywhere else** (macOS, OpenBSD, Linux < 5.6, or a sandbox that
//!   blocks `openat2`): canonicalize the target's parent and require it
//!   to stay under the canonical root, then open by path. This defeats
//!   symlinked *directories*, but the final component is opened as named,
//!   so a symlink planted there is still followed. Nothing the plugin can
//!   do creates one - there is no symlink host call - but it is a weaker
//!   guarantee than the Linux path.
//!
//! Resolution never re-derives the root: that work happens once per
//! plugin. [`root_for`] touches the thread-local cache and is main-thread
//! only; [`open_read`] and [`open_write`] take a `&Root` and are callable
//! from the fs worker thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use tmux_plugin_abi::ErrorCode;

/// A plugin's sandbox root. Shared with the fs worker through an `Arc`,
/// which keeps the descriptor alive even if the cache entry is dropped
/// while a job is in flight.
pub struct Root {
    /// Directory descriptor, the anchor for openat-based resolution.
    fd: OwnedFd,
    /// Canonical absolute path: fs_root's answer, and the base the
    /// portable resolver compares against.
    path: PathBuf,
}

impl Root {
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn dir(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// Why an fs operation failed, in terms the ABI can report.
#[derive(Debug)]
pub enum FsError {
    /// The guest asked for something malformed or outside the sandbox.
    BadPath(String),
    /// The file, or a directory on the way to it, does not exist.
    NotFound(String),
    /// Anything else the OS said.
    Io(String),
}

impl FsError {
    pub fn code(&self) -> ErrorCode {
        match self {
            FsError::BadPath(_) => ErrorCode::BadRequest,
            FsError::NotFound(_) => ErrorCode::NoSuchObject,
            FsError::Io(_) => ErrorCode::Host,
        }
    }

    pub fn message(self) -> String {
        match self {
            FsError::BadPath(m) | FsError::NotFound(m) | FsError::Io(m) => m,
        }
    }
}

/// Classify an OS error against the path it came from.
pub(crate) fn io_err(rel: &str, e: &std::io::Error) -> FsError {
    match e.raw_os_error() {
        // RESOLVE_BENEATH rejects an escape with EXDEV; ELOOP is a symlink
        // loop or a magic link. Both are the guest's fault, not the host's.
        Some(libc::EXDEV) => {
            FsError::BadPath(format!("path escapes the sandbox: {rel:?}"))
        }
        Some(libc::ELOOP) => {
            FsError::BadPath(format!("too many symbolic links: {rel:?}"))
        }
        _ if e.kind() == std::io::ErrorKind::NotFound => {
            FsError::NotFound(format!("{rel}: {e}"))
        }
        _ => FsError::Io(format!("{rel}: {e}")),
    }
}

// ---------------------------------------------------------------------------
// The root cache.
// ---------------------------------------------------------------------------

thread_local! {
    static ROOTS: RefCell<HashMap<String, Arc<Root>>> =
        RefCell::new(HashMap::new());
}

fn data_home() -> Result<PathBuf, String> {
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        if !x.is_empty() {
            return Ok(PathBuf::from(x));
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "neither XDG_DATA_HOME nor HOME is set".to_string())?;
    Ok(PathBuf::from(home).join(".local/share"))
}

/// Build (and create) a plugin's root from scratch. The per-call path is
/// [`root_for`]; this is the cache miss.
fn open_root(plugin: &str) -> Result<Root, String> {
    if plugin.is_empty()
        || plugin.contains('/')
        || plugin.contains('\\')
        || plugin == "."
        || plugin == ".."
    {
        return Err(format!("bad plugin name {plugin:?}"));
    }
    let path = data_home()?.join("tmux/plugins").join(plugin);
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    // Canonical from here on, so the portable resolver never re-derives it.
    let path = path
        .canonicalize()
        .map_err(|e| format!("canonicalize root: {e}"))?;
    let fd = OwnedFd::from(
        File::open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?,
    );
    Ok(Root { fd, path })
}

/// The plugin's sandbox root, resolved once and cached. Main thread only.
pub fn root_for(plugin: &str) -> Result<Arc<Root>, FsError> {
    ROOTS.with(|c| {
        if let Some(root) = c.borrow().get(plugin) {
            return Ok(Arc::clone(root));
        }
        let root = Arc::new(open_root(plugin).map_err(FsError::Io)?);
        c.borrow_mut().insert(plugin.to_string(), Arc::clone(&root));
        Ok(root)
    })
}

/// Drop a plugin's cached root (plugin unloaded). An in-flight fs job
/// keeps its own `Arc`, so this never invalidates work in progress.
pub fn forget(plugin: &str) {
    ROOTS.with(|c| {
        c.borrow_mut().remove(plugin);
    });
}

/// Forget every cached root (host shutdown), releasing the descriptors.
pub fn forget_all() {
    ROOTS.with(|c| c.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// Path validation: the syntactic half of containment.
// ---------------------------------------------------------------------------

/// Reject an empty path, and - at `Sandbox` reach - an absolute or
/// `..`-bearing one. The OS enforces containment again on the open; this
/// is here for the error message and to keep the check independent of
/// the platform.
///
/// At `Anywhere` reach both are allowed, because an absolute path
/// already reaches anywhere: refusing `../../etc/passwd` while allowing
/// `/etc/passwd` would be the same power with more typing.
fn check_rel(rel: &str, reach: Reach) -> Result<&Path, FsError> {
    if rel.is_empty() {
        return Err(FsError::BadPath("empty path".into()));
    }
    let path = Path::new(rel);
    if reach == Reach::Sandbox {
        if path.is_absolute() {
            return Err(FsError::BadPath(
                "absolute paths are not allowed".into(),
            ));
        }
        for comp in path.components() {
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(FsError::BadPath(format!(
                        "path escapes the sandbox: {rel:?}"
                    )))
                }
            }
        }
    }
    Ok(path)
}

/// The absolute path an `Anywhere` open acts on: an absolute path as
/// given, a relative one joined to the plugin's data directory.
fn anywhere_path(root: &Root, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.path.join(path)
    }
}

/// Expand a leading `~` in a manifest prefix to the server user's home.
fn expand_home(prefix: &str) -> PathBuf {
    if let Some(rest) = prefix.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    } else if prefix == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(prefix)
}

/// The canonical absolute path a read would act on, resolving symlinks
/// and `..`. Canonicalizes the deepest existing ancestor, so a not-yet-
/// existing leaf still yields a real answer.
fn canonical_target(root: &Root, rel: &str) -> PathBuf {
    let full = anywhere_path(root, Path::new(rel));
    if let Ok(c) = full.canonicalize() {
        return c;
    }
    if let (Some(parent), Some(name)) = (full.parent(), full.file_name()) {
        if let Ok(c) = parent.canonicalize() {
            return c.join(name);
        }
    }
    full
}

/// Enforce a scoped read grant: the target must resolve inside the
/// plugin's own sandbox, or under one of the manifest-named prefixes.
/// This backs `fs-read` with a `[caps.fs-read] paths` list, a middle
/// ground between the sandbox and the blanket `fs-read-any`.
pub fn allowed_read(
    root: &Root,
    rel: &str,
    prefixes: &[String],
) -> Result<(), FsError> {
    let target = canonical_target(root, rel);
    // The sandbox itself is always reachable.
    if target.starts_with(&root.path) {
        return Ok(());
    }
    for p in prefixes {
        let base = expand_home(p);
        let base = base.canonicalize().unwrap_or(base);
        if target.starts_with(&base) {
            return Ok(());
        }
    }
    Err(FsError::BadPath(format!(
        "path {rel:?} is not under an allowed fs-read prefix"
    )))
}

// ---------------------------------------------------------------------------
// Linux: openat2 with RESOLVE_BENEATH.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod beneath {
    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicU8, Ordering};

    /// `struct open_how` (linux/openat2.h). Not in the libc crate.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    /// RESOLVE_BENEATH: every component must stay under the dirfd. No
    /// absolute paths, no `..` escape, no symlink out of the tree - all
    /// enforced inside the kernel's walk, so there is no TOCTOU.
    const RESOLVE_BENEATH: u64 = 0x08;

    const UNKNOWN: u8 = 0;
    const PRESENT: u8 = 1;
    const MISSING: u8 = 2;

    /// Whether this kernel (or its seccomp policy) has openat2.
    static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

    /// `Some(result)` when openat2 ran; `None` when it is unavailable and
    /// the caller must use the portable resolver.
    pub fn openat2(
        dir: BorrowedFd<'_>,
        rel: &CStr,
        flags: i32,
        mode: u32,
    ) -> Option<std::io::Result<OwnedFd>> {
        let state = STATE.load(Ordering::Relaxed);
        if state == MISSING {
            return None;
        }
        let how = OpenHow {
            flags: flags as u64,
            mode: u64::from(mode),
            resolve: RESOLVE_BENEATH,
        };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir.as_raw_fd(),
                rel.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if rc >= 0 {
            if state == UNKNOWN {
                STATE.store(PRESENT, Ordering::Relaxed);
            }
            return Some(Ok(unsafe { OwnedFd::from_raw_fd(rc as i32) }));
        }
        let e = std::io::Error::last_os_error();
        // ENOSYS: kernel < 5.6, at any time. EPERM on the very first call
        // only: a seccomp policy that denies unknown syscalls (Docker's
        // default profile did this for years) is indistinguishable from a
        // real EPERM, so fall back and let the portable path report the
        // truth. Once openat2 has worked, EPERM means EPERM.
        let enosys = e.raw_os_error() == Some(libc::ENOSYS);
        let seccomp = state == UNKNOWN
            && e.raw_os_error() == Some(libc::EPERM);
        if enosys || seccomp {
            STATE.store(MISSING, Ordering::Relaxed);
            return None;
        }
        if state == UNKNOWN {
            STATE.store(PRESENT, Ordering::Relaxed);
        }
        Some(Err(e))
    }

    /// mkdirat for ONE component (no slashes, so it cannot escape), then
    /// descend into it with a BENEATH-resolved O_PATH descriptor.
    pub fn descend(
        dir: BorrowedFd<'_>,
        name: &CStr,
        create: bool,
    ) -> Option<std::io::Result<OwnedFd>> {
        if create {
            let rc =
                unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return Some(Err(e));
                }
            }
        }
        openat2(dir, name, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
    }

    pub fn in_use() -> bool {
        STATE.load(Ordering::Relaxed) != MISSING
    }
}

/// What the caller wants from the file. Each backend derives its own
/// flags from this, so no raw open flags cross module boundaries.
#[derive(Clone, Copy)]
enum Intent {
    Read,
    Write { append: bool },
}

/// How far a path may resolve.
///
/// `Sandbox` is the default and the only reach an ordinary plugin gets:
/// the path is relative, `..` is refused, and the OS enforces containment
/// during the walk.
///
/// `Anywhere` is granted by `fs-read-any` / `fs-write-any`. It behaves
/// like a process cwd: a relative path still resolves against the
/// plugin's data directory, but an absolute path means what it says and
/// `..` may walk out. Containment is deliberately not enforced, so
/// `RESOLVE_BENEATH` is dropped for these opens - the point is to leave.
#[derive(Clone, Copy, PartialEq)]
pub enum Reach {
    Sandbox,
    Anywhere,
}

impl Intent {
    fn creates(self) -> bool {
        matches!(self, Intent::Write { .. })
    }

    #[cfg(target_os = "linux")]
    fn flags(self) -> (i32, u32) {
        match self {
            Intent::Read => (libc::O_RDONLY | libc::O_CLOEXEC, 0),
            Intent::Write { append } => (
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_CLOEXEC
                    | if append { libc::O_APPEND } else { libc::O_TRUNC },
                0o666,
            ),
        }
    }
}

/// Open `rel` under `root` through openat2, or `None` if this platform or
/// kernel cannot, in which case the caller uses the portable resolver.
#[cfg(target_os = "linux")]
fn open_beneath(
    root: &Root,
    rel: &Path,
    name: &std::ffi::OsStr,
    intent: Intent,
) -> Option<std::io::Result<File>> {
    use std::ffi::CString;

    if !beneath::in_use() {
        return None;
    }
    let cstr = |s: &std::ffi::OsStr| {
        CString::new(s.as_encoded_bytes()).map_err(|_| {
            std::io::Error::from(std::io::ErrorKind::InvalidInput)
        })
    };
    // Walk the directory components one descriptor at a time, so every
    // mkdirat and every openat2 sees a single component and cannot be
    // steered by a symlink partway down.
    let mut held: Option<OwnedFd> = None;
    for comp in rel.parent().into_iter().flat_map(|p| p.components()) {
        let Component::Normal(part) = comp else { continue };
        let c = match cstr(part) {
            Ok(c) => c,
            Err(e) => return Some(Err(e)),
        };
        let dir = held.as_ref().map_or(root.dir(), |f| f.as_fd());
        match beneath::descend(dir, &c, intent.creates())? {
            Ok(fd) => held = Some(fd),
            Err(e) => return Some(Err(e)),
        }
    }
    let c = match cstr(name) {
        Ok(c) => c,
        Err(e) => return Some(Err(e)),
    };
    let (flags, mode) = intent.flags();
    let dir = held.as_ref().map_or(root.dir(), |f| f.as_fd());
    Some(beneath::openat2(dir, &c, flags, mode)?.map(File::from))
}

#[cfg(not(target_os = "linux"))]
fn open_beneath(
    _root: &Root,
    _rel: &Path,
    _name: &std::ffi::OsStr,
    _intent: Intent,
) -> Option<std::io::Result<File>> {
    None
}

// ---------------------------------------------------------------------------
// Portable fallback: canonicalize the parent, compare, open by path.
// ---------------------------------------------------------------------------

/// Resolve `rel` to an absolute path proven to sit under `root`. The root
/// is already canonical (cached), so this canonicalizes only the target's
/// parent - which is the root itself for a flat path.
fn resolve_portable(
    root: &Root,
    rel: &Path,
    create_dirs: bool,
) -> Result<PathBuf, FsError> {
    let full = root.path.join(rel);
    let parent = full
        .parent()
        .ok_or_else(|| FsError::BadPath("path has no parent".into()))?;
    let name = full
        .file_name()
        .ok_or_else(|| FsError::BadPath("path has no file name".into()))?;

    // A flat path's parent IS the cached canonical root: nothing to check.
    if parent == root.path {
        return Ok(root.path.join(name));
    }
    if create_dirs {
        std::fs::create_dir_all(parent).map_err(|e| {
            FsError::Io(format!("create {}: {e}", parent.display()))
        })?;
    }
    // Canonicalize the PARENT (the file itself may not exist yet) and
    // require it to stay under the root: a symlinked directory inside the
    // data dir cannot lead outside.
    let canon_parent = parent.canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FsError::NotFound(format!("no such directory {}", parent.display()))
        } else {
            FsError::Io(format!("{}: {e}", parent.display()))
        }
    })?;
    if !canon_parent.starts_with(&root.path) {
        return Err(FsError::BadPath(format!(
            "path escapes the sandbox: {}",
            rel.display()
        )));
    }
    Ok(canon_parent.join(name))
}

// ---------------------------------------------------------------------------
// The two entry points. `&Root` only, so the fs worker can call them.
// ---------------------------------------------------------------------------

/// `allow_beneath` exists so the tests can exercise the portable
/// resolver on a kernel that has openat2. Production callers pass true.
fn open_impl(
    root: &Root,
    rel: &str,
    intent: Intent,
    allow_beneath: bool,
    reach: Reach,
) -> Result<File, FsError> {
    let path = check_rel(rel, reach)?;
    let mut opts = std::fs::OpenOptions::new();
    match intent {
        Intent::Read => {
            opts.read(true);
        }
        Intent::Write { append } => {
            opts.write(true)
                .create(true)
                .append(append)
                .truncate(!append);
        }
    }

    // Escaping: no containment to enforce, so no beneath-walk and no
    // canonicalize-and-compare. Resolve like a process would and open.
    if reach == Reach::Anywhere {
        if path.file_name().is_none() {
            return Err(FsError::BadPath(format!(
                "path has no file name: {rel:?}"
            )));
        }
        let full = anywhere_path(root, path);
        if intent.creates() {
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io_err(rel, &e))?;
            }
        }
        return opts.open(&full).map_err(|e| io_err(rel, &e));
    }

    // A file open, unlike a listing, needs something to name.
    let name = path.file_name().ok_or_else(|| {
        FsError::BadPath(format!("path has no file name: {rel:?}"))
    })?;
    if allow_beneath {
        if let Some(result) = open_beneath(root, path, name, intent) {
            return result.map_err(|e| io_err(rel, &e));
        }
    }
    let full = resolve_portable(root, path, intent.creates())?;
    opts.open(&full).map_err(|e| io_err(rel, &e))
}

/// Open `rel` for reading. Fails if it does not exist.
pub fn open_read(root: &Root, rel: &str, reach: Reach) -> Result<File, FsError> {
    open_impl(root, rel, Intent::Read, true, reach)
}

/// Open `rel` for writing, creating it and any missing parents.
/// `append` adds to the end instead of truncating.
pub fn open_write(
    root: &Root,
    rel: &str,
    append: bool,
    reach: Reach,
) -> Result<File, FsError> {
    open_impl(root, rel, Intent::Write { append }, true, reach)
}

/// Open a directory for listing.
///
/// At `Sandbox` reach this goes through the same contained resolution as
/// a file open, so a plugin cannot list its way out. At `Anywhere` reach
/// it resolves like a process cwd. `read_dir` is used rather than a
/// hand-rolled `getdents64` loop: it is `readdir(3)` underneath on both
/// Linux and macOS, both of which already batch the syscall, and
/// `DirEntry::file_type` reads `d_type` out of the entry - no `stat` per
/// name unless the filesystem returns `DT_UNKNOWN`.
// The form without a descriptor. Kept for callers that only want names,
// and used by the tests below.
#[allow(dead_code)]
pub fn open_dir(
    root: &Root,
    rel: &str,
    reach: Reach,
) -> Result<std::fs::ReadDir, FsError> {
    Ok(open_dir_full(root, rel, reach)?.0)
}

/// As [`open_dir`], but also hands back an open descriptor for the
/// directory itself.
///
/// A caller that wants timestamps needs one: `DirEntry::metadata` builds
/// its own `fstatat` per entry and gives no way to run those in
/// parallel. With the descriptor in hand a worker can `statx` a name
/// against it directly, which is the same syscall the entry would have
/// made, minus the entry.
pub fn open_dir_full(
    root: &Root,
    rel: &str,
    reach: Reach,
) -> Result<(std::fs::ReadDir, std::fs::File), FsError> {
    let path = check_rel(rel, reach)?;
    let full = if reach == Reach::Anywhere {
        anywhere_path(root, path)
    } else {
        resolve_dir(root, path)?
    };
    let dir = std::fs::read_dir(&full).map_err(|e| io_err(rel, &e))?;
    // Reopened rather than derived from `dir`: std keeps the DIR* to
    // itself. Same resolved path, so this reaches the same directory.
    let fd = std::fs::File::open(&full).map_err(|e| io_err(rel, &e))?;
    Ok((dir, fd))
}

/// Resolve a rename endpoint: the parent directory must already exist
/// and stay inside the sandbox; the final name need not exist yet (it is
/// a rename source or target, and rename never follows a symlink on the
/// last component). At `Anywhere` reach the path resolves like a process
/// cwd.
pub fn resolve_entry(
    root: &Root,
    rel: &str,
    reach: Reach,
) -> Result<PathBuf, FsError> {
    let path = check_rel(rel, reach)?;
    if reach == Reach::Anywhere {
        return Ok(anywhere_path(root, path));
    }
    let name = path
        .file_name()
        .ok_or_else(|| {
            FsError::BadPath(format!("path has no file name: {rel:?}"))
        })?
        .to_owned();
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => resolve_dir(root, p)?,
        _ => root.path.clone(),
    };
    Ok(parent.join(name))
}

/// Resolve a contained directory path.
///
/// `resolve_portable` cannot serve here: it splits a path into parent and
/// name, which is meaningless for `"."` and wrong for a directory. A
/// directory must already exist, so canonicalize the whole thing and
/// require it to stay under the root. That rejects a symlinked directory
/// leading out of the data dir. As with the portable file resolver there
/// is a window between the check and the open, and as there it is not
/// reachable: a plugin has no way to create a symlink.
fn resolve_dir(root: &Root, rel: &Path) -> Result<PathBuf, FsError> {
    let full = root.path.join(rel);
    let canon = full.canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            FsError::NotFound(format!("no such directory {}", full.display()))
        } else {
            FsError::Io(format!("{}: {e}", full.display()))
        }
    })?;
    if !canon.starts_with(&root.path) {
        return Err(FsError::BadPath(format!(
            "path escapes the sandbox: {}",
            rel.display()
        )));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both resolvers, so the portable path (macOS, OpenBSD, Linux < 5.6)
    /// is covered even on a kernel that has openat2.
    const BACKENDS: [(&str, bool); 2] =
        [("openat2", true), ("portable", false)];

    fn read(root: &Root, rel: &str, beneath: bool) -> Result<File, FsError> {
        open_impl(root, rel, Intent::Read, beneath, Reach::Sandbox)
    }

    fn write(
        root: &Root,
        rel: &str,
        append: bool,
        beneath: bool,
    ) -> Result<File, FsError> {
        open_impl(root, rel, Intent::Write { append }, beneath, Reach::Sandbox)
    }

    /// A Root over a fresh temp directory, without touching XDG_DATA_HOME.
    fn root(tag: &str) -> (Root, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("pgh-fsbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.canonicalize().unwrap();
        let fd = OwnedFd::from(File::open(&path).unwrap());
        (Root { fd, path: path.clone() }, path)
    }

    fn outside(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("pgh-fsbox-out-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("secret"), b"leak").unwrap();
        dir
    }

    #[test]
    fn rejects_absolute_and_dotdot() {
        let (r, dir) = root("rej");
        for (name, beneath) in BACKENDS {
            for bad in ["/etc/passwd", "a/../../x", "..", "", "a/../b"] {
                let e = read(&r, bad, beneath)
                    .err()
                    .unwrap_or_else(|| panic!("{name}: {bad:?} accepted"));
                assert_eq!(e.code(), ErrorCode::BadRequest, "{name}: {bad:?}");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The escape hatch: with Anywhere reach an absolute path means what
    /// it says and `..` may leave, while Sandbox reach still refuses both.
    #[test]
    fn anywhere_reach_escapes_and_sandbox_does_not() {
        use std::io::Read as _;

        let (r, dir) = root("reach");
        let out = outside("reach");
        let abs = out.join("secret");
        let up = format!(
            "../{}/secret",
            out.file_name().unwrap().to_str().unwrap()
        );

        for bad in [abs.to_str().unwrap(), up.as_str()] {
            let e = open_impl(&r, bad, Intent::Read, true, Reach::Sandbox)
                .err()
                .unwrap_or_else(|| panic!("sandbox accepted {bad:?}"));
            assert_eq!(e.code(), ErrorCode::BadRequest, "{bad:?}");

            let mut got = String::new();
            open_impl(&r, bad, Intent::Read, true, Reach::Anywhere)
                .unwrap_or_else(|e| panic!("anywhere refused {bad:?}: {e:?}"))
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "leak", "{bad:?}");
        }

        // A relative path still resolves against the data directory, the
        // way a process resolves against its cwd.
        std::fs::write(dir.join("mine"), b"ours").unwrap();
        let mut got = String::new();
        open_impl(&r, "mine", Intent::Read, true, Reach::Anywhere)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "ours");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn lists_a_directory_and_reports_kinds() {
        let (r, dir) = root("list");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();

        let mut names: Vec<(String, bool)> = open_dir(&r, ".", Reach::Sandbox)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.file_type().unwrap().is_dir(),
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![("a.txt".to_string(), false), ("sub".to_string(), true)]
        );

        // Listing out is refused without the escape, allowed with it.
        let out = outside("list");
        let abs = out.to_str().unwrap();
        assert!(open_dir(&r, abs, Reach::Sandbox).is_err());
        assert_eq!(open_dir(&r, abs, Reach::Anywhere).unwrap().count(), 1);

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn creates_nested_dirs_and_round_trips() {
        use std::io::{Read as _, Write as _};

        let (r, dir) = root("nest");
        for (name, beneath) in BACKENDS {
            let rel = format!("{name}/deep/c.txt");
            write(&r, &rel, false, beneath).unwrap().write_all(b"hi").unwrap();
            let mut got = String::new();
            read(&r, &rel, beneath)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "hi", "{name}");
            assert!(dir.join(&rel).is_file(), "{name}");

            // Truncate, then append.
            write(&r, &rel, false, beneath).unwrap().write_all(b"x").unwrap();
            write(&r, &rel, true, beneath).unwrap().write_all(b"y").unwrap();
            let mut got = String::new();
            read(&r, &rel, beneath)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "xy", "{name}: append");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_not_found() {
        let (r, dir) = root("missing");
        for (name, beneath) in BACKENDS {
            let e = read(&r, "nope.txt", beneath).unwrap_err();
            assert_eq!(e.code(), ErrorCode::NoSuchObject, "{name}");
            let e = read(&r, "no/such/dir.txt", beneath).unwrap_err();
            assert_eq!(e.code(), ErrorCode::NoSuchObject, "{name}: nested");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A symlinked DIRECTORY inside the sandbox must not lead outside.
    /// Both resolvers enforce this.
    #[test]
    fn symlink_dir_escape_is_caught() {
        let (r, dir) = root("symdir");
        let out = outside("symdir");
        std::os::unix::fs::symlink(&out, dir.join("link")).unwrap();

        for (name, beneath) in BACKENDS {
            let e = read(&r, "link/secret", beneath)
                .err()
                .unwrap_or_else(|| panic!("{name}: escape allowed"));
            assert_eq!(e.code(), ErrorCode::BadRequest, "{name}");
        }
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    /// A symlink at the FINAL component. This is where the two resolvers
    /// differ, and the difference is documented in the module docs:
    /// openat2 refuses to leave the tree, the portable resolver opens the
    /// final component as named and so follows it.
    #[test]
    fn final_component_symlink_differs_by_backend() {
        let (r, dir) = root("symfinal");
        let out = outside("symfinal");
        std::os::unix::fs::symlink(out.join("secret"), dir.join("f")).unwrap();

        // openat2, when this kernel has it.
        #[cfg(target_os = "linux")]
        {
            let result = read(&r, "f", true);
            if beneath::in_use() {
                let e = result
                    .err()
                    .expect("openat2 followed an escaping symlink");
                assert_eq!(e.code(), ErrorCode::BadRequest, "{}", e.message());
            }
        }

        // Portable resolver: the known weaker guarantee. Asserted so the
        // behavior is a decision on record, not an accident.
        use std::io::Read as _;
        let mut got = String::new();
        read(&r, "f", false)
            .expect("portable resolver should open the final component")
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "leak");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    /// openat2 must actually be the syscall in use on a modern kernel.
    /// Guards against the fallback silently swallowing everything.
    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_is_used_when_available() {
        use std::io::Write as _;

        let (r, dir) = root("probe");
        write(&r, "f", false, true).unwrap().write_all(b"x").unwrap();
        // A 5.6+ kernel with no seccomp filter must report PRESENT.
        assert!(
            beneath::in_use(),
            "openat2 fell back; if this kernel is < 5.6 that is expected"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bad_plugin_names_rejected() {
        for bad in ["", "a/b", "..", "."] {
            assert!(open_root(bad).is_err(), "{bad} was accepted");
        }
    }
}
