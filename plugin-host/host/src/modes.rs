//! Mode ownership bookkeeping.
//!
//! A mode id identifies one open plugin UI mode (a C-side floating pane
//! running window_plugin_mode). The map records which instance (plugin,
//! scope, generation) owns it; mode events whose owner died or was
//! reloaded (generation mismatch) are dropped silently, and instance
//! teardown force-closes the C side through the vtable.
//!
//! Mirrors tokens.rs: its own thread-local cell so registration can happen
//! during dispatch (while the instance is checked out of the registry) and
//! pgh_mode_event stays enqueue-only.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::abi::StoreData;
use crate::registry::ScopeId;

#[derive(Debug, Clone)]
pub struct ModeOwner {
    pub plugin: String,
    pub scope: ScopeId,
    pub generation: u64,
}

thread_local! {
    static MODES: RefCell<HashMap<u64, ModeOwner>> =
        RefCell::new(HashMap::new());
}

/// Record ownership of a freshly opened mode.
pub fn register(mode_id: u64, plugin: &str, scope: ScopeId, generation: u64) {
    MODES.with(|m| {
        m.borrow_mut().insert(
            mode_id,
            ModeOwner { plugin: plugin.to_string(), scope, generation },
        );
    });
}

/// Look up a mode's owner (None = unknown or already closed).
pub fn owner_of(mode_id: u64) -> Option<ModeOwner> {
    MODES.with(|m| m.borrow().get(&mode_id).cloned())
}

/// Remove a mode from the map (on mode-closed). None = unknown.
pub fn take(mode_id: u64) -> Option<ModeOwner> {
    MODES.with(|m| m.borrow_mut().remove(&mode_id))
}

/// Does the calling instance own this mode? Guards mode_write/preview/
/// close so one plugin can never touch another plugin's mode.
pub fn owned_by(mode_id: u64, data: &StoreData) -> bool {
    MODES.with(|m| {
        m.borrow().get(&mode_id).is_some_and(|o| {
            o.plugin == data.plugin
                && o.scope == data.scope
                && o.generation == data.generation
        })
    })
}

/// Remove every mode belonging to a dying instance, returning the ids so
/// the caller can force-close them through the vtable.
pub fn purge_instance(plugin: &str, scope: ScopeId, generation: u64) -> Vec<u64> {
    MODES.with(|m| {
        let mut map = m.borrow_mut();
        let doomed: Vec<u64> = map
            .iter()
            .filter(|(_, o)| {
                o.plugin == plugin && o.scope == scope && o.generation == generation
            })
            .map(|(k, _)| *k)
            .collect();
        for id in &doomed {
            map.remove(id);
        }
        doomed
    })
}
