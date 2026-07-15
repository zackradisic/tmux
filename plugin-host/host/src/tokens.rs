//! Async token bookkeeping.
//!
//! A token identifies one pending async operation. The map entry records
//! which instance (plugin, scope, generation) is waiting; completions whose
//! instance died or was reloaded (generation mismatch) are dropped
//! silently - the C side never needs to know about plugin lifecycles.
//!
//! Lives in its own thread-local cell so async starters (which run during
//! dispatch, while the instance is checked out of the registry) can
//! allocate tokens, and so pgh_async_complete can be re-entered from vtable
//! callbacks (enqueue-only, like pgh_notify).

use std::cell::RefCell;
use std::collections::HashMap;

use crate::registry::ScopeId;

#[derive(Debug, Clone)]
pub struct PendingToken {
    pub plugin: String,
    pub scope: ScopeId,
    pub generation: u64,
    /// C-side timer id when this token belongs to a timer (so instance
    /// teardown can cancel it through the vtable).
    pub timer_id: Option<u64>,
}

#[derive(Default)]
pub struct TokenMap {
    next: u64,
    map: HashMap<u64, PendingToken>,
}

thread_local! {
    static TOKENS: RefCell<TokenMap> = RefCell::new(TokenMap::default());
}

/// Allocate a token for an instance's pending operation.
pub fn allocate(plugin: &str, scope: ScopeId, generation: u64) -> u64 {
    TOKENS.with(|t| {
        let mut tm = t.borrow_mut();
        tm.next += 1;
        let token = tm.next;
        tm.map.insert(
            token,
            PendingToken {
                plugin: plugin.to_string(),
                scope,
                generation,
                timer_id: None,
            },
        );
        token
    })
}

/// Attach a C-side timer id to a token (after timer_start returns).
pub fn set_timer_id(token: u64, timer_id: u64) {
    TOKENS.with(|t| {
        if let Some(p) = t.borrow_mut().map.get_mut(&token) {
            p.timer_id = Some(timer_id);
        }
    });
}

/// Take a pending token out of the map (on completion). None = unknown or
/// already-cancelled token.
pub fn take(token: u64) -> Option<PendingToken> {
    TOKENS.with(|t| t.borrow_mut().map.remove(&token))
}

/// Drop a token allocated for a request that failed to start.
pub fn discard(token: u64) {
    TOKENS.with(|t| {
        t.borrow_mut().map.remove(&token);
    });
}

/// Remove every pending token belonging to a dying instance, returning the
/// C-side timer ids that must be cancelled through the vtable.
pub fn purge_instance(plugin: &str, scope: ScopeId, generation: u64) -> Vec<u64> {
    TOKENS.with(|t| {
        let mut tm = t.borrow_mut();
        let doomed: Vec<u64> = tm
            .map
            .iter()
            .filter(|(_, p)| {
                p.plugin == plugin && p.scope == scope && p.generation == generation
            })
            .map(|(k, _)| *k)
            .collect();
        let mut timers = Vec::new();
        for token in doomed {
            if let Some(p) = tm.map.remove(&token) {
                if let Some(id) = p.timer_id {
                    timers.push(id);
                }
            }
        }
        timers
    })
}
