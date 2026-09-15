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

/// A peer that pushed `plugin` here may call it: option 1, auto-allow the
/// pushed pair. Overwrites a `pending`/`deny` row left by the handshake.
pub fn allow_pushed(server: &str, plugin: &str) {
    sqlite::peers_set(server, plugin, "allow");
}

/// A peer said hello (or a new plugin appeared on it): reconcile the
/// grant rows for the (server, plugin) pairs it runs that this side
/// serves. Returns the plugin names that are newly `pending` (an
/// initiator link only), for the caller to open a menu.
pub fn reconcile(peer: u32) -> Vec<String> {
    let Some(server) = bridge::peer_name(peer) else {
        return Vec::new();
    };
    if server == tmux_plugin_abi::LOCAL_SERVER {
        return Vec::new();
    }
    let initiator = bridge::peer_is_initiator(peer);
    let remote_plugins = bridge::peer_plugin_names(peer);
    let served = services::providers();
    let mut new_pending = Vec::new();
    for name in remote_plugins {
        if !served.iter().any(|p| *p == name) {
            continue;
        }
        if sqlite::peers_get(&server, &name).is_some()
            || sqlite::peers_get(&server, ALL).is_some()
        {
            continue; // already decided (or wildcarded)
        }
        if initiator {
            if sqlite::peers_ensure(&server, &name, "pending") {
                new_pending.push(name);
            }
        } else {
            // Inbound: default deny, no prompt.
            sqlite::peers_ensure(&server, &name, "deny");
        }
    }
    new_pending
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
