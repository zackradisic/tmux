//! Shared ABI definitions for the tmux plugin system.
//!
//! This crate is compiled for both the host (native, inside the tmux
//! server) and the guest (wasm32-unknown-unknown, via the SDK), so the
//! wire formats defined here cannot drift between the two sides.
//!
//! The ABI is C-like: one typed wasm import per method, scalars only.
//! Strings and buffers cross as flattened (ptr, len) pairs; results come
//! back through caller-provided out-buffers or ownership-transferred
//! allocations. There is no JSON and no base64 anywhere on the call path.
//!
//! ## Buffer taxonomy
//!
//! | shape      | direction    | wire form            | contract |
//! |------------|--------------|----------------------|----------|
//! | `Str`      | guest→host   | `ptr, len` scalars   | borrowed for the call; UTF-8; **NUL byte at `data[len]`** (so the host can pass `base+ptr` straight into C) |
//! | `Bytes`    | guest→host   | `ptr, len` scalars   | borrowed for the call; raw bytes, no NUL |
//! | pinned     | guest→host   | `ptr, len` scalars   | async input: the SDK future owns the buffer until the completion arrives; the host/worker may read it after the call returns |
//! | `OutBuf`   | host→guest   | `ptr, cap, len_out_ptr` | caller-provided output; host writes data and the length; `-E_LIMIT` with the needed size in `len_out` when it does not fit |
//! | `OwnedBuf` | host→guest   | out-ptr to `{ptr: u32, len: u32}` | host allocates via `pgh_alloc` (exactly `len`), guest frees `(ptr, len)` via `pgh_free` |
//!
//! ## Memory rules
//!
//! - Request buffers are guest-owned; the host consumes them before the
//!   import returns (async fs inputs are the one exception: pinned by the
//!   SDK future until completion).
//! - Host-written payloads (events, config, migrate state, completions,
//!   `OwnedBuf` results) are allocated with `pgh_alloc` and freed by the
//!   guest with the exact same size.
//! - The host never caches a raw guest pointer across a guest call.

use serde::{Deserialize, Serialize};
use std::fmt;

/// ABI version spoken by this host. Guests export `pgh_abi_version()` and
/// must return the same value to be loaded.
pub const ABI_VERSION: i32 = 1;

/// Sentinel for "no object" in u32 id slots (tmux ids are monotonic and
/// never reach this).
pub const NONE_ID: u32 = u32::MAX;

/// Object kind codes, shared with the C vtable (PGH_OBJ_*).
pub const KIND_SERVER: i32 = -1;
pub const KIND_SESSION: i32 = 0;
pub const KIND_WINDOW: i32 = 1;
pub const KIND_PANE: i32 = 2;
pub const KIND_CLIENT: i32 = 3;

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

/// Names of the wasm exports a guest must (or may) provide.
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

/// Host import module name and per-method function names.
///
/// Signatures (wasm core types; `u32` values travel as `i32` bit-casts):
///
/// ```text
/// // strings: (ptr, len) with NUL at data[len]; len 0 + ptr 0 = absent
/// // OutBuf:  (out_ptr, out_cap, len_out_ptr)
/// // OwnedBuf out-param: ptr to 8 bytes {ptr: u32, len: u32}
///
/// intern(ptr, len) -> i64                          // key/event id > 0, or -err
///                                                  // (raw UTF-8; host-consumed, no NUL needed)
/// intern_name(id, out, cap, len_out) -> i32
/// subscribe(event_id) -> i32
/// unsubscribe(event_id) -> i32
/// list(kind, owned_out) -> i32                     // object list buffer
/// resolve(kind, id, owned_out) -> i32              // one object record
/// self_info(out_ptr) -> i32                        // 16-byte SelfInfo
/// get_option(kind, id, name_ptr, name_len, out, cap, len_out) -> i32
/// set_option(kind, id, name_ptr, name_len, val_ptr, val_len) -> i32
/// format_expand(kind, id, fmt_ptr, fmt_len, out, cap, len_out) -> i32
///                                                  // #{...} against the scope; #() disabled
/// send_keys(pane, keys_ptr, keys_len, literal) -> i32
/// capture_pane(pane, start, end, escapes, out, cap, len_out) -> i32
/// display_message(client /* -1 = all */, msg_ptr, msg_len) -> i32
/// timer_cancel(token: i64) -> i32
/// mode_open(window /* -1 = default */, width, height, x, y,
///           title_ptr, title_len) -> i64           // mode id > 0, or -err
/// mode_write(mode: i64, data_ptr, data_len) -> i32
/// mode_preview(mode: i64, pane: i64 /* -1 = clear */, x, y, w, h) -> i32
/// mode_move(mode: i64, window /* -1 = default */, x, y) -> i32
/// mode_close(mode: i64) -> i32
/// last_error(out, cap, len_out) -> i32             // message of the last error
/// log(level, ptr, len)
///
/// // async: return token > 0, or -err; completion via pgh_on_async_complete
/// run_job(cmd_ptr, cmd_len, cwd_ptr, cwd_len) -> i64
/// run_command(cmd_ptr, cmd_len) -> i64
/// timer_start(ms: i64) -> i64
///
/// // filesystem (sandboxed to the plugin data dir; paths raw UTF-8):
/// fs_write(path_ptr, path_len, data_ptr, data_len, append) -> i64
///                       // async; data PINNED by the SDK future; v0 = bytes
/// fs_read(path_ptr, path_len, offset: i64, out_ptr, out_cap) -> i64
///                       // async; out PINNED; v0 = bytes read, v1 = eof
/// fs_write_sync(path_ptr, path_len, data_ptr, data_len, append) -> i64
/// fs_read_sync(path_ptr, path_len, offset: i64, out, cap,
///              len_out, eof_out) -> i32
/// fs_root(out, cap, len_out) -> i32              // the data dir's abs path
/// ```
pub mod imports {
    pub const MODULE: &str = "tmux";

    pub const INTERN: &str = "intern";
    pub const INTERN_NAME: &str = "intern_name";
    pub const SUBSCRIBE: &str = "subscribe";
    pub const UNSUBSCRIBE: &str = "unsubscribe";
    pub const LIST: &str = "list";
    pub const RESOLVE: &str = "resolve";
    pub const SELF_INFO: &str = "self_info";
    pub const GET_OPTION: &str = "get_option";
    pub const SET_OPTION: &str = "set_option";
    pub const FORMAT_EXPAND: &str = "format_expand";
    pub const SEND_KEYS: &str = "send_keys";
    pub const CAPTURE_PANE: &str = "capture_pane";
    pub const DISPLAY_MESSAGE: &str = "display_message";
    pub const TIMER_CANCEL: &str = "timer_cancel";
    pub const MODE_OPEN: &str = "mode_open";
    pub const MODE_WRITE: &str = "mode_write";
    pub const MODE_PREVIEW: &str = "mode_preview";
    pub const MODE_MOVE: &str = "mode_move";
    pub const MODE_CLOSE: &str = "mode_close";
    pub const LAST_ERROR: &str = "last_error";
    pub const LOG: &str = "log";

    pub const RUN_JOB: &str = "run_job";
    pub const RUN_COMMAND: &str = "run_command";
    pub const TIMER_START: &str = "timer_start";

    pub const FS_WRITE: &str = "fs_write";
    pub const FS_READ: &str = "fs_read";
    pub const FS_WRITE_SYNC: &str = "fs_write_sync";
    pub const FS_READ_SYNC: &str = "fs_read_sync";
    pub const FS_ROOT: &str = "fs_root";
}

/// Structured error codes. Sync imports return `-code`; `host_request`-style
/// imports return `-code` instead of a token; async completions carry the
/// code in the `err` parameter. The numbers are pinned wire values.
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
    /// Stable numeric code (wire value).
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

    pub fn from_num(num: i32) -> ErrorCode {
        match num {
            1 => ErrorCode::BadRequest,
            2 => ErrorCode::UnknownMethod,
            3 => ErrorCode::CapDenied,
            4 => ErrorCode::NoSuchObject,
            5 => ErrorCode::OutOfScope,
            6 => ErrorCode::Limit,
            8 => ErrorCode::Cancelled,
            9 => ErrorCode::Unsupported,
            _ => ErrorCode::Host,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "E_BAD_REQUEST",
            ErrorCode::UnknownMethod => "E_UNKNOWN_METHOD",
            ErrorCode::CapDenied => "E_CAP_DENIED",
            ErrorCode::NoSuchObject => "E_NO_SUCH_OBJECT",
            ErrorCode::OutOfScope => "E_OUT_OF_SCOPE",
            ErrorCode::Limit => "E_LIMIT",
            ErrorCode::Host => "E_HOST",
            ErrorCode::Cancelled => "E_CANCELLED",
            ErrorCode::Unsupported => "E_UNSUPPORTED",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error surface used by the SDK: the code from the negative return plus
/// the host's last-error message (fetched via the `last_error` import).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.name(), self.message)
    }
}

/// Descriptor for pgh_plugin_load (built by the host from load-plugin
/// arguments or a sync-plugins manifest entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadDescriptor {
    pub name: String,
    pub path: String,
    #[serde(default = "default_scope")]
    pub scope: ScopeType,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub caps: Vec<String>,
}

fn default_scope() -> ScopeType {
    ScopeType::Server
}

// ---------------------------------------------------------------------------
// Field block: the flat key/value binary format used for event payloads,
// mode-event payloads and plugin config. All integers little-endian,
// unaligned, packed.
//
//   block := u16 count, count * field
//   field := u32 key_id            (0 = inline key: u32 len + bytes follow)
//            u8 tag
//            value                 (per tag, see Tag)
// ---------------------------------------------------------------------------

/// Value tags in a field block.
pub mod tag {
    pub const NULL: u8 = 0;
    pub const BOOL: u8 = 1; // u8
    pub const I64: u8 = 2; // 8 bytes LE
    pub const F64: u8 = 3; // 8 bytes LE
    pub const STR: u8 = 4; // u32 len + bytes (UTF-8, no NUL)
    /// Escape hatch for nested values (arrays/objects in plugin config):
    /// the bytes are JSON text, interpreted by the guest SDK only. Never
    /// used by events.
    pub const JSON: u8 = 5;
}

/// A field key: interned id (events) or inline name (config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRef<'a> {
    Id(u32),
    Name(&'a str),
}

/// A decoded field value borrowing the underlying buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueRef<'a> {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(&'a str),
    Json(&'a str),
}

impl<'a> ValueRef<'a> {
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            ValueRef::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ValueRef::I64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ValueRef::Bool(v) => Some(*v),
            _ => None,
        }
    }
}

/// Append-only writer for a field block (used by the host and by tests;
/// the C side has its own emitter in plugin-buf.c).
#[derive(Default)]
pub struct FieldWriter {
    buf: Vec<u8>,
    count: u16,
}

impl FieldWriter {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(&mut self, key: KeyRef<'_>) {
        self.count = self.count.saturating_add(1);
        match key {
            KeyRef::Id(id) => {
                debug_assert!(id != 0, "interned ids start at 1");
                self.buf.extend_from_slice(&id.to_le_bytes());
            }
            KeyRef::Name(name) => {
                self.buf.extend_from_slice(&0u32.to_le_bytes());
                self.buf
                    .extend_from_slice(&(name.len() as u32).to_le_bytes());
                self.buf.extend_from_slice(name.as_bytes());
            }
        }
    }

    pub fn null(&mut self, key: KeyRef<'_>) {
        self.key(key);
        self.buf.push(tag::NULL);
    }

    pub fn bool(&mut self, key: KeyRef<'_>, v: bool) {
        self.key(key);
        self.buf.push(tag::BOOL);
        self.buf.push(u8::from(v));
    }

    pub fn i64(&mut self, key: KeyRef<'_>, v: i64) {
        self.key(key);
        self.buf.push(tag::I64);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn f64(&mut self, key: KeyRef<'_>, v: f64) {
        self.key(key);
        self.buf.push(tag::F64);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn str_with_tag(&mut self, key: KeyRef<'_>, t: u8, v: &str) {
        self.key(key);
        self.buf.push(t);
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v.as_bytes());
    }

    pub fn str(&mut self, key: KeyRef<'_>, v: &str) {
        self.str_with_tag(key, tag::STR, v);
    }

    pub fn json(&mut self, key: KeyRef<'_>, v: &str) {
        self.str_with_tag(key, tag::JSON, v);
    }

    /// Finish: u16 count header + fields.
    pub fn finish(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + self.buf.len());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}

/// Bounds-checked little-endian cursor over a byte buffer.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or(WireError::Truncated)?;
        if end > self.buf.len() {
            return Err(WireError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /// u32-length-prefixed UTF-8 string (lossless check; invalid UTF-8 is a
    /// wire error rather than silent mangling).
    pub fn str(&mut self) -> Result<&'a str, WireError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes).map_err(|_| WireError::BadUtf8)
    }
}

/// Wire decoding failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    Truncated,
    BadUtf8,
    BadTag(u8),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated => write!(f, "truncated buffer"),
            WireError::BadUtf8 => write!(f, "invalid UTF-8 in string"),
            WireError::BadTag(t) => write!(f, "unknown value tag {t}"),
        }
    }
}

/// Iterate the fields of a field block.
pub struct FieldReader<'a> {
    cursor: Cursor<'a>,
    remaining: u16,
}

impl<'a> FieldReader<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self, WireError> {
        let mut cursor = Cursor::new(buf);
        let remaining = cursor.u16()?;
        Ok(Self { cursor, remaining })
    }

    /// Continue reading fields from an existing cursor (event payloads:
    /// the field block follows the event header in the same buffer).
    pub fn from_cursor(mut cursor: Cursor<'a>) -> Result<Self, WireError> {
        let remaining = cursor.u16()?;
        Ok(Self { cursor, remaining })
    }
}

impl<'a> Iterator for FieldReader<'a> {
    type Item = Result<(KeyRef<'a>, ValueRef<'a>), WireError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(read_field(&mut self.cursor))
    }
}

fn read_field<'a>(
    c: &mut Cursor<'a>,
) -> Result<(KeyRef<'a>, ValueRef<'a>), WireError> {
    let id = c.u32()?;
    let key = if id == 0 {
        KeyRef::Name(c.str()?)
    } else {
        KeyRef::Id(id)
    };
    let t = c.u8()?;
    let value = match t {
        tag::NULL => ValueRef::Null,
        tag::BOOL => ValueRef::Bool(c.u8()? != 0),
        tag::I64 => ValueRef::I64(c.i64()?),
        tag::F64 => ValueRef::F64(c.f64()?),
        tag::STR => ValueRef::Str(c.str()?),
        tag::JSON => ValueRef::Json(c.str()?),
        other => return Err(WireError::BadTag(other)),
    };
    Ok((key, value))
}

// ---------------------------------------------------------------------------
// Event buffer: fixed header + field block.
//
//   event := u32 event_id
//            u64 seq          (0 from C; the host patches it at delivery)
//            u32 client, session, window, pane   (NONE_ID = absent)
//            field block
// ---------------------------------------------------------------------------

/// Byte offset of `seq` in an event buffer (for in-place patching).
pub const EVENT_SEQ_OFFSET: usize = 4;
/// Total event header size, in bytes.
pub const EVENT_HEADER_LEN: usize = 4 + 8 + 4 * 4;

/// Object-id scope attached to an event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EventScope {
    pub client: Option<u32>,
    pub session: Option<u32>,
    pub window: Option<u32>,
    pub pane: Option<u32>,
}

fn opt_id(v: u32) -> Option<u32> {
    if v == NONE_ID {
        None
    } else {
        Some(v)
    }
}

/// Decoded event header; the fields follow in the same buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventHeader {
    pub event_id: u32,
    pub seq: u64,
    pub scope: EventScope,
}

impl EventHeader {
    /// Parse the header and return a cursor positioned at the field block.
    pub fn parse(buf: &[u8]) -> Result<(EventHeader, Cursor<'_>), WireError> {
        let mut c = Cursor::new(buf);
        let event_id = c.u32()?;
        let seq = c.u64()?;
        let scope = EventScope {
            client: opt_id(c.u32()?),
            session: opt_id(c.u32()?),
            window: opt_id(c.u32()?),
            pane: opt_id(c.u32()?),
        };
        Ok((EventHeader { event_id, seq, scope }, c))
    }

    /// Serialize header bytes (seq as given; the C side writes 0).
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.event_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        for id in [
            self.scope.client,
            self.scope.session,
            self.scope.window,
            self.scope.pane,
        ] {
            out.extend_from_slice(&id.unwrap_or(NONE_ID).to_le_bytes());
        }
    }
}

/// Patch the sequence number into an event buffer in place.
pub fn patch_event_seq(buf: &mut [u8], seq: u64) -> Result<(), WireError> {
    let slot = buf
        .get_mut(EVENT_SEQ_OFFSET..EVENT_SEQ_OFFSET + 8)
        .ok_or(WireError::Truncated)?;
    slot.copy_from_slice(&seq.to_le_bytes());
    Ok(())
}

// ---------------------------------------------------------------------------
// Object records: the sequential binary format for list/resolve results.
// Inline u32-length-prefixed strings, no offsets. The C side emits these
// (plugin-buf.c); the guest SDK parses them; the host passes them through
// untouched.
//
//   list := u32 count, count * record        (resolve: one bare record)
//
//   session := u32 id, u8 attached, u32 current_window(NONE_ID),
//              str name, u32 nwindows, nwindows * { u32 index, u32 id }
//   window  := u32 id, u32 width, u32 height, u32 active_pane(NONE_ID),
//              str name, u32 nsessions, nsessions * u32,
//              u32 npanes, npanes * u32
//   pane    := u32 id, u32 window, u32 width, u32 height, u8 flags,
//              str title, str shell, str cwd      (empty = absent)
//   client  := u32 id, u32 session(NONE_ID), u8 flags, str name
// ---------------------------------------------------------------------------

/// Pane record flag bits.
pub mod pane_flags {
    pub const ACTIVE: u8 = 1 << 0;
    pub const FLOATING: u8 = 1 << 1;
    pub const DEAD: u8 = 1 << 2;
}

/// Client record flag bits.
pub mod client_flags {
    pub const ATTACHED: u8 = 1 << 0;
    pub const CONTROL: u8 = 1 << 1;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: u32,
    pub attached: bool,
    pub current_window: Option<u32>,
    pub name: String,
    /// (index, window id) pairs in session order.
    pub windows: Vec<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub active_pane: Option<u32>,
    pub name: String,
    pub sessions: Vec<u32>,
    /// Pane ids in window (TAILQ) order.
    pub panes: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: u32,
    pub window: u32,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub floating: bool,
    pub dead: bool,
    /// Empty string = absent.
    pub title: String,
    pub shell: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: u32,
    pub session: Option<u32>,
    pub attached: bool,
    pub control: bool,
    pub name: String,
}

impl SessionInfo {
    pub fn parse(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        let id = c.u32()?;
        let attached = c.u8()? != 0;
        let current_window = opt_id(c.u32()?);
        let name = c.str()?.to_string();
        let n = c.u32()?;
        let mut windows = Vec::with_capacity(n.min(4096) as usize);
        for _ in 0..n {
            windows.push((c.u32()?, c.u32()?));
        }
        Ok(Self { id, attached, current_window, name, windows })
    }

    pub fn emit(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.push(u8::from(self.attached));
        out.extend_from_slice(
            &self.current_window.unwrap_or(NONE_ID).to_le_bytes(),
        );
        emit_str(out, &self.name);
        out.extend_from_slice(&(self.windows.len() as u32).to_le_bytes());
        for (idx, id) in &self.windows {
            out.extend_from_slice(&idx.to_le_bytes());
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
}

impl WindowInfo {
    pub fn parse(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        let id = c.u32()?;
        let width = c.u32()?;
        let height = c.u32()?;
        let active_pane = opt_id(c.u32()?);
        let name = c.str()?.to_string();
        let n = c.u32()?;
        let mut sessions = Vec::with_capacity(n.min(4096) as usize);
        for _ in 0..n {
            sessions.push(c.u32()?);
        }
        let n = c.u32()?;
        let mut panes = Vec::with_capacity(n.min(4096) as usize);
        for _ in 0..n {
            panes.push(c.u32()?);
        }
        Ok(Self { id, width, height, active_pane, name, sessions, panes })
    }

    pub fn emit(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(
            &self.active_pane.unwrap_or(NONE_ID).to_le_bytes(),
        );
        emit_str(out, &self.name);
        out.extend_from_slice(&(self.sessions.len() as u32).to_le_bytes());
        for id in &self.sessions {
            out.extend_from_slice(&id.to_le_bytes());
        }
        out.extend_from_slice(&(self.panes.len() as u32).to_le_bytes());
        for id in &self.panes {
            out.extend_from_slice(&id.to_le_bytes());
        }
    }
}

impl PaneInfo {
    pub fn parse(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        let id = c.u32()?;
        let window = c.u32()?;
        let width = c.u32()?;
        let height = c.u32()?;
        let flags = c.u8()?;
        let title = c.str()?.to_string();
        let shell = c.str()?.to_string();
        let cwd = c.str()?.to_string();
        Ok(Self {
            id,
            window,
            width,
            height,
            active: flags & pane_flags::ACTIVE != 0,
            floating: flags & pane_flags::FLOATING != 0,
            dead: flags & pane_flags::DEAD != 0,
            title,
            shell,
            cwd,
        })
    }

    pub fn emit(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.window.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        let mut flags = 0u8;
        if self.active {
            flags |= pane_flags::ACTIVE;
        }
        if self.floating {
            flags |= pane_flags::FLOATING;
        }
        if self.dead {
            flags |= pane_flags::DEAD;
        }
        out.push(flags);
        emit_str(out, &self.title);
        emit_str(out, &self.shell);
        emit_str(out, &self.cwd);
    }
}

impl ClientInfo {
    pub fn parse(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        let id = c.u32()?;
        let session = opt_id(c.u32()?);
        let flags = c.u8()?;
        let name = c.str()?.to_string();
        Ok(Self {
            id,
            session,
            attached: flags & client_flags::ATTACHED != 0,
            control: flags & client_flags::CONTROL != 0,
            name,
        })
    }

    pub fn emit(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(
            &self.session.unwrap_or(NONE_ID).to_le_bytes(),
        );
        let mut flags = 0u8;
        if self.attached {
            flags |= client_flags::ATTACHED;
        }
        if self.control {
            flags |= client_flags::CONTROL;
        }
        out.push(flags);
        emit_str(out, &self.name);
    }
}

fn emit_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// Parse a `u32 count`-prefixed list of records.
pub fn parse_list<T>(
    buf: &[u8],
    parse: impl Fn(&mut Cursor<'_>) -> Result<T, WireError>,
) -> Result<Vec<T>, WireError> {
    let mut c = Cursor::new(buf);
    let n = c.u32()?;
    let mut out = Vec::with_capacity(n.min(4096) as usize);
    for _ in 0..n {
        out.push(parse(&mut c)?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Fixed out-structs.
// ---------------------------------------------------------------------------

/// `self_info` out-struct: 16 packed LE bytes
/// { scope_kind: i32, scope_id: u32, generation: u64 }.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfInfo {
    pub scope_kind: i32,
    pub scope_id: u32,
    pub generation: u64,
}

pub const SELF_INFO_LEN: usize = 16;

impl SelfInfo {
    pub fn to_bytes(self) -> [u8; SELF_INFO_LEN] {
        let mut out = [0u8; SELF_INFO_LEN];
        out[0..4].copy_from_slice(&self.scope_kind.to_le_bytes());
        out[4..8].copy_from_slice(&self.scope_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.generation.to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, WireError> {
        let mut c = Cursor::new(buf);
        Ok(Self {
            scope_kind: c.u32()? as i32,
            scope_id: c.u32()?,
            generation: c.u64()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Limits (single source of truth for both sides).
// ---------------------------------------------------------------------------

/// Per-call cap on mode_write's ANSI payload. The bytes are parsed into
/// a pane's screen on the main thread, so this bounds that parse.
///
/// The fs imports are NOT capped: they are bounded by the instance's
/// linear-memory limit, since every fs transfer names a buffer inside it.
pub const MAX_MODE_WRITE_BYTES: usize = 256 * 1024;
/// Cap on captured job output carried in a completion.
pub const MAX_JOB_OUTPUT_BYTES: usize = 256 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_block_round_trip() {
        let mut w = FieldWriter::new();
        w.str(KeyRef::Id(7), "hello");
        w.i64(KeyRef::Id(9), -42);
        w.bool(KeyRef::Name("attached"), true);
        w.null(KeyRef::Id(3));
        w.f64(KeyRef::Name("ratio"), 0.5);
        w.json(KeyRef::Name("nested"), "[1,2]");
        let buf = w.finish();

        let fields: Vec<_> = FieldReader::new(&buf)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[0], (KeyRef::Id(7), ValueRef::Str("hello")));
        assert_eq!(fields[1], (KeyRef::Id(9), ValueRef::I64(-42)));
        assert_eq!(
            fields[2],
            (KeyRef::Name("attached"), ValueRef::Bool(true))
        );
        assert_eq!(fields[3], (KeyRef::Id(3), ValueRef::Null));
        assert_eq!(fields[4], (KeyRef::Name("ratio"), ValueRef::F64(0.5)));
        assert_eq!(
            fields[5],
            (KeyRef::Name("nested"), ValueRef::Json("[1,2]"))
        );
    }

    #[test]
    fn field_block_truncation_is_an_error() {
        let mut w = FieldWriter::new();
        w.str(KeyRef::Id(1), "abcdef");
        let buf = w.finish();
        for cut in 0..buf.len() - 1 {
            let r: Result<Vec<_>, _> = match FieldReader::new(&buf[..cut]) {
                Ok(it) => it.collect(),
                Err(e) => Err(e),
            };
            assert!(r.is_err(), "cut at {cut} should fail");
        }
    }

    #[test]
    fn event_header_round_trip_and_seq_patch() {
        let hdr = EventHeader {
            event_id: 12,
            seq: 0,
            scope: EventScope {
                client: None,
                session: Some(3),
                window: Some(9),
                pane: None,
            },
        };
        let mut buf = Vec::new();
        hdr.write(&mut buf);
        let mut w = FieldWriter::new();
        w.str(KeyRef::Id(2), "main");
        buf.extend_from_slice(&w.finish());

        patch_event_seq(&mut buf, 77).unwrap();
        let (parsed, cursor) = EventHeader::parse(&buf).unwrap();
        assert_eq!(parsed.event_id, 12);
        assert_eq!(parsed.seq, 77);
        assert_eq!(parsed.scope.session, Some(3));
        assert_eq!(parsed.scope.client, None);
        let fields: Vec<_> = FieldReader::from_cursor(cursor)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(fields, vec![(KeyRef::Id(2), ValueRef::Str("main"))]);
    }

    #[test]
    fn object_records_round_trip() {
        let s = SessionInfo {
            id: 1,
            attached: true,
            current_window: Some(4),
            name: "main".into(),
            windows: vec![(0, 4), (1, 5)],
        };
        let w = WindowInfo {
            id: 4,
            width: 200,
            height: 50,
            active_pane: None,
            name: "vim".into(),
            sessions: vec![1],
            panes: vec![7, 8],
        };
        let p = PaneInfo {
            id: 7,
            window: 4,
            width: 100,
            height: 50,
            active: true,
            floating: false,
            dead: false,
            title: "t".into(),
            shell: "/bin/zsh".into(),
            cwd: "/home/x".into(),
        };
        let c = ClientInfo {
            id: 0,
            session: Some(1),
            attached: true,
            control: false,
            name: "/dev/pts/1".into(),
        };

        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_le_bytes());
        s.emit(&mut buf);
        s.emit(&mut buf);
        let list = parse_list(&buf, SessionInfo::parse).unwrap();
        assert_eq!(list, vec![s.clone(), s]);

        for (emit, check) in [
            (
                {
                    let mut b = Vec::new();
                    w.emit(&mut b);
                    b
                },
                WindowInfo::parse(&mut Cursor::new(&{
                    let mut b = Vec::new();
                    w.emit(&mut b);
                    b
                }))
                .map(|got| got == w),
            ),
            (
                {
                    let mut b = Vec::new();
                    p.emit(&mut b);
                    b
                },
                PaneInfo::parse(&mut Cursor::new(&{
                    let mut b = Vec::new();
                    p.emit(&mut b);
                    b
                }))
                .map(|got| got == p),
            ),
            (
                {
                    let mut b = Vec::new();
                    c.emit(&mut b);
                    b
                },
                ClientInfo::parse(&mut Cursor::new(&{
                    let mut b = Vec::new();
                    c.emit(&mut b);
                    b
                }))
                .map(|got| got == c),
            ),
        ] {
            assert!(!emit.is_empty());
            assert_eq!(check, Ok(true));
        }
    }

    #[test]
    fn self_info_round_trip() {
        let si = SelfInfo { scope_kind: KIND_PANE, scope_id: 12, generation: 9 };
        assert_eq!(SelfInfo::from_bytes(&si.to_bytes()), Ok(si));
    }

    #[test]
    fn error_code_numbers_are_pinned() {
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::UnknownMethod,
            ErrorCode::CapDenied,
            ErrorCode::NoSuchObject,
            ErrorCode::OutOfScope,
            ErrorCode::Limit,
            ErrorCode::Host,
            ErrorCode::Cancelled,
            ErrorCode::Unsupported,
        ] {
            assert_eq!(ErrorCode::from_num(code.as_num()), code);
        }
    }
}
