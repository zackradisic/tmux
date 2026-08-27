//! Microbenchmarks for the fs host calls and the guest<->host ABI
//! boundary they sit on. Not a test: run it explicitly.
//!
//!   cargo test --release --test bench_fs -- --nocapture --ignored
//!
//! Method. Each scenario is one generated WAT guest whose `pgh_on_event`
//! runs the import K times in a loop. The same module is loaded twice -
//! once with K iterations, once with 0 - and the difference divided by
//! (events x K) is the per-call cost. Subtracting the K=0 run removes
//! event delivery, pgh_alloc, the wasm trampoline into on_event and the
//! drain bookkeeping, leaving the import itself.
//!
//! The data directory is placed on tmpfs, so the numbers measure the ABI
//! boundary and the page cache, not a disk.

mod common;

use std::ffi::CString;
use std::os::raw::{c_char, c_int};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::*;
use plugin_host::*;

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

// Guest memory map (bytes).
const EV_NAME: i32 = 64; // "bench-ev"
const PATH: i32 = 128; // "f"
const SCRATCH: i32 = 256; // fs_root out, 256 cap
const LEN_OUT: i32 = 512;
const EOF_OUT: i32 = 516;
const BUMP_LO: i32 = 1024; // pgh_alloc arena, wraps at BUMP_HI
const BUMP_HI: i32 = 60000;
const DATA: i32 = 65536; // 2 MiB source buffer
const OUT: i32 = 65536 + 2 * 1024 * 1024; // 2 MiB destination
const PAGES: i32 = 80; // 5 MiB linear memory

fn guest_wat(iters: u32, body: &str) -> String {
    format!(
        r#"
(module
  (import "tmux" "intern" (func $intern (param i32 i32) (result i64)))
  (import "tmux" "subscribe" (func $subscribe (param i32) (result i32)))
  (import "tmux" "log" (func $log (param i32 i32 i32)))
  (import "tmux" "fs_root" (func $fs_root (param i32 i32 i32) (result i32)))
  (import "tmux" "fs_read_sync"
    (func $fs_read_sync (param i32 i32 i64 i32 i32 i32 i32) (result i32)))
  (import "tmux" "fs_write_sync"
    (func $fs_write_sync (param i32 i32 i32 i32 i32) (result i64)))
  (import "tmux" "fs_write"
    (func $fs_write (param i32 i32 i32 i32 i32) (result i64)))
  (import "tmux" "fs_read"
    (func $fs_read (param i32 i32 i64 i32 i32) (result i64)))

  (memory (export "memory") {PAGES})
  (data (i32.const {EV_NAME}) "bench-ev")
  (data (i32.const {PATH}) "f")
  (global $next (mut i32) (i32.const {BUMP_LO}))

  (func (export "pgh_abi_version") (result i32) (i32.const 1))

  (func (export "pgh_alloc") (param i32) (result i32)
    (local $p i32)
    (if (i32.gt_u (i32.add (global.get $next) (local.get 0))
                  (i32.const {BUMP_HI}))
      (then (global.set $next (i32.const {BUMP_LO}))))
    (local.set $p (global.get $next))
    (global.set $next (i32.add (global.get $next) (local.get 0)))
    (local.get $p))

  (func (export "pgh_free") (param i32 i32))

  (func (export "pgh_init") (param i32 i32) (result i32)
    (drop (call $subscribe
      (i32.wrap_i64 (call $intern (i32.const {EV_NAME}) (i32.const 8)))))
    (i32.const 0))

  (func (export "pgh_on_event") (param i32 i32)
    (local $i i32)
    (local.set $i (i32.const 0))
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (i32.const {iters})))
        {body}
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l))))

  (func (export "pgh_on_async_complete") (param i64 i32 i64 i64 i32 i32))
  (func (export "pgh_on_unload"))
)
"#
    )
}

fn body_log() -> String {
    format!("(call $log (i32.const 0) (i32.const {EV_NAME}) (i32.const 8))")
}

fn body_fs_root() -> String {
    format!(
        "(drop (call $fs_root (i32.const {SCRATCH}) (i32.const 256) \
         (i32.const {LEN_OUT})))"
    )
}

fn body_read_sync(size: i32) -> String {
    format!(
        "(drop (call $fs_read_sync (i32.const {PATH}) (i32.const 1) \
         (i64.const 0) (i32.const {OUT}) (i32.const {size}) \
         (i32.const {LEN_OUT}) (i32.const {EOF_OUT})))"
    )
}

fn body_write_sync(size: i32) -> String {
    format!(
        "(drop (call $fs_write_sync (i32.const {PATH}) (i32.const 1) \
         (i32.const {DATA}) (i32.const {size}) (i32.const 0)))"
    )
}

fn body_write_async(size: i32) -> String {
    format!(
        "(drop (call $fs_write (i32.const {PATH}) (i32.const 1) \
         (i32.const {DATA}) (i32.const {size}) (i32.const 0)))"
    )
}

fn body_read_async(size: i32) -> String {
    format!(
        "(drop (call $fs_read (i32.const {PATH}) (i32.const 1) \
         (i64.const 0) (i32.const {OUT}) (i32.const {size})))"
    )
}

struct Bench {
    dir: std::path::PathBuf,
    ev: Vec<u8>,
    n: u32,
}

impl Bench {
    /// Load a generated guest under `name`, seed its sandbox with the
    /// read fixture, and instantiate it.
    fn load(&self, name: &str, iters: u32, body: &str) {
        let wasm = wat::parse_str(&guest_wat(iters, body))
            .unwrap_or_else(|e| panic!("{name}: bad wat: {e}"));
        let path = self.dir.join(format!("{name}.wasm"));
        std::fs::write(&path, &wasm).unwrap();

        // The fixture the read scenarios pull from, 2 MiB of 'x'.
        let sandbox = self.dir.join("data/tmux/plugins").join(name);
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("f"), vec![b'x'; 2 * 1024 * 1024]).unwrap();

        let (rc, err) = load_plugin(
            name,
            &path,
            "server",
            &["read-state", "fs-read", "fs-write"],
        );
        assert_eq!(rc, 0, "{name}: load failed: {err}");
        while pgh_drain(0) != 0 {}
    }

    fn unload(&self, name: &str) {
        let c = CString::new(name).unwrap();
        unsafe { pgh_plugin_unload(c.as_ptr()) };
        while pgh_drain(0) != 0 {}
    }

    /// Total wall time for `n` event deliveries into the loaded guest.
    fn run(&self, warmup: u32) -> Duration {
        for _ in 0..warmup {
            notify(&self.ev);
            while pgh_drain(0) != 0 {}
        }
        let t0 = Instant::now();
        for _ in 0..self.n {
            notify(&self.ev);
            while pgh_drain(0) != 0 {}
        }
        t0.elapsed()
    }

    /// One scenario: time the K-iteration guest and the 0-iteration
    /// guest, and report the difference per import call.
    fn scenario(&self, label: &str, iters: u32, body: String, bytes: u64) {
        LOGS.lock().unwrap().clear();

        let hot = format!("b_{}_hot", label.replace(['/', ' '], "_"));
        self.load(&hot, iters, &body);
        let t_hot = self.run(20);
        self.unload(&hot);

        let cold = format!("b_{}_base", label.replace(['/', ' '], "_"));
        self.load(&cold, 0, &body);
        let t_base = self.run(20);
        self.unload(&cold);

        let calls = u64::from(self.n) * u64::from(iters);
        let delta = t_hot.saturating_sub(t_base).as_nanos() as f64;
        let per_call = delta / calls as f64;

        let thru = if bytes > 0 && delta > 0.0 {
            let gbps = (bytes * calls) as f64 / delta;
            format!("{gbps:>7.2} GB/s")
        } else {
            String::from("          -")
        };

        let budget: Vec<String> = LOGS
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.contains("budget") || l.contains("trap"))
            .cloned()
            .collect();
        let flag = if budget.is_empty() { "" } else { "  !! BUDGET" };

        println!(
            "{label:<24} {iters:>5} x {n:<5} {per_call:>9.0} ns/call  {thru}{flag}",
            n = self.n
        );
        for b in budget {
            println!("      {b}");
        }
    }

    /// Move every finished fs completion out of the worker's channel and
    /// into the guests, clearing the doorbell. The submit-only scenarios
    /// fire thousands of jobs and never drain them, which leaves ARMED
    /// set and one stale byte in the eventfd - the next round-trip
    /// measurement would consume that stale ring instead of its own.
    fn clear_async_backlog(&self) {
        for _ in 0..100 {
            pgh_fs_drain();
            let mut moved = false;
            while pgh_drain(0) != 0 {
                moved = true;
            }
            if !moved {
                break;
            }
        }
        pgh_fs_drain();
        while pgh_drain(0) != 0 {}
    }

    /// End-to-end async round trip: submit one job, sleep on the real
    /// doorbell fd, drain, deliver the completion to the guest.
    fn async_roundtrip(&self, label: &str, body: String, bytes: u64) {
        let name = format!("a_{}", label.replace(['/', ' '], "_"));
        self.clear_async_backlog();
        self.load(&name, 1, &body);
        let fd = pgh_fs_notify_fd();
        assert!(fd >= 0, "no doorbell fd");

        let iters = 2000u32;
        for phase in 0..2 {
            let count = if phase == 0 { 50 } else { iters };
            let t0 = Instant::now();
            for _ in 0..count {
                notify(&self.ev);
                while pgh_drain(0) != 0 {}
                // Sleep until the worker rings, exactly like libevent.
                let mut pfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let rc = unsafe { libc::poll(&mut pfd, 1, 2000) };
                assert!(rc > 0, "{label}: doorbell timed out (phase {phase})");
                pgh_fs_drain();
                while pgh_drain(0) != 0 {}
            }
            if phase == 1 {
                let dt = t0.elapsed();
                let per = dt.as_nanos() as f64 / f64::from(count);
                let thru = if bytes > 0 {
                    format!(
                        "{:>7.2} GB/s",
                        (bytes * u64::from(count)) as f64
                            / dt.as_nanos() as f64
                    )
                } else {
                    String::from("          -")
                };
                println!(
                    "{label:<24} {:>5} x {:<5} {per:>9.0} ns/rt   {thru}",
                    1, count
                );
            }
        }
        self.unload(&name);
    }
}

/// Long single-scenario run for `perf record`. Selected with
/// PGH_BENCH_PROFILE=<scenario>; the process then does nothing else, so
/// the flame graph is that one path.
#[test]
#[ignore = "profiling driver; PGH_BENCH_PROFILE=<name> cargo test ..."]
fn profile_one() {
    let Ok(which) = std::env::var("PGH_BENCH_PROFILE") else {
        println!("set PGH_BENCH_PROFILE to one of: \
                  read_sync_4k, read_sync_1m, write_sync_4k, fs_root, \
                  write_async_4k");
        return;
    };
    let root = std::path::Path::new("/dev/shm")
        .join(format!("pgh-prof-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));

    let vt = pgh_host_vtable { log: rec_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    const KB: i32 = 1024;
    const MB: i32 = 1024 * 1024;
    let (iters, body) = match which.as_str() {
        "read_sync_4k" => (200u32, body_read_sync(4 * KB)),
        "read_sync_1m" => (8, body_read_sync(MB)),
        "write_sync_4k" => (200, body_write_sync(4 * KB)),
        "fs_root" => (200, body_fs_root()),
        "write_async_4k" => (20, body_write_async(4 * KB)),
        other => panic!("unknown scenario {other}"),
    };

    let b = Bench {
        dir: root.clone(),
        ev: make_event("bench-ev", None, None, None, None, &[]),
        n: 4000,
    };
    b.load("prof", iters, &body);
    for l in LOGS.lock().unwrap().iter() {
        println!("  log: {l}");
    }
    let t = b.run(20);
    println!(
        "{which}: {} events x {iters} calls in {:?} ({:.0} ns/call)",
        b.n,
        t,
        t.as_nanos() as f64 / (f64::from(b.n) * f64::from(iters))
    );
    b.unload("prof");
    pgh_shutdown();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
#[ignore = "benchmark; run with --ignored --nocapture"]
fn bench_fs_abi() {
    // tmpfs when available, so the disk never enters the measurement.
    let base = std::path::Path::new("/dev/shm");
    let root = if base.is_dir() {
        base.join(format!("pgh-bench-{}", std::process::id()))
    } else {
        std::env::temp_dir().join(format!("pgh-bench-{}", std::process::id()))
    };
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));

    let vt = pgh_host_vtable { log: rec_log, ..base_vtable() };
    assert_eq!(unsafe { pgh_init(&vt) }, 0);

    let b = Bench {
        dir: root.clone(),
        ev: make_event("bench-ev", None, None, None, None, &[]),
        n: 200,
    };

    const KB: i32 = 1024;
    const MB: i32 = 1024 * 1024;

    println!("\nfs / ABI boundary microbenchmarks");
    println!("data dir: {}", root.join("data").display());
    println!(
        "\n{:<24} {:>5}   {:<5} {:>9}            {}",
        "scenario", "K", "evts", "cost", "throughput"
    );
    println!("{}", "-".repeat(78));

    // --- boundary floor: how much does one event delivery cost? -------
    {
        b.load("b_delivery", 0, "");
        let t = b.run(20);
        b.unload("b_delivery");
        println!(
            "{:<24} {:>5}   {:<5} {:>9.0} ns/event  {}",
            "event delivery (K=0)",
            0,
            b.n,
            t.as_nanos() as f64 / f64::from(b.n),
            "          -"
        );
    }

    // --- imports with no syscall --------------------------------------
    b.scenario("log (mock vtable)", 500, body_log(), 0);
    b.scenario("fs_root", 200, body_fs_root(), 0);

    // --- sync fs ------------------------------------------------------
    b.scenario("fs_read_sync 0B", 200, body_read_sync(0), 0);
    b.scenario("fs_read_sync 4K", 100, body_read_sync(4 * KB), 4 * 1024);
    b.scenario("fs_read_sync 64K", 50, body_read_sync(64 * KB), 64 * 1024);
    b.scenario("fs_read_sync 256K", 16, body_read_sync(256 * KB), 256 * 1024);
    b.scenario("fs_read_sync 1M", 8, body_read_sync(MB), 1024 * 1024);
    b.scenario("fs_write_sync 4K", 50, body_write_sync(4 * KB), 4 * 1024);
    b.scenario("fs_write_sync 64K", 20, body_write_sync(64 * KB), 64 * 1024);
    b.scenario("fs_write_sync 256K", 8, body_write_sync(256 * KB), 256 * 1024);
    b.scenario("fs_write_sync 1M", 4, body_write_sync(MB), 1024 * 1024);

    // --- async submit cost (the import only, worker excluded) ---------
    b.scenario("fs_write submit 4K", 20, body_write_async(4 * KB), 0);
    b.scenario("fs_read submit 4K", 20, body_read_async(4 * KB), 0);

    // --- what the fixed per-call cost is made of ----------------------
    // fs_path() does, on EVERY fs call: plugin_data_dir() ->
    // create_dir_all(root), then sandboxed_path() -> create_dir_all(parent)
    // on writes, root.canonicalize(), parent.canonicalize(). Time those
    // same std calls on the same tmpfs paths to size each one.
    {
        println!("{}", "-".repeat(78));
        let sandbox = root.join("data/tmux/plugins/b_fs_root_hot");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::write(sandbox.join("f"), vec![b'x'; 4096]).unwrap();
        let reps = 20_000;

        let time = |label: &str, f: &mut dyn FnMut()| {
            for _ in 0..200 {
                f();
            }
            let t0 = Instant::now();
            for _ in 0..reps {
                f();
            }
            let per = t0.elapsed().as_nanos() as f64 / f64::from(reps);
            println!("{label:<24} {:>5}   {:<5} {per:>9.0} ns/call", "-", reps);
        };

        time("  create_dir_all(root)", &mut || {
            std::fs::create_dir_all(&sandbox).unwrap();
        });
        time("  canonicalize(root)", &mut || {
            sandbox.canonicalize().unwrap();
        });
        let file = sandbox.join("f");
        time("  open+stat+seek+read 4K", &mut || {
            use std::io::{Read, Seek};
            let mut f = std::fs::File::open(&file).unwrap();
            let _ = f.metadata().unwrap().len();
            f.seek(std::io::SeekFrom::Start(0)).unwrap();
            let mut buf = [0u8; 4096];
            let _ = f.read(&mut buf).unwrap();
        });
    }

    // --- async end to end, through the real eventfd -------------------
    println!("{}", "-".repeat(78));
    b.async_roundtrip("fs_write rt 4K", body_write_async(4 * KB), 4 * 1024);
    b.async_roundtrip("fs_write rt 64K", body_write_async(64 * KB), 64 * 1024);
    b.async_roundtrip("fs_write rt 1M", body_write_async(MB), 1024 * 1024);
    b.async_roundtrip("fs_read rt 4K", body_read_async(4 * KB), 4 * 1024);
    b.async_roundtrip("fs_read rt 1M", body_read_async(MB), 1024 * 1024);
    println!();

    pgh_shutdown();
    let _ = std::fs::remove_dir_all(&root);
}
