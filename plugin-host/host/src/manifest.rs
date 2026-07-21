//! Manifest sync: declarative plugin loading.
//!
//! A TOML manifest is the desired state for the *managed* plugin pool;
//! `sync_manifest` reconciles the world against it: load new entries,
//! upsert changed ones (reusing the load-plugin reconcile rules), and
//! unload managed plugins the manifest no longer names. Interactive
//! `load-plugin` definitions are unmanaged and never swept; a manifest
//! entry with the same name adopts them.
//!
//! Identity is the `[plugins.NAME]` key, not the file path: renaming the
//! .wasm and updating `path` is a no-op when the content is unchanged.
//! Ownership is pool-global, not per-manifest-file, so renaming the
//! manifest itself cannot orphan plugins.
//!
//! Validation is atomic: a manifest that fails to parse or validate
//! changes nothing. Per-entry *apply* failures (e.g. a module that no
//! longer compiles) are reported and counted; the rest of the sync
//! proceeds.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tmux_plugin_abi::{LoadDescriptor, ScopeType};

use crate::hostlog;
use crate::reload;
use crate::state::REGISTRY;

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    path: String,
    #[serde(default)]
    scope: Option<ScopeType>,
    #[serde(default)]
    caps: Vec<String>,
    #[serde(default)]
    config: Option<toml::Value>,
    #[serde(default = "default_true")]
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    plugins: BTreeMap<String, ManifestEntry>,
}

/// Expand `~/` and resolve relative paths against `base` (the manifest's
/// directory for entries; the process cwd for the manifest itself).
fn expand_path(path: &str, base: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        toml::Value::String(s) => J::String(s.clone()),
        toml::Value::Integer(n) => J::Number((*n).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map_or(J::Null, J::Number),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(dt) => J::String(dt.to_string()),
        toml::Value::Array(a) => J::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => J::Object(
            t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect(),
        ),
    }
}

/// Reconcile the managed plugin pool against the manifest at `path`.
/// Returns a one-line summary, or Err (with nothing changed) when the
/// manifest itself is unusable.
pub fn sync_manifest(path: &str) -> Result<String, String> {
    let manifest_path = expand_path(path, Path::new("."));
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_str(&text)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let base = manifest_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    // Validate everything before touching anything.
    let mut entries: Vec<(String, PathBuf, &ManifestEntry)> = Vec::new();
    for (name, entry) in &manifest.plugins {
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(format!("bad plugin name {name:?}"));
        }
        let wasm = expand_path(&entry.path, &base);
        if let Err(e) = std::fs::metadata(&wasm) {
            return Err(format!("{name}: {}: {e}", wasm.display()));
        }
        for cap in &entry.caps {
            if crate::caps::cap_from_name(cap).is_none() {
                return Err(format!("{name}: unknown capability {cap:?}"));
            }
        }
        entries.push((name.clone(), wasm, entry));
    }

    // Apply: upsert each entry and stamp it managed.
    let (mut loaded, mut updated, mut unchanged, mut failed) = (0, 0, 0, 0);
    let mut report = String::new();
    for (name, wasm, entry) in &entries {
        let desc = LoadDescriptor {
            name: name.clone(),
            path: wasm.to_string_lossy().into_owned(),
            scope: entry.scope.unwrap_or(ScopeType::Server),
            config: entry
                .config
                .as_ref()
                .map_or(serde_json::Value::Null, toml_to_json),
            caps: entry.caps.clone(),
        };
        match reload::upsert(desc) {
            Ok(outcome) => {
                match outcome {
                    "loaded" => loaded += 1,
                    "unchanged" => unchanged += 1,
                    _ => updated += 1,
                }
                REGISTRY.with(|r| {
                    if let Some(def) =
                        r.borrow_mut().plugins.get_mut(name.as_str())
                    {
                        def.managed = true;
                    }
                });
                // Honor the enabled flag; sync is explicit user intent, so
                // it also re-enables a failure-disabled plugin.
                let running = REGISTRY.with(|r| r.borrow().is_running(name));
                if entry.enabled != running {
                    let _ = reload::set_enabled(name, entry.enabled);
                }
            }
            Err(e) => {
                failed += 1;
                let _ = writeln!(report, "{name}: {e}");
                hostlog::error(name, &format!("sync: {e}"));
            }
        }
    }

    // Sweep: managed plugins the manifest no longer names.
    let stale: Vec<String> = REGISTRY.with(|r| {
        r.borrow()
            .plugins
            .values()
            .filter(|d| d.managed && !manifest.plugins.contains_key(&d.name))
            .map(|d| d.name.clone())
            .collect()
    });
    let unloaded = stale.len();
    for name in stale {
        hostlog::info(&name, "removed from manifest, unloading");
        REGISTRY.with(|r| r.borrow_mut().unload(&name));
    }

    let _ = write!(
        report,
        "synced {}: {loaded} loaded, {updated} updated, {unchanged} \
         unchanged, {unloaded} unloaded",
        manifest_path.display()
    );
    if failed > 0 {
        let _ = write!(report, ", {failed} FAILED");
    }
    Ok(report)
}
