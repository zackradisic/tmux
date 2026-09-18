//! Content-addressed plugin cache.
//!
//! Every module that did not come from a local `path` lands here, keyed by
//! its blake3 hash: a fetch from a release or a url, and (later) a push
//! over a link. The layout is `$XDG_DATA_HOME/tmux/plugin-cache/cas/
//! <hash>.wasm`, with the capability sidecar at `<hash>.toml` so
//! `caps::compute` finds it with `with_extension("toml")` as for any
//! other module. Partial downloads live in `plugin-cache/tmp/` and move
//! into place with a rename once the hash checks out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const CACHE_DIR: &str = "tmux/plugin-cache";

/// Files nothing references stay this long after their last write before
/// `gc` removes them: another server sharing the data home may still be
/// running a module this one no longer knows.
const GC_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

pub fn root() -> Result<PathBuf, String> {
    Ok(crate::fsbox::data_home()?.join(CACHE_DIR))
}

fn cas_dir() -> Result<PathBuf, String> {
    let dir = root()?.join("cas");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Directory for in-flight downloads (created on demand).
pub fn tmp_dir() -> Result<PathBuf, String> {
    let dir = root()?.join("tmp");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Accept `blake3:<hex>` or bare `<hex>`; returns the lowercase hex.
pub fn normalize_hash(text: &str) -> Result<String, String> {
    let hex = text.strip_prefix("blake3:").unwrap_or(text).trim().to_ascii_lowercase();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("bad blake3 hash {text:?}"));
    }
    Ok(hex)
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The module path for `hash` (hex), whether or not it exists.
pub fn path_for(hash: &str) -> Result<PathBuf, String> {
    Ok(cas_dir()?.join(format!("{hash}.wasm")))
}

pub fn has(hash: &str) -> bool {
    path_for(hash).map(|p| p.is_file()).unwrap_or(false)
}

/// Store `bytes` (already in memory) and its sidecar; returns the hash and
/// the module path. A module already present is left alone, but the
/// sidecar is rewritten (or removed when `sidecar` is None).
pub fn put(bytes: &[u8], sidecar: Option<&str>) -> Result<(String, PathBuf), String> {
    let hash = hash_bytes(bytes);
    let path = path_for(&hash)?;
    if !path.is_file() {
        let tmp = tmp_dir()?.join(format!("{hash}.put.{}", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    write_sidecar(&path, sidecar)?;
    Ok((hash, path))
}

/// Move a downloaded file into the cache after checking that it hashes to
/// `expected` (hex). The file is removed when the hash does not match.
pub fn put_file(tmp: &Path, expected: &str, sidecar: Option<&str>) -> Result<PathBuf, String> {
    let bytes = std::fs::read(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let got = hash_bytes(&bytes);
    if got != expected {
        let _ = std::fs::remove_file(tmp);
        return Err(format!("hash mismatch: expected {expected}, got {got}"));
    }
    let path = path_for(expected)?;
    if path.is_file() {
        let _ = std::fs::remove_file(tmp);
    } else {
        std::fs::rename(tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    write_sidecar(&path, sidecar)?;
    Ok(path)
}

fn write_sidecar(module: &Path, sidecar: Option<&str>) -> Result<(), String> {
    let side = module.with_extension("toml");
    match sidecar {
        Some(text) => {
            std::fs::write(&side, text).map_err(|e| format!("{}: {e}", side.display()))
        }
        None => {
            match std::fs::remove_file(&side) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("{}: {e}", side.display())),
            }
        }
    }
}

/// The hash of a cache path (`<hash>.wasm`), if it is one.
pub fn hash_of_path(path: &Path) -> Option<String> {
    let dir = cas_dir().ok()?;
    if path.parent()? != dir {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    normalize_hash(stem).ok()
}

/// Remove cached modules (and sidecars) whose hash is not in `keep` and
/// whose last write is older than [`GC_AGE`]. Stale tmp files go too.
/// Returns how many modules went.
pub fn gc(keep: &HashSet<String>) -> usize {
    let mut removed = 0;
    let cutoff = SystemTime::now().checked_sub(GC_AGE);
    let old = |p: &Path| -> bool {
        match (std::fs::metadata(p).and_then(|m| m.modified()), cutoff) {
            (Ok(mtime), Some(cut)) => mtime < cut,
            _ => false,
        }
    };
    if let Ok(dir) = cas_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().is_none_or(|x| x != "wasm") {
                    continue;
                }
                let Some(hash) = hash_of_path(&path) else { continue };
                if keep.contains(&hash) || !old(&path) {
                    continue;
                }
                if std::fs::remove_file(&path).is_ok() {
                    let _ = std::fs::remove_file(path.with_extension("toml"));
                    removed += 1;
                }
            }
        }
    }
    if let Ok(dir) = root().map(|r| r.join("tmp")) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if old(&path) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("pgh-cas-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_DATA_HOME", &dir);
        let out = f();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn put_and_lookup() {
        with_home(|| {
            let (hash, path) = put(b"module", Some("[caps]\n")).unwrap();
            assert!(has(&hash));
            assert_eq!(path, path_for(&hash).unwrap());
            assert_eq!(std::fs::read_to_string(path.with_extension("toml")).unwrap(), "[caps]\n");
            assert_eq!(hash_of_path(&path).as_deref(), Some(hash.as_str()));
            // A second put without a sidecar removes it.
            put(b"module", None).unwrap();
            assert!(!path.with_extension("toml").exists());
        });
    }

    #[test]
    fn put_file_checks_hash() {
        with_home(|| {
            let tmp = tmp_dir().unwrap().join("dl");
            std::fs::write(&tmp, b"payload").unwrap();
            let bad = "0".repeat(64);
            let err = put_file(&tmp, &bad, None).unwrap_err();
            assert!(err.contains("hash mismatch"), "{err}");
            assert!(!tmp.exists());
            std::fs::write(&tmp, b"payload").unwrap();
            let good = hash_bytes(b"payload");
            let path = put_file(&tmp, &good, None).unwrap();
            assert!(path.is_file() && !tmp.exists());
        });
    }

    #[test]
    fn normalize() {
        let h = "a".repeat(64);
        assert_eq!(normalize_hash(&format!("blake3:{h}")).unwrap(), h);
        assert_eq!(normalize_hash(&h.to_uppercase()).unwrap(), h);
        assert!(normalize_hash("blake3:abc").is_err());
    }

    #[test]
    fn gc_keeps_fresh_and_referenced() {
        with_home(|| {
            let (h1, p1) = put(b"one", None).unwrap();
            let (h2, p2) = put(b"two", None).unwrap();
            // Fresh files stay even when unreferenced.
            assert_eq!(gc(&HashSet::new()), 0);
            assert!(p1.exists() && p2.exists());
            // Age both; only the unreferenced one goes.
            let old = SystemTime::now() - GC_AGE - Duration::from_secs(60);
            for p in [&p1, &p2] {
                let f = std::fs::File::options().write(true).open(p).unwrap();
                f.set_modified(old).unwrap();
            }
            let keep: HashSet<String> = [h1.clone()].into_iter().collect();
            assert_eq!(gc(&keep), 1);
            assert!(p1.exists() && !p2.exists());
            let _ = h2;
        });
    }
}
