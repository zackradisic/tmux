//! Network: HTTP requests and websockets (capability `net`).
//!
//! The host does the I/O on threads of its own and hands the result
//! back: an HTTP request resolves a future with the response; a
//! websocket resolves `ws_open` with a handle, then delivers its frames
//! as events (`ws-message`, and the terminal `ws-closed`) to the plugin
//! that opened it. `ws_open` subscribes to both, so a plugin only has to
//! decode them in `on_event` with [`WsMessage::from_event`] and
//! [`WsClosed::from_event`]. Hosts may be narrowed by a `[caps.net]
//! hosts` sidecar list; a request for another host fails with
//! `E_CAP_DENIED` before anything is sent.

use std::fmt;

use tmux_plugin_abi::net as wire;
use tmux_plugin_abi::{ErrorCode, HostError};

use crate::api::{check, start_async, subscribe};
use crate::event::Event;
use crate::executor::Completion;
use crate::runtime::raw;
use crate::strings::AsTmuxStr;

/// One HTTP request. Build it with [`HttpRequest::get`] or
/// [`HttpRequest::post`] and the chaining helpers, or fill the fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// 0 = the host default (30 s); capped at 300 s.
    pub timeout_ms: u64,
}

impl HttpRequest {
    pub fn new(method: &str, url: &str) -> Self {
        Self { method: method.to_string(), url: url.to_string(), ..Default::default() }
    }

    pub fn get(url: &str) -> Self {
        Self::new("GET", url)
    }

    pub fn post(url: &str) -> Self {
        Self::new("POST", url)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// A JSON body with its content type.
    pub fn json(self, value: &impl serde::Serialize) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_default();
        self.header("Content-Type", "application/json").body(body)
    }

    /// An `application/x-www-form-urlencoded` body (the Slack web API's
    /// native shape), with its content type.
    pub fn form(self, fields: &[(&str, &str)]) -> Self {
        let body = form_encode(fields);
        self.header("Content-Type", "application/x-www-form-urlencoded").body(body.into_bytes())
    }

    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }

    fn wire(&self) -> wire::HttpRequestWire {
        wire::HttpRequestWire {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: join_headers(&self.headers),
            body: self.body.clone(),
            timeout_ms: self.timeout_ms.min(i64::MAX as u64) as i64,
        }
    }
}

/// `Name: value` lines; a header value's newlines are dropped so one
/// header cannot smuggle another.
fn join_headers(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        let name: String = name.chars().filter(|c| !c.is_control() && *c != ':').collect();
        let value: String = value.chars().filter(|c| !c.is_control()).collect();
        if name.is_empty() {
            continue;
        }
        out.push_str(&name);
        out.push_str(": ");
        out.push_str(&value);
        out.push('\n');
    }
    out
}

/// `application/x-www-form-urlencoded`: unreserved bytes pass, space is
/// `+`, everything else is `%XX`.
pub fn form_encode(fields: &[(&str, &str)]) -> String {
    fn push(out: &mut String, s: &str) {
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    let mut out = String::new();
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        push(&mut out, k);
        out.push('=');
        push(&mut out, v);
    }
    out
}

/// What came back. A non-2xx status is a response, not an error; only a
/// transport failure (DNS, TLS, refused, timeout) or a body over 8 MiB
/// fails the future.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    /// Header names are lower-case; a repeated header appears once per
    /// value, in order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text (lossy).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The first value of a header, case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, HostError> {
        serde_json::from_slice(&self.body).map_err(|e| HostError {
            code: ErrorCode::BadRequest,
            message: format!("decode: {e}"),
        })
    }

    fn from_completion(c: Completion) -> Result<Self, HostError> {
        let header_len = usize::try_from(c.v1).unwrap_or(0).min(c.data.len());
        let (head, body) = c.data.split_at(header_len);
        let headers = wire::parse_headers(&String::from_utf8_lossy(head));
        Ok(HttpResponse {
            status: u16::try_from(c.v0).unwrap_or(0),
            headers,
            body: body.to_vec(),
        })
    }
}

/// Perform one HTTP request (capability `net`). The host runs it on a
/// thread of its own; the future resolves with the whole response.
pub async fn http_request(req: &HttpRequest) -> Result<HttpResponse, HostError> {
    let block = req.wire().encode();
    let fut = start_async(unsafe {
        raw::http_request(block.as_ptr() as i32, block.len() as i32)
    })?;
    HttpResponse::from_completion(fut.await?)
}

/// A websocket handle, unique within the plugin instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WsId(pub u64);

impl fmt::Display for WsId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ws{}", self.0)
    }
}

/// Open a websocket (`ws://` or `wss://`; capability `net`), with extra
/// handshake headers. Resolves once the handshake succeeds; frames then
/// arrive as `ws-message` events and the end as `ws-closed` (both
/// subscribed here). A failed handshake fails the future.
pub async fn ws_open(url: &str, headers: &[(&str, &str)]) -> Result<WsId, HostError> {
    subscribe(&[wire::EVENT_MESSAGE, wire::EVENT_CLOSED])?;
    let fut = {
        let owned: Vec<(String, String)> =
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let joined = join_headers(&owned);
        let hdr = joined.as_str().to_tmux();
        let url = url.to_tmux();
        let (up, ul) = url.parts();
        let (hp, hl) = if joined.is_empty() { (0, 0) } else { hdr.parts() };
        start_async(unsafe { raw::ws_open(up, ul, hp, hl) })?
    };
    let Completion { v0, .. } = fut.await?;
    Ok(WsId(v0 as u64))
}

fn ws_send(ws: WsId, data: &[u8], text: bool) -> Result<(), HostError> {
    check(unsafe {
        raw::ws_send(ws.0 as i64, data.as_ptr() as i32, data.len() as i32, i32::from(text))
    })
}

/// Queue a text frame. Fails with `E_NO_SUCH_OBJECT` once the socket is
/// closed or closing, `E_LIMIT` when 256 frames are already waiting.
pub fn ws_send_text(ws: WsId, text: &str) -> Result<(), HostError> {
    ws_send(ws, text.as_bytes(), true)
}

/// Queue a binary frame (see [`ws_send_text`] for the errors).
pub fn ws_send_binary(ws: WsId, data: &[u8]) -> Result<(), HostError> {
    ws_send(ws, data, false)
}

/// Start a graceful close. A `ws-closed` event still follows; the handle
/// is dead after it.
pub fn ws_close(ws: WsId) -> Result<(), HostError> {
    check(unsafe { raw::ws_close(ws.0 as i64) })
}

/// A `ws-message` event: one frame from the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsMessage {
    pub socket: WsId,
    /// A text frame (UTF-8) rather than a binary one.
    pub text: bool,
    pub data: Vec<u8>,
}

impl WsMessage {
    /// Decode a `ws-message` event; None for any other event.
    pub fn from_event(event: &Event) -> Option<WsMessage> {
        if !event.is(wire::EVENT_MESSAGE) {
            return None;
        }
        Some(WsMessage {
            socket: WsId(event.get_i64(wire::F_SOCKET)? as u64),
            text: event.get_i64(wire::F_TEXT)? != 0,
            data: event.get_bytes(wire::F_DATA)?.to_vec(),
        })
    }

    /// The payload as text (lossy for a binary frame).
    pub fn as_text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }

    /// The payload as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, HostError> {
        serde_json::from_slice(&self.data).map_err(|e| HostError {
            code: ErrorCode::BadRequest,
            message: format!("decode: {e}"),
        })
    }
}

/// A `ws-closed` event: the socket is gone. `code` is the peer's close
/// code, or 0 when the transport failed (`reason` is then the error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsClosed {
    pub socket: WsId,
    pub code: u16,
    pub reason: String,
}

impl WsClosed {
    /// Decode a `ws-closed` event; None for any other event.
    pub fn from_event(event: &Event) -> Option<WsClosed> {
        if !event.is(wire::EVENT_CLOSED) {
            return None;
        }
        Some(WsClosed {
            socket: WsId(event.get_i64(wire::F_SOCKET)? as u64),
            code: event.get_i64(wire::F_CODE).unwrap_or(0).clamp(0, u16::MAX as i64) as u16,
            reason: event.get_str(wire::F_REASON).unwrap_or("").to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_encode(&[("channel", "C1"), ("text", "hi there & bye"), ("x", "a=b/é")]),
            "channel=C1&text=hi+there+%26+bye&x=a%3Db%2F%C3%A9"
        );
    }

    #[test]
    fn headers_join_and_response_split() {
        let req = HttpRequest::post("https://slack.com/api/x")
            .header("Authorization", "Bearer t")
            .header("X-Bad\nName", "v\r\nInjected: yes")
            .form(&[("a", "1")]);
        let w = req.wire();
        assert_eq!(
            w.headers,
            "Authorization: Bearer t\nX-BadName: vInjected: yes\nContent-Type: application/x-www-form-urlencoded\n"
        );
        assert_eq!(w.body, b"a=1");
        let c = Completion { v0: 201, v1: 26, data: b"content-type: text/x\nx: y\nbody bytes".to_vec() };
        let r = HttpResponse::from_completion(c).unwrap();
        assert_eq!(r.status, 201);
        assert_eq!(r.header("Content-Type"), Some("text/x"));
        assert_eq!(r.headers.len(), 2);
        assert_eq!(r.text(), "body bytes");
    }
}
