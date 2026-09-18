//! Manifest sync: declarative plugin loading.
//!
//! A TOML manifest is the desired state for the *managed* plugin pool;
//! `sync_manifest` reconciles the world against it: load new entries,
//! upsert changed ones (reusing the load-plugin reconcile rules), and
//! unload managed plugins the manifest no longer names. Interactive
//! `load-plugin` definitions are unmanaged and never swept; a manifest
//! entry with the same name adopts them.
//!
//! An entry names where its module comes from:
//!
//! - `path`: a local file (relative to the manifest).
//! - `url` + `hash`: one file anywhere `curl` reaches, pinned by blake3.
//! - neither: the release registry (`[registry]`, default the tmux2
//!   GitHub releases). What such entries resolved to lives in the lock
//!   file next to the manifest (`plugins.lock` for `plugins.toml`);
//!   `update-plugins` moves the lock, a sync only follows it.
//!
//! Fetched modules land in the content-addressed cache (cas.rs). A sync
//! applies what is on disk at once and starts downloads for the rest;
//! those entries load when their download completes, at drain time.
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

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tmux_plugin_abi::{LoadDescriptor, Role, ScopeType};

use crate::fetch::{self, Done};
use crate::hostlog;
use crate::release::{self, Lock, RegistryCfg};
use crate::reload;
use crate::state::REGISTRY;

/// Server option: run the daily update check after a sync.
pub const UPDATE_CHECK_OPTION: &str = "plugin-update-check";
/// How old a lock's last check may be before a sync checks again.
const CHECK_EVERY_SECS: u64 = 24 * 3600;

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    hash: Option<String>,
    /// Sidecar url for a `url` entry.
    #[serde(default)]
    sidecar: Option<String>,
    #[serde(default)]
    scope: Option<ScopeType>,
    #[serde(default)]
    role: Option<Role>,
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
    registry: Option<RegistryCfg>,
    #[serde(default)]
    plugins: BTreeMap<String, ManifestEntry>,
}

/// Where one entry's module comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    Path(PathBuf),
    Url { url: String, hash: String, sidecar: Option<String> },
    Registry,
}

#[derive(Clone, Debug)]
struct Planned {
    name: String,
    entry: ManifestEntry,
    source: Source,
}

/// A parsed, validated manifest with its lock: everything a sync or an
/// update needs, computed before anything changes.
#[derive(Debug)]
struct Plan {
    manifest_path: PathBuf,
    lock_path: PathBuf,
    registry: RegistryCfg,
    entries: Vec<Planned>,
    lock: Option<Lock>,
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

impl Plan {
    fn load(path: &str) -> Result<Plan, String> {
        let manifest_path = expand_path(path, Path::new("."));
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
        let base = manifest_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

        let mut entries = Vec::new();
        for (name, entry) in &manifest.plugins {
            if name.is_empty()
                || name.contains(char::is_whitespace)
                || name.contains('/')
                || name.starts_with('.')
            {
                return Err(format!("bad plugin name {name:?}"));
            }
            let source = match (&entry.path, &entry.url) {
                (Some(_), Some(_)) => {
                    return Err(format!("{name}: give path or url, not both"));
                }
                (Some(p), None) => {
                    if entry.hash.is_some() || entry.sidecar.is_some() {
                        return Err(format!("{name}: hash and sidecar go with url, not path"));
                    }
                    let wasm = expand_path(p, &base);
                    if let Err(e) = std::fs::metadata(&wasm) {
                        return Err(format!("{name}: {}: {e}", wasm.display()));
                    }
                    Source::Path(wasm)
                }
                (None, Some(url)) => {
                    let Some(hash) = &entry.hash else {
                        return Err(format!("{name}: url needs hash = \"blake3:<hex>\""));
                    };
                    let hash = crate::cas::normalize_hash(hash)
                        .map_err(|e| format!("{name}: {e}"))?;
                    Source::Url { url: url.clone(), hash, sidecar: entry.sidecar.clone() }
                }
                (None, None) => {
                    if entry.hash.is_some() || entry.sidecar.is_some() {
                        return Err(format!("{name}: hash and sidecar go with url"));
                    }
                    Source::Registry
                }
            };
            for cap in &entry.caps {
                if crate::caps::cap_from_name(cap).is_none() {
                    return Err(format!("{name}: unknown capability {cap:?}"));
                }
            }
            entries.push(Planned { name: name.clone(), entry: entry.clone(), source });
        }

        let lock_path = Lock::path_for(&manifest_path);
        let lock = Lock::read(&lock_path)?;
        Ok(Plan {
            manifest_path,
            lock_path,
            registry: manifest.registry.unwrap_or_default(),
            entries,
            lock,
        })
    }

    fn registry_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|p| p.source == Source::Registry)
            .map(|p| p.name.clone())
            .collect()
    }

    /// The lock entry a registry entry follows, when the lock has one
    /// for the release the manifest asks for.
    fn locked(&self, name: &str) -> Option<&release::LockEntry> {
        let lock = self.lock.as_ref()?;
        if let Some(tag) = self.registry.pinned() {
            if lock.registry.tag != tag {
                return None;
            }
        }
        lock.plugins.get(name)
    }

    /// Registry entries the lock does not cover for the wanted release.
    fn unresolved(&self) -> Vec<String> {
        self.registry_names()
            .into_iter()
            .filter(|n| self.locked(n).is_none())
            .collect()
    }

    fn names(&self) -> HashSet<String> {
        self.entries.iter().map(|p| p.name.clone()).collect()
    }
}

/// Counters for one sync report.
#[derive(Default)]
struct Tally {
    loaded: usize,
    updated: usize,
    unchanged: usize,
    fetching: usize,
    failed: usize,
    report: String,
}

impl Tally {
    fn record(&mut self, name: &str, outcome: Result<&'static str, String>) {
        match outcome {
            Ok("loaded") => self.loaded += 1,
            Ok("unchanged") => self.unchanged += 1,
            Ok(_) => self.updated += 1,
            Err(e) => {
                self.failed += 1;
                let _ = writeln!(self.report, "{name}: {e}");
                hostlog::error(name, &format!("sync: {e}"));
            }
        }
    }
}

/// Upsert one entry from the module at `module` and stamp it managed.
/// `source` is the url (and sidecar url) a peer could fetch the same
/// module from; None for a local file.
fn apply_entry(
    planned: &Planned,
    module: &Path,
    manifest: &Path,
    source: Option<(&str, Option<&str>)>,
) -> Result<&'static str, String> {
    let entry = &planned.entry;
    let desc = LoadDescriptor {
        name: planned.name.clone(),
        path: module.to_string_lossy().into_owned(),
        scope: entry.scope.unwrap_or(ScopeType::Server),
        config: entry
            .config
            .as_ref()
            .map_or(serde_json::Value::Null, toml_to_json),
        caps: entry.caps.clone(),
        role: entry.role.unwrap_or_default(),
    };
    let outcome = reload::upsert(desc)?;
    let name = planned.name.as_str();
    REGISTRY.with(|r| {
        if let Some(def) = r.borrow_mut().plugins.get_mut(name) {
            def.managed = true;
            def.manifest = Some(manifest.to_path_buf());
            def.source_url = source.map(|(u, _)| u.to_string());
            def.sidecar_url = source.and_then(|(_, s)| s.map(str::to_string));
        }
    });
    // Honor the enabled flag; sync is explicit user intent, so it also
    // re-enables a failure-disabled plugin.
    let running = REGISTRY.with(|r| r.borrow().is_running(name));
    if entry.enabled != running {
        let _ = reload::set_enabled(name, entry.enabled);
    }
    Ok(outcome)
}

/// Load `planned` from the cache when its module is there, or start the
/// download and load it on completion. Returns "fetching" when a
/// download started.
fn apply_remote(
    planned: &Planned,
    manifest: &Path,
    url: &str,
    hash: &str,
    sidecar: Option<&str>,
    what: &str,
) -> Result<&'static str, String> {
    if crate::cas::has(hash) {
        return apply_entry(planned, &crate::cas::path_for(hash)?, manifest, Some((url, sidecar)));
    }
    let name = planned.name.clone();
    hostlog::info(&name, &format!("fetching {what} from {url}"));
    let owned = planned.clone();
    let manifest = manifest.to_path_buf();
    let source_url = url.to_string();
    let source_sidecar = sidecar.map(str::to_string);
    fetch::fetch_module(
        url,
        hash.to_string(),
        sidecar.map(str::to_string),
        Box::new(move |res| {
            let applied = res.and_then(|module| {
                apply_entry(
                    &owned,
                    &module,
                    &manifest,
                    Some((&source_url, source_sidecar.as_deref())),
                )
            });
            match applied {
                Ok(outcome) => hostlog::info(&name, &format!("fetched: {outcome}")),
                Err(e) => hostlog::error(&name, &format!("fetch failed: {e}")),
            }
        }),
    );
    Ok("fetching")
}

/// Apply every entry the plan can place now: local files, urls and
/// registry entries the lock covers. Then sweep and collect garbage.
fn apply(plan: &Plan) -> Tally {
    let mut tally = Tally::default();
    for planned in &plan.entries {
        let outcome = match &planned.source {
            Source::Path(wasm) => apply_entry(planned, wasm, &plan.manifest_path, None),
            Source::Url { url, hash, sidecar } => {
                apply_remote(planned, &plan.manifest_path, url, hash, sidecar.as_deref(), "module")
            }
            Source::Registry => match plan.locked(&planned.name) {
                Some(le) => apply_remote(
                    planned,
                    &plan.manifest_path,
                    &le.url,
                    &le.blake3,
                    le.sidecar.as_deref(),
                    &format!("release {}", plan.lock.as_ref().map_or("", |l| &l.registry.tag)),
                ),
                None => continue, // waits for the resolution
            },
        };
        if outcome == Ok("fetching") {
            tally.fetching += 1;
        } else {
            tally.record(&planned.name, outcome);
        }
    }

    // Sweep: managed plugins the manifest no longer names.
    let names = plan.names();
    let stale: Vec<String> = REGISTRY.with(|r| {
        r.borrow()
            .plugins
            .values()
            .filter(|d| d.managed && !names.contains(&d.name))
            .map(|d| d.name.clone())
            .collect()
    });
    let unloaded = stale.len();
    for name in stale {
        hostlog::info(&name, "removed from manifest, unloading");
        REGISTRY.with(|r| r.borrow_mut().unload(&name));
    }

    gc(plan);

    let _ = write!(
        tally.report,
        "synced {}: {} loaded, {} updated, {} unchanged, {unloaded} unloaded",
        plan.manifest_path.display(),
        tally.loaded,
        tally.updated,
        tally.unchanged
    );
    if tally.fetching > 0 {
        let _ = write!(tally.report, ", {} fetching", tally.fetching);
    }
    if tally.failed > 0 {
        let _ = write!(tally.report, ", {} FAILED", tally.failed);
    }
    tally
}

/// Remove cached modules nothing references: not a loaded plugin, not
/// the lock, not a url entry of this manifest.
fn gc(plan: &Plan) {
    let mut keep: HashSet<String> = REGISTRY.with(|r| {
        r.borrow().plugins.values().map(|d| d.hash.to_hex().to_string()).collect()
    });
    if let Some(lock) = &plan.lock {
        keep.extend(lock.plugins.values().map(|e| e.blake3.clone()));
    }
    for p in &plan.entries {
        if let Source::Url { hash, .. } = &p.source {
            keep.insert(hash.clone());
        }
    }
    let removed = crate::cas::gc(&keep);
    if removed > 0 {
        hostlog::info("host", &format!("plugin cache: removed {removed} unreferenced module(s)"));
    }
}

/// Resolve the registry for `plan`, write the lock and apply the
/// registry entries. Errors and the outcome go to the plugin log.
fn resolve_and_apply(plan: Plan) {
    let cfg = plan.registry.clone();
    let manifest = plan.manifest_path.to_string_lossy().into_owned();
    release::resolve_index(
        &cfg.clone(),
        Box::new(move |res| {
            let index = match res {
                Ok(i) => i,
                Err(e) => {
                    hostlog::error("host", &format!("{manifest}: registry: {e}"));
                    return;
                }
            };
            let (lock, missing) = Lock::from_index(&cfg, &index, plan.registry_names());
            for name in &missing {
                hostlog::error(name, &format!("not in release {} of {}", index.tag, cfg.describe()));
            }
            if let Err(e) = lock.write(&plan.lock_path) {
                hostlog::error("host", &format!("{manifest}: {e}"));
                return;
            }
            hostlog::info(
                "host",
                &format!("{manifest}: resolved release {} of {}", index.tag, cfg.describe()),
            );
            match Plan::load(&manifest) {
                Ok(fresh) => {
                    let tally = apply(&fresh);
                    hostlog::info("host", tally.report.trim_end());
                }
                Err(e) => hostlog::error("host", &format!("{manifest}: {e}")),
            }
        }),
    );
}

/// Reconcile the managed plugin pool against the manifest at `path`.
/// Returns a one-line summary, or Err (with nothing changed) when the
/// manifest itself is unusable. Entries the lock does not cover yet
/// load after the registry resolves; a download or resolution failure
/// goes to the plugin log.
pub fn sync_manifest(path: &str) -> Result<String, String> {
    let plan = Plan::load(path)?;
    let mut tally = apply(&plan);
    let unresolved = plan.unresolved();
    if !unresolved.is_empty() {
        let _ = write!(
            tally.report,
            "; resolving release {} of {} for {}",
            plan.registry.release,
            plan.registry.describe(),
            unresolved.join(", ")
        );
        resolve_and_apply(plan);
    } else {
        maybe_check_updates(plan);
    }
    Ok(tally.report)
}

fn update_check_enabled() -> bool {
    match crate::dispatch::server_option(UPDATE_CHECK_OPTION) {
        Some(v) => v.trim() == "on" || v.trim() == "1",
        None => true,
    }
}

/// The daily check: a manifest on the latest release whose lock was last
/// checked more than a day ago asks the registry again and tells the
/// user when updates exist. It changes the lock's `checked` stamp only.
fn maybe_check_updates(plan: Plan) {
    if plan.registry.pinned().is_some() || plan.registry_names().is_empty() {
        return;
    }
    let Some(lock) = plan.lock.clone() else { return };
    if lock.checked_age_secs().is_some_and(|age| age < CHECK_EVERY_SECS) {
        return;
    }
    if !update_check_enabled() {
        return;
    }
    let cfg = plan.registry.clone();
    let manifest = plan.manifest_path.to_string_lossy().into_owned();
    hostlog::debug("host", &format!("{manifest}: checking {} for plugin updates", cfg.describe()));
    release::resolve_index(
        &cfg.clone(),
        Box::new(move |res| {
            let index = match res {
                Ok(i) => i,
                Err(e) => {
                    hostlog::warn("host", &format!("{manifest}: update check: {e}"));
                    return;
                }
            };
            let (fresh, _) = Lock::from_index(&cfg, &index, plan.registry_names());
            let changes = diff_lock(&lock, &fresh);
            let mut stamped = lock.clone();
            stamped.registry.checked = release::now_rfc3339();
            let _ = stamped.write(&plan.lock_path);
            if changes.changed == 0 {
                return;
            }
            let msg = format!(
                "{} plugin update{} in release {} of {}; run update-plugins {manifest}",
                changes.changed,
                if changes.changed == 1 { "" } else { "s" },
                index.tag,
                cfg.describe()
            );
            hostlog::warn("host", &msg);
            crate::dispatch::host_message(&msg);
        }),
    );
}

struct Changes {
    changed: usize,
    lines: Vec<String>,
}

/// One line per plugin whose bytes would change from `old` to `new`.
fn diff_lock(old: &Lock, new: &Lock) -> Changes {
    let mut lines = Vec::new();
    let mut changed = 0;
    for (name, ne) in &new.plugins {
        match old.plugins.get(name) {
            None => {
                changed += 1;
                lines.push(format!("{name}: new, version {}", ne.version));
            }
            Some(oe) if oe.blake3 != ne.blake3 => {
                changed += 1;
                if oe.version == ne.version {
                    lines.push(format!("{name}: {} (same version, new build)", ne.version));
                } else {
                    lines.push(format!("{name}: {} -> {}", oe.version, ne.version));
                }
            }
            Some(_) => lines.push(format!("{name}: {} unchanged", ne.version)),
        }
    }
    Changes { changed, lines }
}

/// `update-plugins [-n] manifest`: resolve the registry again, report
/// what changes against the lock, and unless `check_only` write the new
/// lock and sync. `done` gets the report (or the error) once, later.
pub fn update_plugins(path: &str, check_only: bool, done: Done<String>) -> Result<(), String> {
    let plan = Plan::load(path)?;
    if plan.registry_names().is_empty() {
        return Err(format!(
            "{}: no registry plugins (every entry has a path or url)",
            plan.manifest_path.display()
        ));
    }
    let cfg = plan.registry.clone();
    release::resolve_index(
        &cfg,
        Box::new(move |res| match res {
            Ok(index) => done(finish_update(plan, &index, check_only)),
            Err(e) => done(Err(e)),
        }),
    );
    Ok(())
}

/// The second half of an update, once the index is in: the report, and
/// unless `check_only` the new lock and a sync.
fn finish_update(plan: Plan, index: &release::Index, check_only: bool) -> Result<String, String> {
    let cfg = &plan.registry;
    let (mut fresh, missing) = Lock::from_index(cfg, index, plan.registry_names());
    let old = plan.lock.clone().unwrap_or_default();
    let changes = diff_lock(&old, &fresh);
    let mut text = String::new();
    if old.registry.tag.is_empty() {
        let _ = writeln!(text, "release {} of {}", index.tag, cfg.describe());
    } else if old.registry.tag != index.tag {
        let _ = writeln!(
            text,
            "release {} of {} (lock had {})",
            index.tag,
            cfg.describe(),
            old.registry.tag
        );
    } else {
        let _ = writeln!(text, "release {} of {} (as locked)", index.tag, cfg.describe());
    }
    for line in &changes.lines {
        let _ = writeln!(text, "{line}");
    }
    for name in &missing {
        let _ = writeln!(text, "{name}: not in this release");
    }
    if changes.changed == 0 && missing.is_empty() {
        let _ = writeln!(text, "up to date");
    }
    if check_only {
        if let Some(mut lock) = plan.lock.clone() {
            lock.registry.checked = release::now_rfc3339();
            let _ = lock.write(&plan.lock_path);
        }
        return Ok(text.trim_end().to_string());
    }
    // Keep lock rows for plugins the new release lacks: their bytes are
    // still what the manifest last resolved to.
    for name in &missing {
        if let Some(oe) = old.plugins.get(name) {
            fresh.plugins.insert(name.clone(), oe.clone());
        }
    }
    fresh.write(&plan.lock_path)?;
    let manifest = plan.manifest_path.to_string_lossy().into_owned();
    let plan = Plan::load(&manifest)?;
    let tally = apply(&plan);
    let _ = write!(text, "{}", tally.report);
    Ok(text.trim_end().to_string())
}

/// A linked server runs a newer build of `plugin` than the own copy this
/// manifest manages. Ask the manifest's registry: when its current
/// release carries exactly the peer's bytes (`hash`), move the lock and
/// sync, so the two copies converge through this side's own registry,
/// never through the peer's bytes. Otherwise say why not. The outcome
/// goes to the plugin log.
pub fn adopt_from_peer(manifest: &Path, plugin: &str, hash: &str, peer: &str, theirs: &str) {
    let path = manifest.to_string_lossy().into_owned();
    let plan = match Plan::load(&path) {
        Ok(p) => p,
        Err(e) => {
            hostlog::warn(plugin, &format!("{path}: {e}"));
            return;
        }
    };
    if plan.registry.pinned().is_some() {
        hostlog::info(
            plugin,
            &format!(
                "{peer} runs {theirs}; {path} pins release {}, not updating",
                plan.registry.release
            ),
        );
        return;
    }
    if !plan.registry_names().iter().any(|n| n == plugin) {
        return;
    }
    let cfg = plan.registry.clone();
    let plugin = plugin.to_string();
    let hash = hash.to_string();
    let peer = peer.to_string();
    let theirs = theirs.to_string();
    release::resolve_index(
        &cfg.clone(),
        Box::new(move |res| {
            let index = match res {
                Ok(i) => i,
                Err(e) => {
                    hostlog::warn(&plugin, &format!("{peer} runs {theirs}; registry: {e}"));
                    return;
                }
            };
            let listed = index
                .lookup(&plugin)
                .map(|e| crate::cas::normalize_hash(&e.blake3).unwrap_or_default());
            if listed.as_deref() != Some(hash.as_str()) {
                hostlog::warn(
                    &plugin,
                    &format!(
                        "{peer} runs {theirs}, which release {} of {} does not carry; \
                         a dev build there, or update this side by hand",
                        index.tag,
                        cfg.describe()
                    ),
                );
                return;
            }
            hostlog::info(
                &plugin,
                &format!("{peer} runs {theirs} from release {}; updating {}", index.tag, path),
            );
            match finish_update(plan, &index, false) {
                Ok(report) => {
                    for line in report.lines() {
                        hostlog::info("host", line);
                    }
                }
                Err(e) => hostlog::error(&plugin, &format!("update from release {}: {e}", index.tag)),
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path, text: &str) -> String {
        let p = dir.join("plugins.toml");
        std::fs::write(&p, text).unwrap();
        p.to_string_lossy().into_owned()
    }

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pgh-manifest-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sources_parse() {
        let dir = tmp();
        std::fs::write(dir.join("a.wasm"), b"x").unwrap();
        let hash = "cd".repeat(32);
        let m = write_manifest(
            &dir,
            &format!(
                "[registry]\nrelease = \"v1\"\n\
                 [plugins.a]\npath = \"a.wasm\"\n\
                 [plugins.b]\nurl = \"https://x/b.wasm\"\nhash = \"blake3:{hash}\"\nsidecar = \"https://x/b.toml\"\n\
                 [plugins.c]\nscope = \"pane\"\n"
            ),
        );
        let plan = Plan::load(&m).unwrap();
        assert_eq!(plan.entries.len(), 3);
        assert_eq!(plan.entries[0].source, Source::Path(dir.join("a.wasm")));
        assert_eq!(
            plan.entries[1].source,
            Source::Url {
                url: "https://x/b.wasm".into(),
                hash: hash.clone(),
                sidecar: Some("https://x/b.toml".into())
            }
        );
        assert_eq!(plan.entries[2].source, Source::Registry);
        assert_eq!(plan.registry.pinned(), Some("v1"));
        assert_eq!(plan.unresolved(), vec!["c".to_string()]);
        assert!(plan.lock.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sources_reject() {
        let dir = tmp();
        std::fs::write(dir.join("a.wasm"), b"x").unwrap();
        for (text, needle) in [
            ("[plugins.a]\npath = \"a.wasm\"\nurl = \"https://x\"\n", "not both"),
            ("[plugins.a]\nurl = \"https://x\"\n", "needs hash"),
            ("[plugins.a]\nurl = \"https://x\"\nhash = \"nope\"\n", "bad blake3"),
            ("[plugins.a]\npath = \"a.wasm\"\nhash = \"x\"\n", "go with url"),
            ("[plugins.a]\nhash = \"x\"\n", "go with url"),
            ("[plugins.a]\npath = \"missing.wasm\"\n", "missing.wasm"),
            ("[plugins.a]\npath = \"a.wasm\"\ncaps = [\"fly\"]\n", "unknown capability"),
            ("[plugins.\"a/b\"]\npath = \"a.wasm\"\n", "bad plugin name"),
            ("[registry]\nbogus = 1\n[plugins.a]\npath = \"a.wasm\"\n", "bogus"),
        ] {
            let m = write_manifest(&dir, text);
            let err = Plan::load(&m).unwrap_err();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_follows_pinned_tag() {
        let dir = tmp();
        let m = write_manifest(&dir, "[registry]\nrelease = \"v2\"\n[plugins.c]\n");
        let lock = Lock {
            registry: release::LockRegistry { tag: "v1".into(), ..Default::default() },
            plugins: [(
                "c".to_string(),
                release::LockEntry {
                    version: "0.1.0".into(),
                    blake3: "ab".repeat(32),
                    url: "file:///c.wasm".into(),
                    sidecar: None,
                },
            )]
            .into_iter()
            .collect(),
        };
        lock.write(&Lock::path_for(&dir.join("plugins.toml"))).unwrap();
        let plan = Plan::load(&m).unwrap();
        // Lock is for v1, manifest pins v2: unresolved.
        assert_eq!(plan.unresolved(), vec!["c".to_string()]);
        let m = write_manifest(&dir, "[registry]\nrelease = \"v1\"\n[plugins.c]\n");
        let plan = Plan::load(&m).unwrap();
        assert!(plan.unresolved().is_empty());
        assert_eq!(plan.locked("c").unwrap().url, "file:///c.wasm");
        // Latest follows whatever the lock says.
        let m = write_manifest(&dir, "[plugins.c]\n");
        assert!(Plan::load(&m).unwrap().unresolved().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diff_lines() {
        let entry = |v: &str, h: &str| release::LockEntry {
            version: v.into(),
            blake3: h.repeat(32),
            url: String::new(),
            sidecar: None,
        };
        let old = Lock {
            plugins: [("a".to_string(), entry("0.1.0", "aa")), ("b".to_string(), entry("0.1.0", "bb"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let new = Lock {
            plugins: [
                ("a".to_string(), entry("0.2.0", "cc")),
                ("b".to_string(), entry("0.1.0", "bb")),
                ("c".to_string(), entry("0.1.0", "dd")),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let d = diff_lock(&old, &new);
        assert_eq!(d.changed, 2);
        assert_eq!(
            d.lines,
            vec!["a: 0.1.0 -> 0.2.0", "b: 0.1.0 unchanged", "c: new, version 0.1.0"]
        );
        let same_ver = Lock {
            plugins: [("a".to_string(), entry("0.1.0", "ee"))].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(diff_lock(&old, &same_ver).lines[0], "a: 0.1.0 (same version, new build)");
    }
}
