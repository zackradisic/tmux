//! Services: plugin-to-plugin RPC and topics, on one server and across
//! bridge peers.
//!
//! A provider instance registers `(plugin, method)`; a caller names a
//! target `plugin` or `plugin@server`. Every delivery is queued on the
//! EVENTS cell and runs at the next drain, never re-entrantly (an import
//! must not call into another instance). The host moves payload bytes and
//! never parses them.
//!
//! Ids: a caller gets a token (tokens.rs) that its completions name. A
//! call arriving from a peer gets a local call id from a separate range,
//! so `service_reply(call)` finds it in the same map. Frames carry the
//! CALLER's token; each host translates at its edge.
//!
//! Mirrors modes.rs and tokens.rs: thread-local cells, so registration can
//! happen during dispatch while the instance is checked out.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tmux_plugin_abi::{
    service_fields, service_flags, ErrorCode, EventHeader, EventScope,
    FieldWriter, KeyRef, LOCAL_SERVER,
};

use crate::abi::{err, HostError};
use crate::bridge;
use crate::intern;
use crate::registry::ScopeId;
use crate::state::{Delivery, EVENTS};

/// A call the remote never answers fails after this long.
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);

/// First local call id for calls that arrive from a peer, well clear of
/// the token range (tokens count up from 1).
const REMOTE_CALL_BASE: u64 = 1 << 40;

/// A call for a plugin that is loaded but has not registered the method
/// yet (its init is queued, as right after a push) waits this long for
/// the registration before it fails.
pub const PENDING_DEADLINE: Duration = Duration::from_secs(10);

/// One instance: the owner of a registration, a subscription or a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub plugin: String,
    pub scope: ScopeId,
    pub generation: u64,
}

/// Who answers a call.
#[derive(Debug, Clone)]
enum Callee {
    Local(Owner),
    Remote { peer: u32 },
}

/// Who asked.
#[derive(Debug, Clone)]
enum Origin {
    /// A local instance; `token` is what its completions name.
    Local { token: u64, caller: Owner },
    /// A peer; `call_id` is the caller's token on that peer.
    Remote { peer: u32, call_id: u64 },
}

#[derive(Debug, Clone)]
struct Call {
    origin: Origin,
    callee: Callee,
    pages: u32,
    deadline: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Subscriber {
    Local(Owner),
    Peer(u32),
}

/// A call waiting for its plugin to register the method.
#[derive(Debug, Clone)]
struct Pending {
    origin: Origin,
    plugin: String,
    method: String,
    payload: Vec<u8>,
    from_plugin: String,
    deadline: Instant,
}

#[derive(Default)]
struct Services {
    /// (plugin, method) -> the instance that registered it.
    registry: HashMap<(String, String), Owner>,
    /// Open calls by local call id.
    calls: HashMap<u64, Call>,
    /// (server, plugin, topic) -> subscribers. `server` is "local" for
    /// topics emitted here and a peer name for topics emitted there.
    subs: HashMap<(String, String, String), Vec<Subscriber>>,
    /// Host-stamped sequence per (plugin, topic) emitted here.
    seq: HashMap<(String, String), u64>,
    /// Calls held until their method is registered (see PENDING_DEADLINE).
    pending: Vec<Pending>,
    next_remote_call: u64,
}

thread_local! {
    static SERVICES: RefCell<Services> = RefCell::new(Services {
        next_remote_call: REMOTE_CALL_BASE,
        ..Services::default()
    });
}

fn push(delivery: Delivery) {
    EVENTS.with(|e| e.borrow_mut().deliveries.push_back(delivery));
}

/// Split `plugin@server` into (plugin, server); a bare name means local,
/// and an empty plugin part (`@server`) means the caller's own plugin as
/// loaded, which the caller fills in: a plugin does not know the name it
/// was loaded under.
pub fn parse_target(target: &str, own: &str) -> Result<(String, String), HostError> {
    let (plugin, server) = match target.split_once('@') {
        Some((p, s)) => (p, s),
        None => (target, LOCAL_SERVER),
    };
    let plugin = if plugin.is_empty() { own } else { plugin };
    if plugin.is_empty() || server.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty service target"));
    }
    Ok((plugin.to_string(), server.to_string()))
}

/// Build a `service-request` event: `call` FIRST (fixed offset for raw
/// readers), then method, from_server, from_plugin, payload.
fn request_event(
    call: u64,
    method: &str,
    from_server: &str,
    from_plugin: &str,
    payload: &[u8],
) -> Vec<u8> {
    let header = EventHeader {
        event_id: intern::intern("service-request"),
        seq: 0,
        scope: EventScope::default(),
    };
    let mut buf = Vec::with_capacity(64 + payload.len());
    header.write(&mut buf);
    let mut w = FieldWriter::new();
    w.i64(KeyRef::Id(intern::intern(service_fields::CALL)), call as i64);
    w.str(KeyRef::Id(intern::intern(service_fields::METHOD)), method);
    w.str(KeyRef::Id(intern::intern(service_fields::FROM_SERVER)), from_server);
    w.str(KeyRef::Id(intern::intern(service_fields::FROM_PLUGIN)), from_plugin);
    w.bytes(KeyRef::Id(intern::intern(service_fields::PAYLOAD)), payload);
    buf.extend_from_slice(&w.finish());
    buf
}

/// Build a `service-event` event.
fn topic_event(
    plugin: &str,
    topic: &str,
    seq: u64,
    server: &str,
    payload: &[u8],
) -> Vec<u8> {
    let header = EventHeader {
        event_id: intern::intern("service-event"),
        seq: 0,
        scope: EventScope::default(),
    };
    let mut buf = Vec::with_capacity(64 + payload.len());
    header.write(&mut buf);
    let mut w = FieldWriter::new();
    w.str(KeyRef::Id(intern::intern(service_fields::PLUGIN)), plugin);
    w.str(KeyRef::Id(intern::intern(service_fields::TOPIC)), topic);
    w.i64(KeyRef::Id(intern::intern(service_fields::SEQ)), seq as i64);
    w.str(KeyRef::Id(intern::intern(service_fields::SERVER)), server);
    w.bytes(KeyRef::Id(intern::intern(service_fields::PAYLOAD)), payload);
    buf.extend_from_slice(&w.finish());
    buf
}

/// Fail a local caller's token with an error completion.
fn fail_token(token: u64, code: ErrorCode, message: &str) {
    push(Delivery::AsyncComplete {
        token,
        err: code.as_num(),
        v0: 0,
        v1: 0,
        data: message.as_bytes().to_vec(),
    });
}

// ---------------------------------------------------------------------------
// Registration and local calls.
// ---------------------------------------------------------------------------

/// Register `method` for the calling instance's plugin. A later
/// registration by another instance of the same plugin takes over.
pub fn register(owner: &Owner, method: &str) -> Result<(), HostError> {
    if method.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty method name"));
    }
    let ready: Vec<Pending> = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        s.registry
            .insert((owner.plugin.clone(), method.to_string()), owner.clone());
        let (ready, rest): (Vec<Pending>, Vec<Pending>) = s
            .pending
            .drain(..)
            .partition(|p| p.plugin == owner.plugin && p.method == method);
        s.pending = rest;
        ready
    });
    for p in ready {
        open_local_call(p.origin, owner.clone(), &p.plugin, &p.method, &p.from_plugin, &p.payload);
    }
    Ok(())
}

fn owner_of(plugin: &str, method: &str) -> Option<Owner> {
    SERVICES.with(|s| {
        s.borrow().registry.get(&(plugin.to_string(), method.to_string())).cloned()
    })
}

/// Is the plugin loaded and running here? Its init may still be queued,
/// so a call for it waits rather than fails.
fn plugin_loading(plugin: &str) -> bool {
    crate::state::REGISTRY.with(|r| r.borrow().is_running(plugin))
}

/// Open a call from `origin` on a local owner and deliver the request.
fn open_local_call(
    origin: Origin,
    owner: Owner,
    plugin: &str,
    method: &str,
    from_plugin: &str,
    payload: &[u8],
) {
    let (call_id, from_server) = match &origin {
        Origin::Local { token, .. } => (*token, LOCAL_SERVER.to_string()),
        Origin::Remote { peer, .. } => {
            let id = SERVICES.with(|s| {
                let mut s = s.borrow_mut();
                s.next_remote_call += 1;
                s.next_remote_call
            });
            (id, bridge::peer_name(*peer).unwrap_or_else(|| "?".to_string()))
        }
    };
    let _ = plugin;
    SERVICES.with(|s| {
        s.borrow_mut().calls.insert(
            call_id,
            Call {
                origin,
                callee: Callee::Local(owner.clone()),
                pages: 0,
                deadline: Instant::now() + CALL_DEADLINE,
            },
        );
    });
    let bytes = request_event(call_id, method, &from_server, from_plugin, payload);
    push(Delivery::ServiceRequest { owner, bytes });
}

/// Hold a call until the method is registered, or fail it when the plugin
/// is not even loaded.
fn defer_or_fail(
    origin: Origin,
    plugin: &str,
    method: &str,
    from_plugin: &str,
    payload: &[u8],
) -> Result<(), HostError> {
    let message = format!("no provider for {plugin}.{method}");
    if !plugin_loading(plugin) {
        return Err(err(ErrorCode::NoSuchObject, message));
    }
    SERVICES.with(|s| {
        s.borrow_mut().pending.push(Pending {
            origin,
            plugin: plugin.to_string(),
            method: method.to_string(),
            payload: payload.to_vec(),
            from_plugin: from_plugin.to_string(),
            deadline: Instant::now() + PENDING_DEADLINE,
        });
    });
    Ok(())
}

/// Which plugins have registered at least one method here, for `hello`.
pub fn providers() -> Vec<String> {
    SERVICES.with(|s| {
        let mut names: Vec<String> =
            s.borrow().registry.keys().map(|(p, _)| p.clone()).collect();
        names.sort();
        names.dedup();
        names
    })
}

/// Start a call from a local instance. `token` was allocated for the
/// caller; the completion (or pages) will name it.
pub fn call(
    caller: &Owner,
    token: u64,
    plugin: &str,
    server: &str,
    method: &str,
    payload: Vec<u8>,
) -> Result<(), HostError> {
    if server == LOCAL_SERVER {
        let origin = Origin::Local { token, caller: caller.clone() };
        return match owner_of(plugin, method) {
            Some(owner) => {
                open_local_call(origin, owner, plugin, method, &caller.plugin, &payload);
                Ok(())
            }
            None => defer_or_fail(origin, plugin, method, &caller.plugin, &payload),
        };
    }

    let Some(peer) = bridge::peer_by_name(server) else {
        crate::hostlog::debug(&caller.plugin, &format!("call {method} on {server}: no such server"));
        return Err(err(ErrorCode::Unreachable, format!("no server {server}")));
    };
    if !bridge::peer_is_up(peer) {
        crate::hostlog::debug(&caller.plugin, &format!("call {method} on {server}: not connected"));
        return Err(err(
            ErrorCode::Unreachable,
            format!("server {server} is not connected"),
        ));
    }
    if !bridge::peer_accepts(peer, plugin) {
        return Err(err(ErrorCode::Version, bridge::version_message(peer, plugin)));
    }
    SERVICES.with(|s| {
        s.borrow_mut().calls.insert(
            token,
            Call {
                origin: Origin::Local { token, caller: caller.clone() },
                callee: Callee::Remote { peer },
                pages: 0,
                deadline: Instant::now() + CALL_DEADLINE,
            },
        );
    });
    crate::hostlog::debug(&caller.plugin, &format!("call {plugin}.{method} on {server} (peer {peer}, token {token})"));
    if let Err(e) = bridge::send_call(peer, token, plugin, method, &payload) {
        SERVICES.with(|s| s.borrow_mut().calls.remove(&token));
        crate::hostlog::debug(&caller.plugin, &format!("call {method} on {server}: send failed: {e}"));
        return Err(err(ErrorCode::Unreachable, e));
    }
    Ok(())
}

/// The provider answered one page. `flags`: MORE keeps the call open,
/// ERROR makes the payload the message.
pub fn reply(
    replier: &Owner,
    call_id: u64,
    payload: Vec<u8>,
    flags: u32,
) -> Result<(), HostError> {
    let call = SERVICES.with(|s| s.borrow().calls.get(&call_id).cloned());
    let Some(call) = call else {
        return Err(err(ErrorCode::NoSuchObject, format!("no open call {call_id}")));
    };
    match &call.callee {
        Callee::Local(owner) if owner == replier => {}
        _ => {
            return Err(err(
                ErrorCode::NoSuchObject,
                format!("call {call_id} is not this instance's to answer"),
            ))
        }
    }
    let error = flags & service_flags::ERROR != 0;
    let more = !error && flags & service_flags::MORE != 0;
    match &call.origin {
        Origin::Local { token, .. } => {
            if error {
                let message = String::from_utf8_lossy(&payload).into_owned();
                fail_token(*token, ErrorCode::Host, &message);
            } else {
                push(Delivery::ServicePage {
                    token: *token,
                    page: call.pages,
                    flags: flags & service_flags::MORE,
                    data: payload,
                });
            }
        }
        Origin::Remote { peer, call_id: remote_id } => {
            let _ = bridge::send_reply(*peer, *remote_id, call.pages, flags, &payload);
        }
    }
    SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        if more {
            if let Some(c) = s.calls.get_mut(&call_id) {
                c.pages += 1;
                c.deadline = Instant::now() + CALL_DEADLINE;
            }
        } else {
            s.calls.remove(&call_id);
        }
    });
    Ok(())
}

/// The caller gave up. The token is taken so a late reply is dropped.
pub fn cancel(caller: &Owner, token: u64) -> Result<(), HostError> {
    let call = SERVICES.with(|s| s.borrow_mut().calls.remove(&token));
    let Some(call) = call else {
        crate::tokens::take(token);
        return Ok(());
    };
    match &call.origin {
        Origin::Local { caller: c, .. } if c == caller => {}
        _ => {
            // Not this instance's call: put it back untouched.
            SERVICES.with(|s| {
                s.borrow_mut().calls.insert(token, call.clone());
            });
            return Err(err(ErrorCode::NoSuchObject, "not this instance's call"));
        }
    }
    if let Callee::Remote { peer } = call.callee {
        let _ = bridge::send_cancel(peer, token);
    }
    crate::tokens::take(token);
    Ok(())
}

// ---------------------------------------------------------------------------
// Topics.
// ---------------------------------------------------------------------------

/// Emit a topic event for the calling instance's plugin.
pub fn emit(emitter: &Owner, topic: &str, payload: Vec<u8>) -> Result<(), HostError> {
    if topic.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty topic"));
    }
    let plugin = emitter.plugin.clone();
    let (seq, subs) = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        let seq = s.seq.entry((plugin.clone(), topic.to_string())).or_insert(0);
        *seq += 1;
        let seq = *seq;
        let subs = s
            .subs
            .get(&(LOCAL_SERVER.to_string(), plugin.clone(), topic.to_string()))
            .cloned()
            .unwrap_or_default();
        (seq, subs)
    });
    for sub in subs {
        match sub {
            Subscriber::Local(target) => {
                let bytes = topic_event(&plugin, topic, seq, LOCAL_SERVER, &payload);
                push(Delivery::ServiceEvent { target, bytes });
            }
            Subscriber::Peer(peer) => {
                if bridge::peer_accepts(peer, &plugin) {
                    let _ = bridge::send_event(peer, &plugin, topic, seq, &payload);
                }
            }
        }
    }
    Ok(())
}

/// Subscribe the calling instance to `plugin@server`'s `topic`.
pub fn subscribe(
    sub: &Owner,
    plugin: &str,
    server: &str,
    topic: &str,
) -> Result<(), HostError> {
    if topic.is_empty() {
        return Err(err(ErrorCode::BadRequest, "empty topic"));
    }
    let key = (server.to_string(), plugin.to_string(), topic.to_string());
    let peer = if server == LOCAL_SERVER {
        None
    } else {
        let Some(peer) = bridge::peer_by_name(server) else {
            return Err(err(ErrorCode::Unreachable, format!("no server {server}")));
        };
        Some(peer)
    };
    SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        let list = s.subs.entry(key).or_default();
        let me = Subscriber::Local(sub.clone());
        if !list.contains(&me) {
            list.push(me);
        }
    });
    if let Some(peer) = peer {
        // Best effort: a down peer gets the subscription again on link-up.
        let _ = bridge::send_subscribe(peer, plugin, topic);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frames from peers.
// ---------------------------------------------------------------------------

/// A peer calls one of our providers.
pub fn incoming_call(
    peer: u32,
    remote_call_id: u64,
    plugin: &str,
    method: &str,
    payload: &[u8],
) {
    crate::hostlog::debug(plugin, &format!("incoming call {method} from peer {peer} (call {remote_call_id})"));
    if !bridge::peer_accepts(peer, plugin) {
        let _ = bridge::send_reply(
            peer,
            remote_call_id,
            0,
            service_flags::ERROR,
            bridge::version_message(peer, plugin).as_bytes(),
        );
        return;
    }
    // The gate is one-directional. A call from an inbound peer (someone who
    // linked to us) is always allowed: they ssh'd in, the OS already trusts
    // that user. A call from a link this side made (remote -> initiator) is
    // gated: the method must be one the plugin serves to remote peers, and
    // the user must have allowed the (server, plugin) pair.
    if bridge::peer_is_initiator(peer) {
        let server = bridge::peer_name(peer).unwrap_or_default();
        match crate::peers::check_remote_call(&server, plugin, method) {
            Ok(()) => {}
            Err(crate::peers::Refusal::NotServed) => {
                crate::hostlog::info(
                    plugin,
                    &format!(
                        "{server} called {plugin}.{method}, not served to                          remote peers"
                    ),
                );
                let _ = bridge::send_reply(
                    peer,
                    remote_call_id,
                    0,
                    service_flags::ERROR,
                    format!("{plugin}.{method} is not served to remote peers")
                        .as_bytes(),
                );
                return;
            }
            Err(crate::peers::Refusal::NotGranted(msg)) => {
                let _ = bridge::send_reply(
                    peer,
                    remote_call_id,
                    0,
                    service_flags::ERROR,
                    msg.as_bytes(),
                );
                return;
            }
        }
    }
    let origin = Origin::Remote { peer, call_id: remote_call_id };
    match owner_of(plugin, method) {
        Some(owner) => open_local_call(origin, owner, plugin, method, plugin, payload),
        None => {
            if let Err(e) = defer_or_fail(origin, plugin, method, plugin, payload) {
                let _ = bridge::send_reply(
                    peer,
                    remote_call_id,
                    0,
                    service_flags::ERROR,
                    e.message.as_bytes(),
                );
            }
        }
    }
}

/// A peer answered a call one of our instances made.
pub fn incoming_reply(peer: u32, token: u64, page: u32, flags: u32, payload: &[u8]) {
    let call = SERVICES.with(|s| s.borrow().calls.get(&token).cloned());
    let Some(call) = call else {
        crate::hostlog::debug("services", &format!("reply from peer {peer} for unknown call {token}"));
        return;
    };
    match call.callee {
        Callee::Remote { peer: p } if p == peer => {}
        _ => {
            crate::hostlog::debug("services", &format!("reply from peer {peer} for call {token} made to another peer"));
            return;
        }
    }
    crate::hostlog::debug("services", &format!("reply from peer {peer} for call {token}: flags {flags}, {} bytes", payload.len()));
    let error = flags & service_flags::ERROR != 0;
    let more = !error && flags & service_flags::MORE != 0;
    if error {
        let message = String::from_utf8_lossy(payload).into_owned();
        fail_token(token, ErrorCode::Host, &message);
    } else {
        push(Delivery::ServicePage {
            token,
            page,
            flags: flags & service_flags::MORE,
            data: payload.to_vec(),
        });
    }
    SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        if more {
            if let Some(c) = s.calls.get_mut(&token) {
                c.deadline = Instant::now() + CALL_DEADLINE;
            }
        } else {
            s.calls.remove(&token);
        }
    });
}

/// A peer gave up on a call to one of our providers.
pub fn incoming_cancel(peer: u32, remote_call_id: u64) {
    SERVICES.with(|s| {
        s.borrow_mut().calls.retain(|_, c| {
            !matches!(c.origin, Origin::Remote { peer: p, call_id }
                if p == peer && call_id == remote_call_id)
        });
    });
}

/// A peer wants `plugin`'s `topic` as emitted here.
pub fn incoming_subscribe(peer: u32, plugin: &str, topic: &str) {
    SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        let list = s
            .subs
            .entry((LOCAL_SERVER.to_string(), plugin.to_string(), topic.to_string()))
            .or_default();
        if !list.contains(&Subscriber::Peer(peer)) {
            list.push(Subscriber::Peer(peer));
        }
    });
}

pub fn incoming_unsubscribe(peer: u32, plugin: &str, topic: &str) {
    SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(list) = s.subs.get_mut(&(
            LOCAL_SERVER.to_string(),
            plugin.to_string(),
            topic.to_string(),
        )) {
            list.retain(|x| *x != Subscriber::Peer(peer));
        }
    });
}

/// A topic event emitted on a peer, for our subscribers to it.
pub fn incoming_event(peer: u32, plugin: &str, topic: &str, seq: u64, payload: &[u8]) {
    let Some(server) = bridge::peer_name(peer) else { return };
    if !bridge::peer_accepts(peer, plugin) {
        return;
    }
    let subs = SERVICES.with(|s| {
        s.borrow()
            .subs
            .get(&(server.clone(), plugin.to_string(), topic.to_string()))
            .cloned()
            .unwrap_or_default()
    });
    for sub in subs {
        if let Subscriber::Local(target) = sub {
            let bytes = topic_event(plugin, topic, seq, &server, payload);
            push(Delivery::ServiceEvent { target, bytes });
        }
    }
}

// ---------------------------------------------------------------------------
// Lifecycle.
// ---------------------------------------------------------------------------

/// A peer came up: send it every subscription our instances hold to its
/// topics, so a reconnect restores the flow.
pub fn peer_up(peer: u32) {
    let Some(server) = bridge::peer_name(peer) else { return };
    let wanted: Vec<(String, String)> = SERVICES.with(|s| {
        s.borrow()
            .subs
            .iter()
            .filter(|((srv, _, _), list)| *srv == server && !list.is_empty())
            .map(|((_, plugin, topic), _)| (plugin.clone(), topic.clone()))
            .collect()
    });
    for (plugin, topic) in wanted {
        let _ = bridge::send_subscribe(peer, &plugin, &topic);
    }
}

/// A peer went down: fail every call waiting on it, drop calls it made
/// and forget it as a subscriber. Local subscriptions to its topics stay
/// and are sent again on peer_up.
pub fn peer_down(peer: u32) {
    let doomed: Vec<(u64, Call)> = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        let ids: Vec<u64> = s
            .calls
            .iter()
            .filter(|(_, c)| {
                matches!(c.callee, Callee::Remote { peer: p } if p == peer)
                    || matches!(c.origin, Origin::Remote { peer: p, .. } if p == peer)
            })
            .map(|(id, _)| *id)
            .collect();
        let out = ids
            .into_iter()
            .filter_map(|id| s.calls.remove(&id).map(|c| (id, c)))
            .collect();
        for list in s.subs.values_mut() {
            list.retain(|x| *x != Subscriber::Peer(peer));
        }
        s.pending
            .retain(|p| !matches!(p.origin, Origin::Remote { peer: q, .. } if q == peer));
        out
    });
    for (_, call) in doomed {
        if let Origin::Local { token, .. } = call.origin {
            fail_token(token, ErrorCode::Unreachable, "remote link went down");
        }
    }
}

/// An instance is going away: drop its registrations and subscriptions,
/// fail the calls it was answering, cancel the calls it was waiting on.
pub fn purge_instance(plugin: &str, scope: ScopeId, generation: u64) {
    let me = Owner { plugin: plugin.to_string(), scope, generation };
    let doomed: Vec<(u64, Call)> = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        s.registry.retain(|_, o| *o != me);
        for list in s.subs.values_mut() {
            list.retain(|x| *x != Subscriber::Local(me.clone()));
        }
        s.pending
            .retain(|p| !matches!(&p.origin, Origin::Local { caller, .. } if *caller == me));
        let ids: Vec<u64> = s
            .calls
            .iter()
            .filter(|(_, c)| {
                matches!(&c.callee, Callee::Local(o) if *o == me)
                    || matches!(&c.origin, Origin::Local { caller, .. } if *caller == me)
            })
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| s.calls.remove(&id).map(|c| (id, c)))
            .collect()
    });
    for (_, call) in doomed {
        match (&call.origin, &call.callee) {
            // We were answering: tell the caller the provider is gone.
            (Origin::Local { token, .. }, Callee::Local(_)) => {
                fail_token(*token, ErrorCode::NoSuchObject, "provider unloaded");
            }
            (Origin::Remote { peer, call_id }, Callee::Local(_)) => {
                let _ = bridge::send_reply(
                    *peer,
                    *call_id,
                    0,
                    service_flags::ERROR,
                    b"provider unloaded",
                );
            }
            // We were asking a peer: tell it to stop.
            (Origin::Local { token, .. }, Callee::Remote { peer }) => {
                let _ = bridge::send_cancel(*peer, *token);
            }
            _ => {}
        }
    }
}

/// Fail calls past their deadline. Called from every drain; cheap when
/// nothing is open.
pub fn sweep_deadlines() {
    let now = Instant::now();
    let stale: Vec<Pending> = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        if s.pending.is_empty() {
            return Vec::new();
        }
        let (stale, rest): (Vec<Pending>, Vec<Pending>) =
            s.pending.drain(..).partition(|p| p.deadline <= now);
        s.pending = rest;
        stale
    });
    for p in stale {
        let message = format!("no provider for {}.{}", p.plugin, p.method);
        match p.origin {
            Origin::Local { token, .. } => {
                fail_token(token, ErrorCode::NoSuchObject, &message);
            }
            Origin::Remote { peer, call_id } => {
                let _ = bridge::send_reply(
                    peer,
                    call_id,
                    0,
                    service_flags::ERROR,
                    message.as_bytes(),
                );
            }
        }
    }
    let late: Vec<(u64, Call)> = SERVICES.with(|s| {
        let mut s = s.borrow_mut();
        if s.calls.is_empty() {
            return Vec::new();
        }
        let ids: Vec<u64> = s
            .calls
            .iter()
            .filter(|(_, c)| c.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| s.calls.remove(&id).map(|c| (id, c)))
            .collect()
    });
    for (_, call) in late {
        match (&call.origin, &call.callee) {
            (Origin::Local { token, .. }, callee) => {
                if let Callee::Remote { peer } = callee {
                    let _ = bridge::send_cancel(*peer, *token);
                }
                fail_token(*token, ErrorCode::Timeout, "service call timed out");
            }
            (Origin::Remote { peer, call_id }, _) => {
                let _ = bridge::send_reply(
                    *peer,
                    *call_id,
                    0,
                    service_flags::ERROR,
                    b"service call timed out",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parsing() {
        assert_eq!(
            parse_target("agents", "me").unwrap(),
            ("agents".to_string(), "local".to_string())
        );
        assert_eq!(
            parse_target("agents@devbox", "me").unwrap(),
            ("agents".to_string(), "devbox".to_string())
        );
        assert_eq!(
            parse_target("@devbox", "me").unwrap(),
            ("me".to_string(), "devbox".to_string())
        );
        assert!(parse_target("@devbox", "").is_err());
        assert!(parse_target("agents@", "me").is_err());
    }
}
