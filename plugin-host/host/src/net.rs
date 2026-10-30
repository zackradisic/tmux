//! The network capability (`net`): HTTP requests and websockets for a
//! plugin that must talk to a web service - the Slack plugin, which calls
//! the web API and holds an RTM socket. Wasm cannot open a socket, and
//! `curl` as a tmux job (fetch.rs) cannot stream, so the host does both
//! with its own stack: `ureq` for HTTP, `tungstenite` for websockets,
//! rustls underneath with the bundled webpki roots - no system TLS
//! library, so the same build runs on a bare Linux box.
//!
//! Threads: network I/O never runs on the fs executor (worker.rs). A slow
//! TLS handshake would starve file jobs, and the executor's rule - never
//! block a thread on another task - says nothing about blocking on a
//! remote that answers in thirty seconds. Each HTTP request gets a thread
//! of its own that lives for the request; each websocket gets one thread
//! that reads with a short socket timeout and, between reads, drains the
//! channel of frames the guest queued with `ws_send`. The threads touch
//! no host state: they are handed copies of their inputs and hand their
//! results back through the worker doorbell - `worker::post` for a
//! completion, `worker::post_delivery` for a websocket's events - in one
//! FIFO, so a socket's "opened" completion is never overtaken by its
//! first frame.
//!
//! Ownership: a socket belongs to the instance that opened it, recorded
//! here on the main thread (plugin, scope, generation). `ws_send` and
//! `ws_close` check it; an event for a socket whose entry is gone, or
//! whose owner's generation moved on, is dropped, exactly as a stale
//! async completion is. Instance teardown ([`purge_instance`]) removes
//! the entries, which disconnects the channels; the socket thread sees
//! that within one poll interval, sends a close frame and exits. Nothing
//! waits for it: the thread holds no guest memory.
//!
//! Reach: a URL must be `http(s)://` or `ws(s)://`, and its host must
//! pass the plugin's `[caps.net] hosts` allowlist (caps.rs) at call
//! time. Redirects are not followed, so a reply cannot lead the host to a
//! host the allowlist did not name; the plugin sees the 3xx and decides.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tmux_plugin_abi::net::{self as wire, HttpRequestWire};
use tmux_plugin_abi::{ErrorCode, EventHeader, EventScope, FieldWriter, KeyRef};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::abi::{err, HostError};
use crate::caps::EffectiveCaps;
use crate::intern;
use crate::registry::ScopeId;
use crate::services::Owner;
use crate::state::Delivery;
use crate::worker::{self, Completion};

/// TCP connect ceiling for a websocket (HTTP has `timeout_global`).
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// The websocket handshake (TLS + HTTP upgrade) must finish within this.
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Socket read timeout of the serve loop: how long a queued `ws_send`
/// waits at most before the thread notices it.
const WS_POLL: Duration = Duration::from_millis(25);
/// After we send a close frame, how long we wait for the peer's.
const WS_CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Which schemes an import accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    Http,
    Ws,
}

/// What a websocket thread reports. `Opened`/`OpenFailed` answer the
/// `ws_open` token; `Message`/`Closed` become the `ws-*` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    Opened { token: u64 },
    OpenFailed { token: u64, message: String },
    Message { text: bool, data: Vec<u8> },
    /// Terminal. `code` is the peer's close code, or 0 for a transport
    /// error (`reason` is then the error text).
    Closed { code: u16, reason: String },
}

// ---------------------------------------------------------------------------
// URL checks.
// ---------------------------------------------------------------------------

/// The host a URL names, after checking its scheme against `kind`.
pub fn url_host(url: &str, kind: UrlKind) -> Result<String, HostError> {
    let uri: tungstenite::http::Uri = url
        .parse()
        .map_err(|e| err(ErrorCode::BadRequest, format!("bad url {url:?}: {e}")))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| err(ErrorCode::BadRequest, format!("url {url:?} has no scheme")))?;
    let ok = match kind {
        UrlKind::Http => matches!(scheme, "http" | "https"),
        UrlKind::Ws => matches!(scheme, "ws" | "wss"),
    };
    if !ok {
        let want = match kind {
            UrlKind::Http => "http or https",
            UrlKind::Ws => "ws or wss",
        };
        return Err(err(
            ErrorCode::BadRequest,
            format!("url scheme {scheme:?} not allowed (want {want})"),
        ));
    }
    let host = uri
        .host()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| err(ErrorCode::BadRequest, format!("url {url:?} has no host")))?;
    Ok(host.trim_start_matches('[').trim_end_matches(']').to_string())
}

/// Scheme and allowlist check for a URL an import was handed.
pub fn check_url(caps: &EffectiveCaps, url: &str, kind: UrlKind) -> Result<(), HostError> {
    let host = url_host(url, kind)?;
    if !caps.net_host_allowed(&host) {
        return Err(err(
            ErrorCode::CapDenied,
            format!("host {host:?} is not in the plugin's net allowlist"),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP: one thread per request.
// ---------------------------------------------------------------------------

/// A finished request: the status, and `data` = the header block (its
/// length in `header_len`) followed by the body.
#[derive(Debug)]
pub struct HttpResult {
    pub status: u16,
    pub header_len: usize,
    pub data: Vec<u8>,
}

/// Start a request for `token`; its completion is posted from the
/// request's thread. Main thread only (the token is the caller's).
pub fn http_start(token: u64, req: HttpRequestWire) {
    let spawned = std::thread::Builder::new()
        .name("tmux-plugin-net".to_string())
        .spawn(move || {
            let completion = match http_run(&req, wire::MAX_BODY) {
                Ok(r) => Completion {
                    token,
                    err: 0,
                    v0: i64::from(r.status),
                    v1: r.header_len as i64,
                    data: r.data,
                },
                Err(e) => worker::err_completion(token, e.code, e.message),
            };
            worker::post(completion);
        });
    if let Err(e) = spawned {
        worker::post(worker::err_completion(
            token,
            ErrorCode::Host,
            format!("spawn net thread: {e}"),
        ));
    }
}

fn http_err(e: ureq::Error) -> HostError {
    match e {
        ureq::Error::BodyExceedsLimit(limit) => err(
            ErrorCode::Limit,
            format!("response body over the {}-byte cap", limit.saturating_sub(1)),
        ),
        ureq::Error::Timeout(_) => err(ErrorCode::Timeout, e.to_string()),
        ureq::Error::StatusCode(_) => err(ErrorCode::Host, e.to_string()),
        other => err(ErrorCode::Host, other.to_string()),
    }
}

/// Perform one request on the calling thread. `body_cap` is the most
/// body the result may hold (an `E_LIMIT` error past it). A non-2xx
/// status is a result, not an error; only transport failures (DNS, TLS,
/// timeout, refused) are errors.
pub fn http_run(req: &HttpRequestWire, body_cap: usize) -> Result<HttpResult, HostError> {
    let timeout_ms = if req.timeout_ms <= 0 {
        wire::DEFAULT_TIMEOUT_MS
    } else {
        req.timeout_ms.min(wire::MAX_TIMEOUT_MS)
    };
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(timeout_ms as u64)))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent("tmux2-plugin-host")
        .build()
        .new_agent();
    let method = ureq::http::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| err(ErrorCode::BadRequest, format!("bad method {:?}", req.method)))?;
    let mut builder = ureq::http::Request::builder().method(method).uri(req.url.as_str());
    for (name, value) in wire::parse_headers(&req.headers) {
        builder = builder.header(name, value);
    }
    let bad = |e: ureq::http::Error| err(ErrorCode::BadRequest, format!("bad request: {e}"));
    let response = if req.body.is_empty() {
        agent.run(builder.body(()).map_err(bad)?)
    } else {
        agent.run(builder.body(req.body.as_slice()).map_err(bad)?)
    }
    .map_err(http_err)?;

    let status = response.status().as_u16();
    let mut data = Vec::new();
    for (name, value) in response.headers() {
        data.extend_from_slice(name.as_str().as_bytes());
        data.extend_from_slice(b": ");
        data.extend_from_slice(value.as_bytes());
        data.push(b'\n');
    }
    let header_len = data.len();
    let (_, mut body) = response.into_parts();
    // ureq's limit is exclusive: a body of exactly `body_cap` bytes passes.
    let bytes = body
        .with_config()
        .limit(body_cap as u64 + 1)
        .read_to_vec()
        .map_err(http_err)?;
    data.extend_from_slice(&bytes);
    Ok(HttpResult { status, header_len, data })
}

// ---------------------------------------------------------------------------
// Websockets: a registry on the main thread, a thread per socket.
// ---------------------------------------------------------------------------

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocketState {
    /// The handshake is running; the guest has no handle yet.
    Connecting,
    Open,
    /// `ws_close` was called; sends are refused, `ws-closed` will follow.
    Closing,
}

/// The guest's side of a socket thread.
struct Socket {
    owner: Owner,
    tx: crossbeam_channel::Sender<Message>,
    closing: Arc<AtomicBool>,
    state: SocketState,
}

/// The thread's side: frames to send and the close request.
pub(crate) struct WsLink {
    rx: crossbeam_channel::Receiver<Message>,
    closing: Arc<AtomicBool>,
}

thread_local! {
    static SOCKETS: RefCell<HashMap<u64, Socket>> = RefCell::new(HashMap::new());
    static NEXT_SOCKET: Cell<u64> = const { Cell::new(0) };
}

fn new_link() -> (crossbeam_channel::Sender<Message>, Arc<AtomicBool>, WsLink) {
    let (tx, rx) = crossbeam_channel::bounded::<Message>(wire::WS_QUEUE);
    let closing = Arc::new(AtomicBool::new(false));
    (tx, closing.clone(), WsLink { rx, closing })
}

fn register(owner: Owner, tx: crossbeam_channel::Sender<Message>, closing: Arc<AtomicBool>) -> u64 {
    let id = NEXT_SOCKET.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    SOCKETS.with(|s| {
        s.borrow_mut().insert(id, Socket { owner, tx, closing, state: SocketState::Connecting });
    });
    id
}

/// Open a websocket for `owner`; `token` is answered with the handle
/// (`v0`) once the handshake succeeds, or with an error. Main thread.
pub fn ws_open(owner: Owner, token: u64, url: String, headers: String) -> Result<u64, HostError> {
    let (tx, closing, link) = new_link();
    let id = register(owner.clone(), tx, closing);
    let sink = move |event: WsEvent| {
        worker::post_delivery(Delivery::Net { owner: owner.clone(), socket: id, event });
    };
    let spawned = std::thread::Builder::new()
        .name(format!("tmux-plugin-ws{id}"))
        .spawn(move || ws_thread(token, &url, &headers, link, &sink));
    if let Err(e) = spawned {
        SOCKETS.with(|s| s.borrow_mut().remove(&id));
        return Err(err(ErrorCode::Host, format!("spawn websocket thread: {e}")));
    }
    Ok(id)
}

fn socket_of<'a>(
    sockets: &'a mut HashMap<u64, Socket>,
    owner: &Owner,
    id: u64,
) -> Result<&'a mut Socket, HostError> {
    match sockets.get_mut(&id) {
        Some(s) if s.owner == *owner && s.state != SocketState::Connecting => Ok(s),
        _ => Err(err(ErrorCode::NoSuchObject, format!("no such socket {id}"))),
    }
}

/// Queue one frame. Fails when the socket is unknown, not the caller's,
/// closing, or its outgoing queue is full.
pub fn ws_send(owner: &Owner, id: u64, data: Vec<u8>, text: bool) -> Result<(), HostError> {
    let message = if text {
        Message::text(
            String::from_utf8(data)
                .map_err(|_| err(ErrorCode::BadRequest, "text frame is not UTF-8"))?,
        )
    } else {
        Message::binary(data)
    };
    SOCKETS.with(|s| {
        let mut sockets = s.borrow_mut();
        let socket = socket_of(&mut sockets, owner, id)?;
        if socket.state == SocketState::Closing {
            return Err(err(ErrorCode::NoSuchObject, format!("socket {id} is closing")));
        }
        socket.tx.try_send(message).map_err(|e| match e {
            crossbeam_channel::TrySendError::Full(_) => err(
                ErrorCode::Limit,
                format!("socket {id}: outgoing queue full ({} frames)", wire::WS_QUEUE),
            ),
            crossbeam_channel::TrySendError::Disconnected(_) => {
                err(ErrorCode::NoSuchObject, format!("socket {id} is closed"))
            }
        })
    })
}

/// Start a graceful close; `ws-closed` follows.
pub fn ws_close(owner: &Owner, id: u64) -> Result<(), HostError> {
    SOCKETS.with(|s| {
        let mut sockets = s.borrow_mut();
        let socket = socket_of(&mut sockets, owner, id)?;
        socket.state = SocketState::Closing;
        socket.closing.store(true, Ordering::SeqCst);
        Ok(())
    })
}

/// Drop every socket of a dying instance. The threads notice the closed
/// channels, send a close frame and exit; their late events find no
/// entry and are dropped.
pub fn purge_instance(plugin: &str, scope: ScopeId, generation: u64) {
    SOCKETS.with(|s| {
        s.borrow_mut().retain(|_, sock| {
            !(sock.owner.plugin == plugin
                && sock.owner.scope == scope
                && sock.owner.generation == generation)
        });
    });
}

/// Server shutdown: drop every socket.
pub fn forget_all() {
    SOCKETS.with(|s| s.borrow_mut().clear());
}

/// Sockets open right now (for `show-plugins -v` and tests).
pub fn open_sockets() -> usize {
    SOCKETS.with(|s| s.borrow().len())
}

fn build_event(name: &str, fill: impl FnOnce(&mut FieldWriter)) -> Vec<u8> {
    let header = EventHeader { event_id: intern::intern(name), seq: 0, scope: EventScope::default() };
    let mut buf = Vec::with_capacity(64);
    header.write(&mut buf);
    let mut w = FieldWriter::new();
    fill(&mut w);
    buf.extend_from_slice(&w.finish());
    buf
}

fn key(name: &str) -> KeyRef<'static> {
    KeyRef::Id(intern::intern(name))
}

/// Main-thread half of a socket thread's report (from `events::drain`).
/// State moves here, never on the thread: the token is answered, the
/// handle becomes sendable, frames become `ws-message` events for the
/// owner (if it subscribed), and `Closed` removes the entry and delivers
/// the terminal `ws-closed`. A socket that never finished its handshake
/// closes silently: the guest never had the handle.
pub fn deliver(owner: &Owner, id: u64, event: WsEvent) {
    match event {
        WsEvent::Opened { token } => {
            let known = SOCKETS.with(|s| {
                let mut sockets = s.borrow_mut();
                match sockets.get_mut(&id) {
                    Some(sock) if sock.owner == *owner => {
                        if sock.state == SocketState::Connecting {
                            sock.state = SocketState::Open;
                        }
                        true
                    }
                    _ => false,
                }
            });
            if known {
                crate::events::enqueue_delivery(Delivery::AsyncComplete {
                    token,
                    err: 0,
                    v0: id as i64,
                    v1: 0,
                    data: Vec::new(),
                });
            }
        }
        WsEvent::OpenFailed { token, message } => {
            let known = SOCKETS.with(|s| {
                let mut sockets = s.borrow_mut();
                match sockets.get(&id) {
                    Some(sock) if sock.owner == *owner => {
                        sockets.remove(&id);
                        true
                    }
                    _ => false,
                }
            });
            if known {
                crate::events::enqueue_delivery(Delivery::AsyncComplete {
                    token,
                    err: ErrorCode::Host.as_num(),
                    v0: 0,
                    v1: 0,
                    data: message.into_bytes(),
                });
            }
        }
        WsEvent::Message { text, data } => {
            let live = SOCKETS.with(|s| {
                s.borrow()
                    .get(&id)
                    .is_some_and(|sock| sock.owner == *owner && sock.state != SocketState::Connecting)
            });
            if !live {
                return;
            }
            let bytes = build_event(wire::EVENT_MESSAGE, |w| {
                w.i64(key(wire::F_SOCKET), id as i64);
                w.i64(key(wire::F_TEXT), i64::from(text));
                w.bytes(key(wire::F_DATA), &data);
            });
            crate::events::deliver_owned_event(owner, bytes, wire::EVENT_MESSAGE, true);
        }
        WsEvent::Closed { code, reason } => {
            let announced = SOCKETS.with(|s| {
                let mut sockets = s.borrow_mut();
                match sockets.get(&id) {
                    Some(sock) if sock.owner == *owner => {
                        let was = sock.state;
                        sockets.remove(&id);
                        Some(was != SocketState::Connecting)
                    }
                    _ => None,
                }
            });
            if announced != Some(true) {
                return;
            }
            let bytes = build_event(wire::EVENT_CLOSED, |w| {
                w.i64(key(wire::F_SOCKET), id as i64);
                w.i64(key(wire::F_CODE), i64::from(code));
                w.str(key(wire::F_REASON), &reason);
            });
            crate::events::deliver_owned_event(owner, bytes, wire::EVENT_CLOSED, true);
        }
    }
}

// ---------------------------------------------------------------------------
// The socket thread.
// ---------------------------------------------------------------------------

/// Connect, then serve until the socket closes; every outcome reaches
/// `sink`. `Opened` is reported before the first frame, `Closed` last.
pub(crate) fn ws_thread(token: u64, url: &str, headers: &str, link: WsLink, sink: &dyn Fn(WsEvent)) {
    let (mut ws, tcp) = match ws_connect(url, headers) {
        Ok(pair) => pair,
        Err(message) => {
            sink(WsEvent::OpenFailed { token, message });
            return;
        }
    };
    sink(WsEvent::Opened { token });
    let (code, reason) = ws_serve(&mut ws, &link, sink);
    let _ = tcp.shutdown(std::net::Shutdown::Both);
    sink(WsEvent::Closed { code, reason });
}

/// Resolve, connect (with a timeout) and shake hands. The returned
/// `TcpStream` is a dup of the socket, kept for timeouts and shutdown
/// (the TLS wrapper hides the original).
fn ws_connect(url: &str, headers: &str) -> Result<(Ws, TcpStream), String> {
    let uri: tungstenite::http::Uri = url.parse().map_err(|e| format!("bad url: {e}"))?;
    let host = uri.host().ok_or_else(|| "url has no host".to_string())?.to_string();
    let secure = uri.scheme_str() == Some("wss");
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?
        .collect();
    let mut last: Option<std::io::Error> = None;
    let mut stream = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, WS_CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let stream = stream.ok_or_else(|| {
        format!(
            "connect {host}:{port}: {}",
            last.map_or_else(|| "no addresses".to_string(), |e| e.to_string())
        )
    })?;
    let _ = stream.set_nodelay(true);
    let tcp = stream.try_clone().map_err(|e| format!("dup socket: {e}"))?;
    tcp.set_read_timeout(Some(WS_HANDSHAKE_TIMEOUT))
        .map_err(|e| format!("set timeout: {e}"))?;

    let mut request = tungstenite::ClientRequestBuilder::new(uri);
    for (name, value) in wire::parse_headers(headers) {
        request = request.with_header(name, value);
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_FRAME))
        .max_frame_size(Some(wire::MAX_FRAME));
    let (ws, _response) = tungstenite::client_tls_with_config(request, stream, Some(config), None)
        .map_err(|e| match e {
            tungstenite::HandshakeError::Interrupted(_) => "handshake timed out".to_string(),
            tungstenite::HandshakeError::Failure(e) => format!("handshake: {e}"),
        })?;
    // The serve loop polls: a short read timeout is how it notices a
    // queued frame or a close request between reads.
    tcp.set_read_timeout(Some(WS_POLL)).map_err(|e| format!("set timeout: {e}"))?;
    Ok((ws, tcp))
}

/// Read frames until the connection ends, sending what the guest queued
/// between reads. Returns the close code and reason for `ws-closed`.
fn ws_serve(ws: &mut Ws, link: &WsLink, sink: &dyn Fn(WsEvent)) -> (u16, String) {
    let mut peer_close: Option<(u16, String)> = None;
    let mut close_deadline: Option<Instant> = None;
    let start_close = |ws: &mut Ws, deadline: &mut Option<Instant>| {
        if deadline.is_none() {
            let _ = ws.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() }));
            *deadline = Some(Instant::now() + WS_CLOSE_GRACE);
        }
    };
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                sink(WsEvent::Message { text: true, data: text.as_str().as_bytes().to_vec() })
            }
            Ok(Message::Binary(bytes)) => {
                sink(WsEvent::Message { text: false, data: bytes.to_vec() })
            }
            Ok(Message::Close(frame)) => {
                // tungstenite queues the reply; `read` returns
                // ConnectionClosed once it has gone out.
                peer_close = Some(match frame {
                    Some(f) => (u16::from(f.code), f.reason.as_str().to_string()),
                    None => (0, String::new()),
                });
                if close_deadline.is_none() {
                    close_deadline = Some(Instant::now() + WS_CLOSE_GRACE);
                }
            }
            // Pings are answered by tungstenite itself; neither they nor
            // pongs reach the guest.
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return peer_close.unwrap_or((0, "connection closed".to_string()));
            }
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(e) => return peer_close.unwrap_or((0, e.to_string())),
        }
        if close_deadline.is_some_and(|d| Instant::now() >= d) {
            return peer_close.unwrap_or((0, "close handshake timed out".to_string()));
        }
        if link.closing.load(Ordering::SeqCst) {
            start_close(ws, &mut close_deadline);
        }
        loop {
            match link.rx.try_recv() {
                Ok(message) => {
                    if close_deadline.is_some() {
                        continue; // queued before the close: dropped
                    }
                    if let Err(e) = ws.send(message) {
                        return (0, format!("send: {e}"));
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    // The owner is gone: a courtesy close, then out.
                    start_close(ws, &mut close_deadline);
                    let _ = ws.flush();
                    return (0, "owner gone".to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn caps(hosts: &[&str]) -> EffectiveCaps {
        EffectiveCaps {
            flags: crate::caps::NET,
            net_hosts: hosts.iter().map(|h| h.to_string()).collect(),
            ..Default::default()
        }
    }

    fn owner(generation: u64) -> Owner {
        Owner { plugin: "p".into(), scope: ScopeId::Server, generation }
    }

    #[test]
    fn url_schemes_and_hosts() {
        assert_eq!(url_host("https://slack.com/api/x", UrlKind::Http).unwrap(), "slack.com");
        assert_eq!(url_host("http://127.0.0.1:8080/", UrlKind::Http).unwrap(), "127.0.0.1");
        assert_eq!(url_host("wss://wss-primary.slack.com/link", UrlKind::Ws).unwrap(), "wss-primary.slack.com");
        assert_eq!(url_host("ws://[::1]:9/", UrlKind::Ws).unwrap(), "::1");
        for bad in ["ftp://x.org/f", "file:///etc/passwd", "x.org/path", "https://", "mailto:a@b"] {
            let e = url_host(bad, UrlKind::Http).unwrap_err();
            assert_eq!(e.code, ErrorCode::BadRequest, "{bad}");
        }
        // The kinds do not cross.
        assert_eq!(url_host("https://x.org/", UrlKind::Ws).unwrap_err().code, ErrorCode::BadRequest);
        assert_eq!(url_host("wss://x.org/", UrlKind::Http).unwrap_err().code, ErrorCode::BadRequest);
    }

    #[test]
    fn allowlist_is_enforced_on_the_url_host() {
        let c = caps(&["slack.com", "*.slack.com"]);
        assert!(check_url(&c, "https://slack.com/api/rtm.connect", UrlKind::Http).is_ok());
        assert!(check_url(&c, "wss://wss-primary.slack.com/?t=1", UrlKind::Ws).is_ok());
        assert_eq!(
            check_url(&c, "https://evil-slack.com/x", UrlKind::Http).unwrap_err().code,
            ErrorCode::CapDenied
        );
        assert_eq!(
            check_url(&c, "https://slack.com.evil.net/", UrlKind::Http).unwrap_err().code,
            ErrorCode::CapDenied
        );
        // Scheme is checked before the allowlist.
        assert_eq!(
            check_url(&c, "ftp://slack.com/", UrlKind::Http).unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert!(check_url(&caps(&[]), "https://anywhere.example/", UrlKind::Http).is_ok());
    }

    #[test]
    fn request_block_round_trips() {
        let req = HttpRequestWire {
            method: "POST".into(),
            url: "https://slack.com/api/chat.postMessage".into(),
            headers: "Authorization: Bearer xoxb-1\nContent-Type: application/json".into(),
            body: br#"{"channel":"C1","text":"hi"}"#.to_vec(),
            timeout_ms: 5000,
        };
        let block = req.encode();
        assert_eq!(HttpRequestWire::decode(&block).unwrap(), req);
        assert_eq!(
            wire::parse_headers(&req.headers),
            vec![
                ("Authorization".to_string(), "Bearer xoxb-1".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ]
        );
        // A block without method or url is refused.
        let mut w = FieldWriter::new();
        w.str(KeyRef::Name(wire::F_URL), "https://x/");
        assert!(HttpRequestWire::decode(&w.finish()).is_err());
        assert!(HttpRequestWire::decode(b"").is_err());
    }

    /// A one-request HTTP/1.1 server: reads the head and body, hands them
    /// to `answer`, writes what it returns and closes.
    fn serve_once(answer: impl FnOnce(&str, &[u8]) -> Vec<u8> + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head_len, content_len) = loop {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let cl = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    break (pos + 4, cl);
                }
            };
            while buf.len() < head_len + content_len {
                let n = stream.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head = String::from_utf8_lossy(&buf[..head_len]).to_string();
            let body = buf[head_len..].to_vec();
            let reply = answer(&head, &body);
            stream.write_all(&reply).unwrap();
            let _ = stream.flush();
        });
        port
    }

    fn response(status: &str, extra: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn get(url: &str) -> HttpRequestWire {
        HttpRequestWire { method: "GET".into(), url: url.into(), ..Default::default() }
    }

    #[test]
    fn http_get_returns_status_headers_and_body() {
        let port = serve_once(|head, _| {
            assert!(head.starts_with("GET /index.html HTTP/1.1"), "{head}");
            assert!(head.to_ascii_lowercase().contains("x-probe: 1"), "{head}");
            response("200 OK", "Content-Type: text/plain\r\nX-Test: yes\r\n", b"hello")
        });
        let mut req = get(&format!("http://127.0.0.1:{port}/index.html"));
        req.headers = "X-Probe: 1".into();
        let r = http_run(&req, wire::MAX_BODY).unwrap();
        assert_eq!(r.status, 200);
        let headers = std::str::from_utf8(&r.data[..r.header_len]).unwrap();
        assert!(headers.contains("x-test: yes\n"), "{headers}");
        assert!(headers.contains("content-type: text/plain\n"), "{headers}");
        assert_eq!(&r.data[r.header_len..], b"hello");
    }

    #[test]
    fn http_post_sends_the_body_and_a_non_2xx_is_not_an_error() {
        let port = serve_once(|head, body| {
            assert!(head.starts_with("POST /echo HTTP/1.1"), "{head}");
            assert!(head.to_ascii_lowercase().contains("content-type: application/x-www-form-urlencoded"));
            let mut out = b"method=POST;".to_vec();
            out.extend_from_slice(body);
            response("404 Not Found", "", &out)
        });
        let req = HttpRequestWire {
            method: "POST".into(),
            url: format!("http://127.0.0.1:{port}/echo"),
            headers: "Content-Type: application/x-www-form-urlencoded".into(),
            body: b"a=1&b=two".to_vec(),
            timeout_ms: 0,
        };
        let r = http_run(&req, wire::MAX_BODY).unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(&r.data[r.header_len..], b"method=POST;a=1&b=two");
    }

    #[test]
    fn http_body_over_the_cap_is_a_limit_error_and_at_the_cap_is_fine() {
        let big = vec![b'x'; 2000];
        let port = serve_once(move |_, _| response("200 OK", "", &big));
        let e = http_run(&get(&format!("http://127.0.0.1:{port}/")), 1024).unwrap_err();
        assert_eq!(e.code, ErrorCode::Limit, "{}", e.message);

        let exact = vec![b'y'; 1024];
        let port = serve_once(move |_, _| response("200 OK", "", &exact));
        let r = http_run(&get(&format!("http://127.0.0.1:{port}/")), 1024).unwrap();
        assert_eq!(r.data.len() - r.header_len, 1024);
    }

    #[test]
    fn http_transport_failures_are_errors_with_a_message() {
        // Nothing listens here (bind, learn the port, drop the listener).
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let e = http_run(&get(&format!("http://127.0.0.1:{port}/")), wire::MAX_BODY).unwrap_err();
        assert_eq!(e.code, ErrorCode::Host);
        assert!(!e.message.is_empty());
        let e = http_run(
            &HttpRequestWire { method: "BAD METHOD".into(), url: "http://127.0.0.1:1/".into(), ..Default::default() },
            wire::MAX_BODY,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    /// A websocket echo server for one connection: echoes text frames,
    /// pings once after the first frame, closes when the client does.
    fn ws_echo_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let mut pinged = false;
            loop {
                match ws.read() {
                    Ok(Message::Text(t)) => {
                        ws.send(Message::text(format!("echo:{}", t.as_str()))).unwrap();
                        if !pinged {
                            pinged = true;
                            ws.send(Message::Ping(b"p".to_vec().into())).unwrap();
                        }
                    }
                    Ok(Message::Binary(b)) => ws.send(Message::Binary(b)).unwrap(),
                    Ok(Message::Pong(p)) => {
                        assert_eq!(&p[..], b"p");
                        ws.send(Message::text("ponged")).unwrap();
                    }
                    Ok(Message::Close(_)) => {}
                    Ok(_) => {}
                    Err(tungstenite::Error::ConnectionClosed) => break,
                    Err(e) => panic!("server: {e}"),
                }
            }
        });
        port
    }

    #[test]
    fn websocket_echo_through_the_thread() {
        let port = ws_echo_server();
        let (tx, closing, link) = new_link();
        let (etx, erx) = crossbeam_channel::unbounded::<WsEvent>();
        let url = format!("ws://127.0.0.1:{port}/rtm");
        std::thread::spawn(move || {
            let sink = move |e: WsEvent| {
                let _ = etx.send(e);
            };
            ws_thread(7, &url, "X-Probe: 1", link, &sink);
        });
        let next = || erx.recv_timeout(Duration::from_secs(10)).expect("event");
        assert_eq!(next(), WsEvent::Opened { token: 7 });
        tx.send(Message::text("hello")).unwrap();
        assert_eq!(next(), WsEvent::Message { text: true, data: b"echo:hello".to_vec() });
        // The server pinged after the echo; the thread answered without
        // surfacing either frame, and the server told us so.
        assert_eq!(next(), WsEvent::Message { text: true, data: b"ponged".to_vec() });
        tx.send(Message::binary(vec![1u8, 2, 3])).unwrap();
        assert_eq!(next(), WsEvent::Message { text: false, data: vec![1, 2, 3] });
        closing.store(true, Ordering::SeqCst);
        match next() {
            WsEvent::Closed { code, .. } => assert_eq!(code, 1000),
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn websocket_handshake_failure_reports_open_failed() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (_tx, _closing, link) = new_link();
        let (etx, erx) = crossbeam_channel::unbounded::<WsEvent>();
        let url = format!("ws://127.0.0.1:{port}/");
        std::thread::spawn(move || {
            let sink = move |e: WsEvent| {
                let _ = etx.send(e);
            };
            ws_thread(9, &url, "", link, &sink);
        });
        match erx.recv_timeout(Duration::from_secs(10)).unwrap() {
            WsEvent::OpenFailed { token, message } => {
                assert_eq!(token, 9);
                assert!(message.starts_with("connect 127.0.0.1:"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(erx.recv_timeout(Duration::from_millis(200)).is_err(), "nothing after OpenFailed");
    }

    fn take_deliveries() -> Vec<Delivery> {
        crate::state::EVENTS.with(|e| e.borrow_mut().deliveries.drain(..).collect())
    }

    #[test]
    fn main_thread_state_machine_answers_the_token_and_guards_the_handle() {
        let me = owner(3);
        let (tx, closing, link) = new_link();
        let id = register(me.clone(), tx, closing);
        assert!(ws_send(&me, id, b"early".to_vec(), true).is_err(), "no handle before Opened");

        deliver(&me, id, WsEvent::Opened { token: 41 });
        match take_deliveries().as_slice() {
            [Delivery::AsyncComplete { token: 41, err: 0, v0, .. }] => assert_eq!(*v0, id as i64),
            other => panic!("{}", other.len()),
        }
        ws_send(&me, id, b"hi".to_vec(), true).unwrap();
        assert_eq!(link.rx.try_recv().unwrap(), Message::text("hi"));
        assert_eq!(ws_send(&me, id, vec![0xff], true).unwrap_err().code, ErrorCode::BadRequest);
        // Another instance (a later generation) cannot touch it.
        assert_eq!(ws_send(&owner(4), id, b"x".to_vec(), false).unwrap_err().code, ErrorCode::NoSuchObject);
        assert_eq!(ws_close(&owner(4), id).unwrap_err().code, ErrorCode::NoSuchObject);
        // Fill the queue: the 257th frame is refused.
        for _ in 0..wire::WS_QUEUE {
            ws_send(&me, id, b"f".to_vec(), false).unwrap();
        }
        assert_eq!(ws_send(&me, id, b"f".to_vec(), false).unwrap_err().code, ErrorCode::Limit);
        // Frames for an unsubscribed or absent guest are dropped quietly;
        // the entry stays open.
        deliver(&me, id, WsEvent::Message { text: true, data: b"m".to_vec() });
        assert!(take_deliveries().is_empty());
        assert_eq!(open_sockets(), 1);

        ws_close(&me, id).unwrap();
        assert!(link.closing.load(Ordering::SeqCst));
        assert_eq!(ws_send(&me, id, b"late".to_vec(), true).unwrap_err().code, ErrorCode::NoSuchObject);
        deliver(&me, id, WsEvent::Closed { code: 1000, reason: String::new() });
        assert_eq!(open_sockets(), 0);
        assert_eq!(ws_close(&me, id).unwrap_err().code, ErrorCode::NoSuchObject);

        // A failed handshake answers the token with an error and leaves
        // nothing behind; a socket that never opened closes silently.
        let (tx, closing, _link) = new_link();
        let id = register(me.clone(), tx, closing);
        deliver(&me, id, WsEvent::OpenFailed { token: 42, message: "refused".into() });
        match take_deliveries().as_slice() {
            [Delivery::AsyncComplete { token: 42, err, data, .. }] => {
                assert_eq!(*err, ErrorCode::Host.as_num());
                assert_eq!(data, b"refused");
            }
            other => panic!("{}", other.len()),
        }
        assert_eq!(open_sockets(), 0);
        deliver(&me, id, WsEvent::Closed { code: 0, reason: "late".into() });
        assert!(take_deliveries().is_empty());

        // Teardown drops the instance's sockets and disconnects the link.
        let (tx, closing, link) = new_link();
        let id = register(me.clone(), tx, closing);
        deliver(&me, id, WsEvent::Opened { token: 43 });
        take_deliveries();
        purge_instance("p", ScopeId::Server, 3);
        assert_eq!(open_sockets(), 0);
        assert_eq!(link.rx.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected));
    }

    #[test]
    fn socket_thread_events_ride_the_worker_doorbell() {
        let fd = worker::notify_fd();
        assert!(fd >= 0);
        let me = owner(1);
        let posted = me.clone();
        std::thread::spawn(move || {
            worker::post_delivery(Delivery::Net {
                owner: posted,
                socket: 5,
                event: WsEvent::Message { text: true, data: b"frame".to_vec() },
            });
        })
        .join()
        .unwrap();
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let rc = unsafe { libc::poll(&mut pfd, 1, 5000) };
        assert!(rc > 0, "doorbell did not ring");
        worker::drain();
        let got = take_deliveries();
        assert!(
            got.iter().any(|d| matches!(
                d,
                Delivery::Net { owner, socket: 5, event: WsEvent::Message { text: true, data } }
                    if *owner == me && data == b"frame"
            )),
            "{} deliveries, none ours",
            got.len()
        );
    }
}
