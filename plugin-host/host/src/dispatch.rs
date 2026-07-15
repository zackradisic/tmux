//! host_call method dispatch: the single choke point every guest request
//! passes through. Capability and scope checks (M7) live here, before any
//! vtable pointer is touched.
//!
//! Runs while the guest instance is checked out of the registry, so it may
//! only use the vtable and the caller's StoreData - never the registry.
//! Vtable calls may re-enter pgh_notify (enqueue-only); that touches the
//! EVENTS cell only, which is safe here.

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::raw::c_char;

use serde::Deserialize;
use serde_json::{json, Value};
use tmux_plugin_abi::{ErrorCode, HostError, HostRequest};

use crate::abi::StoreData;
use crate::ffi::{PGH_OBJ_CLIENT, PGH_OBJ_PANE, PGH_OBJ_SESSION, PGH_OBJ_WINDOW};

/// Subscription changes requested during a dispatch; applied to StoreData
/// right after (dispatch itself only has a shared borrow).
enum SubDelta {
    Add(Vec<String>),
    Remove(Vec<String>),
}

thread_local! {
    static PENDING_SUBS: RefCell<Vec<SubDelta>> = const { RefCell::new(Vec::new()) };
}

pub fn apply_pending_subscriptions(data: &mut StoreData) {
    PENDING_SUBS.with(|p| {
        for delta in p.borrow_mut().drain(..) {
            match delta {
                SubDelta::Add(events) => {
                    for e in events {
                        data.subscriptions.insert(e);
                    }
                }
                SubDelta::Remove(events) => {
                    for e in events {
                        data.subscriptions.remove(&e);
                    }
                }
            }
        }
    });
}

fn err(code: ErrorCode, message: impl Into<String>) -> HostError {
    HostError { code, message: message.into(), data: Value::Null }
}

fn cstring(s: &str) -> Result<std::ffi::CString, HostError> {
    std::ffi::CString::new(s)
        .map_err(|_| err(ErrorCode::BadRequest, "embedded NUL in string"))
}

/// Option scope selector: {"type": "server"} (default) or
/// {"type": "session"|"window"|"pane", "id": n}.
#[derive(Deserialize)]
struct OptionScope {
    #[serde(rename = "type", default = "default_scope_type")]
    scope_type: String,
    #[serde(default)]
    id: Option<u32>,
}

fn default_scope_type() -> String {
    "server".into()
}

impl Default for OptionScope {
    fn default() -> Self {
        Self { scope_type: default_scope_type(), id: None }
    }
}

impl OptionScope {
    fn to_kind(&self) -> Result<(i32, u32), HostError> {
        if self.scope_type == "server" {
            return Ok((-1, 0));
        }
        let id = self.id.ok_or_else(|| {
            err(ErrorCode::BadRequest, "scope requires an id")
        })?;
        Ok((kind_from_str(&self.scope_type)?, id))
    }
}

/// Collect sink: appends vtable string output into a Vec<u8>.
unsafe extern "C" fn collect_sink(ctx: *mut c_void, ptr: *const c_char, len: usize) {
    let buf = &mut *(ctx as *mut Vec<u8>);
    buf.extend_from_slice(std::slice::from_raw_parts(ptr as *const u8, len));
}

fn vtable() -> Result<&'static crate::ffi::pgh_host_vtable, HostError> {
    crate::vtable().ok_or_else(|| err(ErrorCode::Host, "host vtable unavailable"))
}

fn list_objects(kind: i32) -> Result<Value, HostError> {
    let vt = vtable()?;
    let mut buf: Vec<u8> = Vec::new();
    unsafe {
        (vt.list_objects)(kind, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    serde_json::from_str(&String::from_utf8_lossy(&buf))
        .map_err(|e| err(ErrorCode::Host, format!("bad vtable JSON: {e}")))
}

fn resolve_object(kind: i32, id: u32) -> Result<Value, HostError> {
    let vt = vtable()?;
    let mut buf: Vec<u8> = Vec::new();
    let rc = unsafe {
        (vt.resolve_object)(kind, id, collect_sink, &mut buf as *mut Vec<u8> as *mut c_void)
    };
    if rc != 0 {
        return Err(err(ErrorCode::NoSuchObject, format!("no such object id {id}")));
    }
    serde_json::from_str(&String::from_utf8_lossy(&buf))
        .map_err(|e| err(ErrorCode::Host, format!("bad vtable JSON: {e}")))
}

fn kind_from_str(kind: &str) -> Result<i32, HostError> {
    match kind {
        "session" => Ok(PGH_OBJ_SESSION),
        "window" => Ok(PGH_OBJ_WINDOW),
        "pane" => Ok(PGH_OBJ_PANE),
        "client" => Ok(PGH_OBJ_CLIENT),
        other => Err(err(ErrorCode::BadRequest, format!("bad object kind {other:?}"))),
    }
}

fn params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, HostError> {
    serde_json::from_value(value)
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad params: {e}")))
}

/// The capability a method needs (single choke point; 0 = none).
fn required_cap(method: &str) -> u32 {
    use crate::caps::*;
    match method {
        "subscribe" | "unsubscribe" | "self" | "resolve" | "list_sessions"
        | "list_windows" | "list_panes" | "list_clients" | "get_option" => READ_STATE,
        "set_option" => WRITE_OPTIONS,
        "send_keys" => SEND_KEYS,
        "capture_pane" => CAPTURE_PANE,
        "display_message" => DISPLAY_MESSAGE,
        "timer_start" | "timer_cancel" => TIMERS,
        "run_job" => RUN_PROCESS,
        "run_command" => RUN_COMMAND,
        _ => 0,
    }
}

fn check_cap(data: &StoreData, method: &str) -> Result<(), HostError> {
    let needed = required_cap(method);
    if needed != 0 && !data.caps.has(needed) {
        return Err(err(
            ErrorCode::CapDenied,
            format!(
                "capability {:?} not granted (method {method:?})",
                crate::caps::cap_name(needed)
            ),
        ));
    }
    Ok(())
}

/// Scope-implied targeting: may this instance touch pane `pane_id`?
/// Pane-scoped instances may touch only their own pane; window-scoped their
/// window's panes; session-scoped panes in windows linked to their session;
/// server-scoped (or CROSS_SCOPE) may touch anything.
fn check_pane_target(data: &StoreData, pane_id: u32) -> Result<(), HostError> {
    use crate::registry::ScopeId;

    if data.caps.has(crate::caps::CROSS_SCOPE) {
        return Ok(());
    }
    let out_of_scope = |msg: String| err(ErrorCode::OutOfScope, msg);

    match data.scope {
        ScopeId::Server => Ok(()),
        ScopeId::Pane(own) => {
            if own == pane_id {
                Ok(())
            } else {
                Err(out_of_scope(format!(
                    "pane-scoped instance %{own} may not target pane %{pane_id}"
                )))
            }
        }
        ScopeId::Window(own) => {
            let info = resolve_object(PGH_OBJ_PANE, pane_id)?;
            let window = info.get("window").and_then(Value::as_u64);
            if window == Some(u64::from(own)) {
                Ok(())
            } else {
                Err(out_of_scope(format!(
                    "window-scoped instance @{own} may not target pane %{pane_id}"
                )))
            }
        }
        ScopeId::Session(own) => {
            let info = resolve_object(PGH_OBJ_PANE, pane_id)?;
            let window = info
                .get("window")
                .and_then(Value::as_u64)
                .ok_or_else(|| err(ErrorCode::Host, "pane without window"))?;
            let winfo = resolve_object(PGH_OBJ_WINDOW, window as u32)?;
            let linked = winfo
                .get("sessions")
                .and_then(Value::as_array)
                .is_some_and(|a| {
                    a.iter().any(|v| v.as_u64() == Some(u64::from(own)))
                });
            if linked {
                Ok(())
            } else {
                Err(out_of_scope(format!(
                    "session-scoped instance ${own} may not target pane %{pane_id}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::EffectiveCaps;
    use crate::registry::ScopeId;

    /// xorshift64: deterministic, dependency-free byte soup.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// dispatch/dispatch_async must never panic and must return structured
    /// errors on garbage: raw bytes, truncated JSON, wrong param types.
    /// Every panic here would poison the host in production.
    #[test]
    fn dispatch_survives_garbage() {
        let data = StoreData::for_tests(
            ScopeId::Pane(1),
            EffectiveCaps { flags: u32::MAX, argv0_allow: vec![] },
        );

        let corpus: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"{".to_vec(),
            b"null".to_vec(),
            b"[]".to_vec(),
            b"\xff\xfe\x00garbage".to_vec(),
            br#"{"method":"send_keys"}"#.to_vec(),
            br#"{"method":"send_keys","params":{"pane":"nope"}}"#.to_vec(),
            br#"{"method":"send_keys","params":{"pane":-1,"keys":5}}"#.to_vec(),
            br#"{"method":"capture_pane","params":{"pane":1,"start":2147483647,"end":-2147483648}}"#.to_vec(),
            br#"{"method":"get_option","params":{"scope":{"type":"pane"},"name":"x"}}"#.to_vec(),
            br#"{"method":"subscribe","params":{"events":[null]}}"#.to_vec(),
            br#"{"method":"run_job","params":{"cmd":"x y"}}"#.to_vec(),
            br#"{"method":"timer_start","params":{"ms":18446744073709551615}}"#.to_vec(),
            br#"{"method":"","params":{}}"#.to_vec(),
        ];
        for input in &corpus {
            let _ = dispatch(&data, input);
            let _ = dispatch_async(&data, input);
        }

        // Seeded random soup: raw bytes and JSON-shaped strings.
        let methods = [
            "subscribe", "send_keys", "capture_pane", "get_option",
            "set_option", "display_message", "run_job", "run_command",
            "timer_start", "timer_cancel", "resolve", "bogus",
        ];
        let mut rng = Rng(0x74_6d_75_78_32);
        for _ in 0..5000 {
            let mut bytes = Vec::new();
            let len = (rng.next() % 64) as usize;
            for _ in 0..len {
                bytes.push((rng.next() & 0xff) as u8);
            }
            let _ = dispatch(&data, &bytes);
            let _ = dispatch_async(&data, &bytes);

            let m = methods[(rng.next() as usize) % methods.len()];
            let v = rng.next();
            let shaped = format!(
                r#"{{"method":"{m}","params":{{"pane":{},"keys":"{}","name":"@x","value":"y","ms":{},"token":{},"events":["e{}"],"cmd":"true","command":"list-panes","start":{},"end":{}}}}}"#,
                v % 100,
                v,
                v,
                v % 7,
                v % 3,
                (v as i64) % 5000 - 2500,
                (v as i64) % 5000,
            );
            let _ = dispatch(&data, shaped.as_bytes());
            let _ = dispatch_async(&data, shaped.as_bytes());
        }
        // Applying whatever subscriptions accumulated must not panic
        // either.
        let mut data = data;
        apply_pending_subscriptions(&mut data);
    }
}

/// Dispatch one host_call request. `data` identifies the calling instance.
pub fn dispatch(data: &StoreData, request: &[u8]) -> Result<Value, HostError> {
    let text = String::from_utf8_lossy(request);
    let req: HostRequest = serde_json::from_str(&text)
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad request JSON: {e}")))?;

    check_cap(data, &req.method)?;

    match req.method.as_str() {
        "subscribe" | "unsubscribe" => {
            #[derive(Deserialize)]
            struct SubParams {
                events: Vec<String>,
            }
            let p: SubParams = params(req.params)?;
            let delta = if req.method == "subscribe" {
                SubDelta::Add(p.events)
            } else {
                SubDelta::Remove(p.events)
            };
            PENDING_SUBS.with(|q| q.borrow_mut().push(delta));
            Ok(json!({}))
        }
        "list_sessions" => list_objects(PGH_OBJ_SESSION),
        "list_windows" => list_objects(PGH_OBJ_WINDOW),
        "list_panes" => list_objects(PGH_OBJ_PANE),
        "list_clients" => list_objects(PGH_OBJ_CLIENT),
        "send_keys" => {
            #[derive(Deserialize)]
            struct SendKeysParams {
                pane: u32,
                keys: String,
                #[serde(default)]
                literal: bool,
            }
            let p: SendKeysParams = params(req.params)?;
            check_pane_target(data, p.pane)?;
            let vt = vtable()?;
            let keys = cstring(&p.keys)?;
            let rc = unsafe {
                (vt.send_keys)(p.pane, keys.as_ptr(), p.literal.into())
            };
            match rc {
                0 => Ok(json!({})),
                -2 => Err(err(
                    ErrorCode::BadRequest,
                    format!("bad key name {:?}", p.keys),
                )),
                _ => Err(err(
                    ErrorCode::NoSuchObject,
                    format!("no such pane %{}", p.pane),
                )),
            }
        }
        "capture_pane" => {
            #[derive(Deserialize)]
            struct CaptureParams {
                pane: u32,
                #[serde(default)]
                start: i32,
                #[serde(default = "default_capture_end")]
                end: i32,
                #[serde(default)]
                escapes: bool,
            }
            fn default_capture_end() -> i32 {
                i32::MAX
            }
            const MAX_CAPTURE_BYTES: usize = 256 * 1024;
            let p: CaptureParams = params(req.params)?;
            check_pane_target(data, p.pane)?;
            let vt = vtable()?;
            let mut buf: Vec<u8> = Vec::new();
            let rc = unsafe {
                (vt.capture_pane)(
                    p.pane,
                    p.start,
                    p.end,
                    p.escapes.into(),
                    collect_sink,
                    &mut buf as *mut Vec<u8> as *mut c_void,
                )
            };
            if rc != 0 {
                return Err(err(
                    ErrorCode::NoSuchObject,
                    format!("no such pane %{}", p.pane),
                ));
            }
            if buf.len() > MAX_CAPTURE_BYTES {
                return Err(err(
                    ErrorCode::Limit,
                    format!(
                        "capture exceeds {MAX_CAPTURE_BYTES} bytes; page with start/end"
                    ),
                ));
            }
            Ok(json!({ "text": String::from_utf8_lossy(&buf) }))
        }
        "get_option" => {
            #[derive(Deserialize)]
            struct GetOptionParams {
                #[serde(default)]
                scope: OptionScope,
                name: String,
            }
            let p: GetOptionParams = params(req.params)?;
            let (kind, id) = p.scope.to_kind()?;
            let vt = vtable()?;
            let name = cstring(&p.name)?;
            let mut buf: Vec<u8> = Vec::new();
            let rc = unsafe {
                (vt.get_option)(
                    kind,
                    id,
                    name.as_ptr(),
                    collect_sink,
                    &mut buf as *mut Vec<u8> as *mut c_void,
                )
            };
            match rc {
                0 => Ok(json!({ "value": String::from_utf8_lossy(&buf) })),
                -2 => Err(err(
                    ErrorCode::NoSuchObject,
                    format!("no such option {:?}", p.name),
                )),
                _ => Err(err(ErrorCode::NoSuchObject, "no such target")),
            }
        }
        "set_option" => {
            #[derive(Deserialize)]
            struct SetOptionParams {
                #[serde(default)]
                scope: OptionScope,
                name: String,
                value: String,
            }
            let p: SetOptionParams = params(req.params)?;
            let (kind, id) = p.scope.to_kind()?;
            let vt = vtable()?;
            let name = cstring(&p.name)?;
            let value = cstring(&p.value)?;
            let rc = unsafe {
                (vt.set_option)(kind, id, name.as_ptr(), value.as_ptr())
            };
            match rc {
                0 => Ok(json!({})),
                -2 => Err(err(
                    ErrorCode::Unsupported,
                    "only @-prefixed user options can be set directly in v1",
                )),
                _ => Err(err(ErrorCode::NoSuchObject, "no such target")),
            }
        }
        "display_message" => {
            #[derive(Deserialize)]
            struct DisplayParams {
                #[serde(default)]
                client: Option<u32>,
                message: String,
            }
            let p: DisplayParams = params(req.params)?;
            let vt = vtable()?;
            let plugin = cstring(&data.plugin)?;
            let msg = cstring(&p.message)?;
            let client = p.client.map_or(-1i32, |c| c as i32);
            let rc = unsafe {
                (vt.display_message)(client, plugin.as_ptr(), msg.as_ptr())
            };
            if rc != 0 {
                return Err(err(
                    ErrorCode::NoSuchObject,
                    "no such attached client",
                ));
            }
            Ok(json!({}))
        }
        "resolve" => {
            #[derive(Deserialize)]
            struct ResolveParams {
                kind: String,
                id: u32,
            }
            let p: ResolveParams = params(req.params)?;
            resolve_object(kind_from_str(&p.kind)?, p.id)
        }
        "self" => {
            // The instance's own identity: scope type + id.
            Ok(json!({
                "plugin": data.plugin,
                "scope": data.scope.to_json(),
                "generation": data.generation,
            }))
        }
        "timer_cancel" => {
            #[derive(Deserialize)]
            struct CancelParams {
                token: u64,
            }
            let p: CancelParams = params(req.params)?;
            let vt = vtable()?;
            // Taking the token also guarantees a raced, already-queued
            // completion is dropped at drain time.
            match crate::tokens::take(p.token) {
                Some(pending) => {
                    if let Some(id) = pending.timer_id {
                        unsafe { (vt.timer_cancel)(id) };
                    }
                    Ok(json!({}))
                }
                None => Ok(json!({ "already_completed": true })),
            }
        }
        other => Err(err(
            ErrorCode::UnknownMethod,
            format!("unknown method {other:?}"),
        )),
    }
}

/// Dispatch one host_request (async) call: start the operation and return
/// the token whose completion will arrive via pgh_on_async_complete.
pub fn dispatch_async(data: &StoreData, request: &[u8]) -> Result<u64, HostError> {
    let text = String::from_utf8_lossy(request);
    let req: HostRequest = serde_json::from_str(&text)
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad request JSON: {e}")))?;

    check_cap(data, &req.method)?;

    let token =
        crate::tokens::allocate(&data.plugin, data.scope, data.generation);
    let result: Result<(), HostError> = (|| {
        match req.method.as_str() {
            "run_job" => {
                #[derive(Deserialize)]
                struct RunJobParams {
                    cmd: String,
                    #[serde(default)]
                    cwd: Option<String>,
                }
                let p: RunJobParams = params(req.params)?;
                // Advisory argv0 allowlist (run_job is a shell string in
                // v1): check the first token's basename.
                if !data.caps.argv0_allow.is_empty() {
                    let argv0 = p
                        .cmd
                        .split_whitespace()
                        .next()
                        .map(|t| t.rsplit('/').next().unwrap_or(t))
                        .unwrap_or("");
                    if !data.caps.argv0_allow.iter().any(|a| a == argv0) {
                        return Err(err(
                            ErrorCode::CapDenied,
                            format!("command {argv0:?} not in argv0 allowlist"),
                        ));
                    }
                }
                let vt = vtable()?;
                let cmd = cstring(&p.cmd)?;
                let cwd = match &p.cwd {
                    Some(c) => Some(cstring(c)?),
                    None => None,
                };
                let rc = unsafe {
                    (vt.run_job)(
                        cmd.as_ptr(),
                        cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                        token,
                    )
                };
                if rc != 0 {
                    return Err(err(ErrorCode::Host, "failed to start job"));
                }
                Ok(())
            }
            "run_command" => {
                #[derive(Deserialize)]
                struct RunCommandParams {
                    command: String,
                }
                let p: RunCommandParams = params(req.params)?;
                let vt = vtable()?;
                let cmd = cstring(&p.command)?;
                let rc = unsafe { (vt.run_command)(cmd.as_ptr(), token) };
                if rc != 0 {
                    return Err(err(ErrorCode::Host, "failed to queue command"));
                }
                Ok(())
            }
            "timer_start" => {
                #[derive(Deserialize)]
                struct TimerParams {
                    ms: u64,
                }
                let p: TimerParams = params(req.params)?;
                let vt = vtable()?;
                let id = unsafe { (vt.timer_start)(p.ms, token) };
                crate::tokens::set_timer_id(token, id);
                Ok(())
            }
            other => Err(err(
                ErrorCode::UnknownMethod,
                format!("unknown async method {other:?}"),
            )),
        }
    })();

    match result {
        Ok(()) => Ok(token),
        Err(e) => {
            crate::tokens::discard(token);
            Err(e)
        }
    }
}
