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

pub fn pane_env(
    mem: &mut GuestMem<'_, '_>,
    pane: i32,
    name_ptr: i32,
    name_len: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    // Either cap grants the call; env-read-any also lifts the allowlist.
    let any = mem.data().caps.has(crate::caps::ENV_READ_ANY);
    if !any && !mem.data().caps.has(crate::caps::ENV_READ) {
        return Err(err(ErrorCode::CapDenied, "env-read not granted"));
    }
    let pane = pane_id(pane)?;
    check_pane_target(mem, pane)?;
    // env-read-any lifts the allowlist. Otherwise, when an allowlist is
    // configured the name must be on it; an empty list means unrestricted
    // (as with run_job's argv0 list under trust-the-user).
    if !any && !mem.data().caps.env_allow.is_empty() {
        let name = mem.read_str(name_ptr, name_len)?;
        let allowed =
            mem.data().caps.env_allow.iter().any(|n| *n == name);
        if !allowed {
            return Err(err(
                ErrorCode::CapDenied,
                format!("env var {name:?} not in env-read allowlist"),
            ));
        }
    }
    let vt = vtable()?;
    let name = mem.c_str(name_ptr, name_len)?;
    let mut sink = mem.out_sink(out, cap)?;
    let rc = unsafe {
        (vt.pane_env)(pane, name, out_sink, &mut sink as *mut _ as *mut c_void)
    };
    match rc {
        0 => mem.finish_out(sink, len_out),
        -2 => Err(err(
            ErrorCode::NoSuchObject,
            "no such environment variable",
        )),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such pane %{pane}"))),
    }
}

pub fn pane_fds(
    mem: &mut GuestMem<'_, '_>,
    pane: i32,
    out: i32,
    cap: i32,
    len_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::PANE_FDS)?;
    let pane = pane_id(pane)?;
    check_pane_target(mem, pane)?;
    let vt = vtable()?;
    let mut sink = mem.out_sink(out, cap)?;
    let rc = unsafe {
        (vt.pane_fds)(pane, out_sink, &mut sink as *mut _ as *mut c_void)
    };
    match rc {
        // -2 means no file-backed fds: the sink stays empty, so finish_out
        // reports a zero-length result, not an error.
        0 | -2 => mem.finish_out(sink, len_out),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such pane %{pane}"))),
    }
}

/// Grep the grids of a set of panes for a pattern. The needle and the
/// pane-id array cross the ABI; the pane contents never do - the search
/// runs in C over the live grid. The result is a `u32 count`-prefixed
/// list of `{pane, line, col, snippet}` records (see `search_flags`).
/// Reuses the `capture-pane` cap: reading a match is no more than
/// capturing the pane would already allow.
pub fn panes_search(
    mem: &mut GuestMem<'_, '_>,
    ids_ptr: i32,
    ids_len: i32,
    pat_ptr: i32,
    pat_len: i32,
    flags: i32,
    max_lines: i32,
    owned_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::CAPTURE_PANE)?;
    if ids_len < 0 || max_lines < 0 {
        return Err(err(ErrorCode::BadRequest, "negative length"));
    }
    let nbytes = (ids_len as usize)
        .checked_mul(4)
        .ok_or_else(|| err(ErrorCode::BadRequest, "ids array overflow"))?;
    // Copy the id array out (owned; survives the give_owned re-entry).
    let raw = mem.read(ids_ptr, nbytes as i32)?;
    let ids: Vec<u32> = raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    // Borrowed NUL-terminated needle: valid until the next guest re-entry,
    // which happens only in give_owned, after the C search returns.
    let pattern = mem.c_str(pat_ptr, pat_len)?;
    let vt = vtable()?;
    let mut buf: Vec<u8> = Vec::new();
    let n = unsafe {
        (vt.panes_search)(
            ids.as_ptr(),
            ids.len() as u32,
            pattern,
            flags as u32,
            max_lines as u32,
            collect_sink,
            &mut buf as *mut Vec<u8> as *mut c_void,
        )
    };
    if n < 0 {
        return Err(err(
            ErrorCode::BadRequest,
            "panes_search failed (unsupported flag?)",
        ));
    }
    // The C side streams the records; prepend the count the wire list wants.
    let mut out = (n as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&buf);
    mem.give_owned(&out, owned_out)
}

pub fn pane_pid(
    mem: &mut GuestMem<'_, '_>,
    pane: i32,
) -> Result<i64, HostError> {
    // Just an integer, no more sensitive than the pane info a plugin
    // already reads, so scope targeting is the only gate.
    let pane = pane_id(pane)?;
    check_pane_target(mem, pane)?;
    let vt = vtable()?;
    let pid = unsafe { (vt.pane_pid)(pane) };
    if pid <= 0 {
        return Err(err(ErrorCode::NoSuchObject, format!("no such pane %{pane}")));
    }
    Ok(i64::from(pid))
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

/// How far a READ may reach for this plugin, and enforce the scoped grant
/// along the way. `fs-read-any` reaches anywhere; a bare `fs-read` with a
/// `[caps.fs-read] paths` list reaches those prefixes (checked here, so a
/// path outside them is denied before any worker sees it); otherwise the
/// read stays in the sandbox.
fn read_reach(
    mem: &GuestMem<'_, '_>,
    root: &crate::fsbox::Root,
    rel: &str,
) -> Result<crate::fsbox::Reach, HostError> {
    if mem.data().caps.has(crate::caps::FS_READ_ANY) {
        return Ok(crate::fsbox::Reach::Anywhere);
    }
    let allow = &mem.data().caps.fs_allow;
    if !allow.is_empty() {
        crate::fsbox::allowed_read(root, rel, allow).map_err(fs_err)?;
        return Ok(crate::fsbox::Reach::Anywhere);
    }
    Ok(crate::fsbox::Reach::Sandbox)
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
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let reach = read_reach(mem, &root, &rel)?;
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
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let reach = read_reach(mem, &root, &rel)?;
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

/// Rename `from` to `to` on the fs worker. Atomic within one
/// filesystem; `flags` selects plain replace, no-replace or exchange
/// (see fsworker's RENAME_* wire values). Gated like a write.
pub fn fs_rename_async(
    mem: &mut GuestMem<'_, '_>,
    from_ptr: i32,
    from_len: i32,
    to_ptr: i32,
    to_len: i32,
    flags: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::FS_WRITE)?;
    if !(0..=2).contains(&flags) {
        return Err(err(ErrorCode::BadRequest, "bad rename flags"));
    }
    let reach = fs_reach(mem, crate::caps::FS_WRITE_ANY);
    let root = fs_root_of(mem)?;
    let rel_from = fs_rel(mem, from_ptr, from_len)?;
    let rel_to = fs_rel(mem, to_ptr, to_len)?;
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let job = crate::fsworker::FsJob::Rename {
        token,
        key,
        root,
        rel_from,
        rel_to,
        flags: flags as u32,
        reach,
    };
    if let Err(e) = crate::fsworker::submit(job) {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, e));
    }
    Ok(token as i64)
}

/// Unlink `path` on the fs worker. Gated like a write: removing a file
/// is a write to its directory.
pub fn fs_remove_async(
    mem: &mut GuestMem<'_, '_>,
    path_ptr: i32,
    path_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::FS_WRITE)?;
    let reach = fs_reach(mem, crate::caps::FS_WRITE_ANY);
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let job = crate::fsworker::FsJob::Remove { token, key, root, rel, reach };
    if let Err(e) = crate::fsworker::submit(job) {
        crate::tokens::discard(token);
        return Err(err(ErrorCode::Host, e));
    }
    Ok(token as i64)
}

/// Unix time in milliseconds. No capability: every process can read the
/// clock, and a plugin already observes time through its timers.
pub fn time_now(_mem: &GuestMem<'_, '_>) -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    let root = fs_root_of(mem)?;
    let rel = fs_rel(mem, path_ptr, path_len)?;
    let reach = read_reach(mem, &root, &rel)?;
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

// ---------------------------------------------------------------------------
// Database: the plugin's own SQLite file, `store.db` in its data
// directory (see sqlite.rs). SQL and parameter blocks are raw bytes
// (host-consumed, no NUL rule) and are COPIED out of guest memory at call
// time, so the async pair pins nothing: the statement runs on a worker
// thread against host-owned copies and the rows come back as completion
// data. The sync pair runs on the main thread with a 500 ms cap, for
// `init` migrations and tiny reads.
// ---------------------------------------------------------------------------

/// The SQL text, copied and UTF-8 checked. Empty is a BadRequest here
/// rather than a SQLite error, for the message.
fn db_sql(mem: &GuestMem<'_, '_>, ptr: i32, len: i32) -> Result<String, HostError> {
    if len <= 0 {
        return Err(err(ErrorCode::BadRequest, "empty SQL statement"));
    }
    let bytes = mem.read(ptr, len)?;
    String::from_utf8(bytes)
        .map_err(|_| err(ErrorCode::BadRequest, "SQL is not valid UTF-8"))
}

/// The bound parameters. `ptr 0, len 0` (or any zero-length block) means
/// none, which also permits a multi-statement script.
fn db_params(
    mem: &GuestMem<'_, '_>,
    ptr: i32,
    len: i32,
) -> Result<Vec<tmux_plugin_abi::db::DbValue>, HostError> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let bytes = mem.read(ptr, len)?;
    tmux_plugin_abi::db::decode_params(&bytes)
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad params block: {e}")))
}

/// Request size cap, checked before anything is copied.
fn db_check_size(len: i32, extra: i32) -> Result<(), HostError> {
    let total = len.max(0) as usize + extra.max(0) as usize;
    if total > tmux_plugin_abi::MAX_DB_REQUEST_BYTES {
        return Err(err(
            ErrorCode::Limit,
            format!(
                "request is {total} bytes, the limit is {}",
                tmux_plugin_abi::MAX_DB_REQUEST_BYTES
            ),
        ));
    }
    Ok(())
}

/// Reject ZSTD_REF parameters where they are not supported: the sync
/// imports (compression belongs on a worker thread) and `db_query`.
fn db_reject_refs(params: &[tmux_plugin_abi::db::DbValue]) -> Result<(), HostError> {
    if params.iter().any(|p| matches!(p, tmux_plugin_abi::db::DbValue::ZstdRef { .. })) {
        return Err(err(
            ErrorCode::BadRequest,
            "ZSTD_REF parameters are accepted by db_exec and db_batch only",
        ));
    }
    Ok(())
}

/// Resolve one ZSTD_REF into a pinned guest slice. Bounds-checked against
/// linear memory here; the worker reads it in place under a pinned guard.
fn db_zstd_slice(
    mem: &GuestMem<'_, '_>,
    ptr: u32,
    len: u32,
) -> Result<crate::fsworker::GuestSlice, HostError> {
    if len as usize > tmux_plugin_abi::MAX_DB_ZSTD_RAW_BYTES {
        return Err(err(
            ErrorCode::Limit,
            format!(
                "ZSTD_REF is {len} bytes, the limit is {}",
                tmux_plugin_abi::MAX_DB_ZSTD_RAW_BYTES
            ),
        ));
    }
    let (Ok(p), Ok(l)) = (i32::try_from(ptr), i32::try_from(len)) else {
        return Err(err(ErrorCode::BadRequest, "ZSTD_REF outside linear memory"));
    };
    let ptr = mem.pinned_bytes(p, l)?;
    Ok(crate::fsworker::GuestSlice { ptr, len: len as usize })
}

/// Collect the ZSTD_REF parameters of a job as pinned inputs. Empty for
/// a job without any, which then runs detached as before.
fn db_zstd_inputs(
    mem: &GuestMem<'_, '_>,
    job: &crate::sqlite::DbJob,
) -> Result<Vec<crate::sqlite::ZstdInput>, HostError> {
    use tmux_plugin_abi::db::DbValue;
    let mut out = Vec::new();
    let mut visit = |stmt: Option<usize>, params: &[DbValue]| -> Result<(), HostError> {
        for (i, p) in params.iter().enumerate() {
            if let DbValue::ZstdRef { ptr, len } = p {
                let src = db_zstd_slice(mem, *ptr, *len)?;
                out.push(crate::sqlite::ZstdInput { stmt, param: i, src });
            }
        }
        Ok(())
    };
    match job {
        crate::sqlite::DbJob::Exec { params, .. } => visit(None, params)?,
        crate::sqlite::DbJob::Batch { stmts } => {
            for (i, s) in stmts.iter().enumerate() {
                visit(Some(i), &s.params)?;
            }
        }
        crate::sqlite::DbJob::Query { params, .. } => db_reject_refs(params)?,
    }
    Ok(out)
}

/// Start one async statement task: allocate the token, take the in-flight
/// guards (detached always, pinned when ZSTD_REF inputs exist), hand the
/// job to sqlite::submit.
fn db_start(
    mem: &mut GuestMem<'_, '_>,
    job: crate::sqlite::DbJob,
) -> Result<i64, HostError> {
    let zstd = db_zstd_inputs(mem, &job)?;
    let root = fs_root_of(mem)?;
    let handle = crate::sqlite::handle_for(&mem.data().plugin, &root);
    let data = mem.data();
    let key = (data.plugin.clone(), data.scope, data.generation);
    let token = alloc_token(mem);
    let guard = match crate::worker::track(key.clone(), false) {
        Ok(g) => g,
        Err(e) => {
            crate::tokens::discard(token);
            return Err(err(ErrorCode::Host, e));
        }
    };
    let pinned = if zstd.is_empty() {
        None
    } else {
        match crate::worker::track(key, true) {
            Ok(g) => Some(g),
            Err(e) => {
                crate::tokens::discard(token);
                return Err(err(ErrorCode::Host, e));
            }
        }
    };
    crate::sqlite::submit(
        &handle,
        crate::sqlite::DbRequest { token, guard, pinned, zstd, job },
    );
    Ok(token as i64)
}

/// Inflate a BLOB that was stored from a ZSTD_REF parameter. Sync; the
/// result is an OwnedBuf in guest memory.
pub fn db_decompress(
    mem: &mut GuestMem<'_, '_>,
    src_ptr: i32,
    src_len: i32,
    owned_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::DB)?;
    let raw = {
        let src = mem.byte_slice(src_ptr, src_len)?;
        crate::sqlite::decompress(src)?
    };
    // The borrow of guest memory ended above; give_owned may re-enter
    // the guest allocator now.
    mem.give_owned(&raw, owned_out)
}

pub fn db_exec_async(
    mem: &mut GuestMem<'_, '_>,
    sql_ptr: i32,
    sql_len: i32,
    params_ptr: i32,
    params_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::DB)?;
    db_check_size(sql_len, params_len)?;
    let sql = db_sql(mem, sql_ptr, sql_len)?;
    let params = db_params(mem, params_ptr, params_len)?;
    db_start(mem, crate::sqlite::DbJob::Exec { sql, params })
}

pub fn db_query_async(
    mem: &mut GuestMem<'_, '_>,
    sql_ptr: i32,
    sql_len: i32,
    params_ptr: i32,
    params_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::DB)?;
    db_check_size(sql_len, params_len)?;
    let sql = db_sql(mem, sql_ptr, sql_len)?;
    let params = db_params(mem, params_ptr, params_len)?;
    db_start(mem, crate::sqlite::DbJob::Query { sql, params })
}

pub fn db_batch_async(
    mem: &mut GuestMem<'_, '_>,
    block_ptr: i32,
    block_len: i32,
) -> Result<i64, HostError> {
    check_cap(mem, crate::caps::DB)?;
    db_check_size(block_len, 0)?;
    let bytes = mem.read(block_ptr, block_len)?;
    let stmts = tmux_plugin_abi::db::decode_batch(&bytes)
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad batch block: {e}")))?;
    if stmts.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty batch"));
    }
    db_start(mem, crate::sqlite::DbJob::Batch { stmts })
}

pub fn db_exec_sync(
    mem: &mut GuestMem<'_, '_>,
    sql_ptr: i32,
    sql_len: i32,
    params_ptr: i32,
    params_len: i32,
    out_ptr: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::DB)?;
    db_check_size(sql_len, params_len)?;
    let sql = db_sql(mem, sql_ptr, sql_len)?;
    let params = db_params(mem, params_ptr, params_len)?;
    db_reject_refs(&params)?;
    let root = fs_root_of(mem)?;
    let result = crate::sqlite::with_sync(&mem.data().plugin, &root, |conn| {
        crate::sqlite::exec(conn, &sql, &params)
    })?;
    mem.write_at(out_ptr, &result.to_bytes())
}

pub fn db_query_sync(
    mem: &mut GuestMem<'_, '_>,
    sql_ptr: i32,
    sql_len: i32,
    params_ptr: i32,
    params_len: i32,
    owned_out: i32,
) -> Result<(), HostError> {
    check_cap(mem, crate::caps::DB)?;
    db_check_size(sql_len, params_len)?;
    let sql = db_sql(mem, sql_ptr, sql_len)?;
    let params = db_params(mem, params_ptr, params_len)?;
    db_reject_refs(&params)?;
    let root = fs_root_of(mem)?;
    let (rows, _, _) = crate::sqlite::with_sync(&mem.data().plugin, &root, |conn| {
        crate::sqlite::query(conn, &sql, &params)
    })?;
    // The connection borrow ended above; give_owned may re-enter the
    // guest allocator now.
    mem.give_owned(&rows, owned_out)
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
