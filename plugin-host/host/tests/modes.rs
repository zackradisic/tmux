//! Plugin UI mode lifecycle against the real pgh_* entry points, with a
//! mock vtable and an inline-WAT guest: open -> write -> key event ->
//! reload purge -> unload purge, plus stale-event drops. One test function
//! (pgh state is thread-local + a process-global vtable).

use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MODE_OPENS: Mutex<Vec<(u32, u32, u32)>> = Mutex::new(Vec::new());
static MODE_WRITES: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
static MODE_CLOSES: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static NEXT_MODE_ID: Mutex<u64> = Mutex::new(0);

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

unsafe extern "C" fn vt_mode_open(
    window: u32,
    width: u32,
    height: u32,
    _x: c_int,
    _y: c_int,
    _title: *const c_char,
) -> i64 {
    MODE_OPENS.lock().unwrap().push((window, width, height));
    let mut next = NEXT_MODE_ID.lock().unwrap();
    *next += 1;
    *next as i64
}

unsafe extern "C" fn vt_mode_write(_mode: u64, data: *const u8, len: usize) -> c_int {
    let bytes = std::slice::from_raw_parts(data, len).to_vec();
    MODE_WRITES.lock().unwrap().push(bytes);
    0
}

unsafe extern "C" fn vt_mode_preview(
    _m: u64,
    _p: i64,
    _x: u32,
    _y: u32,
    _w: u32,
    _h: u32,
) -> c_int {
    0
}

unsafe extern "C" fn vt_mode_close(mode: u64) -> c_int {
    MODE_CLOSES.lock().unwrap().push(mode);
    0
}

unsafe extern "C" fn collect_sink(ctx: *mut c_void, ptr: *const c_char, len: usize) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

/// Guest that opens a mode (and writes "hi" to it) from init, and logs
/// every event it receives verbatim. Built with the two host_call request
/// JSONs spliced into data segments.
fn guest_wat() -> String {
    let open = r#"{"method":"mode_open","params":{"window":1,"width":10,"height":5}}"#;
    let write = r#"{"method":"mode_write","params":{"mode":1,"data_b64":"aGk="}}"#;
    format!(
        r#"
(module
  (import "tmux" "host_call" (func $call (param i32 i32 i32 i32) (result i32)))
  (import "tmux" "host_log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 4)
  (global $next (mut i32) (i32.const 2048))
  (data (i32.const 0) "{open_escaped}")
  (data (i32.const 512) "{write_escaped}")
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
  (func (export "pgh_init") (param i32 i32) (result i32)
    (drop (call $call (i32.const 0) (i32.const {open_len}) (i32.const 1024) (i32.const 1028)))
    (drop (call $call (i32.const 512) (i32.const {write_len}) (i32.const 1024) (i32.const 1028)))
    (i32.const 0))
  (func (export "pgh_on_event") (param i32 i32)
    (call $log (i32.const 1) (local.get 0) (local.get 1)))
  (func (export "pgh_on_unload"))
)
"#,
        open_escaped = open.replace('"', "\\\""),
        write_escaped = write.replace('"', "\\\""),
        open_len = open.len(),
        write_len = write.len(),
    )
}

fn logs_snapshot() -> Vec<String> {
    LOGS.lock().unwrap().clone()
}

#[test]
fn mode_lifecycle() {
    let wasm = wat::parse_str(guest_wat()).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-modes-{}.wasm", std::process::id()));
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
        mode_open: vt_mode_open,
        mode_write: vt_mode_write,
        mode_preview: vt_mode_preview,
        mode_close: vt_mode_close,
    };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Load with the mode capability granted; init opens mode 1 and writes
    // base64("hi") to it.
    let desc = CString::new(format!(
        r#"{{"name":"modes","path":"{}","scope":"server","caps":["mode"]}}"#,
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
    assert_eq!(MODE_OPENS.lock().unwrap().as_slice(), &[(1, 10, 5)]);
    assert_eq!(MODE_WRITES.lock().unwrap().as_slice(), &[b"hi".to_vec()]);

    // A key event for the open mode reaches the guest, with the mode id
    // in data, no subscription needed.
    let name = CString::new("mode-key").unwrap();
    let data = CString::new(r#"{"key":"q"}"#).unwrap();
    unsafe { pgh_mode_event(1, name.as_ptr(), data.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    let logs = logs_snapshot();
    let delivered = logs
        .iter()
        .find(|l| l.contains("mode-key"))
        .expect("mode-key not delivered");
    assert!(delivered.contains(r#""mode":1"#), "{delivered}");
    assert!(delivered.contains(r#""key":"q""#), "{delivered}");

    // An event for a mode nothing owns is dropped silently.
    let before = logs_snapshot().len();
    unsafe { pgh_mode_event(99, name.as_ptr(), data.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before);

    // Reload swaps the instance: the old generation's mode is force-closed
    // through the vtable and the fresh init opens mode 2.
    let pname = CString::new("modes").unwrap();
    let mut errbuf: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_reload(
            pname.as_ptr(),
            collect_sink,
            &mut errbuf as *mut Vec<u8> as *mut c_void,
        )
    };
    assert_eq!(rc, 0, "{}", String::from_utf8_lossy(&errbuf));
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(MODE_CLOSES.lock().unwrap().as_slice(), &[1]);
    assert_eq!(MODE_OPENS.lock().unwrap().len(), 2);

    // Events for the closed mode 1 no longer reach anyone; mode 2 does.
    let before = logs_snapshot().len();
    unsafe { pgh_mode_event(1, name.as_ptr(), data.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before);
    unsafe { pgh_mode_event(2, name.as_ptr(), data.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before + 1);

    // mode-closed is terminal: delivered once, then the id is unknown.
    let closed = CString::new("mode-closed").unwrap();
    let reason = CString::new(r#"{"reason":"killed"}"#).unwrap();
    unsafe { pgh_mode_event(2, closed.as_ptr(), reason.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    let logs = logs_snapshot();
    let last = logs.last().unwrap();
    assert!(last.contains("mode-closed") && last.contains("killed"), "{last}");
    let before = logs.len();
    unsafe { pgh_mode_event(2, name.as_ptr(), data.as_ptr()) };
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before);

    // Fresh instance for the unload half: reload again opens mode 3.
    let mut errbuf: Vec<u8> = Vec::new();
    unsafe {
        pgh_plugin_reload(
            pname.as_ptr(),
            collect_sink,
            &mut errbuf as *mut Vec<u8> as *mut c_void,
        )
    };
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(MODE_OPENS.lock().unwrap().len(), 3);

    // Unload purges the instance's modes: force-closed through the vtable.
    assert_eq!(unsafe { pgh_plugin_unload(pname.as_ptr()) }, 0);
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(MODE_CLOSES.lock().unwrap().as_slice(), &[1, 3]);

    pgh_shutdown();
    std::fs::remove_file(&path).ok();
}
