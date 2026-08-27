//! Wire-level ABI checks with a hand-rolled WAT guest: interning is
//! stable from the guest side, a string missing its NUL at data[len] is
//! rejected with E_BAD_REQUEST before any vtable pointer is touched, an
//! OutBuf too small for the result fails with E_LIMIT and reports the
//! needed size through len_out, and a fitting OutBuf round-trips.

mod common;

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use common::*;
use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn logging_vt_log(
    _level: c_int,
    _plugin: *const c_char,
    msg: *const c_char,
) {
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(msg.into_owned());
}

/// get_option mock: sinks "val".
unsafe extern "C" fn my_get_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    sink: pgh_sink,
    ctx: *mut c_void,
) -> c_int {
    let s = "val";
    sink(ctx, s.as_ptr() as *const c_char, s.len());
    0
}

/// capture_pane mock: sinks 10 bytes (more than the guest's 4-byte
/// OutBuf) so the E_LIMIT + needed-size path fires.
unsafe extern "C" fn my_capture_pane(
    _p: u32,
    _s: c_int,
    _e: c_int,
    _x: c_int,
    sink: pgh_sink,
    ctx: *mut c_void,
) -> c_int {
    let s = "aaaaaaaaaa";
    sink(ctx, s.as_ptr() as *const c_char, s.len());
    0
}

/// Guest memory layout:
///   0: "xy"          (name with NO NUL at [len] when passed as len 1)
///   8: "@n\00"       (well-formed name, len 2)
///  32: "ev-x"        (name to intern)
///  64: "intern-ok"  80: "nul-ok"  96: "limit-ok"  112: "get-ok"
///  1024: OutBuf      1100: len_out slot
const GUEST_WAT: &str = r#"
(module
  (import "tmux" "intern" (func $intern (param i32 i32) (result i64)))
  (import "tmux" "get_option"
    (func $get (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "tmux" "capture_pane"
    (func $cap (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "tmux" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 4)
  (global $next (mut i32) (i32.const 2048))
  (data (i32.const 0) "xy")
  (data (i32.const 8) "@n\00")
  (data (i32.const 32) "ev-x")
  (data (i32.const 64) "intern-ok")
  (data (i32.const 80) "nul-ok")
  (data (i32.const 96) "limit-ok")
  (data (i32.const 112) "get-ok")
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
    (local $a i64) (local $b i64) (local $rc i32)

    ;; interning is stable: same name, same id, id > 0
    (local.set $a (call $intern (i32.const 32) (i32.const 4)))
    (local.set $b (call $intern (i32.const 32) (i32.const 4)))
    (if (i32.and
          (i64.gt_s (local.get $a) (i64.const 0))
          (i64.eq (local.get $a) (local.get $b)))
      (then (call $log (i32.const 1) (i32.const 64) (i32.const 9))))

    ;; "xy" passed as len 1: data[1] is 'y', not NUL -> -E_BAD_REQUEST
    (local.set $rc (call $get (i32.const -1) (i32.const 0)
      (i32.const 0) (i32.const 1)
      (i32.const 1024) (i32.const 64) (i32.const 1100)))
    (if (i32.eq (local.get $rc) (i32.const -1))
      (then (call $log (i32.const 1) (i32.const 80) (i32.const 6))))

    ;; 10-byte capture into a 4-byte OutBuf: -E_LIMIT, len_out = 10
    (local.set $rc (call $cap (i32.const 1) (i32.const 0) (i32.const 0)
      (i32.const 0) (i32.const 1024) (i32.const 4) (i32.const 1100)))
    (if (i32.and
          (i32.eq (local.get $rc) (i32.const -6))
          (i32.eq (i32.load (i32.const 1100)) (i32.const 10)))
      (then (call $log (i32.const 1) (i32.const 96) (i32.const 8))))

    ;; well-formed name: reaches C, result "val" lands in the OutBuf
    (local.set $rc (call $get (i32.const -1) (i32.const 0)
      (i32.const 8) (i32.const 2)
      (i32.const 1024) (i32.const 64) (i32.const 1100)))
    (if (i32.and
          (i32.eq (local.get $rc) (i32.const 0))
          (i32.eq (i32.load (i32.const 1100)) (i32.const 3)))
      (then (call $log (i32.const 1) (i32.const 112) (i32.const 6))))

    (i32.const 0))
  (func (export "pgh_on_event") (param i32 i32))
)
"#;

#[test]
fn wire_validation() {
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-wire-{}.wasm", std::process::id()));
    std::fs::write(&path, &wasm).unwrap();

    let vt = pgh_host_vtable {
        log: logging_vt_log,
        get_option: my_get_option,
        capture_pane: my_capture_pane,
        ..base_vtable()
    };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    let (rc, err) = load_plugin("wire", &path, "server", &["capture-pane"]);
    assert_eq!(rc, 0, "{err}");
    assert_eq!(pgh_drain(0), 0);

    let logs = LOGS.lock().unwrap().clone();
    for marker in ["intern-ok", "nul-ok", "limit-ok", "get-ok"] {
        assert!(
            logs.iter().any(|l| l == marker),
            "missing {marker}; logs: {logs:?}"
        );
    }

    pgh_shutdown();
    std::fs::remove_file(&path).ok();
}
