//! Full lifecycle against the real pgh_* entry points with a mock vtable
//! and an inline-WAT guest: load -> instantiate at drain -> binary event
//! delivery -> stale async completion drop -> unload. One test function
//! (pgh state is thread-local + a process-global vtable).

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

const GUEST_WAT: &str = r#"
(module
  (import "tmux" "log" (func $log (param i32 i32 i32)))
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

    let vt = pgh_host_vtable { log: logging_vt_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Load a server-scoped plugin; instantiation happens at drain.
    let (rc, err) = load_plugin("lifecycle", &path, "server", &[]);
    assert_eq!(rc, 0, "{err}");
    assert_eq!(pgh_drain(0), 0);
    assert!(query_plugins().contains("1 instance"), "{}", query_plugins());

    // Bad load reports an error synchronously.
    let (rc, err) =
        load_plugin("nope", std::path::Path::new("/nonexistent.wasm"), "server", &[]);
    assert_eq!(rc, -1);
    assert!(!err.is_empty());

    // Binary events reach the guest (implicit lifecycle event; server
    // scope).
    let event = make_event(
        "session-created",
        None,
        Some(7),
        None,
        None,
        &[("session_name", Field::Str("s"))],
    );
    notify(&event);
    assert_eq!(pgh_drain(0), 0);
    assert!(
        LOGS.lock().unwrap().iter().any(|l| l.contains("guest event")),
        "guest did not log; logs: {:?}",
        LOGS.lock().unwrap()
    );

    // Stale async completion for an unknown token is dropped silently.
    unsafe { pgh_async_complete(9999, 0, 0, 0, std::ptr::null(), 0) };
    assert_eq!(pgh_drain(0), 0);

    // Interning is stable and shared: the same name gives the same id.
    let name = CString::new("session-created").unwrap();
    let a = unsafe { pgh_intern(name.as_ptr()) };
    let b = unsafe { pgh_intern(name.as_ptr()) };
    assert!(a > 0);
    assert_eq!(a, b);

    // Unload tears the instance down at the next drain.
    let name = CString::new("lifecycle").unwrap();
    assert_eq!(unsafe { pgh_plugin_unload(name.as_ptr()) }, 0);
    assert_eq!(pgh_drain(0), 0);
    assert!(query_plugins().contains("no plugins loaded"));

    pgh_shutdown();
    std::fs::remove_file(&path).ok();
}
