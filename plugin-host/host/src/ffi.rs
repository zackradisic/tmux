//! C FFI surface: the host vtable handed to `pgh_init` by tmux.
//!
//! Threading contract (mirrored in include/plugin-host.h):
//! - Every `pgh_*` export is called only from the tmux server main thread.
//! - Vtable function pointers are called only synchronously from inside a
//!   `pgh_*` call, on that same thread.
//! - Vtable calls may re-enter `pgh_notify` (which is enqueue-only) but must
//!   never re-enter any other `pgh_*` entry point.

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

/// Sink used wherever a string crosses the FFI from callee to caller: the
/// callee invokes the sink zero or more times with UTF-8 bytes (not
/// NUL-terminated); ownership never crosses the boundary.
pub type pgh_sink =
    unsafe extern "C" fn(ctx: *mut c_void, ptr: *const c_char, len: usize);

/// Host callbacks provided by tmux at `pgh_init` time.
///
/// The struct is copied by value; tmux may discard its copy after `pgh_init`
/// returns. All function pointers must stay valid for the process lifetime.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct pgh_host_vtable {
    /// Log a message attributed to a plugin ("host" for subsystem messages).
    pub log: unsafe extern "C" fn(level: c_int, plugin: *const c_char, msg: *const c_char),
    /// Emit a JSON array of all live objects of `kind` (PGH_OBJ_*) into the
    /// sink. Each element carries at least {"id": n}.
    pub list_objects: unsafe extern "C" fn(kind: c_int, sink: pgh_sink, ctx: *mut c_void),
    /// Emit a JSON object describing the live object (kind, id) into the
    /// sink and return 0; return -1 without emitting if it no longer exists.
    /// This is the weak-handle validity check.
    pub resolve_object:
        unsafe extern "C" fn(kind: c_int, id: u32, sink: pgh_sink, ctx: *mut c_void) -> c_int,
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
    /// pgh_async_complete(token, {"status","signalled","output"}, 0).
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
    /// One-shot timer; fires pgh_async_complete(token, "{}", 0). Returns a
    /// timer id usable with timer_cancel.
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
}

// Function pointers are Send + Sync; the vtable is stored in a OnceLock.
unsafe impl Send for pgh_host_vtable {}
unsafe impl Sync for pgh_host_vtable {}
