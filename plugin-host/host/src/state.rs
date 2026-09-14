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
    /// Raw binary event buffer from the C bridge (header + field block, see
    /// abi-types); routing happens at drain time (an event may fan out to
    /// several instances). `seq` is patched into the buffer at delivery.
    RawEvent { bytes: Vec<u8>, seq: u64 },
    /// Create + init an instance of `plugin` for `scope` (queued by
    /// pgh_plugin_load so guest code never runs inside the load call).
    Instantiate { plugin: String, scope: ScopeId },
    /// Async completion from the C side; delivered to the owning instance
    /// after a generation check (stale completions are dropped). `err` is 0
    /// or an ErrorCode number; `data` is handed to the guest as an OwnedBuf
    /// (error message bytes when err != 0).
    AsyncComplete { token: u64, err: i32, v0: i64, v1: i64, data: Vec<u8> },
    /// Mode event from the C side (a complete binary event buffer built by
    /// C, including the mode field); delivered only to the instance owning
    /// the mode, after a generation check (stale events are dropped).
    ModeEvent { mode_id: u64, bytes: Vec<u8> },
    /// A service call for the instance that registered the method: a
    /// complete `service-request` event buffer, delivered to `owner`
    /// only, generation-checked.
    ServiceRequest { owner: crate::services::Owner, bytes: Vec<u8> },
    /// One reply page of a service call, for the caller. Like
    /// AsyncComplete, but with `MORE` set the token stays alive.
    ServicePage { token: u64, page: u32, flags: u32, data: Vec<u8> },
    /// A topic event for one subscribed instance: a complete
    /// `service-event` buffer, delivered to `target` only.
    ServiceEvent { target: crate::services::Owner, bytes: Vec<u8> },
    /// A frame from a bridge peer (pgh_bridge_recv is enqueue-only).
    BridgeFrame { peer: u32, bytes: Vec<u8> },
    /// A bridge peer came up (with a name) or went down.
    BridgeState { peer: u32, name: Option<String>, up: bool },
    /// Ask one instance of `plugin` whether it accepts the peer's copy
    /// (a field block with server, version and role); the answer lands
    /// in the bridge's verdicts.
    ServiceAccept { peer: u32, plugin: String, bytes: Vec<u8> },
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
