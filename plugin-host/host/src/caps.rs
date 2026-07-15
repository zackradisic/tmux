//! Capability model.
//!
//! Effective capabilities = manifest **requests** (TOML sidecar next to the
//! .wasm, optional) ∩ user **grants** (`load-plugin -c ...`), with a small
//! always-granted default set. The sidecar travels with the artifact and
//! documents what the plugin needs; the user's config is the sole granting
//! authority - a plugin can never self-grant. Without a sidecar, requests
//! default to the grant set (trust-the-user mode).
//!
//! Checks happen at exactly one choke point: the top of dispatch(), before
//! any vtable pointer is touched.

use serde::Deserialize;
use std::path::Path;

pub const READ_STATE: u32 = 1 << 0;
pub const WRITE_OPTIONS: u32 = 1 << 1;
pub const SEND_KEYS: u32 = 1 << 2;
pub const CAPTURE_PANE: u32 = 1 << 3;
pub const DISPLAY_MESSAGE: u32 = 1 << 4;
pub const TIMERS: u32 = 1 << 5;
pub const RUN_PROCESS: u32 = 1 << 6;
pub const RUN_COMMAND: u32 = 1 << 7;
pub const CROSS_SCOPE: u32 = 1 << 8;
pub const POPUP: u32 = 1 << 9;
pub const MENU: u32 = 1 << 10;
pub const FS_READ: u32 = 1 << 11;
pub const FS_WRITE: u32 = 1 << 12;

/// Granted to every plugin without being asked for.
pub const DEFAULT_CAPS: u32 = READ_STATE | DISPLAY_MESSAGE | TIMERS;

pub fn cap_from_name(name: &str) -> Option<u32> {
    Some(match name {
        "read-state" => READ_STATE,
        "write-options" => WRITE_OPTIONS,
        "send-keys" => SEND_KEYS,
        "capture-pane" => CAPTURE_PANE,
        "display-message" => DISPLAY_MESSAGE,
        "timers" => TIMERS,
        "run-process" => RUN_PROCESS,
        "run-command" => RUN_COMMAND,
        "cross-scope" => CROSS_SCOPE,
        "popup" => POPUP,
        "menu" => MENU,
        "fs-read" => FS_READ,
        "fs-write" => FS_WRITE,
        _ => return None,
    })
}

pub fn cap_name(flag: u32) -> &'static str {
    match flag {
        READ_STATE => "read-state",
        WRITE_OPTIONS => "write-options",
        SEND_KEYS => "send-keys",
        CAPTURE_PANE => "capture-pane",
        DISPLAY_MESSAGE => "display-message",
        TIMERS => "timers",
        RUN_PROCESS => "run-process",
        RUN_COMMAND => "run-command",
        CROSS_SCOPE => "cross-scope",
        POPUP => "popup",
        MENU => "menu",
        FS_READ => "fs-read",
        FS_WRITE => "fs-write",
        _ => "?",
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveCaps {
    pub flags: u32,
    /// argv[0] basenames allowed for run_job (empty = any, if RUN_PROCESS).
    /// Advisory in v1: run_job takes a shell string, so this checks the
    /// first token only.
    pub argv0_allow: Vec<String>,
}

impl EffectiveCaps {
    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    /// Human-readable list for show-plugins -v.
    pub fn describe(&self) -> String {
        let mut names = Vec::new();
        for bit in 0..13 {
            let flag = 1u32 << bit;
            if self.flags & flag != 0 {
                names.push(cap_name(flag));
            }
        }
        names.join(",")
    }
}

#[derive(Debug, Deserialize, Default)]
struct ManifestCapsRunProcess {
    #[serde(default)]
    argv0: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ManifestCaps {
    #[serde(default)]
    requests: Vec<String>,
    #[serde(rename = "run-process", default)]
    run_process: ManifestCapsRunProcess,
}

#[derive(Debug, Deserialize, Default)]
struct Manifest {
    #[serde(default)]
    caps: ManifestCaps,
}

fn parse_names(names: &[String], what: &str) -> Result<u32, String> {
    let mut flags = 0;
    for name in names {
        match cap_from_name(name) {
            Some(f) => flags |= f,
            None => return Err(format!("unknown {what} capability {name:?}")),
        }
    }
    Ok(flags)
}

/// Compute effective capabilities for a plugin at `wasm_path` given the
/// user's grants. Reads `<stem>.toml` next to the .wasm if present.
pub fn compute(wasm_path: &Path, grants: &[String]) -> Result<EffectiveCaps, String> {
    let granted = DEFAULT_CAPS | parse_names(grants, "granted")?;

    let sidecar = wasm_path.with_extension("toml");
    let manifest: Option<Manifest> = match std::fs::read_to_string(&sidecar) {
        Ok(text) => Some(
            toml::from_str(&text)
                .map_err(|e| format!("{}: {e}", sidecar.display()))?,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", sidecar.display())),
    };

    match manifest {
        None => Ok(EffectiveCaps { flags: granted, argv0_allow: Vec::new() }),
        Some(m) => {
            let requested =
                DEFAULT_CAPS | parse_names(&m.caps.requests, "requested")?;
            Ok(EffectiveCaps {
                flags: requested & granted,
                argv0_allow: m.caps.run_process.argv0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sidecar_trusts_grants() {
        let caps = compute(
            Path::new("/nonexistent/plugin.wasm"),
            &["send-keys".into()],
        )
        .unwrap();
        assert!(caps.has(SEND_KEYS));
        assert!(caps.has(READ_STATE)); // default
        assert!(!caps.has(RUN_PROCESS));
    }

    #[test]
    fn unknown_grant_rejected() {
        assert!(compute(Path::new("/x.wasm"), &["frobnicate".into()]).is_err());
    }
}
