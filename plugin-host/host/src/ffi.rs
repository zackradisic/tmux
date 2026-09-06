//! C FFI surface: the host vtable handed to `pgh_init` by tmux.
//!
//! Threading contract (mirrored in include/plugin-host.h):
//! - Every `pgh_*` export is called only from the tmux server main thread.
//! - Vtable function pointers are called only synchronously from inside a
//!   `pgh_*` call, on that same thread.
//! - Vtable calls may re-enter `pgh_notify`, `pgh_async_complete` and
//!   `pgh_mode_event` (all enqueue-only) but must never re-enter any other
//!   `pgh_*` entry point. In particular they must never destroy tmux
//!   objects synchronously (that would re-enter `pgh_object_destroyed`
//!   while the calling instance is checked out of the registry).

#![allow(non_camel_case_types)]

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};

/// Log levels for `pgh_host_vtable.log`.
pub const PGH_LOG_DEBUG: c_int = 0;
pub const PGH_LOG_INFO: c_int = 1;
pub const PGH_LOG_WARN: c_int = 2;
pub const PGH_LOG_ERROR: c_int = 3;

/// Object kinds for handle resolution and `pgh_object_destroyed`.
pub const PGH_OBJ_SESSION: c_int = 0;
pub const PGH_OBJ_WINDOW: c_int = 1;
pub const PGH_OBJ_PANE: c_int = 2;
pub const PGH_OBJ_CLIENT: c_int = 3;

/// Sink used wherever bytes cross the FFI from callee to caller: the
/// callee invokes the sink zero or more times with a byte run (not
/// NUL-terminated); ownership never crosses the boundary.
pub type pgh_sink =
    unsafe extern "C" fn(ctx: *mut c_void, ptr: *const c_char, len: usize);

/// Error codes for the `err` parameter of `pgh_async_complete` (the wire
/// numbers of tmux-plugin-abi's ErrorCode; 0 = success).
pub const PGH_ERR_BAD_REQUEST: c_int = 1;
pub const PGH_ERR_NO_SUCH_OBJECT: c_int = 4;
pub const PGH_ERR_LIMIT: c_int = 6;
pub const PGH_ERR_HOST: c_int = 7;
pub const PGH_ERR_CANCELLED: c_int = 8;

/// Relation queries for `pgh_host_vtable.obj_relation` (scope checks).
pub const PGH_REL_PANE_WINDOW: c_int = 0;
pub const PGH_REL_SESSION_CURWIN: c_int = 1;
pub const PGH_REL_WINDOW_IN_SESSION: c_int = 2;
pub const PGH_REL_PANE_IN_WINDOW: c_int = 3;
pub const PGH_REL_PANE_IN_SESSION: c_int = 4;

/// Host callbacks provided by tmux at `pgh_init` time.
///
/// The struct is copied by value; tmux may discard its copy after `pgh_init`
/// returns. All function pointers must stay valid for the process lifetime.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct pgh_host_vtable {
    /// Log a message attributed to a plugin ("host" for subsystem messages).
    pub log: unsafe extern "C" fn(level: c_int, plugin: *const c_char, msg: *const c_char),
    /// Emit the binary object-list buffer (u32 count + records, see
    /// abi-types) for all live objects of `kind` (PGH_OBJ_*) into the sink.
    pub list_objects: unsafe extern "C" fn(kind: c_int, sink: pgh_sink, ctx: *mut c_void),
    /// Emit one binary object record describing the live object (kind, id)
    /// into the sink and return 0; return -1 without emitting if it no
    /// longer exists. This is the weak-handle validity check.
    pub resolve_object:
        unsafe extern "C" fn(kind: c_int, id: u32, sink: pgh_sink, ctx: *mut c_void) -> c_int,
    /// Relation query for scope checks (PGH_REL_*): PANE_WINDOW(a=pane) ->
    /// window id; SESSION_CURWIN(a=session) -> window id;
    /// WINDOW_IN_SESSION(a=window, b=session), PANE_IN_WINDOW(a=pane,
    /// b=window), PANE_IN_SESSION(a=pane, b=session) -> 1/0.
    /// -1 = no such object.
    pub obj_relation: unsafe extern "C" fn(rel: c_int, a: u32, b: u32) -> i64,
    /// Send keys to a pane; literal != 0 sends `keys` as UTF-8 characters,
    /// otherwise `keys` is one tmux key name. 0 ok, -1 dead pane, -2 bad key.
    pub send_keys:
        unsafe extern "C" fn(pane_id: u32, keys: *const c_char, literal: c_int) -> c_int,
    /// Capture pane text into the sink (one line per row, trailing \n).
    /// start/end rows relative to the visible top (negative = history),
    /// end inclusive. 0 ok, -1 dead pane.
    pub capture_pane: unsafe extern "C" fn(
        pane_id: u32,
        start: c_int,
        end: c_int,
        escapes: c_int,
        sink: pgh_sink,
        ctx: *mut c_void,
    ) -> c_int,
    /// Read one environment variable from a pane's foreground process as
    /// a string. 0 ok, -1 dead pane, -2 no such variable.
    pub pane_env: unsafe extern "C" fn(
        pane_id: u32,
        name: *const c_char,
        sink: pgh_sink,
        ctx: *mut c_void,
    ) -> c_int,
    /// Emit the open-file paths of a pane's foreground process, one per
    /// line. 0 ok, -1 dead pane, -2 none.
    pub pane_fds: unsafe extern "C" fn(
        pane_id: u32,
        sink: pgh_sink,
        ctx: *mut c_void,
    ) -> c_int,
    /// The pid of a pane's foreground process group, or -1 if dead.
    pub pane_pid: unsafe extern "C" fn(pane_id: u32) -> c_int,
    /// Get an option value as a string (kind -1 = server/global scope).
    /// 0 ok, -1 dead target, -2 no such option.
    pub get_option: unsafe extern "C" fn(
        kind: c_int,
        id: u32,
        name: *const c_char,
        sink: pgh_sink,
        ctx: *mut c_void,
    ) -> c_int,
    /// Set a user (@-prefixed) option. 0 ok, -1 dead target, -2 not @-option.
    pub set_option: unsafe extern "C" fn(
        kind: c_int,
        id: u32,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int,
    /// Status-line message (client_id, or -1 for all attached clients) plus
    /// the server message log. 0 ok, -1 no such client.
    pub display_message: unsafe extern "C" fn(
        client_id: c_int,
        plugin: *const c_char,
        msg: *const c_char,
    ) -> c_int,
    /// Start a shell command as a job; completion arrives later via
    /// pgh_async_complete(token, 0, status, signalled, output, len).
    /// 0 started, -1 failed to start.
    pub run_job: unsafe extern "C" fn(
        cmd: *const c_char,
        cwd: *const c_char, // may be NULL
        token: u64,
    ) -> c_int,
    /// Queue a tmux command string on the command queue (NOHOOKS); the
    /// completion callback delivers pgh_async_complete(token, ...) after it
    /// runs (parse errors arrive as error completions). -1 internal failure.
    pub run_command: unsafe extern "C" fn(cmd: *const c_char, token: u64) -> c_int,
    /// One-shot timer; fires pgh_async_complete(token, 0, 0, 0, NULL, 0).
    /// Returns a timer id usable with timer_cancel.
    pub timer_start: unsafe extern "C" fn(ms: u64, token: u64) -> u64,
    /// Cancel a pending timer (no completion is delivered). 0 ok, -1 unknown.
    pub timer_cancel: unsafe extern "C" fn(timer_id: u64) -> c_int,
    /// A plugin changed state in a way the user should see (disabled,
    /// load failed, ...). The C side surfaces it on status lines and in
    /// the server message log.
    pub plugin_state_changed: unsafe extern "C" fn(
        plugin: *const c_char,
        state: *const c_char,
        reason: *const c_char,
    ),
    /// Open a plugin UI mode in a freshly spawned empty floating pane in
    /// `window`. x/y are top-left cell offsets, -1 = centered; `title` may
    /// be NULL. Returns the new mode id (> 0) synchronously, or a negative
    /// error: -1 no such window, -2 spawn failed, -3 mode init failed.
    /// Events for the mode arrive later via pgh_mode_event.
    pub mode_open: unsafe extern "C" fn(
        window: u32,
        width: u32,
        height: u32,
        x: c_int,
        y: c_int,
        title: *const c_char,
    ) -> i64,
    /// Parse ANSI bytes into a mode's screen (server-side escape parser,
    /// no tty round-trip). 0 ok, -1 no such mode.
    pub mode_write: unsafe extern "C" fn(mode: u64, data: *const u8, len: usize) -> c_int,
    /// Set (pane >= 0) or clear (pane < 0) a mode's retained preview rect:
    /// a live blit of the source pane's grid at (x, y), size (w, h),
    /// refreshed periodically until cleared. 0 ok, -1 no such mode,
    /// -2 rect does not fit the mode screen.
    pub mode_preview:
        unsafe extern "C" fn(mode: u64, pane: i64, x: u32, y: u32, w: u32, h: u32) -> c_int,
    /// Close a mode: the floating pane is torn down at the next safe
    /// point (never synchronously inside this call), which delivers
    /// pgh_mode_event(mode, "mode-closed", ...). 0 ok, -1 no such mode.
    pub mode_close: unsafe extern "C" fn(mode: u64) -> c_int,
    /// Move a mode's floating pane to another window, keeping the pane,
    /// the mode and its screen contents intact (join-pane style relink;
    /// at most a mode-resize event follows). x/y as for mode_open
    /// (-1 = centered). 0 ok, -1 no such mode, -2 no such window or
    /// unmovable pane, -3 the move would empty the source window.
    pub mode_move: unsafe extern "C" fn(mode: u64, window: u32, x: c_int, y: c_int) -> c_int,
    /// Resize a mode's floating pane. Width and height are content cells,
    /// as in mode_open; the border sits outside them. Clamped to the
    /// window, top-left corner kept, so a growing panel expands down and
    /// right. A mode-resize event follows with the size actually given.
    /// 0 ok, -1 no such mode or the pane is not floating, -2 window too
    /// small.
    pub mode_resize: unsafe extern "C" fn(mode: u64, width: u32, height: u32) -> c_int,
    /// Expand a format string against a scope (kind -1 = server/global,
    /// else PGH_OBJ_SESSION/WINDOW/PANE) into the sink. Jobs (#()) are
    /// disabled. 0 ok, -1 dead/bad target.
    pub format_expand: unsafe extern "C" fn(
        kind: c_int,
        id: u32,
        fmt: *const c_char,
        sink: pgh_sink,
        ctx: *mut c_void,
    ) -> c_int,
}

// Function pointers are Send + Sync; the vtable is stored in a OnceLock.
unsafe impl Send for pgh_host_vtable {}
unsafe impl Sync for pgh_host_vtable {}
