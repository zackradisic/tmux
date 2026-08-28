//! Plugin UI mode lifecycle against the real pgh_* entry points, with a
//! mock vtable and an inline-WAT guest: open -> write -> key event ->
//! reload purge -> unload purge, plus stale-event drops. One test function
//! (pgh state is thread-local + a process-global vtable).

mod common;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use common::*;
use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
static MODE_OPENS: Mutex<Vec<(u32, u32, u32)>> = Mutex::new(Vec::new());
static MODE_WRITES: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
static MODE_CLOSES: Mutex<Vec<u64>> = Mutex::new(Vec::new());
static MODE_MOVES: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());
static MODE_RESIZES: Mutex<Vec<(u64, u32, u32)>> = Mutex::new(Vec::new());
static NEXT_MODE_ID: Mutex<u64> = Mutex::new(0);

unsafe extern "C" fn logging_vt_log(
    _level: c_int,
    plugin: *const c_char,
    msg: *const c_char,
) {
    let plugin = std::ffi::CStr::from_ptr(plugin).to_string_lossy();
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(format!("{plugin}: {msg}"));
}

unsafe extern "C" fn my_mode_open(
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

unsafe extern "C" fn my_mode_write(_mode: u64, data: *const u8, len: usize) -> c_int {
    let bytes = std::slice::from_raw_parts(data, len).to_vec();
    MODE_WRITES.lock().unwrap().push(bytes);
    0
}

unsafe extern "C" fn my_mode_close(mode: u64) -> c_int {
    MODE_CLOSES.lock().unwrap().push(mode);
    0
}

unsafe extern "C" fn my_mode_move(
    mode: u64,
    window: u32,
    _x: c_int,
    _y: c_int,
) -> c_int {
    MODE_MOVES.lock().unwrap().push((mode, window));
    0
}

unsafe extern "C" fn my_mode_resize(mode: u64, width: u32, height: u32) -> c_int {
    MODE_RESIZES.lock().unwrap().push((mode, width, height));
    0
}

/// Guest that opens a mode via the typed imports, writes "hi" to it
/// (zero-copy Bytes), grows it to 10x9 and moves it to window 2, all from
/// init, and logs every event buffer it receives verbatim.
const GUEST_WAT: &str = r#"
(module
  (import "tmux" "mode_open"
    (func $open (param i32 i32 i32 i32 i32 i32 i32) (result i64)))
  (import "tmux" "mode_write" (func $write (param i64 i32 i32) (result i32)))
  (import "tmux" "mode_move" (func $move (param i64 i32 i32 i32) (result i32)))
  (import "tmux" "mode_resize" (func $resize (param i64 i32 i32) (result i32)))
  (import "tmux" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 4)
  (global $next (mut i32) (i32.const 2048))
  (data (i32.const 0) "hi")
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
    (drop (call $open (i32.const 1) (i32.const 10) (i32.const 5)
      (i32.const -1) (i32.const -1) (i32.const 0) (i32.const 0)))
    (drop (call $write (i64.const 1) (i32.const 0) (i32.const 2)))
    (drop (call $resize (i64.const 1) (i32.const 10) (i32.const 9)))
    (drop (call $move (i64.const 1) (i32.const 2) (i32.const -1) (i32.const -1)))
    (i32.const 0))
  (func (export "pgh_on_event") (param i32 i32)
    (call $log (i32.const 1) (local.get 0) (local.get 1)))
  (func (export "pgh_on_unload"))
)
"#;

fn logs_snapshot() -> Vec<String> {
    LOGS.lock().unwrap().clone()
}

fn key_event(mode: u64, key: &str) -> Vec<u8> {
    make_event(
        "mode-key",
        None,
        None,
        None,
        None,
        &[("mode", Field::I64(mode as i64)), ("key", Field::Str(key))],
    )
}

#[test]
fn mode_lifecycle() {
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-modes-{}.wasm", std::process::id()));
    std::fs::write(&path, &wasm).unwrap();

    let vt = pgh_host_vtable {
        log: logging_vt_log,
        mode_open: my_mode_open,
        mode_write: my_mode_write,
        mode_close: my_mode_close,
        mode_move: my_mode_move,
        mode_resize: my_mode_resize,
        ..base_vtable()
    };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Load with the mode capability granted; init opens mode 1, writes
    // "hi" through the zero-copy Bytes path and moves it to window 2.
    let (rc, err) = load_plugin("modes", &path, "server", &["mode"]);
    assert_eq!(rc, 0, "{err}");
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(MODE_OPENS.lock().unwrap().as_slice(), &[(1, 10, 5)]);
    assert_eq!(MODE_WRITES.lock().unwrap().as_slice(), &[b"hi".to_vec()]);
    // The resize and the move went through with the owner's mode id.
    assert_eq!(MODE_RESIZES.lock().unwrap().as_slice(), &[(1, 10, 9)]);
    assert_eq!(MODE_MOVES.lock().unwrap().as_slice(), &[(1, 2)]);

    // A key event for the open mode reaches the guest, no subscription
    // needed; the guest logs the raw buffer, so the key value shows up.
    let before = logs_snapshot().len();
    mode_event(1, &key_event(1, "q"));
    assert_eq!(pgh_drain(0), 0);
    let logs = logs_snapshot();
    assert_eq!(logs.len(), before + 1, "mode-key not delivered");
    assert!(logs.last().unwrap().contains('q'), "{:?}", logs.last());

    // An event for a mode nothing owns is dropped silently.
    let before = logs_snapshot().len();
    mode_event(99, &key_event(99, "q"));
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
            &mut errbuf as *mut Vec<u8> as *mut std::ffi::c_void,
        )
    };
    assert_eq!(rc, 0, "{}", String::from_utf8_lossy(&errbuf));
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(MODE_CLOSES.lock().unwrap().as_slice(), &[1]);
    assert_eq!(MODE_OPENS.lock().unwrap().len(), 2);

    // Events for the closed mode 1 no longer reach anyone; mode 2 does.
    let before = logs_snapshot().len();
    mode_event(1, &key_event(1, "q"));
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before);
    mode_event(2, &key_event(2, "q"));
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before + 1);

    // mode-closed is terminal: delivered once, then the id is unknown.
    let closed = make_event(
        "mode-closed",
        None,
        None,
        None,
        None,
        &[("mode", Field::I64(2)), ("reason", Field::Str("killed"))],
    );
    mode_event(2, &closed);
    assert_eq!(pgh_drain(0), 0);
    let logs = logs_snapshot();
    assert!(logs.last().unwrap().contains("killed"), "{:?}", logs.last());
    let before = logs.len();
    mode_event(2, &key_event(2, "q"));
    assert_eq!(pgh_drain(0), 0);
    assert_eq!(logs_snapshot().len(), before);

    // Fresh instance for the unload half: reload again opens mode 3.
    let mut errbuf: Vec<u8> = Vec::new();
    unsafe {
        pgh_plugin_reload(
            pname.as_ptr(),
            collect_sink,
            &mut errbuf as *mut Vec<u8> as *mut std::ffi::c_void,
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
