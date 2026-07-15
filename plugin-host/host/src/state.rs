//! Global (main-thread-only) state cells.
//!
//! Deliberately split into separate thread-locals so that re-entrancy is
//! structurally safe: a vtable call made while the registry is borrowed may
//! legally re-enter `pgh_notify`, which touches only EVENTS. Nothing may
//! re-enter any other pgh_* entry point (contract in plugin-host.h).
//!
//! thread_local (not a Mutex) both documents and enforces the threading
//! model: touching these from the ticker thread would find empty state, not
//! corrupt anything.

use std::cell::RefCell;
use std::collections::VecDeque;

use crate::registry::{Registry, ScopeId};

/// A unit of queued work for pgh_drain.
pub enum Delivery {
    /// Raw event JSON from the C bridge; routing happens at drain time
    /// (an event may fan out to several instances).
    RawEvent { json: String, seq: u64 },
    /// Create + init an instance of `plugin` for `scope` (queued by
    /// pgh_plugin_load so guest code never runs inside the load call).
    Instantiate { plugin: String, scope: ScopeId },
    /// Async completion from the C side; delivered to the owning instance
    /// after a generation check (stale completions are dropped).
    AsyncComplete { token: u64, json: String, is_error: bool },
}

pub struct EventQueue {
    pub deliveries: VecDeque<Delivery>,
    pub seq: u64,
}

impl EventQueue {
    pub fn new() -> Self {
        Self { deliveries: VecDeque::new(), seq: 0 }
    }
}

thread_local! {
    pub static REGISTRY: RefCell<Registry> = RefCell::new(Registry::new());
    pub static EVENTS: RefCell<EventQueue> = RefCell::new(EventQueue::new());
}
