//! Typed wrappers over the host_call / host_request ABI.
//!
//! Sync functions return immediately; async functions return futures
//! resolved by the SDK executor when the host delivers the completion.

use serde::Deserialize;
use serde_json::{json, Value};
use tmux_plugin_abi::{ErrorCode, HostError, HostResponse};

use crate::executor::{HostFuture, HostResult};
use crate::ids::{ModeId, PaneId, SessionId, WindowId};
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

/// Resolve a pane's live info: {id, window, width, height, active, floating,
/// dead, cwd?, shell?}. Errors with E_NO_SUCH_OBJECT once the pane is gone.
pub fn resolve_pane(pane: PaneId) -> Result<Value, HostError> {
    host_call("resolve", json!({ "kind": "pane", "id": pane.0 }))
}

/// Resolve a window's live info: {id, name, width, height, sessions, panes,
/// active_pane?}. Errors with E_NO_SUCH_OBJECT once it is gone.
pub fn resolve_window(window: WindowId) -> Result<Value, HostError> {
    host_call("resolve", json!({ "kind": "window", "id": window.0 }))
}

/// Resolve a session's live info: {id, name, attached, current_window?,
/// windows}. Errors with E_NO_SUCH_OBJECT once it is gone.
pub fn resolve_session(session: SessionId) -> Result<Value, HostError> {
    host_call("resolve", json!({ "kind": "session", "id": session.0 }))
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

// ---- UI modes (capability: mode) ----

/// Options for [`mode_open`]. Size is in cells; `x`/`y` are the top-left
/// offset within the window (`None` = centered). `window` defaults to the
/// instance's own window (pane/window scope) or the session's current
/// window (session scope); server-scoped instances must set it.
#[derive(Debug, Clone, Default)]
pub struct ModeOpts {
    pub window: Option<WindowId>,
    pub width: u32,
    pub height: u32,
    pub x: Option<u32>,
    pub y: Option<u32>,
    pub title: Option<String>,
}

/// A retained preview rect for [`mode_preview`]: a live mirror of `pane`'s
/// grid drawn at (x, y), size (w, h), inside the mode screen.
#[derive(Debug, Clone, Copy)]
pub struct PreviewRect {
    pub pane: PaneId,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Open a UI mode: a freshly spawned empty floating pane owned by this
/// instance. Render with [`mode_write`]; `mode-key` / `mode-resize` /
/// `mode-closed` events arrive through `Plugin::on_event` with the mode id
/// in `event.data["mode"]`.
pub fn mode_open(opts: &ModeOpts) -> Result<ModeId, HostError> {
    let mut params = json!({ "width": opts.width, "height": opts.height });
    if let Some(w) = opts.window {
        params["window"] = w.0.into();
    }
    if let Some(x) = opts.x {
        params["x"] = x.into();
    }
    if let Some(y) = opts.y {
        params["y"] = y.into();
    }
    if let Some(t) = &opts.title {
        params["title"] = t.as_str().into();
    }
    let v = host_call("mode_open", params)?;
    v.get("mode")
        .and_then(Value::as_u64)
        .map(ModeId)
        .ok_or_else(|| host_err(ErrorCode::Host, "mode_open returned no id"))
}

/// Send ANSI bytes to a mode's screen (parsed server-side: cursor
/// addressing, SGR, clears, ... - anything a terminal accepts). At most
/// 256 KiB per call; a full-screen redraw is idiomatic.
pub fn mode_write(mode: ModeId, data: &[u8]) -> Result<(), HostError> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    host_call("mode_write", json!({ "mode": mode.0, "data_b64": b64 }))
        .map(|_| ())
}

/// Set (or clear, with `None`) a mode's retained preview rect. The host
/// redraws it from the source pane's live grid every ~500ms until cleared
/// or the source pane dies.
pub fn mode_preview(
    mode: ModeId,
    rect: Option<&PreviewRect>,
) -> Result<(), HostError> {
    let params = match rect {
        Some(r) => json!({
            "mode": mode.0, "pane": r.pane.0,
            "x": r.x, "y": r.y, "w": r.w, "h": r.h,
        }),
        None => json!({ "mode": mode.0 }),
    };
    host_call("mode_preview", params).map(|_| ())
}

/// Close a mode. The floating pane is torn down at the next safe point;
/// a final `mode-closed` event (reason "closed") follows.
pub fn mode_close(mode: ModeId) -> Result<(), HostError> {
    host_call("mode_close", json!({ "mode": mode.0 })).map(|_| ())
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
