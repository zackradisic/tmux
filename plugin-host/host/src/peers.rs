//! Peer grants: which linked server may call which of this server's
//! plugins back over the bridge.
//!
//! The bridge is bidirectional: once a link is up, either side can call
//! the other's registered services. Without a gate any accepted peer
//! could call any served method. This module is that gate. It runs in the
//! host, before a plugin sees an incoming call, and owns its own table
//! (see `sqlite.rs`), separate from any plugin's store.
//!
//! Identity is the peer's name: for a link this side made, the ssh target
//! `remote-attach` was given; for an inbound client (this side is
//! someone's remote), the host name it said in `hello`. Granularity is
//! (server, plugin), with `plugin = "*"` meaning every plugin. The local
//! server is always allowed.
//!
//! Decisions are made at the handshake, not at call time. When `hello`
//! completes on a link we made, every (server, plugin) pair the remote
//! runs that this side also serves and has no row for goes `pending`, and
//! a menu opens on the client that ran `remote-attach`. An inbound peer's
//! pairs go `deny` with no prompt. Until a pair is `allow`, an incoming
//! call for it fails at once with `E_DENIED`.

use std::ffi::CString;

use crate::bridge;
use crate::services;
use crate::sqlite;
use crate::state::REGISTRY;

const ALL: &str = "*";

/// May `server` call this server's `plugin`? The local server always may;
/// otherwise a row for the plugin, or a wildcard row, must be `allow`.
pub fn allowed(server: &str, plugin: &str) -> bool {
    if server == tmux_plugin_abi::LOCAL_SERVER {
        return true;
    }
    if sqlite::peers_get(server, ALL).as_deref() == Some("allow") {
        return true;
    }
    sqlite::peers_get(server, plugin).as_deref() == Some("allow")
}

/// The message an `E_DENIED` reply carries: what was refused and the fix.
pub fn deny_message(server: &str, plugin: &str) -> String {
    format!(
        "{server} may not call {plugin} here; \
         plugin-peers allow {server} {plugin}"
    )
}

/// The served methods of `plugin` that a remote peer may call, from its
/// sidecar's `[caps.services] serve_remote`. Empty means the plugin
/// accepts no remote callers.
fn serve_remote(plugin: &str) -> Vec<String> {
    REGISTRY.with(|r| {
        r.borrow()
            .plugins
            .get(plugin)
            .map(|d| d.caps.serve_remote.clone())
            .unwrap_or_default()
    })
}

/// Does `plugin` accept any remote caller (it declared serve_remote)?
fn accepts_remote(plugin: &str) -> bool {
    !serve_remote(plugin).is_empty()
}

/// A peer said hello (or a new plugin appeared): reconcile the grant rows.
/// Only a link this side made is gated (remote -> initiator), and only
/// plugins that declare serve_remote can reach `pending`. Returns the
/// plugin names newly `pending`, for the caller to open a menu. An inbound
/// peer (someone who linked to us) needs no rows: its calls are always
/// allowed.
pub fn reconcile(peer: u32) -> Vec<String> {
    if !bridge::peer_is_initiator(peer) {
        return Vec::new();
    }
    let Some(server) = bridge::peer_name(peer) else {
        return Vec::new();
    };
    if server == tmux_plugin_abi::LOCAL_SERVER {
        return Vec::new();
    }
    let remote_plugins = bridge::peer_plugin_names(peer);
    let served = services::providers();
    let mut new_pending = Vec::new();
    for name in remote_plugins {
        if !served.iter().any(|p| *p == name) || !accepts_remote(&name) {
            continue;
        }
        if sqlite::peers_get(&server, &name).is_some()
            || sqlite::peers_get(&server, ALL).is_some()
        {
            continue; // already decided (or wildcarded)
        }
        if sqlite::peers_ensure(&server, &name, "pending") {
            new_pending.push(name);
        }
    }
    new_pending
}

/// Why a remote call was refused (see `check_remote_call`).
pub enum Refusal {
    /// The method is not in the plugin's serve_remote list: silent, no
    /// row, one log line.
    NotServed,
    /// The pair is not `allow` yet: the caller gets the grant message.
    NotGranted(String),
}

/// Gate an incoming call from a link this side made (remote -> initiator).
/// Ok means proceed. Only for initiator peers; an inbound peer's calls do
/// not come here (the caller allows them outright).
pub fn check_remote_call(server: &str, plugin: &str, method: &str) -> Result<(), Refusal> {
    if !serve_remote(plugin).iter().any(|m| m == method) {
        return Err(Refusal::NotServed);
    }
    if allowed(server, plugin) {
        Ok(())
    } else {
        Err(Refusal::NotGranted(deny_message(server, plugin)))
    }
}

/// Open the grant menu for `server` on `client`, listing its pending
/// plugins. Built and run here so the handshake and the `plugin-peers
/// menu` command share one path.
pub fn open_menu(server: &str, client: &str) {
    let pending = sqlite::peers_pending(server);
    if pending.is_empty() {
        return;
    }
    let list = pending.join(", ");
    let mut cmd = format!(
        "display-menu -c {} -T '{}'",
        shell_quote(client),
        format!("{server} wants to call back into this server")
    );
    // Allow all.
    cmd.push_str(&format!(
        " '' '' 'Allow all ({})' a 'plugin-peers allow {}'",
        menu_label(&list),
        shell_quote(server),
    ));
    // Each plugin.
    for p in &pending {
        cmd.push_str(&format!(
            " 'Allow {0}' '' 'plugin-peers allow {1} {2}'",
            menu_label(p),
            shell_quote(server),
            shell_quote(p),
        ));
    }
    cmd.push_str(&format!(
        " 'Deny all' d 'plugin-peers deny {}' 'Decide later' l ''",
        shell_quote(server),
    ));
    run(&cmd);
}

fn menu_label(s: &str) -> String {
    s.replace('\'', "")
}

/// Single-quote a value for the tmux command string.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn run(cmd: &str) {
    let Some(vt) = crate::vtable() else { return };
    let Ok(c) = CString::new(cmd) else { return };
    unsafe {
        (vt.run_command)(c.as_ptr(), 0);
    }
}

// ---------------------------------------------------------------------------
// Commands (the C `plugin-peers` command calls these through the FFI).
// ---------------------------------------------------------------------------

/// `plugin-peers list`: one line per row.
pub fn cmd_list() -> String {
    let rows = sqlite::peers_list();
    if rows.is_empty() {
        return "no peer grants\n".to_string();
    }
    let mut out = String::new();
    for (server, plugin, state, _first) in rows {
        out.push_str(&format!("{server}\t{plugin}\t{state}\n"));
    }
    out
}

/// `plugin-peers allow|deny <server> [plugin]`. Default plugin is "*".
pub fn cmd_set(server: &str, plugin: Option<&str>, state: &str) {
    let plugin = plugin.unwrap_or(ALL);
    sqlite::peers_set(server, plugin, state);
}

/// `plugin-peers revoke <server> [plugin]`: delete the row so the next
/// hello asks again.
pub fn cmd_revoke(server: &str, plugin: Option<&str>) -> bool {
    sqlite::peers_delete(server, plugin.unwrap_or(ALL))
}

/// `plugin-peers menu [server]`: reopen the handshake menu on `client`.
pub fn cmd_menu(server: &str, client: &str) {
    open_menu(server, client);
}
