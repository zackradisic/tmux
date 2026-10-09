//! The Claude Code side: where its login lives on this machine, how to
//! read it, and how to put a different one there without racing a
//! running claude.
//!
//! The login is two things. The credential (`{"claudeAiOauth": {...}}`,
//! plus machine-wide extras such as `mcpOAuth`) sits in the macOS
//! Keychain under the service `Claude Code-credentials` (account = the
//! user name), or in `~/.claude/.credentials.json` elsewhere (and on a
//! Mac that has one, as a fallback claude also reads). The identity
//! (`oauthAccount`: email, organization, account uuid) sits in
//! `~/.claude.json`, which claude rewrites on login and refetches from
//! the API at startup.
//!
//! Claude guards its own token refresh with proper-lockfile directories:
//! `~/.claude/.oauth_refresh.lock` then `~/.claude.lock` (stale after
//! 60 s, touched every 5 s by a live holder), and `~/.claude.json` with
//! `~/.claude.json.lock` (stale after 10 s). A swap that lands inside a
//! refresh would be overwritten by the refreshed old token, so every
//! write here holds the same directories the same way: mkdir is the
//! mutex, a directory older than its stale time is somebody's leftover
//! and is removed. The lock is taken in one shell job, the file is read
//! and the new one staged from Rust, and a second job installs and
//! unlocks; the staging files live in this plugin's data directory, made
//! 0700 at init, and the installs go through `cp` under `umask 077` so
//! nothing is ever world-readable on the way in. If the plugin dies
//! between the two jobs the directory goes stale and claude steals it,
//! which is the protocol working as designed.
//!
//! The shell is the session's default shell with `-c`; the snippets are
//! POSIX, and `stat -f %m` (BSD) falls back to `stat -c %Y` (GNU).

use std::cell::RefCell;
use std::rc::Rc;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tmux_plugin_sdk::prelude::*;

use crate::timefmt;

pub const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Test knobs from the plugin config: `home` stands in for the server
/// user's home, `credentials = "file"` keeps a Mac off the Keychain, and
/// `keychain_service` names a throwaway item. None of them matter in
/// use; they let a switch be rehearsed against a fake login.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub home: Option<String>,
    pub file_only: bool,
    pub keychain_service: Option<String>,
}

thread_local! {
    static OVERRIDES: RefCell<Overrides> = RefCell::new(Overrides::default());
}

pub fn set_overrides(o: Overrides) {
    OVERRIDES.with(|s| *s.borrow_mut() = o);
    PLATFORM.with(|p| *p.borrow_mut() = None);
}

/// Credential keys that belong to the machine, not the account: a switch
/// keeps the live copy of each (its absence included), the way cswap
/// does, so an MCP login survives and a stale copy in the slot never
/// comes back.
pub const SHARED_CREDENTIAL_KEYS: [&str; 5] =
    ["mcpOAuth", "mcpOAuthClientConfig", "mcpXaaIdp", "mcpXaaIdpConfig", "pluginSecrets"];

/// What this server is: read once with one shell job.
#[derive(Debug, Clone, Default)]
pub struct Platform {
    pub macos: bool,
    pub home: String,
    pub user: String,
    pub host: String,
    /// Minutes east of UTC, from `date +%z`.
    pub tz_min: i32,
    /// The plugin's data directory, absolute (the fs sandbox root).
    pub root: String,
    pub curl: bool,
    /// The Keychain service of the login (a test may rename it).
    pub service: String,
    /// `run_job` is denied: a copy pushed over a link whose server's
    /// `plugin-remote-caps` leaves out run-process. Nothing works then.
    pub no_shell: bool,
}

impl Platform {
    pub fn config_home(&self) -> String {
        format!("{}/.claude", self.home)
    }
    pub fn credentials_file(&self) -> String {
        format!("{}/.claude/.credentials.json", self.home)
    }
    pub fn config_file(&self) -> String {
        format!("{}/.claude.json", self.home)
    }
    /// `cswap`-style per-session profiles, one directory per slot.
    pub fn profiles_dir(&self) -> String {
        format!("{}/profiles", self.root)
    }
}

thread_local! {
    static PLATFORM: RefCell<Option<Rc<Platform>>> = const { RefCell::new(None) };
}

/// The platform record, detected on first use.
pub async fn platform() -> Rc<Platform> {
    if let Some(p) = PLATFORM.with(|p| p.borrow().clone()) {
        return p;
    }
    let o = OVERRIDES.with(|s| s.borrow().clone());
    let home = o.home.clone().unwrap_or_else(|| home_dir().unwrap_or_default());
    let root = fs_root().unwrap_or_default();
    let service = o.keychain_service.clone().unwrap_or_else(|| KEYCHAIN_SERVICE.to_string());
    let mut p = Platform { home, root, service, ..Default::default() };
    let probe = "uname -s; id -un; date +%z; hostname -s 2>/dev/null || hostname; \
                 command -v curl >/dev/null 2>&1 && echo curl";
    match run_job(probe, None).await {
        Ok(out) => {
            let mut lines = out.output.lines();
            p.macos = lines.next().map(|l| l.trim() == "Darwin").unwrap_or(false) && !o.file_only;
            p.user = lines.next().unwrap_or("").trim().to_string();
            p.tz_min = timefmt::parse_tz(lines.next().unwrap_or("+0000"));
            p.host = lines.next().unwrap_or("").trim().to_string();
            p.curl = lines.any(|l| l.trim() == "curl");
        }
        Err(e) => {
            p.no_shell = e.code == tmux_plugin_sdk::abi::ErrorCode::CapDenied;
            log(&format!("accounts: platform probe: {}", e.message));
        }
    }
    if p.no_shell {
        // Detected again next time: the grant may be added meanwhile.
        return Rc::new(p);
    }
    let rc = Rc::new(p);
    PLATFORM.with(|s| *s.borrow_mut() = Some(Rc::clone(&rc)));
    rc
}

// ---------------------------------------------------------------------------
// shell helpers
// ---------------------------------------------------------------------------

/// Single-quote for the shell.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote for a `security -i` command line: it re-parses the line
/// shell-style, so double quotes with `\"` and `\\` inside.
fn security_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The lock functions every install job starts with. `lock DIR STALE`
/// returns 0 with the directory made, 1 after ~9 s of someone else
/// holding it; `unlock DIR` removes it.
const SH_LOCK: &str = r#"
lock() {
  _i=0
  while :; do
    if mkdir "$1" 2>/dev/null; then return 0; fi
    _m=$(stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null)
    _now=$(date +%s)
    if [ -n "$_m" ] && [ $((_now - _m)) -gt "$2" ]; then rmdir "$1" 2>/dev/null; continue; fi
    _i=$((_i + 1))
    [ "$_i" -gt 45 ] && return 1
    sleep 0.2
  done
}
unlock() { rmdir "$1" 2>/dev/null; }
"#;

thread_local! {
    static SEQ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A staging-file name no other call in this instance is using.
fn nonce() -> String {
    let n = SEQ.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    format!("{}-{n}", now_ms())
}

/// A file in the staging directory, by relative (sandbox) and absolute
/// (shell) path.
struct Staged {
    rel: String,
    abs: String,
}

async fn stage(p: &Platform, name: &str, data: &str) -> Result<Staged, String> {
    let rel = format!("stage/{name}");
    // The directory may not exist yet; fs_write does not create parents.
    let _ = run_job(&format!("umask 077; mkdir -p {}", sq(&format!("{}/stage", p.root))), None)
        .await;
    fs_write(&rel, data.as_bytes().to_vec(), false)
        .await
        .map_err(|e| format!("stage {name}: {}", e.message))?;
    Ok(Staged { abs: format!("{}/{rel}", p.root), rel })
}

async fn unstage(s: &Staged) {
    let _ = fs_remove(&s.rel).await;
}

/// Read a whole file through the host (no size cap), None when missing.
pub async fn read_file(path: &str) -> Result<Option<String>, String> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        match fs_read(path, out.len() as u64, 1 << 20).await {
            Ok((chunk, eof)) => {
                out.extend_from_slice(&chunk);
                if eof || chunk.is_empty() {
                    break;
                }
            }
            Err(e) if e.code == tmux_plugin_sdk::abi::ErrorCode::NoSuchObject => return Ok(None),
            Err(e) => return Err(format!("read {path}: {}", e.message)),
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

// ---------------------------------------------------------------------------
// credentials
// ---------------------------------------------------------------------------

/// The live credential as Claude stores it. On a Mac the Keychain item,
/// else (or when the Keychain has none) the plaintext file. None when
/// nobody is logged in.
pub async fn read_live_credentials(p: &Platform) -> Result<Option<String>, String> {
    if p.macos {
        let cmd = format!(
            "security find-generic-password -a {} -s {} -w 2>/dev/null",
            sq(&p.user),
            sq(&p.service)
        );
        if let Ok(out) = run_job(&cmd, None).await {
            let text = out.output.trim_end_matches('\n').to_string();
            if out.status == 0 && !text.is_empty() && serde_json::from_str::<Value>(&text).is_ok() {
                return Ok(Some(text));
            }
        }
    }
    let text = read_file(&p.credentials_file()).await?;
    Ok(text.filter(|t| serde_json::from_str::<Value>(t).is_ok()))
}

/// The `claudeAiOauth` object of a credential blob.
pub fn oauth_of(creds: &str) -> Option<Map<String, Value>> {
    let v: Value = serde_json::from_str(creds).ok()?;
    v.get("claudeAiOauth")?.as_object().cloned()
}

pub fn access_token(creds: &str) -> Option<String> {
    oauth_of(creds)?.get("accessToken")?.as_str().map(str::to_string)
}

pub fn refresh_token(creds: &str) -> Option<String> {
    oauth_of(creds)?.get("refreshToken")?.as_str().map(str::to_string)
}

pub fn expires_at(creds: &str) -> Option<i64> {
    oauth_of(creds)?.get("expiresAt")?.as_i64()
}

/// Expired, or within five minutes of it (claude's own buffer).
pub fn token_expired(creds: &str, now_ms: i64) -> bool {
    match expires_at(creds) {
        Some(t) => now_ms + 5 * 60 * 1000 >= t,
        None => false,
    }
}

/// A lineage id for a credential: the refresh token's hash when it has
/// one (survives access-token rotation), else the whole blob's.
pub fn fingerprint(creds: &str) -> String {
    let mut h = Sha256::new();
    match refresh_token(creds) {
        Some(t) => h.update(t.as_bytes()),
        None => h.update(creds.as_bytes()),
    }
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// The blob to install for a slot: the slot's stored credential with the
/// machine-wide keys taken from the live credential.
pub fn compose_for_install(stored: &str, live: Option<&str>) -> String {
    let Ok(Value::Object(mut target)) = serde_json::from_str::<Value>(stored) else {
        return stored.to_string();
    };
    if let Some(Ok(Value::Object(live))) = live.map(serde_json::from_str::<Value>) {
        for k in SHARED_CREDENTIAL_KEYS {
            match live.get(k) {
                Some(v) => {
                    target.insert(k.to_string(), v.clone());
                }
                None => {
                    target.remove(k);
                }
            }
        }
    }
    Value::Object(target).to_string()
}

/// The lock job for a credential write: takes both of claude's refresh
/// locks and prints the live credential as it is under them.
pub async fn lock_credentials(p: &Platform) -> Result<Option<String>, String> {
    let read = if p.macos {
        format!(
            "security find-generic-password -a {} -s {} -w 2>/dev/null || cat {} 2>/dev/null",
            sq(&p.user),
            sq(&p.service),
            sq(&p.credentials_file())
        )
    } else {
        format!("cat {} 2>/dev/null", sq(&p.credentials_file()))
    };
    let script = format!(
        "{SH_LOCK}
mkdir -p {home} 2>/dev/null
lock {l1} 60 || {{ echo LOCK-FAILED; exit 3; }}
lock {l2} 60 || {{ unlock {l1}; echo LOCK-FAILED; exit 3; }}
echo LOCKED
{read}
",
        home = sq(&p.config_home()),
        l1 = sq(&format!("{}/.oauth_refresh.lock", p.config_home())),
        l2 = sq(&format!("{}.lock", p.config_home())),
    );
    let out = run_job(&script, None).await.map_err(|e| e.message.clone())?;
    let text = out.output;
    let Some(rest) = text.split_once("LOCKED\n").map(|(_, r)| r) else {
        return Err("claude's credential lock is held by another process; try again".into());
    };
    let rest = rest.trim();
    if rest.is_empty() || serde_json::from_str::<Value>(rest).is_err() {
        return Ok(None);
    }
    Ok(Some(rest.to_string()))
}

pub async fn unlock_credentials(p: &Platform) {
    let _ = run_job(
        &format!(
            "rmdir {} 2>/dev/null; rmdir {} 2>/dev/null",
            sq(&format!("{}/.oauth_refresh.lock", p.config_home())),
            sq(&format!("{}.lock", p.config_home()))
        ),
        None,
    )
    .await;
}

/// The write itself, with both locks held by the caller.
pub async fn write_credentials_locked(p: &Platform, creds: &str) -> Result<(), String> {
    let n = nonce();
    let file = stage(p, &format!("creds-{n}.json"), creds).await?;
    let mut script = format!("umask 077\n");
    if p.macos {
        // `security -i` keeps the secret off argv; its line buffer is
        // 4096 bytes, so a long blob (MCP logins) goes by argv instead.
        let line = format!(
            "add-generic-password -U -a {} -s {} -w {}\n",
            security_quote(&p.user),
            security_quote(&p.service),
            security_quote(creds)
        );
        if line.len() < 4000 {
            let cmd = stage(p, &format!("kc-{n}.cmd"), &line).await?;
            script.push_str(&format!(
                "security -i < {} >/dev/null 2>&1 || {{ echo KC-FAILED; exit 4; }}\n",
                sq(&cmd.abs)
            ));
            script.push_str(&format!("rm -f {}\n", sq(&cmd.abs)));
        } else {
            script.push_str(&format!(
                "security add-generic-password -U -a {} -s {} -w \"$(cat {})\" >/dev/null 2>&1 || {{ echo KC-FAILED; exit 4; }}\n",
                sq(&p.user),
                sq(&p.service),
                sq(&file.abs)
            ));
        }
        // A plaintext file beside the Keychain item is claude's fallback;
        // leave a stale one behind and a Keychain miss would log in the
        // old account. Rewrite it when it exists, never create it.
        script.push_str(&format!(
            "f={}; if [ -f \"$f\" ]; then cp {} \"$f.tmp.$$\" && mv -f \"$f.tmp.$$\" \"$f\"; fi\n",
            sq(&p.credentials_file()),
            sq(&file.abs)
        ));
    } else {
        script.push_str(&format!(
            "f={}; mkdir -p \"$(dirname \"$f\")\"; cp {} \"$f.tmp.$$\" && mv -f \"$f.tmp.$$\" \"$f\" || {{ echo CP-FAILED; exit 4; }}\n",
            sq(&p.credentials_file()),
            sq(&file.abs)
        ));
    }
    script.push_str("echo DONE\n");
    let out = run_job(&script, None).await;
    unstage(&file).await;
    let out = out.map_err(|e| e.message.clone())?;
    if !out.output.contains("DONE") {
        return Err(format!("install credentials: {}", out.output.trim()));
    }
    Ok(())
}

/// Remove the live login entirely (Keychain item and file), under the
/// locks. Used when a slot is removed while active? No: a removal keeps
/// the live login; this is for `logout`.
#[allow(dead_code)]
pub async fn clear_credentials(p: &Platform) -> Result<(), String> {
    let _ = lock_credentials(p).await?;
    let mut script = String::new();
    if p.macos {
        script.push_str(&format!(
            "security delete-generic-password -a {} -s {} >/dev/null 2>&1\n",
            sq(&p.user),
            sq(&p.service)
        ));
    }
    script.push_str(&format!("rm -f {}\n", sq(&p.credentials_file())));
    let r = run_job(&script, None).await;
    unlock_credentials(p).await;
    r.map(|_| ()).map_err(|e| e.message.clone())
}

// ---------------------------------------------------------------------------
// identity: ~/.claude.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Identity {
    pub email: String,
    pub org_name: String,
    pub org_uuid: String,
    pub account_uuid: String,
}

pub fn identity_of(oauth_account: &Value) -> Option<Identity> {
    let o = oauth_account.as_object()?;
    let g = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let email = g("emailAddress");
    if email.is_empty() {
        return None;
    }
    Some(Identity {
        email,
        org_name: g("organizationName"),
        org_uuid: g("organizationUuid"),
        account_uuid: g("accountUuid"),
    })
}

/// The `oauthAccount` object of the live config, and the whole config.
pub async fn read_config(p: &Platform) -> Result<Option<Value>, String> {
    match read_file(&p.config_file()).await? {
        Some(text) => serde_json::from_str::<Value>(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", p.config_file())),
        None => Ok(None),
    }
}

pub async fn live_oauth_account(p: &Platform) -> Result<Option<Value>, String> {
    Ok(read_config(p).await?.and_then(|c| c.get("oauthAccount").cloned()).filter(|v| v.is_object()))
}

/// Splice an `oauthAccount` into `~/.claude.json` under its lock, keeping
/// every other key (projects, MCP servers, the lot).
pub async fn install_oauth_account(p: &Platform, oauth_account: &Value) -> Result<(), String> {
    let lock = sq(&format!("{}.lock", p.config_file()));
    let script = format!("{SH_LOCK}\nlock {lock} 10 || {{ echo LOCK-FAILED; exit 3; }}\necho LOCKED\n");
    let out = run_job(&script, None).await.map_err(|e| e.message.clone())?;
    if !out.output.contains("LOCKED") {
        return Err("~/.claude.json is locked by another process; try again".into());
    }
    let r = splice_config_locked(p, oauth_account).await;
    let _ = run_job(&format!("rmdir {lock} 2>/dev/null"), None).await;
    r
}

async fn splice_config_locked(p: &Platform, oauth_account: &Value) -> Result<(), String> {
    let mut config = read_config(p).await?.unwrap_or_else(|| Value::Object(Map::new()));
    let Some(obj) = config.as_object_mut() else {
        return Err(format!("{} is not a JSON object", p.config_file()));
    };
    obj.insert("oauthAccount".into(), oauth_account.clone());
    let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    let n = nonce();
    let file = stage(p, &format!("config-{n}.json"), &text).await?;
    // claude's own file is 0644; cp honours umask, and a 0600 config is
    // fine for claude, so the staged mode carries over.
    let script = format!(
        "f={}; cp {} \"$f.tmp.$$\" && mv -f \"$f.tmp.$$\" \"$f\" && echo DONE",
        sq(&p.config_file()),
        sq(&file.abs)
    );
    let out = run_job(&script, None).await;
    unstage(&file).await;
    let out = out.map_err(|e| e.message.clone())?;
    if !out.output.contains("DONE") {
        return Err(format!("install config: {}", out.output.trim()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// per-session profiles (CLAUDE_CONFIG_DIR)
// ---------------------------------------------------------------------------

/// Filesystem-safe name for an email, as cswap spells it.
pub fn slug(email: &str) -> String {
    email
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect()
}

#[allow(dead_code)]
/// The Keychain service claude derives for a `CLAUDE_CONFIG_DIR` value:
/// the first 8 hex of the sha256 of the exact string exported.
pub fn profile_keychain_service(config_dir: &str) -> String {
    let mut h = Sha256::new();
    h.update(config_dir.as_bytes());
    let out = h.finalize();
    let hex: String = out.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{KEYCHAIN_SERVICE}-{hex}")
}

/// The items a profile shares with `~/.claude` by symlink: settings,
/// keys, instructions, skills, commands, agents. Conversation history
/// stays per profile.
const SHARED_ITEMS: [&str; 7] =
    ["settings.json", "settings.local.json", "keybindings.json", "CLAUDE.md", "skills", "commands", "agents"];

/// Make (or refresh) the profile directory for a slot and return its
/// path: the value to export as `CLAUDE_CONFIG_DIR` so one claude runs
/// as that account while the machine's login stays what it is. The
/// credential is written only when the profile has none yet or the
/// stored one is newer than the profile's; a live claude in the profile
/// owns its own copy.
pub async fn ensure_profile(
    p: &Platform,
    slot: i64,
    email: &str,
    creds: &str,
    oauth_account: &Value,
) -> Result<String, String> {
    let dir = format!("{}/{slot}-{}", p.profiles_dir(), slug(email));
    let n = nonce();
    let creds_file = stage(p, &format!("prof-creds-{n}.json"), creds).await?;
    // The config seed: identity plus the two keys that keep onboarding
    // from showing (`theme`, `hasCompletedOnboarding`). Merged into an
    // existing profile config so its own projects and history stay.
    let existing = read_file(&format!("{dir}/.claude.json"))
        .await
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut cfg = existing.as_object().cloned().unwrap_or_default();
    cfg.insert("oauthAccount".into(), oauth_account.clone());
    cfg.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    // From the machine's own config: the theme, the MCP servers, and
    // the per-project entries (folder trust above all: without it the
    // new claude stops at "do you trust this folder?"). The profile's
    // own entries win; a project it has not seen yet is copied.
    let live = read_config(p).await.ok().flatten();
    if !cfg.contains_key("theme") {
        let theme = live
            .as_ref()
            .and_then(|c| c.get("theme").cloned())
            .unwrap_or_else(|| Value::String("dark".into()));
        cfg.insert("theme".into(), theme);
    }
    if let Some(live) = &live {
        if !cfg.contains_key("mcpServers") {
            if let Some(m) = live.get("mcpServers") {
                cfg.insert("mcpServers".into(), m.clone());
            }
        }
        if let Some(Value::Object(src)) = live.get("projects") {
            let dst = cfg.entry("projects").or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(dst) = dst {
                for (k, v) in src {
                    dst.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }
    let cfg_text = serde_json::to_string_pretty(&Value::Object(cfg)).map_err(|e| e.to_string())?;
    let cfg_file = stage(p, &format!("prof-config-{n}.json"), &cfg_text).await?;
    let mut links = String::new();
    for item in SHARED_ITEMS {
        links.push_str(&format!(
            "if [ -e {src} ] && [ ! -e {dst} ]; then ln -s {src} {dst}; fi\n",
            src = sq(&format!("{}/{item}", p.config_home())),
            dst = sq(&format!("{dir}/{item}"))
        ));
    }
    let script = format!(
        "umask 077
d={dir}
mkdir -p \"$d\" && chmod 700 \"$d\"
if [ ! -f \"$d/.credentials.json\" ]; then cp {creds} \"$d/.credentials.json\"; fi
cp {cfg} \"$d/.claude.json.tmp\" && mv -f \"$d/.claude.json.tmp\" \"$d/.claude.json\"
{links}
echo DONE
",
        dir = sq(&dir),
        creds = sq(&creds_file.abs),
        cfg = sq(&cfg_file.abs),
    );
    let out = run_job(&script, None).await;
    unstage(&creds_file).await;
    unstage(&cfg_file).await;
    let out = out.map_err(|e| e.message.clone())?;
    if !out.output.contains("DONE") {
        return Err(format!("profile {dir}: {}", out.output.trim()));
    }
    Ok(dir)
}

#[allow(dead_code)]
/// The credential a profile's claude holds now: its hashed Keychain item
/// on a Mac (claude migrates the seed there on first write), else the
/// profile's file.
pub async fn read_profile_credentials(p: &Platform, dir: &str) -> Result<Option<String>, String> {
    if p.macos {
        let cmd = format!(
            "security find-generic-password -a {} -s {} -w 2>/dev/null",
            sq(&p.user),
            sq(&profile_keychain_service(dir))
        );
        if let Ok(out) = run_job(&cmd, None).await {
            let text = out.output.trim_end_matches('\n').to_string();
            if out.status == 0 && serde_json::from_str::<Value>(&text).is_ok() {
                return Ok(Some(text));
            }
        }
    }
    read_file(&format!("{dir}/.credentials.json")).await
}
