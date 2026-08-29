//! Typed wrappers over the per-method host imports.
//!
//! Sync functions return immediately; async functions return futures
//! resolved by the SDK executor when the host delivers the completion.
//! String arguments take [`impl AsTmuxStr`](crate::strings::AsTmuxStr):
//! `c"..."` literals and [`TmuxString`](crate::strings::TmuxString) cross
//! zero-copy, plain `&str` pays one small copy for the trailing NUL.

use tmux_plugin_abi::{
    parse_list, ClientInfo, Cursor, ErrorCode, HostError, PaneInfo,
    SelfInfo, SessionInfo, WindowInfo, KIND_CLIENT, KIND_PANE, KIND_SERVER,
    KIND_SESSION, KIND_WINDOW,
};

use crate::event;
use crate::executor::{Completion, HostFuture};
use crate::ids::{ClientId, ModeId, PaneId, SessionId, WindowId};
use crate::runtime::{self, raw, Owned};
use crate::strings::AsTmuxStr;

/// Fetch the host's message for the error a call just returned.
fn host_err(rc_neg: i32) -> HostError {
    let code = ErrorCode::from_num(-rc_neg);
    let mut buf = vec![0u8; 256];
    let mut message = String::new();
    loop {
        let mut len: u32 = 0;
        let rc = unsafe {
            raw::last_error(
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
                &mut len as *mut u32 as i32,
            )
        };
        if rc == 0 {
            buf.truncate(len as usize);
            message = String::from_utf8_lossy(&buf).into_owned();
            break;
        }
        if -rc == ErrorCode::Limit.as_num() && len as usize > buf.len() {
            buf.resize(len as usize, 0);
            continue;
        }
        break;
    }
    HostError { code, message }
}

fn check(rc: i32) -> Result<(), HostError> {
    if rc == 0 {
        Ok(())
    } else {
        Err(host_err(rc))
    }
}

fn check_i64(rc: i64) -> Result<i64, HostError> {
    if rc > 0 {
        Ok(rc)
    } else {
        Err(host_err(rc as i32))
    }
}

/// Run an OutBuf-shaped call, growing the buffer on -E_LIMIT (the host
/// reports the needed size through len_out either way).
fn call_out(
    initial: usize,
    mut f: impl FnMut(i32, i32, i32) -> i32,
) -> Result<Vec<u8>, HostError> {
    let mut buf: Vec<u8> = vec![0; initial.max(16)];
    loop {
        let mut len: u32 = 0;
        let rc = f(
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
            &mut len as *mut u32 as i32,
        );
        if rc == 0 {
            buf.truncate(len as usize);
            return Ok(buf);
        }
        if -rc == ErrorCode::Limit.as_num() && len as usize > buf.len() {
            buf.resize(len as usize, 0);
            continue;
        }
        return Err(host_err(rc));
    }
}

/// Run an OwnedBuf-shaped call: the host allocates the result in guest
/// memory and RAII owns it from the moment the call returns.
fn call_owned(f: impl FnOnce(i32) -> i32) -> Result<Owned, HostError> {
    let mut out: [u32; 2] = [0, 0];
    let rc = f(out.as_mut_ptr() as i32);
    if rc != 0 {
        return Err(host_err(rc));
    }
    Ok(Owned::from_out_struct(out))
}

fn wire_err() -> HostError {
    HostError {
        code: ErrorCode::Host,
        message: "malformed buffer from host".into(),
    }
}

// ---- interning & subscriptions ----

pub use crate::event::{intern, intern_name};

/// Subscribe to events by name (interned once, then integer routing).
pub fn subscribe(events: &[&str]) -> Result<(), HostError> {
    for name in events {
        let id = event::intern(name);
        check(unsafe { raw::subscribe(id as i32) })?;
    }
    Ok(())
}

pub fn unsubscribe(events: &[&str]) -> Result<(), HostError> {
    for name in events {
        let id = event::intern(name);
        check(unsafe { raw::unsubscribe(id as i32) })?;
    }
    Ok(())
}

// ---- object state ----

pub fn list_sessions() -> Result<Vec<SessionInfo>, HostError> {
    let buf = call_owned(|out| unsafe { raw::list(KIND_SESSION, out) })?;
    parse_list(&buf, SessionInfo::parse).map_err(|_| wire_err())
}

pub fn list_windows() -> Result<Vec<WindowInfo>, HostError> {
    let buf = call_owned(|out| unsafe { raw::list(KIND_WINDOW, out) })?;
    parse_list(&buf, WindowInfo::parse).map_err(|_| wire_err())
}

pub fn list_panes() -> Result<Vec<PaneInfo>, HostError> {
    let buf = call_owned(|out| unsafe { raw::list(KIND_PANE, out) })?;
    parse_list(&buf, PaneInfo::parse).map_err(|_| wire_err())
}

pub fn list_clients() -> Result<Vec<ClientInfo>, HostError> {
    let buf = call_owned(|out| unsafe { raw::list(KIND_CLIENT, out) })?;
    parse_list(&buf, ClientInfo::parse).map_err(|_| wire_err())
}

fn resolve_one<T>(
    kind: i32,
    id: u32,
    parse: impl Fn(&mut Cursor<'_>) -> Result<T, tmux_plugin_abi::WireError>,
) -> Result<T, HostError> {
    let buf =
        call_owned(|out| unsafe { raw::resolve(kind, id as i32, out) })?;
    parse(&mut Cursor::new(&buf)).map_err(|_| wire_err())
}

/// Resolve a pane's live info. Errors with E_NO_SUCH_OBJECT once gone.
pub fn resolve_pane(pane: PaneId) -> Result<PaneInfo, HostError> {
    resolve_one(KIND_PANE, pane.0, PaneInfo::parse)
}

/// Resolve a window's live info. Errors with E_NO_SUCH_OBJECT once gone.
pub fn resolve_window(window: WindowId) -> Result<WindowInfo, HostError> {
    resolve_one(KIND_WINDOW, window.0, WindowInfo::parse)
}

/// Resolve a session's live info. Errors with E_NO_SUCH_OBJECT once gone.
pub fn resolve_session(session: SessionId) -> Result<SessionInfo, HostError> {
    resolve_one(KIND_SESSION, session.0, SessionInfo::parse)
}

/// Resolve a client's live info. Errors with E_NO_SUCH_OBJECT once gone.
pub fn resolve_client(client: ClientId) -> Result<ClientInfo, HostError> {
    resolve_one(KIND_CLIENT, client.0, ClientInfo::parse)
}

/// This instance's identity (scope kind/id + generation).
pub fn self_info() -> Result<SelfInfo, HostError> {
    let mut out = [0u8; tmux_plugin_abi::SELF_INFO_LEN];
    check(unsafe { raw::self_info(out.as_mut_ptr() as i32) })?;
    SelfInfo::from_bytes(&out).map_err(|_| wire_err())
}

/// Where an option lives.
#[derive(Debug, Clone, Copy)]
pub enum OptionTarget {
    Server,
    Session(SessionId),
    Window(WindowId),
    Pane(PaneId),
}

impl OptionTarget {
    fn kind_id(self) -> (i32, u32) {
        match self {
            OptionTarget::Server => (KIND_SERVER, 0),
            OptionTarget::Session(id) => (KIND_SESSION, id.0),
            OptionTarget::Window(id) => (KIND_WINDOW, id.0),
            OptionTarget::Pane(id) => (KIND_PANE, id.0),
        }
    }
}

/// Get an option (server/global scope) as a string.
pub fn get_option(name: impl AsTmuxStr) -> Result<String, HostError> {
    get_option_in(OptionTarget::Server, name)
}

/// Set a user (@-prefixed) option at server/global scope.
pub fn set_option(
    name: impl AsTmuxStr,
    value: impl AsTmuxStr,
) -> Result<(), HostError> {
    set_option_in(OptionTarget::Server, name, value)
}

/// Get an option from a specific scope (inherits along the option tree).
pub fn get_option_in(
    target: OptionTarget,
    name: impl AsTmuxStr,
) -> Result<String, HostError> {
    let (kind, id) = target.kind_id();
    let name = name.to_tmux();
    let (np, nl) = name.parts();
    let buf = call_out(64, |out, cap, len_out| unsafe {
        raw::get_option(kind, id as i32, np, nl, out, cap, len_out)
    })?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Expand a tmux format string (`#{...}`) against a scope. Jobs (`#()`)
/// are disabled. This is the way to read anything the object records do
/// not carry: `#{window_layout}`, `#{pane_current_command}`,
/// `#{history_size}`, ...
pub fn format_expand(
    target: OptionTarget,
    fmt: impl AsTmuxStr,
) -> Result<String, HostError> {
    let (kind, id) = target.kind_id();
    let fmt = fmt.to_tmux();
    let (fp, fl) = fmt.parts();
    let buf = call_out(256, |out, cap, len_out| unsafe {
        raw::format_expand(kind, id as i32, fp, fl, out, cap, len_out)
    })?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Set a user (@-prefixed) option on a specific scope. Options published
/// here are visible to status-line formats as #{@name} (pane options win
/// for the active pane, then window, session, global).
pub fn set_option_in(
    target: OptionTarget,
    name: impl AsTmuxStr,
    value: impl AsTmuxStr,
) -> Result<(), HostError> {
    let (kind, id) = target.kind_id();
    let name = name.to_tmux();
    let value = value.to_tmux();
    let (np, nl) = name.parts();
    let (vp, vl) = value.parts();
    check(unsafe { raw::set_option(kind, id as i32, np, nl, vp, vl) })
}

/// Send a literal string to a pane (one key per character).
pub fn send_text(pane: PaneId, text: impl AsTmuxStr) -> Result<(), HostError> {
    let text = text.to_tmux();
    let (p, l) = text.parts();
    check(unsafe { raw::send_keys(pane.0 as i32, p, l, 1) })
}

/// Send one named key ("Enter", "C-c", "M-x", ...) to a pane.
pub fn send_key(pane: PaneId, key: impl AsTmuxStr) -> Result<(), HostError> {
    let key = key.to_tmux();
    let (p, l) = key.parts();
    check(unsafe { raw::send_keys(pane.0 as i32, p, l, 0) })
}

/// Capture pane text into a reusable buffer (cleared first). Rows are
/// relative to the visible top (negative reaches history), `end`
/// inclusive; `escapes` includes SGR/OSC sequences. At most 2000 lines
/// per call; grow-and-retry is handled internally, so prefer paging with
/// start/end over huge buffers.
pub fn capture_pane_into(
    pane: PaneId,
    start: Option<i32>,
    end: Option<i32>,
    escapes: bool,
    buf: &mut Vec<u8>,
) -> Result<(), HostError> {
    let start = start.unwrap_or(0);
    let end = end.unwrap_or(i32::MAX);
    if buf.capacity() == 0 {
        buf.reserve(4096);
    }
    let cap = buf.capacity();
    buf.clear();
    buf.resize(cap, 0);
    loop {
        let mut len: u32 = 0;
        let rc = unsafe {
            raw::capture_pane(
                pane.0 as i32,
                start,
                end,
                i32::from(escapes),
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
                &mut len as *mut u32 as i32,
            )
        };
        if rc == 0 {
            buf.truncate(len as usize);
            return Ok(());
        }
        if -rc == ErrorCode::Limit.as_num() && len as usize > buf.len() {
            buf.resize(len as usize, 0);
            continue;
        }
        buf.clear();
        return Err(host_err(rc));
    }
}

/// Capture pane text (fresh allocation; see [`capture_pane_into`]).
pub fn capture_pane(
    pane: PaneId,
    start: Option<i32>,
    end: Option<i32>,
) -> Result<String, HostError> {
    let mut buf = Vec::new();
    capture_pane_into(pane, start, end, false, &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Show a status-line message on all attached clients (and the message log).
pub fn display_message(msg: impl AsTmuxStr) -> Result<(), HostError> {
    let msg = msg.to_tmux();
    let (p, l) = msg.parts();
    check(unsafe { raw::display_message(-1, p, l) })
}

/// Show a status-line message on one client.
pub fn display_message_to(
    client: ClientId,
    msg: impl AsTmuxStr,
) -> Result<(), HostError> {
    let msg = msg.to_tmux();
    let (p, l) = msg.parts();
    check(unsafe { raw::display_message(client.0 as i32, p, l) })
}

pub fn log(msg: &str) {
    runtime::log(1, msg);
}

// ---- UI modes (capability: mode) ----

/// Options for [`mode_open`]. Size is in cells; `x`/`y` are the top-left
/// offset within the window (`None` = centered). `window` defaults to the
/// instance's own window (pane/window scope) or the session's current
/// window (session scope); server-scoped instances must set it.
#[derive(Debug, Clone, Default)]
pub struct ModeOpts {
    pub window: Option<WindowId>,
    pub width: u32,
    pub height: u32,
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub title: Option<String>,
}

/// A retained preview rect for [`mode_preview`]: a live mirror of `pane`'s
/// grid drawn at (x, y), size (w, h), inside the mode screen.
#[derive(Debug, Clone, Copy)]
pub struct PreviewRect {
    pub pane: PaneId,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Open a UI mode: a freshly spawned empty floating pane owned by this
/// instance. Render with [`mode_write`]; `mode-key` / `mode-resize` /
/// `mode-closed` events arrive through `Plugin::on_event` with the mode id
/// in the `mode` field.
pub fn mode_open(opts: &ModeOpts) -> Result<ModeId, HostError> {
    let title = opts.title.as_deref().map(|t| t.to_tmux());
    let (tp, tl) = title.as_ref().map_or((0, 0), |t| t.parts());
    let window = opts.window.map_or(-1, |w| w.0 as i32);
    let off = |v: Option<u32>| v.map_or(-1, |n| n as i32);
    let id = check_i64(unsafe {
        raw::mode_open(
            window,
            opts.width as i32,
            opts.height as i32,
            off(opts.x),
            off(opts.y),
            tp,
            tl,
        )
    })?;
    Ok(ModeId(id as u64))
}

/// Send ANSI bytes to a mode's screen (parsed server-side: cursor
/// addressing, SGR, clears, ... - anything a terminal accepts). At most
/// 256 KiB per call; a full-screen redraw is idiomatic. Zero-copy: the
/// bytes are parsed straight out of plugin memory.
pub fn mode_write(mode: ModeId, data: &[u8]) -> Result<(), HostError> {
    check(unsafe {
        raw::mode_write(mode.0 as i64, data.as_ptr() as i32, data.len() as i32)
    })
}

/// Set (or clear, with `None`) a mode's retained preview rect. The host
/// redraws it from the source pane's live grid every ~500ms until cleared
/// or the source pane dies.
pub fn mode_preview(
    mode: ModeId,
    rect: Option<&PreviewRect>,
) -> Result<(), HostError> {
    let rc = match rect {
        Some(r) => unsafe {
            raw::mode_preview(
                mode.0 as i64,
                i64::from(r.pane.0),
                r.x as i32,
                r.y as i32,
                r.w as i32,
                r.h as i32,
            )
        },
        None => unsafe { raw::mode_preview(mode.0 as i64, -1, 0, 0, 0, 0) },
    };
    check(rc)
}

/// Move a mode's floating pane to another window, keeping the mode id,
/// the pane and its rendered screen intact (at most a `mode-resize`
/// follows if the destination clamps the size). `window` defaults as in
/// [`mode_open`]; position is re-centered unless `x`/`y` are given.
/// Fails with `E_LIMIT` if the move would leave the source window empty
/// (close instead).
pub fn mode_move(
    mode: ModeId,
    window: Option<WindowId>,
) -> Result<(), HostError> {
    let w = window.map_or(-1, |w| w.0 as i32);
    check(unsafe { raw::mode_move(mode.0 as i64, w, -1, -1) })
}

/// Resize a mode's floating pane. `width` and `height` are content cells,
/// exactly as in [`mode_open`]; the border sits outside them. The host
/// clamps the size to the window, and the float keeps its top-left corner,
/// so a panel that grows expands down and right instead of jumping.
///
/// A `mode-resize` event follows with the size actually given, so treat
/// that event as the truth and this call as a request. Call it only when
/// the size you want differs from the size the last event reported,
/// otherwise the two chase each other.
pub fn mode_resize(mode: ModeId, width: u32, height: u32) -> Result<(), HostError> {
    check(unsafe {
        raw::mode_resize(mode.0 as i64, width as i32, height as i32)
    })
}

/// Close a mode. The floating pane is torn down at the next safe point;
/// a final `mode-closed` event (reason "closed") follows.
pub fn mode_close(mode: ModeId) -> Result<(), HostError> {
    check(unsafe { raw::mode_close(mode.0 as i64) })
}

// ---- async API ----

#[derive(Debug, Clone)]
pub struct JobOutput {
    /// Exit status, or the signal number if `signalled`.
    pub status: i32,
    pub signalled: bool,
    /// Combined captured output (lossy UTF-8).
    pub output: String,
}

/// Start an async request: the raw call returns a token (> 0) or -err.
fn start_async(token: i64) -> Result<HostFuture, HostError> {
    if token <= 0 {
        return Err(host_err(token as i32));
    }
    Ok(HostFuture::new(token as u64))
}

/// Run a shell command; resolves with its output when it exits.
pub async fn run_job(
    cmd: impl AsTmuxStr,
    cwd: Option<&str>,
) -> Result<JobOutput, HostError> {
    let fut = {
        let cmd = cmd.to_tmux();
        let cwd = cwd.map(|c| c.to_tmux());
        let (cp, cl) = cmd.parts();
        let (wp, wl) = cwd.as_ref().map_or((0, 0), |c| c.parts());
        start_async(unsafe { raw::run_job(cp, cl, wp, wl) })?
    };
    let Completion { v0, v1, data } = fut.await?;
    Ok(JobOutput {
        status: v0 as i32,
        signalled: v1 != 0,
        output: String::from_utf8_lossy(&data).into_owned(),
    })
}

/// Run a tmux command string through the command queue. Note: only parse
/// errors fail; a command that runs and errors still completes as Ok.
pub async fn run_command(command: impl AsTmuxStr) -> Result<(), HostError> {
    let fut = {
        let cmd = command.to_tmux();
        let (p, l) = cmd.parts();
        start_async(unsafe { raw::run_command(p, l) })?
    };
    fut.await.map(|_| ())
}

/// Sleep for `ms` milliseconds (host timer).
///
/// Cancelling the task that owns this sleep stops the host timer too, so
/// it never fires and never re-enters the guest.
pub async fn sleep_ms(ms: u64) -> Result<(), HostError> {
    let token = unsafe { raw::timer_start(ms as i64) };
    if token <= 0 {
        return Err(host_err(token as i32));
    }
    HostFuture::timer(token as u64).await.map(|_| ())
}

// ---- filesystem (capabilities: fs-read / fs-write) ----
//
// Paths are relative to the plugin's private data directory
// ($XDG_DATA_HOME|~/.local/share + tmux/plugins/<name>/); absolute paths
// and `..` are rejected. There is no per-call byte cap; a transfer is
// bounded by the plugin's own memory. Awaited calls are fully ordered;
// do not keep two writes to the SAME file in flight at once.

/// Write (append=false truncates/creates) a file asynchronously on the
/// host's fs worker - the tmux event loop never blocks. Zero-copy: the
/// worker reads `data` straight out of plugin memory; the SDK keeps the
/// buffer pinned until the completion arrives (even if the future is
/// cancelled). Resolves with the byte count.
pub async fn fs_write(
    path: &str,
    data: Vec<u8>,
    append: bool,
) -> Result<u64, HostError> {
    let token = unsafe {
        raw::fs_write(
            path.as_ptr() as i32,
            path.len() as i32,
            data.as_ptr() as i32,
            data.len() as i32,
            i32::from(append),
        )
    };
    let fut = start_async(token)?;
    let token = token as u64;
    crate::executor::pin_buffer(token, data);
    let result = fut.await;
    let _ = crate::executor::take_buffer(token);
    result.map(|c| c.v0 as u64)
}

/// Read up to `capacity` bytes at `offset` asynchronously (fs worker;
/// zero-copy into the returned buffer, pinned until completion).
/// Resolves with (bytes, eof).
pub async fn fs_read(
    path: &str,
    offset: u64,
    capacity: usize,
) -> Result<(Vec<u8>, bool), HostError> {
    let cap = capacity.max(1);
    let mut buf = vec![0u8; cap];
    let token = unsafe {
        raw::fs_read(
            path.as_ptr() as i32,
            path.len() as i32,
            offset as i64,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
        )
    };
    let fut = start_async(token)?;
    let token = token as u64;
    crate::executor::pin_buffer(token, buf);
    let result = fut.await;
    let buf = crate::executor::take_buffer(token);
    let c = result?;
    let mut buf = buf.unwrap_or_default();
    buf.truncate((c.v0.max(0) as usize).min(buf.len()));
    Ok((buf, c.v1 != 0))
}

/// What a directory entry is, from `d_type`. `Unknown` means the
/// filesystem did not say and no `stat` was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Unknown,
    Dir,
    File,
    Symlink,
    Other,
}

impl EntryKind {
    fn from_wire(v: u8) -> EntryKind {
        match v {
            1 => EntryKind::Dir,
            2 => EntryKind::File,
            3 => EntryKind::Symlink,
            4 => EntryKind::Other,
            _ => EntryKind::Unknown,
        }
    }

    pub fn is_dir(self) -> bool {
        self == EntryKind::Dir
    }
}

/// One entry. `name` borrows the listing's buffer, so walking a directory
/// allocates nothing per entry.
#[derive(Debug, Clone, Copy)]
pub struct DirEntry<'a> {
    pub name: &'a str,
    pub kind: EntryKind,
    /// Modification time, seconds since the epoch. Zero unless the
    /// listing asked for [`ListOpts::mtime`].
    pub mtime: i64,
}

/// What to fetch, and what to skip.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListOpts {
    /// Fetch each entry's modification time. This is the one field that
    /// is not free - `d_type` rides along with the directory entry, but a
    /// time costs one `fstatat` per name (about 0.8us). Ask for it only
    /// when you will use it.
    pub mtime: bool,
    /// Skip everything that is not a directory, before any `mtime` cost
    /// is paid. Worth setting in a directory of ten thousand files and
    /// five subdirectories.
    pub dirs_only: bool,
}

impl ListOpts {
    fn bits(self) -> i32 {
        (if self.mtime { 1 } else { 0 }) | (if self.dirs_only { 2 } else { 0 })
    }
}

/// A directory listing: the packed bytes the host wrote into our memory,
/// plus the totals. Iterate it with [`Listing::iter`].
pub struct Listing {
    buf: Vec<u8>,
    used: usize,
    /// Records actually in the buffer. Counted once, when the listing is
    /// built: `truncated` is asked several times per scan, and walking
    /// (and re-validating) the whole buffer each time is not free on a
    /// directory of ten thousand entries.
    count: usize,
    /// Entries the directory holds. Larger than `count` means the buffer
    /// was too small and the tail was dropped.
    pub total: u32,
}

impl Listing {
    /// Records held. Cheap - no walk.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// True when the buffer could not hold every entry.
    pub fn truncated(&self) -> bool {
        self.count < self.total as usize
    }

    /// Walk the entries. Names borrow the buffer; nothing is copied.
    pub fn iter(&self) -> ListingIter<'_> {
        ListingIter { buf: &self.buf[..self.used], off: 0 }
    }
}

pub struct ListingIter<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Iterator for ListingIter<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<DirEntry<'a>> {
        // record: u16 namelen | u8 kind | u8 reserved | i64 mtime | name
        const HEADER: usize = 12;
        loop {
            if self.off + HEADER > self.buf.len() {
                return None;
            }
            let namelen = u16::from_le_bytes([
                self.buf[self.off],
                self.buf[self.off + 1],
            ]) as usize;
            let kind = EntryKind::from_wire(self.buf[self.off + 2]);
            let mut t = [0u8; 8];
            t.copy_from_slice(&self.buf[self.off + 4..self.off + 12]);
            let mtime = i64::from_le_bytes(t);
            let start = self.off + HEADER;
            let end = start + namelen;
            if end > self.buf.len() {
                return None;
            }
            self.off = end;
            // A name that is not UTF-8 is skipped rather than lossily
            // renamed: handing back a name that does not open is worse
            // than not listing it.
            if let Ok(name) = core::str::from_utf8(&self.buf[start..end]) {
                return Some(DirEntry { name, kind, mtime });
            }
        }
    }
}

/// Default listing buffer. Around 3000 short names, which covers almost
/// every real directory in one call.
const LIST_BUF: usize = 64 * 1024;

/// List a directory asynchronously on the host's fs worker (capability
/// `fs-list`).
///
/// The host writes packed records straight into our linear memory - one
/// copy for the whole directory, and no allocation per entry. Each entry
/// carries its `d_type`, so telling a directory from a file costs no
/// extra call.
///
/// `path` resolves like a process cwd: a relative path against the
/// plugin's data directory, an absolute path as given. Leaving the data
/// directory needs the `fs-read-any` capability.
///
/// If the directory does not fit the buffer the call grows and retries,
/// so a complete listing is the normal outcome. That matters if you sort
/// the result: a filesystem returns entries in hash order, so a truncated
/// listing is an arbitrary subset, and sorting one by time gives a
/// confidently wrong answer. Check [`Listing::truncated`] before ranking.
pub async fn fs_list(path: &str) -> Result<Listing, HostError> {
    fs_list_with(path, ListOpts::default()).await
}

/// [`fs_list`] with options - see [`ListOpts`].
pub async fn fs_list_with(
    path: &str,
    opts: ListOpts,
) -> Result<Listing, HostError> {
    let mut cap = LIST_BUF;
    loop {
        let mut buf = vec![0u8; cap];
        let token = unsafe {
            raw::fs_list(
                path.as_ptr() as i32,
                path.len() as i32,
                opts.bits(),
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
            )
        };
        let fut = start_async(token)?;
        let token = token as u64;
        crate::executor::pin_buffer(token, buf);
        let result = fut.await;
        let buf = crate::executor::take_buffer(token);
        let c = result?;
        let buf = buf.unwrap_or_default();
        let used = (c.v0.max(0) as usize).min(buf.len());
        let total = c.v1.max(0) as u32;
        let mut listing = Listing { buf, used, count: 0, total };
        listing.count = listing.iter().count();

        // Grow to what the directory actually needs and go again. The
        // estimate is per-entry header plus an average name; if it is
        // still short (very long names) the next pass doubles again.
        const MAX: usize = 32 * 1024 * 1024;
        if listing.truncated() && cap < MAX {
            let want = (total as usize).saturating_mul(56).max(cap * 2);
            cap = want.min(MAX);
            continue;
        }
        return Ok(listing);
    }
}

/// The plugin's private data directory (absolute path) - where every
/// fs_* path resolves.
/// The server user's home directory, for expanding a leading `~`.
///
/// A guest has no environment, so this is the only way to learn it short
/// of forking a shell to print it. Cheap and synchronous - no process, no
/// capability.
pub fn home_dir() -> Result<String, HostError> {
    let buf = call_out(128, |out, cap, len_out| unsafe {
        raw::home_dir(out, cap, len_out)
    })?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub fn fs_root() -> Result<String, HostError> {
    let buf = call_out(128, |out, cap, len_out| unsafe {
        raw::fs_root(out, cap, len_out)
    })?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Synchronous write for small files (blocks the tmux loop for one
/// page-cache access, like tmux's own file I/O). Keep it small: the time
/// spent here counts against the instance's CPU budget, so a slow write
/// traps the plugin. Use `fs_write` for anything bigger.
pub fn fs_write_sync(
    path: &str,
    data: &[u8],
    append: bool,
) -> Result<u64, HostError> {
    let rc = unsafe {
        raw::fs_write_sync(
            path.as_ptr() as i32,
            path.len() as i32,
            data.as_ptr() as i32,
            data.len() as i32,
            i32::from(append),
        )
    };
    check_i64(rc).map(|n| n as u64)
}

/// Synchronous read for small files. Fills `buf` up to its capacity
/// (allocating 4096 if empty); returns eof. Same budget warning as
/// `fs_write_sync` - use `fs_read` for anything bigger.
pub fn fs_read_sync(
    path: &str,
    offset: u64,
    buf: &mut Vec<u8>,
) -> Result<bool, HostError> {
    if buf.capacity() == 0 {
        buf.reserve(4096);
    }
    let cap = buf.capacity();
    buf.clear();
    buf.resize(cap, 0);
    let mut len: u32 = 0;
    let mut eof: u32 = 0;
    let rc = unsafe {
        raw::fs_read_sync(
            path.as_ptr() as i32,
            path.len() as i32,
            offset as i64,
            buf.as_mut_ptr() as i32,
            buf.len() as i32,
            &mut len as *mut u32 as i32,
            &mut eof as *mut u32 as i32,
        )
    };
    if rc != 0 {
        buf.clear();
        return Err(host_err(rc));
    }
    buf.truncate(len as usize);
    Ok(eof != 0)
}
