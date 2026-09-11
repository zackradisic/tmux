//! ZSTD_REF parameters and `db_decompress` end to end. A WAT guest holds
//! a 4 KiB repetitive payload in its memory and binds it by reference:
//! the sync path must reject the reference, the async `db_exec` must
//! store a compressed BLOB, `db_query_sync` reads the frame back and
//! `db_decompress` inflates it to the original bytes, and a reference
//! past the end of linear memory must fail before anything runs. After
//! shutdown the stored frame is checked from the test with rusqlite and
//! zstd. One test function (pgh state is thread-local + a process-global
//! vtable).

mod common;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;

use common::*;
use plugin_host::*;
use tmux_plugin_abi::db::{encode_params, DbValue};

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

unsafe extern "C" fn rec_log(
    _level: c_int,
    plugin: *const c_char,
    msg: *const c_char,
) {
    let plugin = std::ffi::CStr::from_ptr(plugin).to_string_lossy();
    let msg = std::ffi::CStr::from_ptr(msg).to_string_lossy();
    LOGS.lock().unwrap().push(format!("{plugin}: {msg}"));
}

fn logs() -> Vec<String> {
    LOGS.lock().unwrap().clone()
}

fn has_log(s: &str) -> bool {
    logs().iter().any(|l| l == s)
}

/// Escape bytes for a WAT data segment.
fn wat_bytes(b: &[u8]) -> String {
    b.iter().map(|x| format!("\\{x:02x}")).collect()
}

// Guest memory map (bytes).
const M_OK: i32 = 0; // "zstd ok"
const M_FAIL: i32 = 16; // "zstd fail"
const M_SYNC_OK: i32 = 32; // "zstd sync rejected"
const M_DEC_OK: i32 = 64; // "zstd decompress ok"
const M_OOB_OK: i32 = 96; // "zstd oob rejected"
const EV_NAME: i32 = 128; // "z-ev"
const SCHEMA_SQL: i32 = 160;
const INSERT_SQL: i32 = 256;
const SELECT_SQL: i32 = 320;
const PARAMS_REF: i32 = 384;
const PARAMS_OOB: i32 = 400;
const EXEC_OUT: i32 = 512;
const ROWS_OUT: i32 = 528;
const RAW_OUT: i32 = 544;
const PAYLOAD: i32 = 2048;
const PAYLOAD_LEN: i32 = 4096;
const BUMP: i32 = 8192;

const SCHEMA: &str = "CREATE TABLE b (id INTEGER PRIMARY KEY, data BLOB NOT NULL)";
const INSERT: &str = "INSERT INTO b (id, data) VALUES (1, ?1)";
const SELECT: &str = "SELECT data FROM b WHERE id = 1";

/// Rows block for `SELECT`: u16 ncols(1), str "data" (4+4), u32 nrows,
/// then the BLOB value: type byte at 14, u32 len at 15, bytes at 19.
const ROWS_TY_AT: i32 = 14;
const ROWS_LEN_AT: i32 = 15;
const ROWS_DATA_AT: i32 = 19;

fn payload() -> Vec<u8> {
    let line = b"the quick brown fox 0123456789 \x1b[31mred\x1b[0m\n";
    let mut v = Vec::with_capacity(PAYLOAD_LEN as usize);
    while v.len() < PAYLOAD_LEN as usize {
        v.extend_from_slice(line);
    }
    v.truncate(PAYLOAD_LEN as usize);
    v
}

fn guest_wat() -> String {
    let params_ref = encode_params(&[DbValue::ZstdRef {
        ptr: PAYLOAD as u32,
        len: PAYLOAD_LEN as u32,
    }]);
    assert_eq!(params_ref.len(), 11);
    // One page of memory is 65536 bytes; this reference runs past it.
    let params_oob = encode_params(&[DbValue::ZstdRef { ptr: 60_000, len: 100_000 }]);
    let log = |addr: i32, s: &str, level: i32| {
        format!("(call $log (i32.const {level}) (i32.const {addr}) (i32.const {}))", s.len())
    };
    let fail = log(M_FAIL, "zstd fail", 3);
    let pay = payload();
    let first = i64::from_le_bytes(pay[0..8].try_into().unwrap());
    let last = i64::from_le_bytes(pay[pay.len() - 8..].try_into().unwrap());
    format!(
        r#"
(module
  (import "tmux" "intern" (func $intern (param i32 i32) (result i64)))
  (import "tmux" "subscribe" (func $subscribe (param i32) (result i32)))
  (import "tmux" "log" (func $log (param i32 i32 i32)))
  (import "tmux" "db_exec_sync"
    (func $db_exec_sync (param i32 i32 i32 i32 i32) (result i32)))
  (import "tmux" "db_query_sync"
    (func $db_query_sync (param i32 i32 i32 i32 i32) (result i32)))
  (import "tmux" "db_exec" (func $db_exec (param i32 i32 i32 i32) (result i64)))
  (import "tmux" "db_decompress"
    (func $db_decompress (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const {M_OK}) "zstd ok")
  (data (i32.const {M_FAIL}) "zstd fail")
  (data (i32.const {M_SYNC_OK}) "zstd sync rejected")
  (data (i32.const {M_DEC_OK}) "zstd decompress ok")
  (data (i32.const {M_OOB_OK}) "zstd oob rejected")
  (data (i32.const {EV_NAME}) "z-ev")
  (data (i32.const {SCHEMA_SQL}) "{schema}")
  (data (i32.const {INSERT_SQL}) "{insert}")
  (data (i32.const {SELECT_SQL}) "{select}")
  (data (i32.const {PARAMS_REF}) "{params_ref}")
  (data (i32.const {PARAMS_OOB}) "{params_oob}")
  (data (i32.const {PAYLOAD}) "{payload}")
  (global $next (mut i32) (i32.const {BUMP}))
  (global $phase (mut i32) (i32.const 0))

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
    (local $rc i32)
    (drop (call $subscribe
      (i32.wrap_i64 (call $intern (i32.const {EV_NAME}) (i32.const 4)))))
    (local.set $rc (call $db_exec_sync
      (i32.const {SCHEMA_SQL}) (i32.const {schema_len})
      (i32.const 0) (i32.const 0) (i32.const {EXEC_OUT})))
    (if (i32.ne (local.get $rc) (i32.const 0))
      (then {fail} (return (i32.const 0))))
    ;; a ZSTD_REF on the sync path is a bad request (-1)
    (local.set $rc (call $db_exec_sync
      (i32.const {INSERT_SQL}) (i32.const {insert_len})
      (i32.const {PARAMS_REF}) (i32.const {params_ref_len}) (i32.const {EXEC_OUT})))
    (if (i32.eq (local.get $rc) (i32.const -1))
      (then {sync_ok})
      (else {fail}))
    {ok}
    (i32.const 0))

  (func (export "pgh_on_event") (param i32 i32)
    (local $t i64)
    (local.set $t (i64.const 1))
    (if (i32.eq (global.get $phase) (i32.const 0))
      (then (local.set $t (call $db_exec
        (i32.const {INSERT_SQL}) (i32.const {insert_len})
        (i32.const {PARAMS_REF}) (i32.const {params_ref_len})))
        (if (i64.le_s (local.get $t) (i64.const 0)) (then {fail}))))
    (if (i32.eq (global.get $phase) (i32.const 1))
      (then (local.set $t (call $db_exec
        (i32.const {INSERT_SQL}) (i32.const {insert_len})
        (i32.const {PARAMS_OOB}) (i32.const {params_oob_len})))
        ;; rejected before a task starts: -E_BAD_REQUEST, no completion
        (if (i64.eq (local.get $t) (i64.const -1))
          (then {oob_ok})
          (else {fail}))))
    (global.set $phase (i32.add (global.get $phase) (i32.const 1))))

  (func (export "pgh_on_async_complete") (param $token i64) (param $err i32)
      (param $v0 i64) (param $v1 i64) (param $ptr i32) (param $len i32)
    (local $rc i32) (local $rows i32) (local $blob_len i32) (local $raw i32)
    (if (i32.or (i32.ne (local.get $err) (i32.const 0))
                (i64.ne (local.get $v0) (i64.const 1)))
      (then {fail} (return)))
    ;; read the stored frame back
    (local.set $rc (call $db_query_sync
      (i32.const {SELECT_SQL}) (i32.const {select_len})
      (i32.const 0) (i32.const 0) (i32.const {ROWS_OUT})))
    (if (i32.ne (local.get $rc) (i32.const 0)) (then {fail} (return)))
    (local.set $rows (i32.load (i32.const {ROWS_OUT})))
    (if (i32.ne (i32.load8_u offset={rows_ty} (local.get $rows)) (i32.const 4))
      (then {fail} (return)))
    (local.set $blob_len (i32.load offset={rows_len} (local.get $rows)))
    ;; the frame must be smaller than the payload
    (if (i32.ge_u (local.get $blob_len) (i32.const {payload_len}))
      (then {fail} (return)))
    ;; inflate it
    (local.set $rc (call $db_decompress
      (i32.add (local.get $rows) (i32.const {rows_data}))
      (local.get $blob_len) (i32.const {RAW_OUT})))
    (if (i32.ne (local.get $rc) (i32.const 0)) (then {fail} (return)))
    (local.set $raw (i32.load (i32.const {RAW_OUT})))
    (if (i32.ne (i32.load offset=4 (i32.const {RAW_OUT})) (i32.const {payload_len}))
      (then {fail} (return)))
    (if (i64.ne (i64.load (local.get $raw)) (i64.const {first}))
      (then {fail} (return)))
    (if (i64.ne (i64.load offset={last_at} (local.get $raw)) (i64.const {last}))
      (then {fail} (return)))
    {dec_ok})

  (func (export "pgh_on_unload"))
)
"#,
        schema = wat_bytes(SCHEMA.as_bytes()),
        schema_len = SCHEMA.len(),
        insert = wat_bytes(INSERT.as_bytes()),
        insert_len = INSERT.len(),
        select = wat_bytes(SELECT.as_bytes()),
        select_len = SELECT.len(),
        params_ref = wat_bytes(&params_ref),
        params_ref_len = params_ref.len(),
        params_oob = wat_bytes(&params_oob),
        params_oob_len = params_oob.len(),
        payload = wat_bytes(&pay),
        payload_len = PAYLOAD_LEN,
        rows_ty = ROWS_TY_AT,
        rows_len = ROWS_LEN_AT,
        rows_data = ROWS_DATA_AT,
        first = first,
        last = last,
        last_at = PAYLOAD_LEN - 8,
        ok = log(M_OK, "zstd ok", 1),
        sync_ok = log(M_SYNC_OK, "zstd sync rejected", 1),
        dec_ok = log(M_DEC_OK, "zstd decompress ok", 1),
        oob_ok = log(M_OOB_OK, "zstd oob rejected", 1),
        fail = fail,
    )
}

/// Deliver one event and wait for its async completion: drain, sleep on
/// the doorbell exactly like libevent, move the completion over, drain.
fn event_round_trip(ev: &[u8], fd: i32) {
    notify(ev);
    while pgh_drain(0) != 0 {}
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let rc = unsafe { libc::poll(&mut pfd, 1, 5000) };
    assert!(rc > 0, "doorbell timed out; logs: {:?}", logs());
    pgh_fs_drain();
    while pgh_drain(0) != 0 {}
}

#[test]
fn db_zstd_round_trip() {
    let root = std::env::temp_dir().join(format!("pgh-dbz-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));

    let wasm = wat::parse_str(&guest_wat()).expect("bad wat");
    let path = root.join("dbz.wasm");
    std::fs::write(&path, &wasm).unwrap();

    let vt = pgh_host_vtable { log: rec_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    let (rc, err) = load_plugin("dbz", &path, "server", &["read-state", "db"]);
    assert_eq!(rc, 0, "{err}");
    while pgh_drain(0) != 0 {}
    assert!(has_log("dbz: zstd ok"), "logs: {:?}", logs());
    assert!(has_log("dbz: zstd sync rejected"), "logs: {:?}", logs());

    // The async INSERT with a ZSTD_REF, then the guest reads the frame
    // back and inflates it inside the completion callback.
    let fd = pgh_fs_notify_fd();
    assert!(fd >= 0);
    let ev = make_event("z-ev", None, None, None, None, &[]);
    event_round_trip(&ev, fd);
    assert!(has_log("dbz: zstd decompress ok"), "logs: {:?}", logs());

    // A reference past linear memory fails synchronously; no task, no
    // completion, so a plain drain is enough.
    notify(&ev);
    while pgh_drain(0) != 0 {}
    assert!(has_log("dbz: zstd oob rejected"), "logs: {:?}", logs());
    assert!(!logs().iter().any(|l| l.contains("fail")), "logs: {:?}", logs());

    let c = CString::new("dbz").unwrap();
    assert_eq!(unsafe { pgh_plugin_unload(c.as_ptr()) }, 0);
    while pgh_drain(0) != 0 {}
    pgh_shutdown();

    // What is on disk is one zstd frame that inflates to the payload.
    let db_file = root.join("data/tmux/plugins/dbz/store.db");
    let conn = rusqlite::Connection::open(&db_file).unwrap();
    let stored: Vec<u8> = conn
        .query_row("SELECT data FROM b WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(&stored[0..4], &[0x28, 0xb5, 0x2f, 0xfd], "not a zstd frame");
    assert!(stored.len() < 512, "frame is {} bytes", stored.len());
    assert_eq!(zstd::decode_all(&stored[..]).unwrap(), payload());
    let n: i64 = conn.query_row("SELECT count(*) FROM b", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "the rejected inserts must not have run");
    drop(conn);

    let _ = std::fs::remove_dir_all(&root);
}
