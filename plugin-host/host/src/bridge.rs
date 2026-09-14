//! The bridge: frames between two plugin hosts over a remote link.
//!
//! Version 1 rides the control mode connection a `remote-attach` link
//! already holds. The C side moves opaque bytes both ways (a
//! `plugin-bridge` command outbound, a `%bridge` line inbound) and this
//! module does framing, compression, the hello exchange and plugin push.
//!
//! A peer is one connection: on the initiating server it is the remote
//! link (peer id with PGH_PEER_LINK set); on the answering server it is
//! the control client that carries the link. Both ends run this same
//! code; the PGH_PEER_LINK bit says which side pushes plugins.
//!
//! Frame := magic "PGB1", u8 kind, u8 flags (bit 0: body is zstd), u32
//! raw body length, body. The body is a field block with inline (named)
//! keys, because interned ids differ between servers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tmux_plugin_abi::{
    Cursor, EventHeader, EventScope, FieldReader, FieldWriter, KeyRef,
    LoadDescriptor, Role, ValueRef, ABI_VERSION,
};

use crate::ffi::PGH_PEER_LINK;
use crate::hostlog;
use crate::intern;
use crate::registry::PluginState;
use crate::services;
use crate::state::REGISTRY;

const MAGIC: &[u8; 4] = b"PGB1";
const FLAG_ZSTD: u8 = 1;
/// Bodies above this many bytes are compressed (level 3). Base64 on the
/// wire costs a third; compression pays it back on anything that matters.
pub const COMPRESS_ABOVE: usize = 4096;
/// A pushed plugin stays loaded this long after its peer went down, so a
/// flapping link does not thrash.
pub const PUSH_GRACE: Duration = Duration::from_secs(10 * 60);
/// Server option naming the capabilities a pushed plugin may get.
pub const REMOTE_CAPS_OPTION: &str = "plugin-remote-caps";
/// Where pushed plugins land: `<data>/tmux/plugin-cache/<peer>/<name>.wasm`.
const CACHE_DIR: &str = "tmux/plugin-cache";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Hello = 1,
    Push = 2,
    Call = 3,
    Reply = 4,
    Cancel = 5,
    Subscribe = 6,
    Unsubscribe = 7,
    Event = 8,
    Ping = 9,
    Pong = 10,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            1 => Kind::Hello,
            2 => Kind::Push,
            3 => Kind::Call,
            4 => Kind::Reply,
            5 => Kind::Cancel,
            6 => Kind::Subscribe,
            7 => Kind::Unsubscribe,
            8 => Kind::Event,
            9 => Kind::Ping,
            10 => Kind::Pong,
            _ => return None,
        })
    }
}

/// One provider a host announces in `hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloPlugin {
    pub name: String,
    pub role: Role,
    #[serde(default)]
    pub service_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello {
        abi: i32,
        host: String,
        plugins: Vec<HelloPlugin>,
        /// Capability names the sender grants pushed plugins.
        cap_ceiling: Vec<String>,
    },
    Push {
        /// serde_json of the LoadDescriptor (role forced to Provider,
        /// path and caps left for the receiver).
        descriptor: String,
        sidecar: Option<String>,
        /// blake3 of `wasm`, hex.
        hash: String,
        wasm: Vec<u8>,
    },
    Call { call_id: u64, plugin: String, method: String, payload: Vec<u8> },
    Reply { call_id: u64, page: u32, flags: u32, payload: Vec<u8> },
    Cancel { call_id: u64 },
    Subscribe { plugin: String, topic: String },
    Unsubscribe { plugin: String, topic: String },
    Event { plugin: String, topic: String, seq: u64, payload: Vec<u8> },
    Ping { nonce: u64 },
    Pong { nonce: u64 },
}

// ---------------------------------------------------------------------------
// Codec.
// ---------------------------------------------------------------------------

fn body_of(frame: &Frame) -> (Kind, Vec<u8>) {
    let mut w = FieldWriter::new();
    let kind = match frame {
        Frame::Hello { abi, host, plugins, cap_ceiling } => {
            w.i64(KeyRef::Name("abi"), i64::from(*abi));
            w.str(KeyRef::Name("host"), host);
            w.json(
                KeyRef::Name("plugins"),
                &serde_json::to_string(plugins).unwrap_or_else(|_| "[]".into()),
            );
            w.json(
                KeyRef::Name("caps"),
                &serde_json::to_string(cap_ceiling).unwrap_or_else(|_| "[]".into()),
            );
            Kind::Hello
        }
        Frame::Push { descriptor, sidecar, hash, wasm } => {
            w.str(KeyRef::Name("descriptor"), descriptor);
            if let Some(s) = sidecar {
                w.str(KeyRef::Name("sidecar"), s);
            }
            w.str(KeyRef::Name("hash"), hash);
            w.bytes(KeyRef::Name("wasm"), wasm);
            Kind::Push
        }
        Frame::Call { call_id, plugin, method, payload } => {
            w.i64(KeyRef::Name("call"), *call_id as i64);
            w.str(KeyRef::Name("plugin"), plugin);
            w.str(KeyRef::Name("method"), method);
            w.bytes(KeyRef::Name("payload"), payload);
            Kind::Call
        }
        Frame::Reply { call_id, page, flags, payload } => {
            w.i64(KeyRef::Name("call"), *call_id as i64);
            w.i64(KeyRef::Name("page"), i64::from(*page));
            w.i64(KeyRef::Name("flags"), i64::from(*flags));
            w.bytes(KeyRef::Name("payload"), payload);
            Kind::Reply
        }
        Frame::Cancel { call_id } => {
            w.i64(KeyRef::Name("call"), *call_id as i64);
            Kind::Cancel
        }
        Frame::Subscribe { plugin, topic } => {
            w.str(KeyRef::Name("plugin"), plugin);
            w.str(KeyRef::Name("topic"), topic);
            Kind::Subscribe
        }
        Frame::Unsubscribe { plugin, topic } => {
            w.str(KeyRef::Name("plugin"), plugin);
            w.str(KeyRef::Name("topic"), topic);
            Kind::Unsubscribe
        }
        Frame::Event { plugin, topic, seq, payload } => {
            w.str(KeyRef::Name("plugin"), plugin);
            w.str(KeyRef::Name("topic"), topic);
            w.i64(KeyRef::Name("seq"), *seq as i64);
            w.bytes(KeyRef::Name("payload"), payload);
            Kind::Event
        }
        Frame::Ping { nonce } => {
            w.i64(KeyRef::Name("nonce"), *nonce as i64);
            Kind::Ping
        }
        Frame::Pong { nonce } => {
            w.i64(KeyRef::Name("nonce"), *nonce as i64);
            Kind::Pong
        }
    };
    (kind, w.finish())
}

/// Encode a frame: magic, kind, flags, raw length, body (zstd above the
/// threshold).
pub fn encode(frame: &Frame) -> Vec<u8> {
    let (kind, body) = body_of(frame);
    let mut out = Vec::with_capacity(10 + body.len());
    out.extend_from_slice(MAGIC);
    out.push(kind as u8);
    let compressed = if body.len() > COMPRESS_ABOVE {
        zstd::bulk::compress(&body, 3).ok()
    } else {
        None
    };
    match compressed {
        Some(c) if c.len() < body.len() => {
            out.push(FLAG_ZSTD);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&c);
        }
        _ => {
            out.push(0);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&body);
        }
    }
    out
}

struct Fields {
    map: HashMap<String, Vec<u8>>,
    ints: HashMap<String, i64>,
    strs: HashMap<String, String>,
}

impl Fields {
    fn parse(body: &[u8]) -> Result<Self, String> {
        let mut f = Fields {
            map: HashMap::new(),
            ints: HashMap::new(),
            strs: HashMap::new(),
        };
        let reader = FieldReader::new(body).map_err(|e| e.to_string())?;
        for field in reader {
            let (key, value) = field.map_err(|e| e.to_string())?;
            let KeyRef::Name(name) = key else { continue };
            match value {
                ValueRef::I64(v) => {
                    f.ints.insert(name.to_string(), v);
                }
                ValueRef::Str(s) | ValueRef::Json(s) => {
                    f.strs.insert(name.to_string(), s.to_string());
                }
                ValueRef::Bytes(b) => {
                    f.map.insert(name.to_string(), b.to_vec());
                }
                _ => {}
            }
        }
        Ok(f)
    }

    fn int(&self, k: &str) -> Result<i64, String> {
        self.ints.get(k).copied().ok_or_else(|| format!("missing {k}"))
    }

    fn str(&self, k: &str) -> Result<String, String> {
        self.strs.get(k).cloned().ok_or_else(|| format!("missing {k}"))
    }

    fn bytes(&self, k: &str) -> Vec<u8> {
        self.map.get(k).cloned().unwrap_or_default()
    }
}

/// Decode a frame.
pub fn decode(bytes: &[u8]) -> Result<Frame, String> {
    if bytes.len() < 10 || &bytes[0..4] != MAGIC {
        return Err("not a bridge frame".into());
    }
    let kind = Kind::from_u8(bytes[4]).ok_or_else(|| format!("bad kind {}", bytes[4]))?;
    let flags = bytes[5];
    let raw_len = u32::from_le_bytes(bytes[6..10].try_into().unwrap()) as usize;
    if raw_len > 64 * 1024 * 1024 {
        return Err("frame too large".into());
    }
    let body: Vec<u8> = if flags & FLAG_ZSTD != 0 {
        zstd::bulk::decompress(&bytes[10..], raw_len).map_err(|e| e.to_string())?
    } else {
        bytes[10..].to_vec()
    };
    if body.len() != raw_len {
        return Err("frame length mismatch".into());
    }
    let f = Fields::parse(&body)?;
    Ok(match kind {
        Kind::Hello => Frame::Hello {
            abi: f.int("abi")? as i32,
            host: f.str("host")?,
            plugins: serde_json::from_str(&f.str("plugins").unwrap_or_else(|_| "[]".into()))
                .unwrap_or_default(),
            cap_ceiling: serde_json::from_str(&f.str("caps").unwrap_or_else(|_| "[]".into()))
                .unwrap_or_default(),
        },
        Kind::Push => Frame::Push {
            descriptor: f.str("descriptor")?,
            sidecar: f.str("sidecar").ok(),
            hash: f.str("hash")?,
            wasm: f.bytes("wasm"),
        },
        Kind::Call => Frame::Call {
            call_id: f.int("call")? as u64,
            plugin: f.str("plugin")?,
            method: f.str("method")?,
            payload: f.bytes("payload"),
        },
        Kind::Reply => Frame::Reply {
            call_id: f.int("call")? as u64,
            page: f.int("page")? as u32,
            flags: f.int("flags")? as u32,
            payload: f.bytes("payload"),
        },
        Kind::Cancel => Frame::Cancel { call_id: f.int("call")? as u64 },
        Kind::Subscribe => Frame::Subscribe {
            plugin: f.str("plugin")?,
            topic: f.str("topic")?,
        },
        Kind::Unsubscribe => Frame::Unsubscribe {
            plugin: f.str("plugin")?,
            topic: f.str("topic")?,
        },
        Kind::Event => Frame::Event {
            plugin: f.str("plugin")?,
            topic: f.str("topic")?,
            seq: f.int("seq")? as u64,
            payload: f.bytes("payload"),
        },
        Kind::Ping => Frame::Ping { nonce: f.int("nonce")? as u64 },
        Kind::Pong => Frame::Pong { nonce: f.int("nonce")? as u64 },
    })
}

// ---------------------------------------------------------------------------
// Peers.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: u32,
    /// The host name: what the C link gave us for a link we made, or the
    /// `hello` host for a client that reached us.
    pub name: String,
    pub up: bool,
    /// We made this link (PGH_PEER_LINK); we push plugins to it.
    pub initiator: bool,
    pub hello: Option<(i32, Vec<HelloPlugin>)>,
    pub down_since: Option<Instant>,
}

thread_local! {
    static PEERS: RefCell<HashMap<u32, Peer>> = RefCell::new(HashMap::new());
}

pub fn peer_name(peer: u32) -> Option<String> {
    PEERS.with(|p| p.borrow().get(&peer).map(|x| x.name.clone()))
}

pub fn peer_is_up(peer: u32) -> bool {
    PEERS.with(|p| p.borrow().get(&peer).is_some_and(|x| x.up))
}

/// The peer with this name, an up one first.
pub fn peer_by_name(name: &str) -> Option<u32> {
    PEERS.with(|p| {
        let p = p.borrow();
        let mut best: Option<&Peer> = None;
        for x in p.values() {
            if x.name != name {
                continue;
            }
            if best.is_none_or(|b| !b.up && x.up) {
                best = Some(x);
            }
        }
        best.map(|x| x.id)
    })
}

/// Does the peer run plugins of its own? True when its hello listed at
/// least one provider and the peer is up. tmux uses it to leave a shadow
/// pane's notifications to the remote's plugins instead of firing them
/// twice.
pub fn peer_provides(peer: u32) -> bool {
    PEERS.with(|p| {
        p.borrow().get(&peer).is_some_and(|x| {
            x.up && x.hello.as_ref().is_some_and(|(_, plugins)| !plugins.is_empty())
        })
    })
}

/// Every peer, for the `servers` import.
pub fn peers() -> Vec<Peer> {
    PEERS.with(|p| {
        let mut v: Vec<Peer> = p.borrow().values().cloned().collect();
        v.sort_by_key(|x| x.id);
        v
    })
}

fn send(peer: u32, frame: &Frame) -> Result<(), String> {
    let Some(vt) = crate::vtable() else {
        return Err("host vtable unavailable".into());
    };
    if !peer_is_up(peer) {
        return Err("peer is down".into());
    }
    let bytes = encode(frame);
    let rc = unsafe { (vt.bridge_send)(peer, bytes.as_ptr(), bytes.len()) };
    if rc != 0 {
        return Err("bridge send failed".into());
    }
    Ok(())
}

pub fn send_call(
    peer: u32,
    call_id: u64,
    plugin: &str,
    method: &str,
    payload: &[u8],
) -> Result<(), String> {
    send(
        peer,
        &Frame::Call {
            call_id,
            plugin: plugin.to_string(),
            method: method.to_string(),
            payload: payload.to_vec(),
        },
    )
}

pub fn send_reply(
    peer: u32,
    call_id: u64,
    page: u32,
    flags: u32,
    payload: &[u8],
) -> Result<(), String> {
    send(peer, &Frame::Reply { call_id, page, flags, payload: payload.to_vec() })
}

pub fn send_cancel(peer: u32, call_id: u64) -> Result<(), String> {
    send(peer, &Frame::Cancel { call_id })
}

pub fn send_subscribe(peer: u32, plugin: &str, topic: &str) -> Result<(), String> {
    send(
        peer,
        &Frame::Subscribe { plugin: plugin.to_string(), topic: topic.to_string() },
    )
}

pub fn send_event(
    peer: u32,
    plugin: &str,
    topic: &str,
    seq: u64,
    payload: &[u8],
) -> Result<(), String> {
    send(
        peer,
        &Frame::Event {
            plugin: plugin.to_string(),
            topic: topic.to_string(),
            seq,
            payload: payload.to_vec(),
        },
    )
}

fn local_hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "localhost".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The capability names this server grants pushed plugins, from the
/// `plugin-remote-caps` server option (comma separated).
fn remote_caps() -> Vec<String> {
    let Some(vt) = crate::vtable() else { return Vec::new() };
    let name = std::ffi::CString::new(REMOTE_CAPS_OPTION).unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let rc = unsafe {
        (vt.get_option)(
            -1,
            0,
            name.as_ptr(),
            crate::abi::collect_sink,
            &mut buf as *mut Vec<u8> as *mut std::ffi::c_void,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    String::from_utf8_lossy(&buf)
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn hello_frame() -> Frame {
    let providers = services::providers();
    let plugins: Vec<HelloPlugin> = REGISTRY.with(|r| {
        let reg = r.borrow();
        let mut v: Vec<HelloPlugin> = reg
            .plugins
            .values()
            .filter(|d| d.state == PluginState::Running && d.role.provides())
            .map(|d| HelloPlugin {
                name: d.name.clone(),
                role: d.role,
                service_version: u32::from(providers.contains(&d.name)),
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    });
    Frame::Hello {
        abi: ABI_VERSION,
        host: local_hostname(),
        plugins,
        cap_ceiling: remote_caps(),
    }
}

/// Fire `link-up` / `link-down` to subscribed instances.
fn fire_link_event(name: &str, peer: &Peer, plugins: &[HelloPlugin]) {
    let header = EventHeader {
        event_id: intern::intern(name),
        seq: 0,
        scope: EventScope::default(),
    };
    let mut buf = Vec::new();
    header.write(&mut buf);
    let mut w = FieldWriter::new();
    w.str(KeyRef::Id(intern::intern("server")), &peer.name);
    w.i64(KeyRef::Id(intern::intern("id")), i64::from(peer.id));
    let names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
    w.json(
        KeyRef::Id(intern::intern("plugins")),
        &serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
    );
    buf.extend_from_slice(&w.finish());
    crate::events::enqueue_raw(buf);
}

/// A peer came up (name known for a link we made) or went down.
pub fn state(peer_id: u32, name: Option<String>, up: bool) {
    let initiator = peer_id & PGH_PEER_LINK != 0;
    if up {
        PEERS.with(|p| {
            let mut p = p.borrow_mut();
            let entry = p.entry(peer_id).or_insert_with(|| Peer {
                id: peer_id,
                name: name.clone().unwrap_or_else(|| format!("peer-{peer_id}")),
                up: false,
                initiator,
                hello: None,
                down_since: None,
            });
            if let Some(n) = name {
                entry.name = n;
            }
            entry.up = true;
            entry.down_since = None;
            entry.hello = None;
        });
        if initiator {
            if let Err(e) = send(peer_id, &hello_frame()) {
                hostlog::debug("bridge", &format!("hello to {peer_id}: {e}"));
            }
        }
        services::peer_up(peer_id);
        return;
    }
    let peer = PEERS.with(|p| {
        let mut p = p.borrow_mut();
        let Some(entry) = p.get_mut(&peer_id) else { return None };
        entry.up = false;
        entry.down_since = Some(Instant::now());
        Some(entry.clone())
    });
    services::peer_down(peer_id);
    if let Some(peer) = peer {
        hostlog::info("bridge", &format!("{} down", peer.name));
        fire_link_event("link-down", &peer, &[]);
    }
}

/// A frame arrived from a peer.
pub fn recv(peer_id: u32, bytes: &[u8]) {
    let initiator = peer_id & PGH_PEER_LINK != 0;
    PEERS.with(|p| {
        let mut p = p.borrow_mut();
        let entry = p.entry(peer_id).or_insert_with(|| Peer {
            id: peer_id,
            name: format!("peer-{peer_id}"),
            up: false,
            initiator,
            hello: None,
            down_since: None,
        });
        // A client that speaks to us is up by definition.
        if !initiator {
            entry.up = true;
            entry.down_since = None;
        }
    });
    let frame = match decode(bytes) {
        Ok(f) => f,
        Err(e) => {
            hostlog::warn("bridge", &format!("bad frame from {peer_id}: {e}"));
            return;
        }
    };
    match frame {
        Frame::Hello { abi, host, plugins, cap_ceiling } => {
            // A peer says hello again after it loaded a pushed plugin, so
            // its provider list stays current; only the first hello of a
            // connection starts a push and announces the link.
            let (peer, first) = {
                let r = PEERS.with(|p| {
                    let mut p = p.borrow_mut();
                    let entry = p.get_mut(&peer_id)?;
                    let first = entry.hello.is_none();
                    if !initiator {
                        entry.name = host.clone();
                    }
                    entry.hello = Some((abi, plugins.clone()));
                    Some((entry.clone(), first))
                });
                let Some(r) = r else { return };
                r
            };
            hostlog::info(
                "bridge",
                &format!(
                    "hello{} from {} (abi {abi}, {} providers, caps {})",
                    if first { "" } else { " again" },
                    peer.name,
                    plugins.len(),
                    cap_ceiling.join(",")
                ),
            );
            if !first {
                return;
            }
            if initiator {
                if abi >= ABI_VERSION {
                    push_all(peer_id);
                } else {
                    hostlog::warn(
                        "bridge",
                        &format!(
                            "{}: remote tmux2 is older (abi {abi} < {ABI_VERSION}); run tmux update there",
                            peer.name
                        ),
                    );
                }
            } else if let Err(e) = send(peer_id, &hello_frame()) {
                hostlog::debug("bridge", &format!("hello reply: {e}"));
            }
            services::peer_up(peer_id);
            fire_link_event("link-up", &peer, &plugins);
        }
        Frame::Push { descriptor, sidecar, hash, wasm } => {
            if let Err(e) = accept_push(peer_id, &descriptor, sidecar.as_deref(), &hash, &wasm) {
                hostlog::error("bridge", &format!("push from {peer_id}: {e}"));
            }
        }
        Frame::Call { call_id, plugin, method, payload } => {
            services::incoming_call(peer_id, call_id, &plugin, &method, &payload);
        }
        Frame::Reply { call_id, page, flags, payload } => {
            services::incoming_reply(peer_id, call_id, page, flags, &payload);
        }
        Frame::Cancel { call_id } => services::incoming_cancel(peer_id, call_id),
        Frame::Subscribe { plugin, topic } => {
            services::incoming_subscribe(peer_id, &plugin, &topic);
        }
        Frame::Unsubscribe { plugin, topic } => {
            services::incoming_unsubscribe(peer_id, &plugin, &topic);
        }
        Frame::Event { plugin, topic, seq, payload } => {
            services::incoming_event(peer_id, &plugin, &topic, seq, &payload);
        }
        Frame::Ping { nonce } => {
            let _ = send(peer_id, &Frame::Pong { nonce });
        }
        Frame::Pong { .. } => {}
    }
}

/// Send every local provider-capable plugin to a peer.
fn push_all(peer: u32) {
    let defs: Vec<(String, std::path::PathBuf, LoadDescriptor)> = REGISTRY.with(|r| {
        let reg = r.borrow();
        reg.plugins
            .values()
            .filter(|d| {
                d.state == PluginState::Running
                    && d.role.provides()
                    && d.pushed_by.is_none()
            })
            .map(|d| {
                (
                    d.name.clone(),
                    d.path.clone(),
                    LoadDescriptor {
                        name: d.name.clone(),
                        path: String::new(),
                        scope: d.scope_type,
                        config: d.config.clone(),
                        caps: Vec::new(),
                        role: Role::Provider,
                    },
                )
            })
            .collect()
    });
    for (name, path, desc) in defs {
        let wasm = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                hostlog::warn("bridge", &format!("push {name}: {}: {e}", path.display()));
                continue;
            }
        };
        let sidecar = std::fs::read_to_string(path.with_extension("toml")).ok();
        let frame = Frame::Push {
            descriptor: serde_json::to_string(&desc).unwrap_or_default(),
            sidecar,
            hash: blake3::hash(&wasm).to_hex().to_string(),
            wasm,
        };
        match send(peer, &frame) {
            Ok(()) => hostlog::info("bridge", &format!("pushed {name} to peer {peer}")),
            Err(e) => hostlog::warn("bridge", &format!("push {name}: {e}")),
        }
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "peer".into() } else { s }
}

/// Write a pushed plugin into the cache and load it as a provider with
/// the grants `plugin-remote-caps` allows.
fn accept_push(
    peer: u32,
    descriptor: &str,
    sidecar: Option<&str>,
    hash: &str,
    wasm: &[u8],
) -> Result<(), String> {
    let mut desc: LoadDescriptor =
        serde_json::from_str(descriptor).map_err(|e| format!("descriptor: {e}"))?;
    if desc.name.is_empty()
        || desc.name.contains('/')
        || desc.name.contains('\\')
        || desc.name.starts_with('.')
    {
        return Err(format!("bad plugin name {:?}", desc.name));
    }
    if blake3::hash(wasm).to_hex().to_string() != hash {
        return Err(format!("{}: wasm hash mismatch", desc.name));
    }
    let peer_name = peer_name(peer).unwrap_or_else(|| format!("peer-{peer}"));
    let dir = crate::fsbox::data_home()?
        .join(CACHE_DIR)
        .join(sanitize(&peer_name));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let wasm_path = dir.join(format!("{}.wasm", desc.name));
    let toml_path = dir.join(format!("{}.toml", desc.name));
    // Only rewrite what changed: a re-push of the same bytes must not
    // disturb an unchanged module (the hash is what upsert compares).
    let same = std::fs::read(&wasm_path).map(|b| b == wasm).unwrap_or(false);
    if !same {
        let tmp = dir.join(format!("{}.wasm.tmp", desc.name));
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(wasm).map_err(|e| format!("{}: {e}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, &wasm_path).map_err(|e| format!("{}: {e}", wasm_path.display()))?;
    }
    match sidecar {
        Some(text) => {
            std::fs::write(&toml_path, text).map_err(|e| format!("{}: {e}", toml_path.display()))?;
        }
        None => {
            let _ = std::fs::remove_file(&toml_path);
        }
    }
    desc.path = wasm_path.to_string_lossy().into_owned();
    desc.role = Role::Provider;
    desc.caps = remote_caps();
    let name = desc.name.clone();
    let outcome = crate::reload::upsert(desc)?;
    REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        if let Some(def) = reg.plugins.get_mut(&name) {
            def.managed = false;
            def.pushed_by = Some(peer);
            if def.caps.flags == crate::caps::DEFAULT_CAPS {
                hostlog::warn(
                    &name,
                    &format!(
                        "pushed plugin has only the default capabilities; widen {REMOTE_CAPS_OPTION} on this server"
                    ),
                );
            }
        }
    });
    hostlog::info("bridge", &format!("{name} from {peer_name}: {outcome}"));
    // Announce the new provider to the pusher (its dedupe rules and views
    // read the provider list).
    let _ = send(peer, &hello_frame());
    Ok(())
}

/// Unload pushed plugins whose peer stayed down past the grace period and
/// forget such peers. Called from every drain.
pub fn sweep() {
    let now = Instant::now();
    let stale: Vec<u32> = PEERS.with(|p| {
        p.borrow()
            .values()
            .filter(|x| {
                !x.up && x.down_since.is_some_and(|t| now.duration_since(t) > PUSH_GRACE)
            })
            .map(|x| x.id)
            .collect()
    });
    if stale.is_empty() {
        return;
    }
    let doomed: Vec<String> = REGISTRY.with(|r| {
        r.borrow()
            .plugins
            .values()
            .filter(|d| d.pushed_by.is_some_and(|p| stale.contains(&p)))
            .map(|d| d.name.clone())
            .collect()
    });
    for name in doomed {
        hostlog::info(&name, "pushed by a peer that stayed down; unloading");
        REGISTRY.with(|r| r.borrow_mut().unload(&name));
    }
    PEERS.with(|p| {
        let mut p = p.borrow_mut();
        for id in stale {
            if p.get(&id).is_some_and(|x| !x.initiator) {
                p.remove(&id);
            }
        }
    });
}

/// Serialize the server list for the `servers` import: the local server
/// first, then every peer.
pub fn servers_record() -> Vec<u8> {
    use tmux_plugin_abi::{ServerInfo, LOCAL_SERVER};

    let peers = peers();
    let mut out = Vec::new();
    out.extend_from_slice(&(1 + peers.len() as u32).to_le_bytes());
    ServerInfo { id: 0, name: LOCAL_SERVER.into(), up: true, local: true }.emit(&mut out);
    for p in peers {
        ServerInfo { id: p.id, name: p.name, up: p.up, local: false }.emit(&mut out);
    }
    out
}

#[allow(dead_code)]
fn _cursor_is_used(_: Cursor<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::Hello {
                abi: 1,
                host: "devbox".into(),
                plugins: vec![HelloPlugin {
                    name: "agents".into(),
                    role: Role::Both,
                    service_version: 1,
                }],
                cap_ceiling: vec!["db".into()],
            },
            Frame::Push {
                descriptor: "{}".into(),
                sidecar: None,
                hash: "00".into(),
                wasm: vec![0, 1, 2],
            },
            Frame::Call {
                call_id: 7,
                plugin: "a".into(),
                method: "m".into(),
                payload: b"hi".to_vec(),
            },
            Frame::Reply { call_id: 7, page: 2, flags: 1, payload: vec![] },
            Frame::Cancel { call_id: 9 },
            Frame::Subscribe { plugin: "a".into(), topic: "t".into() },
            Frame::Unsubscribe { plugin: "a".into(), topic: "t".into() },
            Frame::Event { plugin: "a".into(), topic: "t".into(), seq: 3, payload: vec![9] },
            Frame::Ping { nonce: 1 },
            Frame::Pong { nonce: 1 },
        ];
        for f in frames {
            let bytes = encode(&f);
            assert_eq!(decode(&bytes).unwrap(), f, "{f:?}");
        }
    }

    #[test]
    fn big_frames_compress() {
        let f = Frame::Push {
            descriptor: "{}".into(),
            sidecar: Some("x".repeat(100)),
            hash: "00".into(),
            wasm: vec![7u8; 100_000],
        };
        let bytes = encode(&f);
        assert_eq!(bytes[5] & FLAG_ZSTD, FLAG_ZSTD);
        assert!(bytes.len() < 10_000, "{}", bytes.len());
        assert_eq!(decode(&bytes).unwrap(), f);
        assert!(decode(b"junk").is_err());
    }
}
