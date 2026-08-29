//! Per-method host import implementations: every guest call lands in one
//! typed function here. Capability and scope checks happen at the top of
//! each method, before any vtable pointer is touched.
//!
//! Runs while the guest instance is checked out of the registry, so it may
//! only use the vtable and the caller's StoreData (via GuestMem) - never
//! the registry. Vtable calls may re-enter pgh_notify / pgh_async_complete
//! / pgh_mode_event (enqueue-only); that touches the EVENTS cell only,
//! which is safe here.
//!
//! Zero-copy rules: string arguments are validated in place (bounds + NUL
//! at data[len]) and passed to C as raw pointers into linear memory -
//! valid because the guest is frozen for the duration of the call and the
//! only re-entry (give_owned's pgh_alloc) happens after every raw pointer
//! is dead. OutBuf results are written directly into guest memory by the
//! C sink.

use std::ffi::c_void;

use tmux_plugin_abi::{
    ErrorCode, SelfInfo, KIND_CLIENT, KIND_PANE, KIND_SERVER, KIND_SESSION,
    KIND_WINDOW, MAX_MODE_WRITE_BYTES,
};

use crate::abi::{collect_sink, err, out_sink, GuestMem, HostError};
use crate::ffi::{
    pgh_host_vtable, PGH_REL_PANE_IN_SESSION, PGH_REL_PANE_IN_WINDOW,
    PGH_REL_PANE_WINDOW, PGH_REL_SESSION_CURWIN, PGH_REL_WINDOW_IN_SESSION,
};
use crate::intern as interner;
use crate::registry::ScopeId;

fn vtable() -> Result<&'static pgh_host_vtable, HostError> {
    crate::vtable().ok_or_else(|| err(ErrorCode::Host, "host vtable unavailable"))
}

fn check_cap(mem: &GuestMem<'_, '_>, flag: u32) -> Result<(), HostError> {
    if !mem.data().caps.has(flag) {
        return Err(err(
            ErrorCode::CapDenied,
            format!(
                "capability {:?} not granted",
                crate::caps::cap_name(flag)
            ),
        ));
    }
    Ok(())
}

fn check_kind(kind: i32) -> Result<(), HostError> {
    match kind {
        KIND_SERVER | KIND_SESSION | KIND_WINDOW | KIND_PANE | KIND_CLIENT => {
            Ok(())
        }
        other => Err(err(ErrorCode::BadRequest, format!("bad kind {other}"))),
    }
}

fn pane_id(pane: i32) -> Result<u32, HostError> {
    if pane < 0 {
        return Err(err(ErrorCode::BadRequest, "negative pane id"));
    }
    Ok(pane as u32)
}

/// Scope-implied targeting: may this instance touch pane `pane`?
/// Pane-scoped instances may touch only their own pane; window-scoped
/// their window's panes; session-scoped panes in windows linked to their
/// session; server-scoped (or CROSS_SCOPE) may touch anything.
fn check_pane_target(mem: &GuestMem<'_, '_>, pane: u32) -> Result<(), HostError> {
    let data = mem.data();
    if data.caps.has(crate::caps::CROSS_SCOPE) {
        return Ok(());
    }
    let relation = |rel: i32, a: u32, b: u32| -> Result<i64, HostError> {
        let vt = vtable()?;
        Ok(unsafe { (vt.obj_relation)(rel, a, b) })
    };
    let denied = |what: String| Err(err(ErrorCode::OutOfScope, what));

    match data.scope {
        ScopeId::Server => Ok(()),
        ScopeId::Pane(own) if own == pane => Ok(()),
        ScopeId::Pane(own) => denied(format!(
            "pane-scoped instance %{own} may not target pane %{pane}"
        )),
        ScopeId::Window(own) => {
            match relation(PGH_REL_PANE_IN_WINDOW, pane, own)? {
                1 => Ok(()),
                -1 => Err(err(
                    ErrorCode::NoSuchObject,
                    format!("no such pane %{pane}"),
                )),
                _ => denied(format!(
                    "window-scoped instance @{own} may not target pane %{pane}"
                )),
            }
        }
        ScopeId::Session(own) => {
            match relation(PGH_REL_PANE_IN_SESSION, pane, own)? {
                1 => Ok(()),
                -1 => Err(err(
                    ErrorCode::NoSuchObject,
                    format!("no such pane %{pane}"),
                )),
                _ => denied(format!(
                    "session-scoped instance ${own} may not target pane %{pane}"
                )),
            }
        }
    }
}

/// Resolve the window a mode_open/mode_move targets, scope-implied (see
/// ABI.md). `requested` < 0 means "default".
fn mode_target_window(
    mem: &GuestMem<'_, '_>,
    requested: i32,
) -> Result<u32, HostError> {
    let data = mem.data();
    let cross = data.caps.has(crate::caps::CROSS_SCOPE);
    let requested: Option<u32> =
        if requested < 0 { None } else { Some(requested as u32) };
    let relation = |rel: i32, a: u32, b: u32| -> Result<i64, HostError> {
        let vt = vtable()?;
        Ok(unsafe { (vt.obj_relation)(rel, a, b) })
    };
    let denied = |what: String| Err(err(ErrorCode::OutOfScope, what));

    let own_window: Option<u32> = match data.scope {
        ScopeId::Server => None,
        ScopeId::Window(own) => Some(own),
        ScopeId::Pane(own) => match relation(PGH_REL_PANE_WINDOW, own, 0)? {
            id if id >= 0 => Some(id as u32),
            _ => {
                return Err(err(ErrorCode::NoSuchObject, "own pane is gone"))
            }
        },
        ScopeId::Session(own) => match requested {
            // Default: the session's current window.
            None => match relation(PGH_REL_SESSION_CURWIN, own, 0)? {
                id if id >= 0 => Some(id as u32),
                _ => {
                    return Err(err(
                        ErrorCode::NoSuchObject,
                        "session has no current window",
                    ))
                }
            },
            Some(window) => {
                if !cross {
                    match relation(PGH_REL_WINDOW_IN_SESSION, window, own)? {
                        1 => {}
                        -1 => {
                            return Err(err(
                                ErrorCode::NoSuchObject,
                                format!("no such window @{window}"),
                            ))
                        }
                        _ => {
                            return denied(format!(
                                "session-scoped instance ${own} may not open a mode in window @{window}"
                            ))
                        }
                    }
                }
                return Ok(window);
            }
        },
    };

    match (own_window, requested) {
        (Some(own), None) => Ok(own),
        (Some(own), Some(req)) if req == own || cross => Ok(req),
        (Some(own), Some(req)) => denied(format!(
            "instance scoped to window @{own} may not open a mode in window @{req}"
        )),
        (None, Some(req)) => Ok(req),
        (None, None) => Err(err(
            ErrorCode::BadRequest,
            "server-scoped mode targeting requires a window",
        )),
    }
}

// ---------------------------------------------------------------------------
// Interning and subscriptions.
// ---------------------------------------------------------------------------

pub fn intern(
    mem: &mut GuestMem<'_, '_>,
    ptr: i32,
    len: i32,
) -> Result<i64, HostError> {
    // Host-consumed only (never crosses into C), so this is raw UTF-8
    // bytes with no NUL convention.
    let bytes = mem.read(ptr, len)?;
    let name = String::from_utf8(bytes)
        .map_err(|_| err(ErrorCode::BadRequest, "invalid UTF-8 name"))?;
    if name.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty name"));
    }
    Ok(i64::from(interner::intern(&name)))
}

pub fn intern_name(
    mem: &mut GuestMem<'_, '_>,
    id: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    let name = interner::name_of(id.max(0) as u32)
        .ok_or_else(|| err(ErrorCode::NoSuchObject, format!("unknown id {id}")))?;
    mem.write_out(name.as_bytes(), out, cap, len_out)
}

pub fn subscribe(
    mem: &mut GuestMem<'_, '_>,
    id: i32,
    add: bool,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    if id <= 0 {
        return Err(err(ErrorCode::BadRequest, "bad event id"));
    }
    let subs = &mut mem.data_mut().subscriptions;
    if add {
        subs.insert(id as u32);
    } else {
        subs.remove(&(id as u32));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Object state.
// ---------------------------------------------------------------------------

pub fn list(
    mem: &mut GuestMem<'_, '_>,
    kind: i32,
    owned_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    check_kind(kind)?;
    if kind == KIND_SERVER {
        return Err(err(ErrorCode::BadRequest, "cannot list the server"));
    }
    let vt = vtable()?;
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        (vt.list_objects)(kind, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    mem.give_owned(&buf, owned_out)
}

pub fn resolve(
    mem: &mut GuestMem<'_, '_>,
    kind: i32,
    id: i32,
    owned_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    check_kind(kind)?;
    if kind == KIND_SERVER || id < 0 {
        return Err(err(ErrorCode::BadRequest, "bad resolve target"));
    }
    let vt = vtable()?;
    let mut buf: Vec<u8> = Vec::new();
    let rc = unsafe {
        (vt.resolve_object)(
            kind,
            id as u32,
            collect_sink,
            &mut buf as *mut Vec<u8> as *mut c_void,
        )
    };
    if rc != 0 {
        return Err(err(
            ErrorCode::NoSuchObject,
            format!("no such object id {id}"),
        ));
    }
    mem.give_owned(&buf, owned_out)
}

pub fn self_info(mem: &mut GuestMem<'_, '_>, out: i32) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    let data = mem.data();
    let (scope_kind, scope_id) = match data.scope {
        ScopeId::Server => (KIND_SERVER, 0),
        ScopeId::Session(id) => (KIND_SESSION, id),
        ScopeId::Window(id) => (KIND_WINDOW, id),
        ScopeId::Pane(id) => (KIND_PANE, id),
    };
    let info = SelfInfo { scope_kind, scope_id, generation: data.generation };
    mem.write_at(out, &info.to_bytes())
}

pub fn get_option(
    mem: &mut GuestMem<'_, '_>,
    kind: i32,
    id: i32,
    name_ptr: i32,
    name_len: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    check_kind(kind)?;
    let vt = vtable()?;
    let name = mem.c_str(name_ptr, name_len)?;
    let mut sink = mem.out_sink(out, cap)?;
    let rc = unsafe {
        (vt.get_option)(
            kind,
            id.max(0) as u32,
            name,
            out_sink,
            &mut sink as *mut _ as *mut c_void,
        )
    };
    match rc {
        0 => mem.finish_out(sink, len_out),
        -2 => Err(err(ErrorCode::NoSuchObject, "no such option")),
        _ => Err(err(ErrorCode::NoSuchObject, "no such target")),
    }
}

pub fn format_expand(
    mem: &mut GuestMem<'_, '_>,
    kind: i32,
    id: i32,
    fmt_ptr: i32,
    fmt_len: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::READ_STATE)?;
    if kind == KIND_CLIENT {
        return Err(err(ErrorCode::BadRequest, "bad format scope"));
    }
    check_kind(kind)?;
    let vt = vtable()?;
    let fmt = mem.c_str(fmt_ptr, fmt_len)?;
    let mut sink = mem.out_sink(out, cap)?;
    let rc = unsafe {
        (vt.format_expand)(
            kind,
            id.max(0) as u32,
            fmt,
            out_sink,
            &mut sink as *mut _ as *mut c_void,
        )
    };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, "no such target"));
    }
    mem.finish_out(sink, len_out)
}

pub fn set_option(
    mem: &mut GuestMem<'_, '_>,
    kind: i32,
    id: i32,
    name_ptr: i32,
    name_len: i32,
    val_ptr: i32,
    val_len: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::WRITE_OPTIONS)?;
    check_kind(kind)?;
    let vt = vtable()?;
    let name = mem.c_str(name_ptr, name_len)?;
    let value = mem.c_str(val_ptr, val_len)?;
    let rc = unsafe { (vt.set_option)(kind, id.max(0) as u32, name, value) };
    match rc {
        0 => Ok(()),
        -2 => Err(err(
            ErrorCode::Unsupported,
            "only @-prefixed user options can be set directly",
        )),
        _ => Err(err(ErrorCode::NoSuchObject, "no such target")),
    }
}

pub fn send_keys(
    mem: &mut GuestMem<'_, '_>,
    pane: i32,
    keys_ptr: i32,
    keys_len: i32,
    literal: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::SEND_KEYS)?;
    let pane = pane_id(pane)?;
    check_pane_target(mem, pane)?;
    let vt = vtable()?;
    let keys = mem.c_str(keys_ptr, keys_len)?;
    let rc = unsafe { (vt.send_keys)(pane, keys, literal) };
    match rc {
        0 => Ok(()),
        -2 => Err(err(ErrorCode::BadRequest, "bad key name")),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such pane %{pane}"))),
    }
}

pub fn capture_pane(
    mem: &mut GuestMem<'_, '_>,
    pane: i32,
    start: i32,
    end: i32,
    escapes: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::CAPTURE_PANE)?;
    let pane = pane_id(pane)?;
    check_pane_target(mem, pane)?;
    let vt = vtable()?;
    let mut sink = mem.out_sink(out, cap)?;
    let rc = unsafe {
        (vt.capture_pane)(
            pane,
            start,
            end,
            escapes,
            out_sink,
            &mut sink as *mut _ as *mut c_void,
        )
    };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, format!("no such pane %{pane}")));
    }
    mem.finish_out(sink, len_out)
}

pub fn display_message(
    mem: &mut GuestMem<'_, '_>,
    client: i32,
    msg_ptr: i32,
    msg_len: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::DISPLAY_MESSAGE)?;
    let vt = vtable()?;
    let plugin = std::ffi::CString::new(mem.data().plugin.clone())
        .map_err(|_| err(ErrorCode::Host, "bad plugin name"))?;
    let msg = mem.c_str(msg_ptr, msg_len)?;
    let rc = unsafe { (vt.display_message)(client, plugin.as_ptr(), msg) };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, "no such attached client"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// UI modes.
// ---------------------------------------------------------------------------

fn check_mode(mem: &GuestMem<'_, '_>, mode: i64) -> Result<u64, HostError> {
    if mode <= 0 {
        return Err(err(ErrorCode::BadRequest, "bad mode id"));
    }
    let mode = mode as u64;
    if !crate::modes::owned_by(mode, mem.data()) {
        return Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}")));
    }
    Ok(mode)
}

#[allow(clippy::too_many_arguments)]
pub fn mode_open(
    mem: &mut GuestMem<'_, '_>,
    window: i32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    title_ptr: i32,
    title_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::MODE)?;
    if width <= 0 || height <= 0 {
        return Err(err(ErrorCode::BadRequest, "zero mode size"));
    }
    let window = mode_target_window(mem, window)?;
    let vt = vtable()?;
    let title = mem.c_str_opt(title_ptr, title_len)?;
    let rc = unsafe {
        (vt.mode_open)(
            window,
            width as u32,
            height as u32,
            x.max(-1),
            y.max(-1),
            title.unwrap_or(std::ptr::null()),
        )
    };
    match rc {
        id if id > 0 => {
            let data = mem.data();
            crate::modes::register(
                id as u64,
                &data.plugin,
                data.scope,
                data.generation,
            );
            Ok(id)
        }
        -1 => Err(err(
            ErrorCode::NoSuchObject,
            format!("no such window @{window}"),
        )),
        -3 => Err(err(ErrorCode::Host, "mode init failed")),
        _ => Err(err(ErrorCode::Host, "failed to spawn mode pane")),
    }
}

pub fn mode_write(
    mem: &mut GuestMem<'_, '_>,
    mode: i64,
    ptr: i32,
    len: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::MODE)?;
    let mode = check_mode(mem, mode)?;
    if len as usize > MAX_MODE_WRITE_BYTES {
        return Err(err(
            ErrorCode::Limit,
            format!("mode_write exceeds {MAX_MODE_WRITE_BYTES} bytes"),
        ));
    }
    let vt = vtable()?;
    let (data, n) = mem.bytes(ptr, len)?;
    let rc = unsafe { (vt.mode_write)(mode, data, n) };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}")));
    }
    Ok(())
}

pub fn mode_preview(
    mem: &mut GuestMem<'_, '_>,
    mode: i64,
    pane: i64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::MODE)?;
    let mode = check_mode(mem, mode)?;
    let pane = if pane < 0 {
        -1
    } else {
        // Previewing mirrors another pane's content; apply the same
        // scope-implied targeting as capture_pane.
        check_pane_target(mem, pane as u32)?;
        if w <= 0 || h <= 0 {
            return Err(err(ErrorCode::BadRequest, "zero preview size"));
        }
        pane
    };
    let vt = vtable()?;
    let rc = unsafe {
        (vt.mode_preview)(
            mode,
            pane,
            x.max(0) as u32,
            y.max(0) as u32,
            w.max(0) as u32,
            h.max(0) as u32,
        )
    };
    match rc {
        0 => Ok(()),
        -2 => Err(err(
            ErrorCode::BadRequest,
            "preview rect does not fit the mode screen",
        )),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}"))),
    }
}

pub fn mode_move(
    mem: &mut GuestMem<'_, '_>,
    mode: i64,
    window: i32,
    x: i32,
    y: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::MODE)?;
    let mode = check_mode(mem, mode)?;
    let window = mode_target_window(mem, window)?;
    let vt = vtable()?;
    let rc = unsafe { (vt.mode_move)(mode, window, x.max(-1), y.max(-1)) };
    match rc {
        0 => Ok(()),
        -2 => Err(err(
            ErrorCode::NoSuchObject,
            format!("no such window @{window} (or unmovable pane)"),
        )),
        -3 => Err(err(
            ErrorCode::Limit,
            "move would empty the source window; close instead",
        )),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}"))),
    }
}

pub fn mode_resize(
    mem: &mut GuestMem<'_, '_>,
    mode: i64,
    width: i32,
    height: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::MODE)?;
    let mode = check_mode(mem, mode)?;
    if width <= 0 || height <= 0 {
        return Err(err(ErrorCode::BadRequest, "zero mode size"));
    }
    let vt = vtable()?;
    let rc = unsafe { (vt.mode_resize)(mode, width as u32, height as u32) };
    match rc {
        0 => Ok(()),
        -2 => Err(err(
            ErrorCode::Limit,
            "window is too small for a floating pane",
        )),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}"))),
    }
}

pub fn mode_close(mem: &mut GuestMem<'_, '_>, mode: i64) -> Result<(), HostError> {
    check_cap(mem, crate::caps::MODE)?;
    let mode = check_mode(mem, mode)?;
    let vt = vtable()?;
    let rc = unsafe { (vt.mode_close)(mode) };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, format!("no such mode {mode}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Async methods: start the operation and return the token whose completion
// arrives via pgh_on_async_complete.
// ---------------------------------------------------------------------------

pub fn timer_cancel(mem: &mut GuestMem<'_, '_>, token: i64) -> Result<(), HostError> {
    check_cap(mem, crate::caps::TIMERS)?;
    if token <= 0 {
        return Err(err(ErrorCode::BadRequest, "bad token"));
    }
    let vt = vtable()?;
    // Taking the token also guarantees a raced, already-queued completion
    // is dropped at drain time.
    if let Some(pending) = crate::tokens::take(token as u64) {
        if let Some(id) = pending.timer_id {
            unsafe { (vt.timer_cancel)(id) };
        }
    }
    Ok(())
}

fn alloc_token(mem: &GuestMem<'_, '_>) -> u64 {
    let data = mem.data();
    crate::tokens::allocate(&data.plugin, data.scope, data.generation)
}

pub fn run_job(
    mem: &mut GuestMem<'_, '_>,
    cmd_ptr: i32,
    cmd_len: i32,
    cwd_ptr: i32,
    cwd_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::RUN_PROCESS)?;
    // Advisory argv0 allowlist (run_job is a shell string): check the
    // first token's basename. Only pays a copy when a list is configured.
    if !mem.data().caps.argv0_allow.is_empty() {
        let cmd = mem.read_str(cmd_ptr, cmd_len)?;
        let argv0 = cmd
            .split_whitespace()
            .next()
            .map(|t| t.rsplit('/').next().unwrap_or(t))
            .unwrap_or("");
        let allowed =
            mem.data().caps.argv0_allow.iter().any(|a| a == argv0);
        if !allowed {
            return Err(err(
                ErrorCode::CapDenied,
                format!("command {argv0:?} not in argv0 allowlist"),
            ));
        }
    }
    let vt = vtable()?;
    let cmd = mem.c_str(cmd_ptr, cmd_len)?;
    let cwd = mem.c_str_opt(cwd_ptr, cwd_len)?;
    let token = alloc_token(mem);
    let rc = unsafe {
        (vt.run_job)(cmd, cwd.unwrap_or(std::ptr::null()), token)
    };
    if rc != 0 {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, "failed to start job"));
    }
    Ok(token as i64)
}

pub fn run_command(
    mem: &mut GuestMem<'_, '_>,
    cmd_ptr: i32,
    cmd_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::RUN_COMMAND)?;
    let vt = vtable()?;
    let cmd = mem.c_str(cmd_ptr, cmd_len)?;
    let token = alloc_token(mem);
    let rc = unsafe { (vt.run_command)(cmd, token) };
    if rc != 0 {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, "failed to queue command"));
    }
    Ok(token as i64)
}

// ---------------------------------------------------------------------------
// Filesystem: sandboxed to the plugin's data directory. Paths are raw
// UTF-8 (host-consumed, no NUL rule). The async pair runs on the fs
// worker thread with pinned guest buffers (zero-copy); the sync pair
// blocks the loop for one bounded page-cache access, like tmux's own
// file I/O.
//
// No per-call byte cap: every transfer names a buffer inside the guest's
// own linear memory, which the bounds check validates and the store's
// memory limit bounds, and no path copies through a host allocation.
// Two limits remain, and they are the caller's to respect: a sync call
// that runs long burns the instance's wall-clock CPU budget (the epoch
// deadline trips once guest code resumes), and an async call holds off
// instance teardown until the worker finishes.
// ---------------------------------------------------------------------------

fn fs_err(e: crate::fsbox::FsError) -> HostError {
    let code = e.code();
    err(code, e.message())
}

/// The plugin's sandbox root, resolved once per plugin and cached.
fn fs_root_of(
    mem: &GuestMem<'_, '_>,
) -> Result<std::sync::Arc<crate::fsbox::Root>, HostError> {
    crate::fsbox::root_for(&mem.data().plugin).map_err(fs_err)
}

/// How far this plugin's paths may resolve for a read-ish or a write-ish
/// operation. Without the `-any` grant a path stays inside the plugin's
/// data directory; with it, the directory behaves like a process cwd and
/// an absolute path means what it says.
fn fs_reach(mem: &GuestMem<'_, '_>, escape_cap: u32) -> crate::fsbox::Reach {
    if mem.data().caps.has(escape_cap) {
        crate::fsbox::Reach::Anywhere
    } else {
        crate::fsbox::Reach::Sandbox
    }
}

/// The guest's relative path, as an owned String (validated inside fsbox).
fn fs_rel(
    mem: &GuestMem<'_, '_>,
    ptr: i32,
    len: i32,
) -> Result<String, HostError> {
    let bytes = mem.read(ptr, len)?;
    String::from_utf8(bytes)
        .map_err(|_| err(ErrorCode::BadRequest, "invalid UTF-8 path"))
}

pub fn fs_write_async(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
    data_ptr: i32,
    data_len: i32,
    append: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::FS_WRITE)?;
    let reach = fs_reach(mem, crate::caps::FS_WRITE_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let ptr = mem.pinned_bytes(data_ptr, data_len)?;
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let job = crate::fsworker::FsJob::Write {
        token,
        key,
        root,
        rel,
        append: append != 0,
        reach,
        data: crate::fsworker::GuestSlice { ptr, len: data_len as usize },
    };
    if let Err(e) = crate::fsworker::submit(job) {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, e));
    }
    Ok(token as i64)
}

pub fn fs_read_async(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
    offset: i64,
    out_ptr: i32,
    out_cap: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::FS_READ)?;
    if offset < 0 {
        return Err(err(ErrorCode::BadRequest, "negative offset"));
    }
    let reach = fs_reach(mem, crate::caps::FS_READ_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let ptr = mem.pinned_bytes_mut(out_ptr, out_cap)?;
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let job = crate::fsworker::FsJob::Read {
        token,
        key,
        root,
        rel,
        offset: offset as u64,
        reach,
        out: crate::fsworker::GuestSliceMut { ptr, cap: out_cap as usize },
    };
    if let Err(e) = crate::fsworker::submit(job) {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, e));
    }
    Ok(token as i64)
}

/// List a directory into the guest's pinned buffer. Async on the fs
/// worker, so a slow or huge directory never stalls the event loop.
/// Completion: `v0` = bytes written, `v1` = entries the directory holds
/// (more than fit means the guest should retry with a bigger buffer).
pub fn fs_list_async(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
    flags: i32,
    out_ptr: i32,
    out_cap: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::FS_LIST)?;
    if out_cap <= 0 {
        return Err(err(ErrorCode::BadRequest, "zero output buffer"));
    }
    let reach = fs_reach(mem, crate::caps::FS_READ_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let ptr = mem.pinned_bytes_mut(out_ptr, out_cap)?;
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let job = crate::fsworker::FsJob::List {
        token,
        key,
        root,
        rel,
        reach,
        flags: flags.max(0) as u32,
        out: crate::fsworker::GuestSliceMut { ptr, cap: out_cap as usize },
    };
    if let Err(e) = crate::fsworker::submit(job) {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, e));
    }
    Ok(token as i64)
}

pub fn fs_write_sync(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
    data_ptr: i32,
    data_len: i32,
    append: i32,
) -> Result<i64, HostError> {
    use std::io::Write as _;

    check_cap(mem, crate::caps::FS_WRITE)?;
    let reach = fs_reach(mem, crate::caps::FS_WRITE_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let mut file =
        crate::fsbox::open_write(&root, &rel, append != 0, reach).map_err(fs_err)?;
    // Write straight out of guest memory: no host copy sized by the
    // guest. Nothing re-enters the guest while the borrow is live.
    let bytes = mem.byte_slice(data_ptr, data_len)?;
    file.write_all(bytes)
        .map_err(|e| err(ErrorCode::Host, format!("{rel}: {e}")))?;
    Ok(bytes.len() as i64)
}

#[allow(clippy::too_many_arguments)]
pub fn fs_read_sync(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
    offset: i64,
    out: i32,
    cap: i32,
    len_out: i32,
    eof_out: i32,
) -> Result<(), HostError> {
    use std::io::{Read as _, Seek as _};

    check_cap(mem, crate::caps::FS_READ)?;
    if offset < 0 {
        return Err(err(ErrorCode::BadRequest, "negative offset"));
    }
    let reach = fs_reach(mem, crate::caps::FS_READ_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let mut file = crate::fsbox::open_read(&root, &rel, reach).map_err(fs_err)?;
    // Read straight into the guest's out-buffer: no host copy at all, and
    // the bounds check happens before anything is sized by the guest.
    // On an I/O error the guest buffer may hold a partial read - the call
    // returns an error and never writes len_out, so the guest must not
    // read it (the SDK clears its buffer on error).
    let (read, eof) = {
        let dst = mem.out_bytes_mut(out, cap)?;
        (|| -> std::io::Result<(usize, bool)> {
            let size = file.metadata()?.len();
            file.seek(std::io::SeekFrom::Start(offset as u64))?;
            let mut read = 0;
            while read < dst.len() {
                let n = file.read(&mut dst[read..])?;
                if n == 0 {
                    break;
                }
                read += n;
            }
            let eof = (offset as u64).saturating_add(read as u64) >= size;
            Ok((read, eof))
        })()
        .map_err(|e| err(ErrorCode::Host, format!("{rel}: {e}")))?
    };
    mem.write_u32_at(eof_out, u32::from(eof))?;
    mem.write_u32_at(len_out, read as u32)?;
    Ok(())
}

/// The server user's home directory.
///
/// A guest has no environment - core wasm with no WASI means no getenv -
/// so `~` in a path is unexpandable without asking. Without this a plugin
/// forks a shell to print one constant. No capability: it is a path, and
/// the plugin's own data directory normally sits under it.
pub fn home_dir(
    mem: &mut GuestMem<'_, '_>,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    let home = std::env::var_os("HOME")
        .map(|h| h.to_string_lossy().into_owned())
        .ok_or_else(|| err(ErrorCode::NoSuchObject, "HOME is not set"))?;
    mem.write_out(home.as_bytes(), out, cap, len_out)
}

pub fn fs_root(
    mem: &mut GuestMem<'_, '_>,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    let root = fs_root_of(mem)?;
    let text = root.path().to_string_lossy().into_owned();
    mem.write_out(text.as_bytes(), out, cap, len_out)
}

pub fn timer_start(mem: &mut GuestMem<'_, '_>, ms: i64) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::TIMERS)?;
    if ms < 0 {
        return Err(err(ErrorCode::BadRequest, "negative delay"));
    }
    let vt = vtable()?;
    let token = alloc_token(mem);
    let id = unsafe { (vt.timer_start)(ms as u64, token) };
    crate::tokens::set_timer_id(token, id);
    Ok(token as i64)
}
