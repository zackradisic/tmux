//! Manifest sync (pgh_plugin_sync) against the real pgh_* entry points:
//! load, no-op resync, config update, sweep of removed entries, adoption
//! and demotion of interactive loads, disabled entries, and atomic
//! rejection of a bad manifest. One test function (pgh state is
//! thread-local + a process-global vtable).

mod common;

use std::ffi::{c_void, CString};

use common::*;
use plugin_host::*;

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

#[test]
fn manifest_sync() {
    let dir = std::env::temp_dir().join(format!("pgh-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wasm = wat::parse_str(GUEST_WAT).unwrap();
    std::fs::write(dir.join("alpha.wasm"), &wasm).unwrap();
    std::fs::write(dir.join("beta.wasm"), &wasm).unwrap();
    let manifest = dir.join("plugins.toml");

    let vt = base_vtable();
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
    let (rc, err) = load_plugin("gamma", &dir.join("beta.wasm"), "server", &[]);
    assert_eq!(rc, 0, "{err}");
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
