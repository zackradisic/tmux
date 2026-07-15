//! Randomized interleavings of the pgh_* entry points with a real guest:
//! load/unload/reload/enable/notify/destroy/async-complete/drain in
//! arbitrary orders must never panic (= poison the host), must always
//! converge when drained, and must never deliver stale completions (a
//! delivery to a dead instance would trap the WAT guest, which counts
//! failures we assert on indirectly via the poisoned check).
//!
//! Deterministic xorshift seeds, no fuzzing infrastructure needed.

use std::ffi::{c_void, CString};
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn vt_log(_l: c_int, plugin: *const c_char, msg: *const c_char) {
    let plugin = std::ffi::CStr::from_ptr(plugin).to_string_lossy();
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(format!("{plugin}: {msg}"));
}
unsafe extern "C" fn vt_list_objects(_k: c_int, sink: pgh_sink, ctx: *mut c_void) {
    // Pretend objects 0 and 1 exist for every kind.
    let s = r#"[{"id":0},{"id":1}]"#;
    sink(ctx, s.as_ptr() as *const c_char, s.len());
}
unsafe extern "C" fn vt_resolve_object(
    _k: c_int,
    id: u32,
    sink: pgh_sink,
    ctx: *mut c_void,
) -> c_int {
    if id > 1 {
        return -1;
    }
    let s = format!(r#"{{"id":{id},"window":0,"sessions":[0]}}"#);
    sink(ctx, s.as_ptr() as *const c_char, s.len());
    0
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
    // Started but never completes within the test: exercises token purge.
    0
}
unsafe extern "C" fn vt_run_command(_c: *const c_char, _t: u64) -> c_int {
    0
}
unsafe extern "C" fn vt_timer_start(_ms: u64, _t: u64) -> u64 {
    7
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

/// Guest: conforming ABI, logs nothing, allocates by bumping.
const GUEST_WAT: &str = r#"
(module
  (memory (export "memory") 4)
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
  (func (export "pgh_on_async_complete") (param i64 i32 i32 i32))
  (func (export "pgh_on_unload"))
)
"#;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn drain_until_empty() {
    for _ in 0..200 {
        if pgh_drain(0) == 0 {
            return;
        }
    }
    panic!("drain did not converge");
}

#[test]
fn random_interleavings_never_poison() {
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-interleave-{}.wasm", std::process::id()));
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

    let names = ["alpha", "beta", "gamma"];
    let scopes = ["server", "session", "window", "pane"];
    let events = [
        "session-created",
        "window-created",
        "pane-created",
        "window-linked",
        "window-renamed",
        "session-closed",
    ];

    for seed in 1..=20u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        for step in 0..400 {
            let name = names[(rng.next() as usize) % names.len()];
            match rng.next() % 10 {
                0 | 1 => {
                    let scope = scopes[(rng.next() as usize) % scopes.len()];
                    let desc = CString::new(format!(
                        r#"{{"name":"{name}","path":"{}","scope":"{scope}"}}"#,
                        path.display()
                    ))
                    .unwrap();
                    let mut err: Vec<u8> = Vec::new();
                    unsafe {
                        pgh_plugin_load(
                            desc.as_ptr(),
                            collect_sink,
                            &mut err as *mut Vec<u8> as *mut c_void,
                        )
                    };
                }
                2 | 3 => {
                    let ev = events[(rng.next() as usize) % events.len()];
                    let id = rng.next() % 3;
                    let json = CString::new(format!(
                        r#"{{"event":"{ev}","session":{{"id":{id},"name":"s"}},"window":{{"id":{id},"name":"w"}},"pane":{{"id":{id},"window":{id}}}}}"#
                    ))
                    .unwrap();
                    unsafe { pgh_notify(json.as_ptr()) };
                }
                4 => {
                    pgh_drain(0);
                }
                5 => {
                    let kind = (rng.next() % 4) as c_int;
                    pgh_object_destroyed(kind, (rng.next() % 3) as u32);
                }
                6 => {
                    let payload = CString::new("{}").unwrap();
                    unsafe {
                        pgh_async_complete(
                            rng.next() % 16,
                            payload.as_ptr(),
                            (rng.next() % 2) as c_int,
                        )
                    };
                }
                7 => {
                    let n = CString::new(name).unwrap();
                    unsafe { pgh_plugin_unload(n.as_ptr()) };
                }
                8 => {
                    let n = CString::new(name).unwrap();
                    let mut err: Vec<u8> = Vec::new();
                    unsafe {
                        pgh_plugin_reload(
                            n.as_ptr(),
                            collect_sink,
                            &mut err as *mut Vec<u8> as *mut c_void,
                        )
                    };
                }
                _ => {
                    let n = CString::new(name).unwrap();
                    unsafe {
                        pgh_plugin_set_enabled(
                            n.as_ptr(),
                            (rng.next() % 2) as c_int,
                        )
                    };
                }
            }
            if step % 50 == 49 {
                drain_until_empty();
                // show-plugins path must stay coherent mid-chaos.
                let mut buf: Vec<u8> = Vec::new();
                unsafe {
                    pgh_query_plugins(
                        1,
                        collect_sink,
                        &mut buf as *mut Vec<u8> as *mut c_void,
                    )
                };
                assert!(!buf.is_empty());
            }
        }
        drain_until_empty();
        pgh_shutdown();
    }

    let logs = LOGS.lock().unwrap();
    assert!(
        !logs.iter().any(|l| l.contains("poisoned")),
        "host poisoned during interleaving; logs tail: {:?}",
        &logs[logs.len().saturating_sub(10)..]
    );
    std::fs::remove_file(&path).ok();
}
