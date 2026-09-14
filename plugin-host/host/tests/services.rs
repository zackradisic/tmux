//! Services against the real pgh_* entry points with a mock vtable and
//! two inline-WAT guests: a provider that registers three methods and a
//! caller that calls them from init. Covers register/call/reply, reply
//! pages (MORE), an error reply, a missing provider, a denied capability,
//! and a provider unloaded while a call is open. One test function (pgh
//! state is thread-local + a process-global vtable).

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

/// Provider: registers ping, boom and hang. A service-request event has
/// `call` as its first field (i64 at offset 35) and `method` second (its
/// bytes at offset 52). ping answers two pages ("po" with MORE, then
/// "ng"), boom answers an error ("bad"), hang never answers.
const PROVIDER_WAT: &str = r#"
(module
  (import "tmux" "intern" (func $intern (param i32 i32) (result i64)))
  (import "tmux" "service_register" (func $register (param i32 i32) (result i32)))
  (import "tmux" "service_reply" (func $reply (param i64 i32 i32 i32) (result i32)))
  (memory (export "memory") 4)
  (global $next (mut i32) (i32.const 4096))
  (global $req_id (mut i32) (i32.const 0))
  (data (i32.const 0) "ping")
  (data (i32.const 8) "service-request")
  (data (i32.const 32) "po")
  (data (i32.const 36) "ng")
  (data (i32.const 40) "bad")
  (data (i32.const 44) "boom")
  (data (i32.const 56) "hang")
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
    (global.set $req_id
      (i32.wrap_i64 (call $intern (i32.const 8) (i32.const 15))))
    (drop (call $register (i32.const 0) (i32.const 4)))
    (drop (call $register (i32.const 44) (i32.const 4)))
    (drop (call $register (i32.const 56) (i32.const 4)))
    (i32.const 0))
  (func (export "pgh_on_event") (param $ptr i32) (param $len i32)
    (local $call i64) (local $m i32)
    (if (i32.ne (i32.load (local.get $ptr)) (global.get $req_id))
      (then return))
    (local.set $call (i64.load offset=35 align=1 (local.get $ptr)))
    (local.set $m (i32.load8_u offset=52 (local.get $ptr)))
    (if (i32.eq (local.get $m) (i32.const 112))
      (then
        (drop (call $reply (local.get $call) (i32.const 32) (i32.const 2) (i32.const 1)))
        (drop (call $reply (local.get $call) (i32.const 36) (i32.const 2) (i32.const 0)))))
    (if (i32.eq (local.get $m) (i32.const 98))
      (then
        (drop (call $reply (local.get $call) (i32.const 40) (i32.const 3) (i32.const 2))))))
  (func (export "pgh_on_unload"))
)
"#;

/// Caller: from init calls prov.ping, prov.boom, ghost.ping (a plugin
/// that is not loaded) and prov.hang with payload "hi"; logs "denied" for
/// -E_CAP_DENIED, "nocall" for any other immediate error, and every
/// completion's data verbatim.
const CALLER_WAT: &str = r#"
(module
  (import "tmux" "service_call"
    (func $call (param i32 i32 i32 i32 i32 i32) (result i64)))
  (import "tmux" "log" (func $log (param i32 i32 i32)))
  (memory (export "memory") 4)
  (global $next (mut i32) (i32.const 4096))
  (data (i32.const 0) "prov")
  (data (i32.const 8) "ping")
  (data (i32.const 16) "boom")
  (data (i32.const 24) "ghost")
  (data (i32.const 32) "hang")
  (data (i32.const 40) "hi")
  (data (i32.const 48) "nocall")
  (data (i32.const 56) "denied")
  (func $try (param $tptr i32) (param $tlen i32) (param $mptr i32) (param $mlen i32)
    (local $rc i64)
    (local.set $rc
      (call $call (local.get $tptr) (local.get $tlen) (local.get $mptr) (local.get $mlen)
        (i32.const 40) (i32.const 2)))
    (if (i64.eq (local.get $rc) (i64.const -3))
      (then (call $log (i32.const 1) (i32.const 56) (i32.const 6)) (return)))
    (if (i64.lt_s (local.get $rc) (i64.const 0))
      (then (call $log (i32.const 1) (i32.const 48) (i32.const 6)))))
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
    (call $try (i32.const 0) (i32.const 4) (i32.const 8) (i32.const 4))
    (call $try (i32.const 0) (i32.const 4) (i32.const 16) (i32.const 4))
    (call $try (i32.const 24) (i32.const 5) (i32.const 8) (i32.const 4))
    (call $try (i32.const 0) (i32.const 4) (i32.const 32) (i32.const 4))
    (i32.const 0))
  (func (export "pgh_on_event") (param i32 i32))
  (func (export "pgh_on_async_complete")
    (param $tok i64) (param $err i32) (param $v0 i64) (param $v1 i64)
    (param $ptr i32) (param $len i32)
    (call $log (i32.const 1) (local.get $ptr) (local.get $len)))
  (func (export "pgh_on_unload"))
)
"#;

fn logs() -> Vec<String> {
    LOGS.lock().unwrap().clone()
}

fn drain_all() {
    for _ in 0..50 {
        if pgh_drain(0) == 0 {
            return;
        }
    }
    panic!("drain did not settle");
}

fn write_wasm(name: &str, wat: &str) -> std::path::PathBuf {
    let wasm = wat::parse_str(wat).unwrap();
    let path = std::env::temp_dir()
        .join(format!("pgh-services-{name}-{}.wasm", std::process::id()));
    std::fs::write(&path, &wasm).unwrap();
    path
}

#[test]
fn services_round_trip() {
    let prov = write_wasm("prov", PROVIDER_WAT);
    let call = write_wasm("call", CALLER_WAT);

    let vt = pgh_host_vtable { log: logging_vt_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    let (rc, err) = load_plugin("prov", &prov, "server", &["service-serve"]);
    assert_eq!(rc, 0, "{err}");
    drain_all();

    // The caller runs its four calls from init. ghost (no such plugin)
    // fails at once; ping streams two pages; boom fails with its message;
    // hang waits. (A method missing on a LOADED plugin would wait for the
    // registration instead: a pushed plugin's init may still be queued.)
    let (rc, err) = load_plugin("call", &call, "server", &["service-call"]);
    assert_eq!(rc, 0, "{err}");
    drain_all();
    // Only what the guest logged, not the host's own load/start lines.
    let got: Vec<String> = logs()
        .into_iter()
        .filter(|l| {
            l.starts_with("call: ") && !l.contains("loaded") && !l.contains("instance")
        })
        .collect();
    assert_eq!(
        got,
        vec![
            "call: nocall".to_string(),
            "call: po".to_string(),
            "call: ng".to_string(),
            "call: bad".to_string(),
        ],
        "all logs: {:?}",
        logs()
    );

    // Without service-call every call is refused before it starts.
    let (rc, err) = load_plugin("deny", &call, "server", &[]);
    assert_eq!(rc, 0, "{err}");
    drain_all();
    let denied = logs().iter().filter(|l| l.as_str() == "deny: denied").count();
    assert_eq!(denied, 4, "{:?}", logs());

    // Unloading the provider fails the open call with E_NO_SUCH_OBJECT.
    let name = CString::new("prov").unwrap();
    assert_eq!(unsafe { pgh_plugin_unload(name.as_ptr()) }, 0);
    drain_all();
    assert!(
        logs().iter().any(|l| l == "call: provider unloaded"),
        "{:?}",
        logs()
    );

    // With the provider gone a new call fails at once.
    let (rc, err) = load_plugin("call2", &call, "server", &["service-call"]);
    assert_eq!(rc, 0, "{err}");
    drain_all();
    let nocall = logs().iter().filter(|l| l.as_str() == "call2: nocall").count();
    assert_eq!(nocall, 4, "{:?}", logs());

    pgh_shutdown();
    std::fs::remove_file(&prov).ok();
    std::fs::remove_file(&call).ok();
}
