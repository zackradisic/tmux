//! Services: plugin-to-plugin calls and topics, on this server and on
//! linked servers.
//!
//! A plugin runs as a *provider* on every server (it sees that server's
//! panes, processes and files) and as a *view* on the local server (it
//! merges what the providers report and owns the UI). The two halves talk
//! through services: the provider registers methods with [`register`] and
//! publishes topics with [`emit`]; the view calls them with [`call`] and
//! [`call_all`] and follows topics with [`subscribe`]. Payloads are raw
//! bytes; the `_json` helpers use serde_json.
//!
//! A target is `plugin` (this server) or `plugin@server`, where `server`
//! is a name from [`servers`]. A plugin in role `Both` reaches its own
//! provider half by calling its methods directly; the host path exists
//! for other plugins and other servers.

use std::collections::{BTreeMap, HashMap};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tmux_plugin_abi::{
    parse_list, service_fields, service_flags, ErrorCode, HostError,
    ServerInfo, LOCAL_SERVER,
};

use crate::api::{call_owned, check, host_err, start_async, wire_err};
use crate::event::Event;
use crate::executor::Completion;
use crate::runtime::raw;
use crate::strings::AsTmuxStr;

// ---------------------------------------------------------------------------
// Servers.
// ---------------------------------------------------------------------------

/// The local server and every linked remote server, with their link
/// state. Names are what service targets use after `@`.
pub fn servers() -> Result<Vec<ServerInfo>, HostError> {
    let buf = call_owned(|out| unsafe { raw::servers(out) })?;
    parse_list(&buf, ServerInfo::parse).map_err(|_| wire_err())
}

/// The local server's record.
pub fn local_server() -> ServerInfo {
    ServerInfo { id: 0, name: LOCAL_SERVER.into(), up: true, local: true }
}

// ---------------------------------------------------------------------------
// Provider side.
// ---------------------------------------------------------------------------

/// Register a method on this instance's plugin. Calls arrive as
/// [`crate::Plugin::on_service_request`]. Needs `service-serve`.
pub fn register(method: &str) -> Result<(), HostError> {
    let m = method.to_tmux();
    let (mp, ml) = m.parts();
    check(unsafe { raw::service_register(mp, ml) })
}

/// Publish a payload on a topic of this instance's plugin. Subscribers on
/// this server and on linked servers receive it with a host-stamped
/// sequence number. Needs `service-serve`.
pub fn emit(topic: &str, payload: &[u8]) -> Result<(), HostError> {
    let t = topic.to_tmux();
    let (tp, tl) = t.parts();
    check(unsafe {
        raw::service_emit(tp, tl, payload.as_ptr() as i32, payload.len() as i32)
    })
}

/// [`emit`] with a serde_json payload.
pub fn emit_json<T: Serialize>(topic: &str, value: &T) -> Result<(), HostError> {
    let bytes = serde_json::to_vec(value).map_err(|e| HostError {
        code: ErrorCode::BadRequest,
        message: format!("encode: {e}"),
    })?;
    emit(topic, &bytes)
}

/// A call to a method this instance registered.
#[derive(Debug, Clone)]
pub struct ServiceRequest {
    /// The call id to answer with.
    pub call: i64,
    /// The server the caller runs on ("local" or a link name).
    pub server: String,
    /// The calling plugin.
    pub plugin: String,
    pub method: String,
    pub payload: Vec<u8>,
}

impl ServiceRequest {
    /// Decode a `service-request` event; None for any other event.
    pub fn from_event(event: &Event) -> Option<ServiceRequest> {
        if !event.is("service-request") {
            return None;
        }
        Some(ServiceRequest {
            call: event.get_i64(service_fields::CALL)?,
            server: event.get_str(service_fields::FROM_SERVER)?.to_string(),
            plugin: event.get_str(service_fields::FROM_PLUGIN)?.to_string(),
            method: event.get_str(service_fields::METHOD)?.to_string(),
            payload: event.get_bytes(service_fields::PAYLOAD)?.to_vec(),
        })
    }

    /// The payload as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HostError> {
        serde_json::from_slice(&self.payload).map_err(|e| HostError {
            code: ErrorCode::BadRequest,
            message: format!("decode: {e}"),
        })
    }

    fn send(&self, payload: &[u8], flags: u32) -> Result<(), HostError> {
        check(unsafe {
            raw::service_reply(
                self.call,
                payload.as_ptr() as i32,
                payload.len() as i32,
                flags as i32,
            )
        })
    }

    /// Answer with one payload and close the call.
    pub fn reply(&self, payload: &[u8]) -> Result<(), HostError> {
        self.send(payload, 0)
    }

    /// Answer with a serde_json payload.
    pub fn reply_json<T: Serialize>(&self, value: &T) -> Result<(), HostError> {
        let bytes = serde_json::to_vec(value).map_err(|e| HostError {
            code: ErrorCode::BadRequest,
            message: format!("encode: {e}"),
        })?;
        self.reply(&bytes)
    }

    /// Answer with one page; `more` keeps the call open for the next.
    pub fn reply_page(&self, payload: &[u8], more: bool) -> Result<(), HostError> {
        self.send(payload, if more { service_flags::MORE } else { 0 })
    }

    /// Fail the call; the caller sees `E_HOST` with this message.
    pub fn fail(&self, message: &str) -> Result<(), HostError> {
        self.send(message.as_bytes(), service_flags::ERROR)
    }
}

// ---------------------------------------------------------------------------
// Caller side.
// ---------------------------------------------------------------------------

fn start_call(target: &str, method: &str, payload: &[u8]) -> Result<u64, HostError> {
    let t = target.to_tmux();
    let m = method.to_tmux();
    let (tp, tl) = t.parts();
    let (mp, ml) = m.parts();
    let token = unsafe {
        raw::service_call(
            tp,
            tl,
            mp,
            ml,
            payload.as_ptr() as i32,
            payload.len() as i32,
        )
    };
    if token <= 0 {
        return Err(host_err(token as i32));
    }
    Ok(token as u64)
}

/// Call `method` on `target` (`plugin` or `plugin@server`) and collect
/// every reply page into one buffer. Needs `service-call`. Fails with
/// `E_UNREACHABLE` when the server is not linked, `E_NO_SUCH_OBJECT` when
/// no provider registered the method, `E_TIMEOUT` after 30 s without a
/// reply, and `E_HOST` with the provider's message on an error reply.
pub async fn call(target: &str, method: &str, payload: &[u8]) -> Result<Vec<u8>, HostError> {
    let mut stream = call_stream(target, method, payload)?;
    let mut out = Vec::new();
    while let Some(page) = stream.next().await {
        out.extend_from_slice(&page?);
    }
    Ok(out)
}

/// [`call`] with serde_json request and reply payloads.
pub async fn call_json<Req: Serialize, Resp: DeserializeOwned>(
    target: &str,
    method: &str,
    request: &Req,
) -> Result<Resp, HostError> {
    let bytes = serde_json::to_vec(request).map_err(|e| HostError {
        code: ErrorCode::BadRequest,
        message: format!("encode: {e}"),
    })?;
    let reply = call(target, method, &bytes).await?;
    serde_json::from_slice(&reply).map_err(|e| HostError {
        code: ErrorCode::Host,
        message: format!("decode reply: {e}"),
    })
}

/// The pages of one call, in order. Dropping the stream before the last
/// page cancels the call.
pub struct PageStream {
    token: u64,
    done: bool,
}

/// Start a call and read its pages one at a time.
pub fn call_stream(target: &str, method: &str, payload: &[u8]) -> Result<PageStream, HostError> {
    let token = start_call(target, method, payload)?;
    Ok(PageStream { token, done: false })
}

impl PageStream {
    /// The next page, or None after the last one.
    pub async fn next(&mut self) -> Option<Result<Vec<u8>, HostError>> {
        if self.done {
            return None;
        }
        // Each page is one completion on the same token; the host keeps
        // the token alive while MORE is set.
        let fut = match start_async(self.token as i64) {
            Ok(f) => f,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        match fut.await {
            Ok(Completion { v1, data, .. }) => {
                if v1 as u32 & service_flags::MORE == 0 {
                    self.done = true;
                }
                Some(Ok(data))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

impl Drop for PageStream {
    fn drop(&mut self) {
        if !self.done {
            unsafe { raw::service_cancel(self.token as i64) };
        }
    }
}

/// Call `method` on `plugin` on every connected server (the local one
/// first) and collect the results per server. A server whose call fails
/// keeps its error in the list; the others still answer.
pub async fn call_all(
    plugin: &str,
    method: &str,
    payload: &[u8],
) -> Vec<(ServerInfo, Result<Vec<u8>, HostError>)> {
    let list = servers().unwrap_or_else(|_| vec![local_server()]);
    let mut out = Vec::with_capacity(list.len());
    for server in list {
        if !server.up {
            continue;
        }
        let target = if server.local {
            plugin.to_string()
        } else {
            format!("{plugin}@{}", server.name)
        };
        let result = call(&target, method, payload).await;
        out.push((server, result));
    }
    out
}

/// Follow `topic` of `target` (`plugin` or `plugin@server`). Events
/// arrive as [`crate::Plugin::on_service_event`]. A subscription to a
/// remote server survives its link going down: the host re-sends it when
/// the link returns. Needs `service-call`.
pub fn subscribe(target: &str, topic: &str) -> Result<(), HostError> {
    let t = target.to_tmux();
    let p = topic.to_tmux();
    let (tp, tl) = t.parts();
    let (pp, pl) = p.parts();
    check(unsafe { raw::service_subscribe(tp, tl, pp, pl) })
}

/// A topic event from a provider.
#[derive(Debug, Clone)]
pub struct ServiceEvent {
    pub plugin: String,
    pub topic: String,
    /// Host-stamped, per (plugin, topic, server), starting at 1 and
    /// counting up by one. A gap means a lost event: resync.
    pub seq: u64,
    /// "local" or the link name the event came from.
    pub server: String,
    pub payload: Vec<u8>,
}

impl ServiceEvent {
    /// Decode a `service-event` event; None for any other event.
    pub fn from_event(event: &Event) -> Option<ServiceEvent> {
        if !event.is("service-event") {
            return None;
        }
        Some(ServiceEvent {
            plugin: event.get_str(service_fields::PLUGIN)?.to_string(),
            topic: event.get_str(service_fields::TOPIC)?.to_string(),
            seq: event.get_i64(service_fields::SEQ)?.max(0) as u64,
            server: event.get_str(service_fields::SERVER)?.to_string(),
            payload: event.get_bytes(service_fields::PAYLOAD)?.to_vec(),
        })
    }

    /// The payload as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HostError> {
        serde_json::from_slice(&self.payload).map_err(|e| HostError {
            code: ErrorCode::BadRequest,
            message: format!("decode: {e}"),
        })
    }
}

// ---------------------------------------------------------------------------
// Replica: a view's copy of what each provider reports.
// ---------------------------------------------------------------------------

/// One change to a replica, as a provider publishes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Delta<T> {
    /// Replace everything for the server.
    Full { rows: Vec<(String, T)> },
    Upsert { key: String, row: T },
    Remove { key: String },
}

#[derive(Debug, Clone)]
struct ServerRows<T> {
    rows: BTreeMap<String, T>,
    /// The last topic sequence applied; 0 before the first full load.
    seq: u64,
    /// When the rows last changed (host clock, ms).
    updated_ms: u64,
    /// Set by [`Replica::mark_stale`] (link down) until the next full load.
    stale: bool,
}

/// A per-server map of rows fed by a provider's `list`-style reply and
/// its `changed`-style topic events. Detects a skipped sequence number
/// so the view knows to fetch the full list again.
#[derive(Debug, Clone)]
pub struct Replica<T> {
    servers: HashMap<String, ServerRows<T>>,
}

impl<T> Default for Replica<T> {
    fn default() -> Self {
        Self { servers: HashMap::new() }
    }
}

impl<T> Replica<T> {
    pub fn new() -> Self {
        Self::default()
    }

    fn entry(&mut self, server: &str) -> &mut ServerRows<T> {
        self.servers.entry(server.to_string()).or_insert_with(|| ServerRows {
            rows: BTreeMap::new(),
            seq: 0,
            updated_ms: 0,
            stale: false,
        })
    }

    /// Replace a server's rows from a full list at sequence `seq` (the
    /// provider reports the sequence its list corresponds to).
    pub fn apply_full(&mut self, server: &str, seq: u64, rows: Vec<(String, T)>) {
        let now = crate::api::now_ms();
        let e = self.entry(server);
        e.rows = rows.into_iter().collect();
        e.seq = seq;
        e.updated_ms = now;
        e.stale = false;
    }

    /// Apply one topic event. Returns false when `seq` does not follow
    /// the last applied sequence, in which case nothing changed and the
    /// caller should fetch the full list again.
    pub fn apply(&mut self, server: &str, seq: u64, delta: Delta<T>) -> bool {
        let now = crate::api::now_ms();
        let e = self.entry(server);
        if let Delta::Full { rows } = delta {
            e.rows = rows.into_iter().collect();
            e.seq = seq;
            e.updated_ms = now;
            e.stale = false;
            return true;
        }
        if e.seq != 0 && seq != e.seq + 1 {
            return false;
        }
        if e.seq == 0 && seq != 1 {
            return false;
        }
        match delta {
            Delta::Upsert { key, row } => {
                e.rows.insert(key, row);
            }
            Delta::Remove { key } => {
                e.rows.remove(&key);
            }
            Delta::Full { .. } => unreachable!(),
        }
        e.seq = seq;
        e.updated_ms = now;
        true
    }

    /// The link to `server` went down: keep the rows, flag them.
    pub fn mark_stale(&mut self, server: &str) {
        if let Some(e) = self.servers.get_mut(server) {
            e.stale = true;
        }
    }

    pub fn is_stale(&self, server: &str) -> bool {
        self.servers.get(server).is_some_and(|e| e.stale)
    }

    /// Forget a server entirely.
    pub fn remove_server(&mut self, server: &str) {
        self.servers.remove(server);
    }

    /// Milliseconds since the server's rows last changed; None when the
    /// server was never loaded.
    pub fn age_ms(&self, server: &str) -> Option<u64> {
        let e = self.servers.get(server)?;
        if e.updated_ms == 0 {
            return None;
        }
        Some(crate::api::now_ms().saturating_sub(e.updated_ms))
    }

    /// The last sequence applied for a server (0 = never loaded).
    pub fn seq(&self, server: &str) -> u64 {
        self.servers.get(server).map_or(0, |e| e.seq)
    }

    /// Server names with rows, sorted, "local" first.
    pub fn servers(&self) -> Vec<String> {
        let mut v: Vec<String> = self.servers.keys().cloned().collect();
        v.sort_by(|a, b| {
            (a != LOCAL_SERVER).cmp(&(b != LOCAL_SERVER)).then_with(|| a.cmp(b))
        });
        v
    }

    /// Every row as (server, key, row), servers in [`Replica::servers`]
    /// order and keys sorted within a server.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &T)> + '_ {
        self.servers().into_iter().flat_map(move |name| {
            let e = &self.servers[&name];
            let name: &str = self.servers.get_key_value(&name).map(|(k, _)| k.as_str()).unwrap();
            e.rows.iter().map(move |(k, v)| (name, k.as_str(), v))
        })
    }

    /// Rows of one server.
    pub fn rows(&self, server: &str) -> impl Iterator<Item = (&str, &T)> + '_ {
        self.servers
            .get(server)
            .into_iter()
            .flat_map(|e| e.rows.iter().map(|(k, v)| (k.as_str(), v)))
    }

    pub fn get(&self, server: &str, key: &str) -> Option<&T> {
        self.servers.get(server)?.rows.get(key)
    }

    pub fn len(&self) -> usize {
        self.servers.values().map(|e| e.rows.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_tracks_sequence_gaps() {
        let mut r: Replica<u32> = Replica::new();
        r.apply_full("local", 3, vec![("a".into(), 1)]);
        assert!(r.apply("local", 4, Delta::Upsert { key: "b".into(), row: 2 }));
        assert!(!r.apply("local", 6, Delta::Remove { key: "a".into() }));
        assert_eq!(r.len(), 2);
        assert!(r.apply("local", 5, Delta::Remove { key: "a".into() }));
        assert_eq!(r.get("local", "a"), None);
        assert!(r.apply("devbox", 1, Delta::Upsert { key: "x".into(), row: 9 }));
        assert_eq!(r.servers(), vec!["local".to_string(), "devbox".to_string()]);
        let all: Vec<(&str, &str, &u32)> = r.iter().collect();
        assert_eq!(all, vec![("local", "b", &2), ("devbox", "x", &9)]);
        r.mark_stale("devbox");
        assert!(r.is_stale("devbox"));
        assert!(r.apply("devbox", 2, Delta::Full { rows: vec![] }));
        assert!(!r.is_stale("devbox"));
    }
}
