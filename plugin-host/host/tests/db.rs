//! The db_* imports end to end against the real pgh_* entry points: a
//! WAT guest migrates its schema with `db_exec_sync` in init, reads it
//! back with `db_query_sync`, then runs `db_exec`/`db_query` through the
//! worker pool and the doorbell, including a failing statement. A second
//! load of the same module without the `db` capability is denied. After
//! shutdown the file is inspected from the test with rusqlite. One test
//! function (pgh state is thread-local + a process-global vtable).

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

// Guest memory map (bytes). Strings are fixed at these addresses; the
// lengths come from the Rust constants below.
const M_OK: i32 = 0; // "db ok"
const M_DENIED: i32 = 16; // "db denied"
const M_EXEC_OK: i32 = 32; // "db exec ok"
const M_QUERY_OK: i32 = 48; // "db query ok"
const M_BAD_OK: i32 = 64; // "db bad sql ok"
const M_INIT_FAIL: i32 = 96; // "db init fail"
const M_ASYNC_FAIL: i32 = 112; // "db async fail"
const EV_NAME: i32 = 128; // "db-ev"
const SCRIPT: i32 = 160;
const QUERY_SQL: i32 = 320;
const PARAMS_A: i32 = 384;
const INSERT_SQL: i32 = 400;
const COUNT_SQL: i32 = 448;
const BOGUS_SQL: i32 = 480;
const EXEC_OUT: i32 = 512;
const OWNED_OUT: i32 = 528;
const BUMP: i32 = 1024;

const SCRIPT_SQL: &str = "CREATE TABLE kv (k TEXT PRIMARY KEY, v INTEGER); \
                          INSERT INTO kv VALUES ('a', 41); \
                          PRAGMA user_version = 1;";
const QUERY: &str = "SELECT v + 1 AS v FROM kv WHERE k = ?1";
const INSERT: &str = "INSERT INTO kv VALUES ('b', 7)";
const COUNT: &str = "SELECT count(*) FROM kv";
const BOGUS: &str = "BOGUS";

/// Rows block for `QUERY`: u16 ncols(1), str "v" (4+1), u32 nrows, then
/// the value: the type byte sits at 11, the i64 at 12, total 20 bytes.
const QUERY_ROWS_LEN: i32 = 2 + 4 + 1 + 4 + 1 + 8;
/// Rows block for `COUNT`: name "count(*)" is 8 bytes, so the type byte
/// sits at 2 + 4 + 8 + 4 = 18 and the i64 at 19.
const COUNT_TY_AT: i32 = 18;
const COUNT_VAL_AT: i32 = 19;

fn guest_wat() -> String {
    let params_a = encode_params(&[DbValue::from("a")]);
    assert_eq!(params_a.len(), 8);
    let log = |addr: i32, s: &str, level: i32| {
        format!("(call $log (i32.const {level}) (i32.const {addr}) (i32.const {}))", s.len())
    };
    let init_fail = log(M_INIT_FAIL, "db init fail", 3);
    let async_fail = log(M_ASYNC_FAIL, "db async fail", 3);
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
  (import "tmux" "db_query" (func $db_query (param i32 i32 i32 i32) (result i64)))

  (memory (export "memory") 1)
  (data (i32.const {M_OK}) "db ok")
  (data (i32.const {M_DENIED}) "db denied")
  (data (i32.const {M_EXEC_OK}) "db exec ok")
  (data (i32.const {M_QUERY_OK}) "db query ok")
  (data (i32.const {M_BAD_OK}) "db bad sql ok")
  (data (i32.const {M_INIT_FAIL}) "db init fail")
  (data (i32.const {M_ASYNC_FAIL}) "db async fail")
  (data (i32.const {EV_NAME}) "db-ev")
  (data (i32.const {SCRIPT}) "{script}")
  (data (i32.const {QUERY_SQL}) "{query}")
  (data (i32.const {PARAMS_A}) "{params_a}")
  (data (i32.const {INSERT_SQL}) "{insert}")
  (data (i32.const {COUNT_SQL}) "{count}")
  (data (i32.const {BOGUS_SQL}) "{bogus}")
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
    (local $rc i32) (local $p i32)
    (drop (call $subscribe
      (i32.wrap_i64 (call $intern (i32.const {EV_NAME}) (i32.const 5)))))
    ;; schema migration as a script: no params block at all
    (local.set $rc (call $db_exec_sync
      (i32.const {SCRIPT}) (i32.const {script_len})
      (i32.const 0) (i32.const 0) (i32.const {EXEC_OUT})))
    (if (i32.eq (local.get $rc) (i32.const -3))
      (then {denied} (return (i32.const 0))))
    (if (i32.ne (local.get $rc) (i32.const 0))
      (then {init_fail} (return (i32.const 0))))
    ;; exec struct: the one INSERT is the only counted change
    (if (i64.ne (i64.load (i32.const {EXEC_OUT})) (i64.const 1))
      (then {init_fail} (return (i32.const 0))))
    ;; a bound-parameter read back through an OwnedBuf rows block
    (local.set $rc (call $db_query_sync
      (i32.const {QUERY_SQL}) (i32.const {query_len})
      (i32.const {PARAMS_A}) (i32.const {params_len}) (i32.const {OWNED_OUT})))
    (if (i32.ne (local.get $rc) (i32.const 0))
      (then {init_fail} (return (i32.const 0))))
    (local.set $p (i32.load (i32.const {OWNED_OUT})))
    (if (i32.ne (i32.load offset=4 (i32.const {OWNED_OUT})) (i32.const {rows_len}))
      (then {init_fail} (return (i32.const 0))))
    (if (i32.ne (i32.load8_u offset=11 (local.get $p)) (i32.const 1))
      (then {init_fail} (return (i32.const 0))))
    (if (i64.ne (i64.load offset=12 (local.get $p)) (i64.const 42))
      (then {init_fail} (return (i32.const 0))))
    {ok}
    (i32.const 0))

  ;; one async request per event, by phase
  (func (export "pgh_on_event") (param i32 i32)
    (local $t i64)
    (local.set $t (i64.const 1))
    (if (i32.eq (global.get $phase) (i32.const 0))
      (then (local.set $t (call $db_exec
        (i32.const {INSERT_SQL}) (i32.const {insert_len}) (i32.const 0) (i32.const 0)))))
    (if (i32.eq (global.get $phase) (i32.const 1))
      (then (local.set $t (call $db_query
        (i32.const {COUNT_SQL}) (i32.const {count_len}) (i32.const 0) (i32.const 0)))))
    (if (i32.eq (global.get $phase) (i32.const 2))
      (then (local.set $t (call $db_exec
        (i32.const {BOGUS_SQL}) (i32.const {bogus_len}) (i32.const 0) (i32.const 0)))))
    (if (i64.le_s (local.get $t) (i64.const 0))
      (then {async_fail}))
    (global.set $phase (i32.add (global.get $phase) (i32.const 1))))

  (func (export "pgh_on_async_complete") (param $token i64) (param $err i32)
      (param $v0 i64) (param $v1 i64) (param $ptr i32) (param $len i32)
    (local $ph i32)
    (local.set $ph (i32.sub (global.get $phase) (i32.const 1)))
    (if (i32.eq (local.get $ph) (i32.const 0))
      (then
        (if (i32.and (i32.eqz (local.get $err)) (i64.eq (local.get $v0) (i64.const 1)))
          (then {exec_ok})
          (else {async_fail}))))
    (if (i32.eq (local.get $ph) (i32.const 1))
      (then
        (if (i32.and
              (i32.and (i32.eqz (local.get $err)) (i64.eq (local.get $v0) (i64.const 1)))
              (i32.and
                (i64.eq (local.get $v1) (i64.const 1))
                (i32.and
                  (i32.eq (i32.load8_u offset={count_ty} (local.get $ptr)) (i32.const 1))
                  (i64.eq (i64.load offset={count_val} (local.get $ptr)) (i64.const 2)))))
          (then {query_ok})
          (else {async_fail}))))
    (if (i32.eq (local.get $ph) (i32.const 2))
      (then
        (if (i32.eq (local.get $err) (i32.const 1))
          (then {bad_ok})
          (else {async_fail})))))

  (func (export "pgh_on_unload"))
)
"#,
        script = wat_bytes(SCRIPT_SQL.as_bytes()),
        script_len = SCRIPT_SQL.len(),
        query = wat_bytes(QUERY.as_bytes()),
        query_len = QUERY.len(),
        params_a = wat_bytes(&params_a),
        params_len = params_a.len(),
        insert = wat_bytes(INSERT.as_bytes()),
        insert_len = INSERT.len(),
        count = wat_bytes(COUNT.as_bytes()),
        count_len = COUNT.len(),
        bogus = wat_bytes(BOGUS.as_bytes()),
        bogus_len = BOGUS.len(),
        rows_len = QUERY_ROWS_LEN,
        count_ty = COUNT_TY_AT,
        count_val = COUNT_VAL_AT,
        denied = log(M_DENIED, "db denied", 1),
        ok = log(M_OK, "db ok", 1),
        exec_ok = log(M_EXEC_OK, "db exec ok", 1),
        query_ok = log(M_QUERY_OK, "db query ok", 1),
        bad_ok = log(M_BAD_OK, "db bad sql ok", 1),
        init_fail = init_fail,
        async_fail = async_fail,
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
fn db_end_to_end() {
    let root = std::env::temp_dir().join(format!("pgh-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));

    let wasm = wat::parse_str(&guest_wat()).expect("bad wat");
    let path = root.join("dbt.wasm");
    std::fs::write(&path, &wasm).unwrap();

    let vt = pgh_host_vtable { log: rec_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    // Load with the db capability: init migrates and reads back.
    let (rc, err) = load_plugin("dbt", &path, "server", &["read-state", "db"]);
    assert_eq!(rc, 0, "{err}");
    while pgh_drain(0) != 0 {}
    assert!(has_log("dbt: db ok"), "logs: {:?}", logs());
    let db_file = root.join("data/tmux/plugins/dbt/store.db");
    assert!(db_file.is_file(), "store.db not created");

    // Three async requests, one per event, each awaited on the doorbell.
    let fd = pgh_fs_notify_fd();
    assert!(fd >= 0);
    let ev = make_event("db-ev", None, None, None, None, &[]);
    event_round_trip(&ev, fd);
    assert!(has_log("dbt: db exec ok"), "logs: {:?}", logs());
    event_round_trip(&ev, fd);
    assert!(has_log("dbt: db query ok"), "logs: {:?}", logs());
    event_round_trip(&ev, fd);
    assert!(has_log("dbt: db bad sql ok"), "logs: {:?}", logs());
    assert!(!logs().iter().any(|l| l.contains("fail")), "logs: {:?}", logs());

    // The same module without the capability is denied at the first call.
    let (rc, err) = load_plugin("nodb", &path, "server", &["read-state"]);
    assert_eq!(rc, 0, "{err}");
    while pgh_drain(0) != 0 {}
    assert!(has_log("nodb: db denied"), "logs: {:?}", logs());
    assert!(!root.join("data/tmux/plugins/nodb/store.db").exists());

    for name in ["dbt", "nodb"] {
        let c = CString::new(name).unwrap();
        assert_eq!(unsafe { pgh_plugin_unload(c.as_ptr()) }, 0);
    }
    while pgh_drain(0) != 0 {}
    pgh_shutdown();

    // The file as left behind: the -wal file checkpointed away by the
    // last close, WAL mode, migrated, both rows.
    assert!(
        !root.join("data/tmux/plugins/dbt/store.db-wal").exists(),
        "WAL not checkpointed on close"
    );
    let conn = rusqlite::Connection::open(&db_file).unwrap();
    let mode: String =
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
    let version: i64 =
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 1);
    let v: i64 = conn
        .query_row("SELECT v FROM kv WHERE k = 'b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 7);
    let n: i64 = conn.query_row("SELECT count(*) FROM kv", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2);
    drop(conn);

    let _ = std::fs::remove_dir_all(&root);
}
