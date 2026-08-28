//! Shared mock vtable + helpers for the pgh_* integration tests.
//!
//! `base_vtable()` is the exhaustive struct literal: adding a field to
//! pgh_host_vtable breaks this file at compile time, which is the safety
//! net the C side (memset + hand assignment) lacks. Tests override the
//! fields they instrument via struct update syntax.

#![allow(dead_code)]

use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};

use plugin_host::*;

pub unsafe extern "C" fn vt_log(_l: c_int, _p: *const c_char, _m: *const c_char) {}

/// Empty binary object list: u32 count = 0.
pub unsafe extern "C" fn vt_list_objects(
    _k: c_int,
    sink: pgh_sink,
    ctx: *mut c_void,
) {
    let empty = 0u32.to_le_bytes();
    sink(ctx, empty.as_ptr() as *const c_char, empty.len());
}

pub unsafe extern "C" fn vt_resolve_object(
    _k: c_int,
    _i: u32,
    _s: pgh_sink,
    _c: *mut c_void,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_obj_relation(_rel: c_int, _a: u32, _b: u32) -> i64 {
    -1
}

pub unsafe extern "C" fn vt_send_keys(
    _p: u32,
    _k: *const c_char,
    _l: c_int,
) -> c_int {
    0
}

pub unsafe extern "C" fn vt_capture_pane(
    _p: u32,
    _s: c_int,
    _e: c_int,
    _x: c_int,
    _sink: pgh_sink,
    _c: *mut c_void,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_get_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    _s: pgh_sink,
    _c: *mut c_void,
) -> c_int {
    -2
}

pub unsafe extern "C" fn vt_set_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    _v: *const c_char,
) -> c_int {
    0
}

pub unsafe extern "C" fn vt_display_message(
    _c: c_int,
    _p: *const c_char,
    _m: *const c_char,
) -> c_int {
    0
}

pub unsafe extern "C" fn vt_run_job(
    _c: *const c_char,
    _w: *const c_char,
    _t: u64,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_run_command(_c: *const c_char, _t: u64) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_timer_start(_ms: u64, _t: u64) -> u64 {
    1
}

pub unsafe extern "C" fn vt_timer_cancel(_id: u64) -> c_int {
    0
}

pub unsafe extern "C" fn vt_state_changed(
    _p: *const c_char,
    _s: *const c_char,
    _r: *const c_char,
) {
}

pub unsafe extern "C" fn vt_mode_open(
    _w: u32,
    _wi: u32,
    _h: u32,
    _x: c_int,
    _y: c_int,
    _t: *const c_char,
) -> i64 {
    -1
}

pub unsafe extern "C" fn vt_mode_write(
    _m: u64,
    _d: *const u8,
    _l: usize,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_mode_preview(
    _m: u64,
    _p: i64,
    _x: u32,
    _y: u32,
    _w: u32,
    _h: u32,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_mode_close(_m: u64) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_mode_move(
    _m: u64,
    _w: u32,
    _x: c_int,
    _y: c_int,
) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_mode_resize(_m: u64, _w: u32, _h: u32) -> c_int {
    -1
}

pub unsafe extern "C" fn vt_format_expand(
    _k: c_int,
    _i: u32,
    _f: *const c_char,
    _s: pgh_sink,
    _c: *mut c_void,
) -> c_int {
    -1
}

/// The exhaustive vtable literal (see module docs).
pub fn base_vtable() -> pgh_host_vtable {
    pgh_host_vtable {
        log: vt_log,
        list_objects: vt_list_objects,
        resolve_object: vt_resolve_object,
        obj_relation: vt_obj_relation,
        send_keys: vt_send_keys,
        capture_pane: vt_capture_pane,
        get_option: vt_get_option,
        set_option: vt_set_option,
        display_message: vt_display_message,
        run_job: vt_run_job,
        run_command: vt_run_command,
        timer_start: vt_timer_start,
        timer_cancel: vt_timer_cancel,
        plugin_state_changed: vt_state_changed,
        mode_open: vt_mode_open,
        mode_write: vt_mode_write,
        mode_preview: vt_mode_preview,
        mode_close: vt_mode_close,
        mode_move: vt_mode_move,
        mode_resize: vt_mode_resize,
        format_expand: vt_format_expand,
    }
}

pub unsafe extern "C" fn collect_sink(
    ctx: *mut c_void,
    ptr: *const c_char,
    len: usize,
) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

/// pgh_plugin_load with the argument-array signature.
pub fn load_plugin(
    name: &str,
    path: &std::path::Path,
    scope: &str,
    caps: &[&str],
) -> (i32, String) {
    let name = CString::new(name).unwrap();
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let scope = CString::new(scope).unwrap();
    let caps_c: Vec<CString> =
        caps.iter().map(|c| CString::new(*c).unwrap()).collect();
    let caps_ptrs: Vec<*const c_char> =
        caps_c.iter().map(|c| c.as_ptr()).collect();
    let mut err: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_load(
            name.as_ptr(),
            path.as_ptr(),
            scope.as_ptr(),
            caps_ptrs.as_ptr(),
            caps_ptrs.len(),
            std::ptr::null(),
            0,
            collect_sink,
            &mut err as *mut Vec<u8> as *mut c_void,
        )
    };
    (rc, String::from_utf8_lossy(&err).into_owned())
}

/// A field value for test event payloads.
pub enum Field<'a> {
    Str(&'a str),
    I64(i64),
    Bool(bool),
}

/// Build a binary event buffer the way the C bridge does: intern the
/// event name and field keys through pgh_intern, fixed header (seq 0),
/// field block.
pub fn make_event(
    name: &str,
    client: Option<u32>,
    session: Option<u32>,
    window: Option<u32>,
    pane: Option<u32>,
    fields: &[(&str, Field<'_>)],
) -> Vec<u8> {
    use tmux_plugin_abi::{EventHeader, EventScope, FieldWriter, KeyRef};

    let intern = |n: &str| -> u32 {
        let c = CString::new(n).unwrap();
        unsafe { pgh_intern(c.as_ptr()) }
    };
    let header = EventHeader {
        event_id: intern(name),
        seq: 0,
        scope: EventScope { client, session, window, pane },
    };
    let mut buf = Vec::new();
    header.write(&mut buf);
    let mut w = FieldWriter::new();
    for (key, value) in fields {
        let key = KeyRef::Id(intern(key));
        match value {
            Field::Str(s) => w.str(key, s),
            Field::I64(v) => w.i64(key, *v),
            Field::Bool(b) => w.bool(key, *b),
        }
    }
    buf.extend_from_slice(&w.finish());
    buf
}

/// pgh_notify with a built event buffer.
pub fn notify(bytes: &[u8]) {
    unsafe { pgh_notify(bytes.as_ptr(), bytes.len()) };
}

/// pgh_mode_event with a built event buffer.
pub fn mode_event(mode_id: u64, bytes: &[u8]) {
    unsafe { pgh_mode_event(mode_id, bytes.as_ptr(), bytes.len()) };
}

pub fn query_plugins() -> String {
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        pgh_query_plugins(1, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    String::from_utf8(buf).unwrap()
}
