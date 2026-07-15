//! Shared ABI types for the tmux plugin system.
//!
//! These serde types define the JSON messages that cross both boundaries:
//! tmux (C) -> plugin-host (Rust) event JSON, and host <-> guest request/
//! response payloads. The host crate and the plugin SDK both depend on this
//! crate, so schema drift between them fails to compile.

use serde::{Deserialize, Serialize};
use std::fmt;

/// ABI version spoken by this host. Guests export `pgh_abi_version()` and
/// must return the same value to be loaded.
pub const ABI_VERSION: i32 = 1;

/// Plugin instantiation scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeType {
    Server,
    Session,
    Window,
    Pane,
}

impl fmt::Display for ScopeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScopeType::Server => write!(f, "server"),
            ScopeType::Session => write!(f, "session"),
            ScopeType::Window => write!(f, "window"),
            ScopeType::Pane => write!(f, "pane"),
        }
    }
}

/// Object-id scope attached to an event, as emitted by the C bridge.
/// tmux ids are monotonic and never reused within a server lifetime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventScope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u32>,
}

/// An event as delivered to guests via `pgh_on_event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub seq: u64,
    #[serde(default)]
    pub scope: EventScope,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Object reference as emitted by the C bridge (plugin-events.c).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeObjectRef {
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Containing window id (panes only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
}

/// Event JSON as emitted by the C bridge, before normalization into `Event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeEvent {
    pub event: String,
    #[serde(default)]
    pub client: Option<BridgeObjectRef>,
    #[serde(default)]
    pub session: Option<BridgeObjectRef>,
    #[serde(default)]
    pub window: Option<BridgeObjectRef>,
    #[serde(default)]
    pub pane: Option<BridgeObjectRef>,
    #[serde(default)]
    pub pbname: Option<String>,
}

impl BridgeEvent {
    /// Normalize into the guest-facing `Event` shape. Names and the paste
    /// buffer name travel in `data`; ids form the scope.
    pub fn into_event(self, seq: u64) -> Event {
        let mut scope = EventScope::default();
        let mut data = serde_json::Map::new();

        scope.client = self.client.as_ref().map(|o| o.id);
        scope.session = self.session.as_ref().map(|o| o.id);
        // A pane's containing window counts as event scope even when the C
        // side did not pass the window object explicitly.
        scope.window = self
            .window
            .as_ref()
            .map(|o| o.id)
            .or(self.pane.as_ref().and_then(|p| p.window));
        scope.pane = self.pane.as_ref().map(|o| o.id);

        if let Some(name) = self.session.and_then(|o| o.name) {
            data.insert("session_name".into(), name.into());
        }
        if let Some(name) = self.window.and_then(|o| o.name) {
            data.insert("window_name".into(), name.into());
        }
        if let Some(name) = self.client.and_then(|o| o.name) {
            data.insert("client_name".into(), name.into());
        }
        if let Some(pbname) = self.pbname {
            data.insert("buffer_name".into(), pbname.into());
        }

        Event {
            event: self.event,
            seq,
            scope,
            data: if data.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::Object(data)
            },
        }
    }
}

/// Names of the wasm exports a guest must (or may) provide. The host binds
/// REQUIRED_EXPORTS at instantiation and treats the rest as optional.
pub mod exports {
    pub const ABI_VERSION: &str = "pgh_abi_version";
    pub const STATE_VERSION: &str = "pgh_state_version";
    pub const ALLOC: &str = "pgh_alloc";
    pub const FREE: &str = "pgh_free";
    pub const INIT: &str = "pgh_init";
    pub const ON_EVENT: &str = "pgh_on_event";
    pub const ON_ASYNC_COMPLETE: &str = "pgh_on_async_complete";
    pub const ON_CONFIG_CHANGED: &str = "pgh_on_config_changed";
    pub const SNAPSHOT: &str = "pgh_snapshot";
    pub const MIGRATE: &str = "pgh_migrate";
    pub const ON_UNLOAD: &str = "pgh_on_unload";
}

/// Host import module name and function names.
pub mod imports {
    pub const MODULE: &str = "tmux";
    pub const HOST_CALL: &str = "host_call";
    pub const HOST_REQUEST: &str = "host_request";
    pub const HOST_LOG: &str = "host_log";
}

/// `host_call` return values.
pub const HOST_CALL_OK: i32 = 0;
pub const HOST_CALL_ERR: i32 = 1;
pub const HOST_CALL_ABI_FAILURE: i32 = 2;

/// Structured error codes returned to guests. Never traps the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "E_BAD_REQUEST")]
    BadRequest,
    #[serde(rename = "E_UNKNOWN_METHOD")]
    UnknownMethod,
    #[serde(rename = "E_CAP_DENIED")]
    CapDenied,
    #[serde(rename = "E_NO_SUCH_OBJECT")]
    NoSuchObject,
    #[serde(rename = "E_OUT_OF_SCOPE")]
    OutOfScope,
    #[serde(rename = "E_LIMIT")]
    Limit,
    #[serde(rename = "E_HOST")]
    Host,
    #[serde(rename = "E_CANCELLED")]
    Cancelled,
    #[serde(rename = "E_UNSUPPORTED")]
    Unsupported,
}

impl ErrorCode {
    /// Stable numeric code, used e.g. as the negated return value of
    /// `host_request` when a request is rejected synchronously.
    pub fn as_num(self) -> i32 {
        match self {
            ErrorCode::BadRequest => 1,
            ErrorCode::UnknownMethod => 2,
            ErrorCode::CapDenied => 3,
            ErrorCode::NoSuchObject => 4,
            ErrorCode::OutOfScope => 5,
            ErrorCode::Limit => 6,
            ErrorCode::Host => 7,
            ErrorCode::Cancelled => 8,
            ErrorCode::Unsupported => 9,
        }
    }
}

/// Descriptor passed to `pgh_plugin_load` (built by the C side from the
/// load-plugin command / tmux.conf declaration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadDescriptor {
    pub name: String,
    pub path: String,
    #[serde(default = "default_scope")]
    pub scope: ScopeType,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Capability grants from the user (names as in the caps model; M7).
    #[serde(default)]
    pub caps: Vec<String>,
}

fn default_scope() -> ScopeType {
    ScopeType::Server
}

/// Host request as sent by guests through `host_call` / `host_request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostRequest {
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Error payload inside a `HostResponse::Err`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Response to a host request: `{"ok": ...}` or `{"err": {...}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostResponse {
    Ok(serde_json::Value),
    Err(HostError),
}
