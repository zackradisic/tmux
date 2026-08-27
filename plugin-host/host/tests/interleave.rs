//! Chaos test: random interleavings of loads, binary events, drains,
//! object teardown, async completions, unloads, reloads and mode events
//! must never poison the host. One test function (pgh state is
//! thread-local + a process-global vtable).

mod common;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use common::*;
use plugin_host::*;

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn logging_vt_log(
    _level: c_int,
    plugin: *const c_char,
    msg: *const c_char,
) {
    let plugin = std::ffi::CStr::from_ptr(plugin).to_string_lossy();
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(format!("{plugin}: {msg}"));
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
  (func (export "pgh_on_async_complete") (param i64 i32 i64 i64 i32 i32))
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

    let vt = pgh_host_vtable { log: logging_vt_log, ..base_vtable() };
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
            match rng.next() % 11 {
                0 | 1 => {
                    let scope = scopes[(rng.next() as usize) % scopes.len()];
                    let _ = load_plugin(name, &path, scope, &[]);
                }
                2 | 3 => {
                    let ev = events[(rng.next() as usize) % events.len()];
                    let id = (rng.next() % 3) as u32;
                    let bytes = make_event(
                        ev,
                        None,
                        Some(id),
                        Some(id),
                        Some(id),
                        &[
                            ("session_name", Field::Str("s")),
                            ("window_name", Field::Str("w")),
                        ],
                    );
                    notify(&bytes);
                }
                4 => {
                    pgh_drain(0);
                }
                5 => {
                    let kind = (rng.next() % 4) as c_int;
                    pgh_object_destroyed(kind, (rng.next() % 3) as u32);
                }
                6 => {
                    // Random token, sometimes an error completion.
                    let err = if rng.next() % 2 == 0 { 0 } else { 7 };
                    let msg = b"chaos";
                    unsafe {
                        pgh_async_complete(
                            rng.next() % 16,
                            err,
                            rng.next() as i64,
                            0,
                            msg.as_ptr(),
                            msg.len(),
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
                            &mut err as *mut Vec<u8>
                                as *mut std::ffi::c_void,
                        )
                    };
                }
                9 => {
                    // Mode events for ids nothing owns: dropped silently.
                    let bytes = make_event(
                        "mode-key",
                        None,
                        None,
                        None,
                        None,
                        &[
                            ("mode", Field::I64((rng.next() % 16) as i64)),
                            ("key", Field::Str("q")),
                        ],
                    );
                    mode_event(rng.next() % 16, &bytes);
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
                assert!(!query_plugins().is_empty());
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
