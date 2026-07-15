//! Typed wrappers over the host_call / host_request ABI.
//!
//! Sync functions return immediately; async functions return futures
//! resolved by the SDK executor when the host delivers the completion.

use serde::Deserialize;
use serde_json::{json, Value};
use tmux_plugin_abi::{ErrorCode, HostError, HostResponse};

use crate::executor::{HostFuture, HostResult};
use crate::ids::PaneId;
use crate::runtime;

fn host_err(code: ErrorCode, message: impl Into<String>) -> HostError {
    HostError { code, message: message.into(), data: Value::Null }
}

/// Synchronous host call returning the `ok` value.
pub fn host_call(method: &str, params: Value) -> HostResult {
    let request = json!({ "method": method, "params": params }).to_string();
    let (status, response) = runtime::raw_host_call(&request);
    if status > 1 {
        return Err(host_err(ErrorCode::Host, "ABI failure in host_call"));
    }
    match serde_json::from_slice::<HostResponse>(&response) {
        Ok(HostResponse::Ok(value)) => Ok(value),
        Ok(HostResponse::Err(e)) => Err(e),
        Err(e) => Err(host_err(ErrorCode::Host, format!("bad response: {e}"))),
    }
}

/// Asynchronous host request; resolves with the completion payload.
pub fn host_request(method: &str, params: Value) -> HostFuture {
    let request = json!({ "method": method, "params": params }).to_string();
    let token = runtime::raw_host_request(&request);
    if token <= 0 {
        // Synthesize an immediately-ready error future via a fake token.
        // Token 0 is never allocated by the host. Register first, then
        // complete, so the result lands in the registered slot.
        let fut = HostFuture::new(0);
        crate::executor::complete(
            0,
            format!(
                "{{\"code\":\"{}\",\"message\":\"request rejected\"}}",
                code_name(-token as i32)
            )
            .as_bytes(),
            true,
        );
        return fut;
    }
    HostFuture::new(token as u64)
}

fn code_name(num: i32) -> &'static str {
    match num {
        1 => "E_BAD_REQUEST",
        2 => "E_UNKNOWN_METHOD",
        3 => "E_CAP_DENIED",
        4 => "E_NO_SUCH_OBJECT",
        5 => "E_OUT_OF_SCOPE",
        6 => "E_LIMIT",
        8 => "E_CANCELLED",
        9 => "E_UNSUPPORTED",
        _ => "E_HOST",
    }
}

// ---- sync API ----

pub fn subscribe(events: &[&str]) -> Result<(), HostError> {
    host_call("subscribe", json!({ "events": events })).map(|_| ())
}

pub fn unsubscribe(events: &[&str]) -> Result<(), HostError> {
    host_call("unsubscribe", json!({ "events": events })).map(|_| ())
}

pub fn list_sessions() -> HostResult {
    host_call("list_sessions", json!({}))
}

pub fn list_windows() -> HostResult {
    host_call("list_windows", json!({}))
}

pub fn list_panes() -> HostResult {
    host_call("list_panes", json!({}))
}

pub fn list_clients() -> HostResult {
    host_call("list_clients", json!({}))
}

/// Send a literal string to a pane (one key per character).
pub fn send_text(pane: PaneId, text: &str) -> Result<(), HostError> {
    host_call(
        "send_keys",
        json!({ "pane": pane.0, "keys": text, "literal": true }),
    )
    .map(|_| ())
}

/// Send one named key ("Enter", "C-c", "M-x", ...) to a pane.
pub fn send_key(pane: PaneId, key: &str) -> Result<(), HostError> {
    host_call(
        "send_keys",
        json!({ "pane": pane.0, "keys": key, "literal": false }),
    )
    .map(|_| ())
}

/// Capture pane text. Rows are relative to the visible top (negative
/// reaches history), `end` inclusive; both optional.
pub fn capture_pane(
    pane: PaneId,
    start: Option<i32>,
    end: Option<i32>,
) -> Result<String, HostError> {
    let mut params = json!({ "pane": pane.0 });
    if let Some(s) = start {
        params["start"] = s.into();
    }
    if let Some(e) = end {
        params["end"] = e.into();
    }
    let v = host_call("capture_pane", params)?;
    Ok(v.get("text").and_then(Value::as_str).unwrap_or("").to_string())
}

/// Where an option lives.
#[derive(Debug, Clone, Copy)]
pub enum OptionTarget {
    Server,
    Session(crate::ids::SessionId),
    Window(crate::ids::WindowId),
    Pane(PaneId),
}

impl OptionTarget {
    fn to_json(self) -> Value {
        match self {
            OptionTarget::Server => json!({ "type": "server" }),
            OptionTarget::Session(id) => {
                json!({ "type": "session", "id": id.0 })
            }
            OptionTarget::Window(id) => json!({ "type": "window", "id": id.0 }),
            OptionTarget::Pane(id) => json!({ "type": "pane", "id": id.0 }),
        }
    }
}

/// Get an option (server/global scope) as a string.
pub fn get_option(name: &str) -> Result<String, HostError> {
    get_option_in(OptionTarget::Server, name)
}

/// Set a user (@-prefixed) option at server/global scope.
pub fn set_option(name: &str, value: &str) -> Result<(), HostError> {
    set_option_in(OptionTarget::Server, name, value)
}

/// Get an option from a specific scope (inherits along the option tree).
pub fn get_option_in(target: OptionTarget, name: &str) -> Result<String, HostError> {
    let v = host_call(
        "get_option",
        json!({ "scope": target.to_json(), "name": name }),
    )?;
    Ok(v.get("value").and_then(Value::as_str).unwrap_or("").to_string())
}

/// Set a user (@-prefixed) option on a specific scope. Options published
/// here are visible to status-line formats as #{@name} (pane options win
/// for the active pane, then window, session, global).
pub fn set_option_in(
    target: OptionTarget,
    name: &str,
    value: &str,
) -> Result<(), HostError> {
    host_call(
        "set_option",
        json!({ "scope": target.to_json(), "name": name, "value": value }),
    )
    .map(|_| ())
}

/// Resolve a pane's live info: {id, window, width, height, active, dead,
/// cwd?, shell?}. Errors with E_NO_SUCH_OBJECT once the pane is gone.
pub fn resolve_pane(pane: PaneId) -> Result<Value, HostError> {
    host_call("resolve", json!({ "kind": "pane", "id": pane.0 }))
}

/// This instance's identity: {plugin, scope: {type, id?}, generation}.
pub fn self_info() -> Result<Value, HostError> {
    host_call("self", json!({}))
}

/// Show a status-line message on all attached clients (and the message log).
pub fn display_message(msg: &str) -> Result<(), HostError> {
    host_call("display_message", json!({ "message": msg })).map(|_| ())
}

pub fn log(msg: &str) {
    runtime::log(1, msg);
}

// ---- async API ----

#[derive(Debug, Clone, Deserialize)]
pub struct JobOutput {
    /// Exit status, or the signal number if `signalled`.
    pub status: i32,
    #[serde(default)]
    pub signalled: bool,
    /// Combined captured output.
    pub output: String,
}

/// Run a shell command; resolves with its output when it exits.
pub async fn run_job(cmd: &str, cwd: Option<&str>) -> Result<JobOutput, HostError> {
    let mut params = json!({ "cmd": cmd });
    if let Some(c) = cwd {
        params["cwd"] = c.into();
    }
    let v = host_request("run_job", params).await?;
    serde_json::from_value(v)
        .map_err(|e| host_err(ErrorCode::Host, format!("bad job output: {e}")))
}

/// Run a tmux command string through the command queue.
pub async fn run_command(command: &str) -> Result<(), HostError> {
    host_request("run_command", json!({ "command": command }))
        .await
        .map(|_| ())
}

/// Sleep for `ms` milliseconds (host timer).
pub async fn sleep_ms(ms: u64) -> Result<(), HostError> {
    host_request("timer_start", json!({ "ms": ms })).await.map(|_| ())
}
