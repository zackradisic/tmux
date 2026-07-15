//! End-to-end lifecycle test against the real pgh_* entry points, with a
//! mock vtable and an inline-WAT guest. Everything lives in ONE test
//! function: pgh state is thread-local + a process-global vtable, so
//! ordering across test threads would otherwise be undefined.

use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn vt_log(_level: c_int, plugin: *const c_char, msg: *const c_char) {
    let plugin = std::ffi::CStr::from_ptr(plugin).to_string_lossy();
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(format!("{plugin}: {msg}"));
}

unsafe extern "C" fn vt_list_objects(_kind: c_int, sink: pgh_sink, ctx: *mut c_void) {
    let s = "[]";
    sink(ctx, s.as_ptr() as *const c_char, s.len());
}

unsafe extern "C" fn vt_resolve_object(
    _kind: c_int,
    _id: u32,
    _sink: pgh_sink,
    _ctx: *mut c_void,
) -> c_int {
    -1
}

unsafe extern "C" fn vt_send_keys(_p: u32, _k: *const c_char, _l: c_int) -> c_int {
    0
}

unsafe extern "C" fn vt_capture_pane(
    _p: u32,
    _s: c_int,
    _e: c_int,
    _esc: c_int,
    _sink: pgh_sink,
    _ctx: *mut c_void,
) -> c_int {
    -1
}

unsafe extern "C" fn vt_get_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    _sink: pgh_sink,
    _ctx: *mut c_void,
) -> c_int {
    -2
}

unsafe extern "C" fn vt_set_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    _v: *const c_char,
) -> c_int {
    0
}

unsafe extern "C" fn vt_display_message(
    _c: c_int,
    _p: *const c_char,
    _m: *const c_char,
) -> c_int {
    0
}

unsafe extern "C" fn vt_run_job(_c: *const c_char, _w: *const c_char, _t: u64) -> c_int {
    -1
}

unsafe extern "C" fn vt_run_command(_c: *const c_char, _t: u64) -> c_int {
    -1
}

unsafe extern "C" fn vt_timer_start(_ms: u64, _t: u64) -> u64 {
    1
}

unsafe extern "C" fn vt_timer_cancel(_id: u64) -> c_int {
    0
}

unsafe extern "C" fn vt_state_changed(
    _p: *const c_char,
    _s: *const c_char,
    _r: *const c_char,
) {
}

unsafe extern "C" fn collect_sink(ctx: *mut c_void, ptr: *const c_char, len: usize) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

fn query_plugins() -> String {
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        pgh_query_plugins(1, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    String::from_utf8(buf).unwrap()
}

const GUEST_WAT: &str = r#"
(module
  (import "tmux" "host_log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 1024))
  (data (i32.const 0) "guest event")
  (func (export "pgh_abi_version") (result i32) (i32.const 1))
  (func (export "pgh_alloc") (param i32) (result i32)
    (local i32)
    global.get $next
    local.set 1
    global.get $next
    local.get 0
    i32.add
    global.set $next
    local.get 1)
  (func (export "pgh_free") (param i32 i32))
  (func (export "pgh_init") (param i32 i32) (result i32) (i32.const 0))
  (func (export "pgh_on_event") (param i32 i32)
    (call $log (i32.const 1) (i32.const 0) (i32.const 11)))
)
"#;

#[test]
fn full_lifecycle() {
    // Write the guest module to a temp file.
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-lifecycle-{}.wasm", std::process::id()));
    std::fs::write(&path, &wasm).unwrap();

    let vt = pgh_host_vtable {
        log: vt_log,
        list_objects: vt_list_objects,
        resolve_object: vt_resolve_object,
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
    };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Load a server-scoped plugin; instantiation happens at drain.
    let desc = CString::new(format!(
        r#"{{"name":"lifecycle","path":"{}","scope":"server"}}"#,
        path.display()
    ))
    .unwrap();
    let mut errbuf: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_load(
            desc.as_ptr(),
            collect_sink,
            &mut errbuf as *mut Vec<u8> as *mut c_void,
        )
    };
    assert_eq!(rc, 0, "{}", String::from_utf8_lossy(&errbuf));
    assert_eq!(pgh_drain(0), 0);
    assert!(query_plugins().contains("1 instance"), "{}", query_plugins());

    // Bad load reports an error synchronously.
    let bad = CString::new(r#"{"name":"nope","path":"/nonexistent.wasm"}"#).unwrap();
    let mut errbuf: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_load(
            bad.as_ptr(),
            collect_sink,
            &mut errbuf as *mut Vec<u8> as *mut c_void,
        )
    };
    assert_eq!(rc, -1);
    assert!(!errbuf.is_empty());

    // Events reach the guest (implicit lifecycle event; server scope).
    let event =
        CString::new(r#"{"event":"session-created","session":{"id":7,"name":"s"}}"#)
            .unwrap();
    unsafe { pgh_notify(event.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    assert!(
        LOGS.lock().unwrap().iter().any(|l| l.contains("guest event")),
        "guest did not log; logs: {:?}",
        LOGS.lock().unwrap()
    );

    // Stale async completion for an unknown token is dropped silently.
    let payload = CString::new("{}").unwrap();
    unsafe { pgh_async_complete(9999, payload.as_ptr(), 0) };
    assert_eq!(pgh_drain(0), 0);

    // Unload tears the instance down at the next drain.
    let name = CString::new("lifecycle").unwrap();
    assert_eq!(unsafe { pgh_plugin_unload(name.as_ptr()) }, 0);
    assert_eq!(pgh_drain(0), 0);
    assert!(query_plugins().contains("no plugins loaded"));

    pgh_shutdown();
    std::fs::remove_file(&path).ok();
}
