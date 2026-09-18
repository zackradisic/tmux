//! The release registry: where a manifest's `[registry]` entries come
//! from.
//!
//! A tmux2 GitHub release carries every bundled plugin, its sidecar and
//! an `index.toml` (written by `plugin-index`) with the ABI, the tag and
//! each plugin's version, blake3 hash and size. This module resolves a
//! release tag (a pinned one, or the latest through the GitHub API),
//! fetches and checks the index, and reads and writes the lock file that
//! pins what a manifest resolved to. Tests point `base_url` and
//! `api_url` at `file://` paths, so nothing here needs the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tmux_plugin_abi::ABI_VERSION;

use crate::fetch::{self, Done};

pub const DEFAULT_REPO: &str = "zackradisic/tmux";
pub const LATEST: &str = "latest";

/// `[registry]` in a manifest.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistryCfg {
    #[serde(default = "default_repo")]
    pub repo: String,
    #[serde(default = "default_release")]
    pub release: String,
    /// Where the release files are; overrides `repo`. For mirrors and
    /// tests (`file:///...`). `{tag}` in it stands for the release tag.
    /// Needs a pinned `release`, or `api_url`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Where to ask for the latest tag (GitHub API shape); overrides
    /// `repo`. For tests.
    #[serde(default)]
    pub api_url: Option<String>,
}

fn default_repo() -> String {
    DEFAULT_REPO.to_string()
}

fn default_release() -> String {
    LATEST.to_string()
}

impl Default for RegistryCfg {
    fn default() -> Self {
        RegistryCfg {
            repo: default_repo(),
            release: default_release(),
            base_url: None,
            api_url: None,
        }
    }
}

impl RegistryCfg {
    pub fn pinned(&self) -> Option<&str> {
        (self.release != LATEST).then_some(self.release.as_str())
    }

    pub fn api_url(&self) -> String {
        self.api_url.clone().unwrap_or_else(|| {
            format!("https://api.github.com/repos/{}/releases/latest", self.repo)
        })
    }

    /// Directory url of the release files for `tag` (no trailing slash).
    pub fn base_url(&self, tag: &str) -> String {
        match &self.base_url {
            Some(b) => b.trim_end_matches('/').replace("{tag}", tag),
            None => format!("https://github.com/{}/releases/download/{tag}", self.repo),
        }
    }

    /// A short name for messages: the repo, or the mirror.
    pub fn describe(&self) -> String {
        match &self.base_url {
            Some(b) => b.replace("{tag}", "").trim_end_matches('/').to_string(),
            None => self.repo.clone(),
        }
    }
}

/// `index.toml` of a release.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Index {
    pub abi: i32,
    pub tag: String,
    #[serde(default)]
    pub plugins: BTreeMap<String, IndexEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub file: String,
    #[serde(default)]
    pub sidecar: Option<String>,
    pub version: String,
    pub blake3: String,
    #[serde(default)]
    pub size: u64,
}

impl Index {
    /// Parse and check: the ABI must be this host's.
    pub fn parse(text: &str) -> Result<Index, String> {
        let index: Index = toml::from_str(text).map_err(|e| format!("index.toml: {e}"))?;
        if index.abi != ABI_VERSION {
            return Err(format!(
                "release {} needs ABI {}, this tmux has {ABI_VERSION}; run tmux update first",
                index.tag, index.abi
            ));
        }
        for (name, e) in &index.plugins {
            crate::cas::normalize_hash(&e.blake3).map_err(|err| format!("index.toml: {name}: {err}"))?;
        }
        Ok(index)
    }

    /// Find a plugin by manifest name: the key as written, or with `-`
    /// turned into `_` (the crate name against the wasm file stem).
    pub fn lookup(&self, name: &str) -> Option<&IndexEntry> {
        self.plugins
            .get(name)
            .or_else(|| self.plugins.get(&name.replace('-', "_")))
    }
}

/// `<manifest stem>.lock` next to the manifest: what the registry
/// entries resolved to.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Lock {
    #[serde(default)]
    pub registry: LockRegistry,
    #[serde(default)]
    pub plugins: BTreeMap<String, LockEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LockRegistry {
    #[serde(default)]
    pub tag: String,
    /// When the tag was resolved (RFC 3339, UTC).
    #[serde(default)]
    pub resolved: String,
    /// When the last update check ran (RFC 3339, UTC).
    #[serde(default)]
    pub checked: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct LockEntry {
    pub version: String,
    pub blake3: String,
    pub url: String,
    #[serde(default)]
    pub sidecar: Option<String>,
}

impl Lock {
    pub fn path_for(manifest: &Path) -> PathBuf {
        manifest.with_extension("lock")
    }

    /// None when there is no lock file yet.
    pub fn read(path: &Path) -> Result<Option<Lock>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let lock: Lock =
                    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
                Ok(Some(lock))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| format!("lock: {e}"))?;
        let header = "# Written by tmux sync-plugins / update-plugins. Commit it with\n\
                      # the manifest: two machines with one lock run the same bytes.\n";
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, format!("{header}{text}"))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Build the lock entries for `names` from an index. Names the index
    /// does not list come back in the error list.
    pub fn from_index(
        cfg: &RegistryCfg,
        index: &Index,
        names: impl IntoIterator<Item = String>,
    ) -> (Lock, Vec<String>) {
        let base = cfg.base_url(&index.tag);
        let mut lock = Lock {
            registry: LockRegistry {
                tag: index.tag.clone(),
                resolved: now_rfc3339(),
                checked: now_rfc3339(),
            },
            plugins: BTreeMap::new(),
        };
        let mut missing = Vec::new();
        for name in names {
            match index.lookup(&name) {
                Some(e) => {
                    lock.plugins.insert(
                        name,
                        LockEntry {
                            version: e.version.clone(),
                            blake3: crate::cas::normalize_hash(&e.blake3).unwrap_or_default(),
                            url: format!("{base}/{}", e.file),
                            sidecar: e.sidecar.as_ref().map(|s| format!("{base}/{s}")),
                        },
                    );
                }
                None => missing.push(name),
            }
        }
        (lock, missing)
    }

    /// Seconds since the last update check (or resolution); None when
    /// the lock never recorded one.
    pub fn checked_age_secs(&self) -> Option<u64> {
        let stamp = if self.registry.checked.is_empty() {
            &self.registry.resolved
        } else {
            &self.registry.checked
        };
        let then = parse_rfc3339(stamp)?;
        let now = now_unix();
        Some(now.saturating_sub(then))
    }
}

/// Resolve the tag of `cfg`: the pinned release at once, or the latest
/// through the API.
pub fn resolve_tag(cfg: &RegistryCfg, done: Done<String>) {
    if let Some(tag) = cfg.pinned() {
        done(Ok(tag.to_string()));
        return;
    }
    if cfg.base_url.is_some() && cfg.api_url.is_none() {
        done(Err("registry: base_url needs a pinned release or an api_url".into()));
        return;
    }
    fetch::download_text(
        &cfg.api_url(),
        Box::new(move |res| {
            done(res.and_then(|text| {
                let v: serde_json::Value =
                    serde_json::from_str(&text).map_err(|e| format!("release api: {e}"))?;
                v.get("tag_name")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
                    .ok_or_else(|| "release api: no tag_name in the reply".to_string())
            }))
        }),
    );
}

/// Fetch and check `index.toml` of release `tag`.
pub fn fetch_index(cfg: &RegistryCfg, tag: &str, done: Done<Index>) {
    let url = format!("{}/index.toml", cfg.base_url(tag));
    let tag = tag.to_string();
    fetch::download_text(
        &url,
        Box::new(move |res| {
            done(res.and_then(|text| {
                let index = Index::parse(&text)?;
                if index.tag != tag {
                    return Err(format!("index.toml says tag {}, expected {tag}", index.tag));
                }
                Ok(index)
            }))
        }),
    );
}

/// Resolve the tag and fetch its index in one go.
pub fn resolve_index(cfg: &RegistryCfg, done: Done<Index>) {
    let cfg2 = cfg.clone();
    resolve_tag(
        cfg,
        Box::new(move |res| match res {
            Ok(tag) => fetch_index(&cfg2, &tag, done),
            Err(e) => done(Err(e)),
        }),
    );
}

// ---------------------------------------------------------------------------
// Time stamps (RFC 3339, UTC, whole seconds) without a date crate.
// ---------------------------------------------------------------------------

pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_rfc3339() -> String {
    unix_to_rfc3339(now_unix())
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's
/// civil_from_days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn unix_to_rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.trim();
    let b = text.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':'
        || b[16] != b':' || b[19] != b'Z'
    {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let y = num(&text[0..4])?;
    let m = num(&text[5..7])? as u32;
    let d = num(&text[8..10])? as u32;
    let hh = num(&text[11..13])?;
    let mm = num(&text[14..16])?;
    let ss = num(&text[17..19])?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let total = days * 86_400 + hh * 3600 + mm * 60 + ss;
    u64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trip() {
        assert_eq!(unix_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_rfc3339(1_789_689_600), "2026-09-18T00:00:00Z");
        assert_eq!(unix_to_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for secs in [0u64, 951_782_400, 1_789_689_600, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&unix_to_rfc3339(secs)), Some(secs));
        }
        assert_eq!(parse_rfc3339("2026-09-18"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn index_parse_and_abi() {
        let good = format!(
            "abi = {ABI_VERSION}\ntag = \"v1\"\n[plugins.notify_toast]\nfile = \"notify_toast.wasm\"\n\
             sidecar = \"notify_toast.toml\"\nversion = \"0.1.0\"\nblake3 = \"{}\"\nsize = 5\n",
            "ab".repeat(32)
        );
        let index = Index::parse(&good).unwrap();
        assert!(index.lookup("notify-toast").is_some());
        assert!(index.lookup("notify_toast").is_some());
        assert!(index.lookup("agents").is_none());
        let bad = good.replace(&format!("abi = {ABI_VERSION}"), "abi = 999");
        let err = Index::parse(&bad).unwrap_err();
        assert!(err.contains("needs ABI 999"), "{err}");
        let badhash = good.replace(&"ab".repeat(32), "zz");
        assert!(Index::parse(&badhash).is_err());
    }

    #[test]
    fn lock_from_index_and_round_trip() {
        let cfg = RegistryCfg { base_url: Some("file:///rel/".into()), ..Default::default() };
        let index = Index {
            abi: ABI_VERSION,
            tag: "v1".into(),
            plugins: [(
                "agents".to_string(),
                IndexEntry {
                    file: "agents.wasm".into(),
                    sidecar: Some("agents.toml".into()),
                    version: "0.2.0".into(),
                    blake3: "AB".repeat(32),
                    size: 1,
                },
            )]
            .into_iter()
            .collect(),
        };
        let (lock, missing) =
            Lock::from_index(&cfg, &index, ["agents".to_string(), "cron".to_string()]);
        assert_eq!(missing, vec!["cron".to_string()]);
        let e = &lock.plugins["agents"];
        assert_eq!(e.url, "file:///rel/agents.wasm");
        assert_eq!(e.sidecar.as_deref(), Some("file:///rel/agents.toml"));
        assert_eq!(e.blake3, "ab".repeat(32));
        assert_eq!(lock.registry.tag, "v1");
        assert!(lock.checked_age_secs().unwrap() < 5);

        let dir = std::env::temp_dir().join(format!("pgh-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = Lock::path_for(&dir.join("plugins.toml"));
        assert!(path.ends_with("plugins.lock"));
        lock.write(&path).unwrap();
        let back = Lock::read(&path).unwrap().unwrap();
        assert_eq!(back, lock);
        assert!(Lock::read(&dir.join("none.lock")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_urls() {
        let cfg = RegistryCfg::default();
        assert_eq!(cfg.pinned(), None);
        assert_eq!(cfg.api_url(), "https://api.github.com/repos/zackradisic/tmux/releases/latest");
        assert_eq!(
            cfg.base_url("v3"),
            "https://github.com/zackradisic/tmux/releases/download/v3"
        );
        let pinned = RegistryCfg { release: "v3".into(), ..Default::default() };
        assert_eq!(pinned.pinned(), Some("v3"));
        let mirror = RegistryCfg { base_url: Some("file:///m/{tag}/".into()), ..Default::default() };
        assert_eq!(mirror.base_url("v3"), "file:///m/v3");
    }
}
