//! Manifest sync (pgh_plugin_sync) against the real pgh_* entry points:
//! load, no-op resync, config update, sweep of removed entries, adoption
//! and demotion of interactive loads, disabled entries, and atomic
//! rejection of a bad manifest. One test function (pgh state is
//! thread-local + a process-global vtable).

use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};

use plugin_host::*;

unsafe extern "C" fn vt_log(_l: c_int, _p: *const c_char, _m: *const c_char) {}

unsafe extern "C" fn vt_list_objects(_k: c_int, sink: pgh_sink, ctx: *mut c_void) {
    let s = "[]";
    sink(ctx, s.as_ptr() as *const c_char, s.len());
}

unsafe extern "C" fn vt_resolve_object(
    _k: c_int,
    _i: u32,
    _s: pgh_sink,
    _c: *mut c_void,
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
    _x: c_int,
    _sink: pgh_sink,
    _c: *mut c_void,
) -> c_int {
    -1
}

unsafe extern "C" fn vt_get_option(
    _k: c_int,
    _i: u32,
    _n: *const c_char,
    _s: pgh_sink,
    _c: *mut c_void,
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
    _w: u32,
    _wi: u32,
    _h: u32,
    _x: c_int,
    _y: c_int,
    _t: *const c_char,
) -> i64 {
    -1
}

unsafe extern "C" fn vt_mode_write(_m: u64, _d: *const u8, _l: usize) -> c_int {
    -1
}

unsafe extern "C" fn vt_mode_preview(
    _m: u64,
    _p: i64,
    _x: u32,
    _y: u32,
    _w: u32,
    _h: u32,
) -> c_int {
    -1
}

unsafe extern "C" fn vt_mode_close(_m: u64) -> c_int {
    -1
}

unsafe extern "C" fn vt_mode_move(_m: u64, _w: u32, _x: c_int, _y: c_int) -> c_int {
    -1
}

unsafe extern "C" fn collect_sink(ctx: *mut c_void, ptr: *const c_char, len: usize) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

const GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $next (mut i32) (i32.const 1024))
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
  (func (export "pgh_on_event") (param i32 i32))
)
"#;

fn sync(path: &std::path::Path) -> (i32, String) {
    let p = CString::new(path.to_str().unwrap()).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_sync(p.as_ptr(), collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    (rc, String::from_utf8_lossy(&buf).into_owned())
}

fn query_plugins() -> String {
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        pgh_query_plugins(1, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    String::from_utf8(buf).unwrap()
}

#[test]
fn manifest_sync() {
    let dir = std::env::temp_dir().join(format!("pgh-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    std::fs::write(dir.join("alpha.wasm"), &wasm).unwrap();
    std::fs::write(dir.join("beta.wasm"), &wasm).unwrap();
    let manifest = dir.join("plugins.toml");

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
        mode_move: vt_mode_move,
    };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Initial sync: two entries, relative paths resolve against the
    // manifest's directory, native-typed config.
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
config = { interval = 5, verbose = true }

[plugins.beta]
path = "beta.wasm"
scope = "server"
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("2 loaded") && out.contains("0 unloaded"), "{out}");
    assert_eq!(pgh_drain(0), 0);
    let q = query_plugins();
    assert!(q.contains("alpha") && q.contains("beta"), "{q}");
    assert!(q.contains("managed"), "{q}");

    // Resync with no changes: everything unchanged, nothing swept.
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("2 unchanged") && out.contains("0 unloaded"), "{out}");

    // Remove beta, change alpha's config: beta swept, alpha updated.
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
config = { interval = 9, verbose = true }
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("1 updated") && out.contains("1 unloaded"), "{out}");
    assert_eq!(pgh_drain(0), 0);
    let q = query_plugins();
    assert!(q.contains("alpha") && !q.contains("beta"), "{q}");

    // A bad manifest is rejected atomically: nothing changes.
    std::fs::write(&manifest, "[plugins.alpha\npath =").unwrap();
    let (rc, _) = sync(&manifest);
    assert_eq!(rc, -1);
    assert!(query_plugins().contains("alpha"));

    // Unknown capability is caught in validation, atomically.
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
caps = ["frobnicate"]
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, -1);
    assert!(out.contains("frobnicate"), "{out}");

    // Interactive loads are unmanaged: never swept by a sync.
    let desc = CString::new(format!(
        r#"{{"name":"gamma","path":"{}","scope":"server"}}"#,
        dir.join("beta.wasm").display()
    ))
    .unwrap();
    let mut err: Vec<u8> = Vec::new();
    let rc = unsafe {
        pgh_plugin_load(desc.as_ptr(), collect_sink, &mut err as *mut Vec<u8> as *mut c_void)
    };
    assert_eq!(rc, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(pgh_drain(0), 0);
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
config = { interval = 9, verbose = true }
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("0 unloaded"), "{out}");
    assert!(query_plugins().contains("gamma"));

    // A manifest entry with the same name adopts the interactive load...
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
config = { interval = 9, verbose = true }

[plugins.gamma]
path = "beta.wasm"

[plugins.delta]
path = "beta.wasm"
enabled = false
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert_eq!(pgh_drain(0), 0);
    // ...and the disabled entry is declared but not running.
    assert!(query_plugins().contains("disabled"), "{}", query_plugins());

    // Dropping gamma from the manifest now sweeps it (it became managed).
    std::fs::write(
        &manifest,
        r#"
[plugins.alpha]
path = "alpha.wasm"
config = { interval = 9, verbose = true }
"#,
    )
    .unwrap();
    let (rc, out) = sync(&manifest);
    assert_eq!(rc, 0, "{out}");
    assert!(out.contains("2 unloaded"), "{out}");
    assert_eq!(pgh_drain(0), 0);
    let q = query_plugins();
    assert!(!q.contains("gamma") && !q.contains("delta"), "{q}");

    pgh_shutdown();
    std::fs::remove_dir_all(&dir).ok();
}
