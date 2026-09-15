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

/// Flags for the `panes_search` import (the `flags` word). All bits clear
/// is the common case: a case-insensitive plain-substring search.
pub mod search_flags {
    /// The low two bits select the matcher.
    pub const MODE_MASK: u32 = 0x3;
    /// Plain substring (SIMD `memmem`).
    pub const MODE_PLAIN: u32 = 0;
    /// POSIX extended regex (`regcomp`/`regexec`).
    pub const MODE_REGEX: u32 = 1;
    /// Fuzzy: score every line, return the best per pane.
    pub const MODE_FUZZY: u32 = 2;
    /// Match case-sensitively. Default (bit clear) is case-insensitive.
    pub const CASE_SENSITIVE: u32 = 1 << 2;
    /// Regex only: let `.` match a newline. Reserved.
    pub const MULTILINE: u32 = 1 << 3;
}

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

/// What half of a plugin an instance runs. A `Provider` sees one server
/// (its own) and answers service calls; a `View` merges what providers
/// report and owns the UI; `Both` runs the two halves in one instance, the
/// usual case on the local server. A remote server runs pushed plugins as
/// providers. Wire values: Both = 0 (an old host writes no role and a
/// zeroed field reads as Both), View = 1, Provider = 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    #[default]
    Both,
    View,
    Provider,
}

impl Role {
    pub fn as_num(self) -> u32 {
        match self {
            Role::Both => 0,
            Role::View => 1,
            Role::Provider => 2,
        }
    }

    pub fn from_num(n: u32) -> Role {
        match n {
            1 => Role::View,
            2 => Role::Provider,
            _ => Role::Both,
        }
    }

    /// Does this role run the provider half (services, detection)?
    pub fn provides(self) -> bool {
        matches!(self, Role::Provider | Role::Both)
    }

    /// Does this role run the view half (UI, merging)?
    pub fn views(self) -> bool {
        matches!(self, Role::View | Role::Both)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::Both => write!(f, "both"),
            Role::View => write!(f, "view"),
            Role::Provider => write!(f, "provider"),
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
    /// `() -> i64`: the plugin's service version, packed by
    /// [`Version::pack`]. Optional; a plugin without it is unversioned.
    pub const SERVICE_VERSION: &str = "pgh_service_version";
    /// `(ptr, len) -> i32`: does this plugin accept a peer's copy of
    /// itself? The field block carries `server`, `version` and `role`
    /// (the peer copy's role as a number). 1 = accept, 0 = reject.
    /// Optional; without it the host applies [`Version::compatible`].
    pub const SERVICE_ACCEPT: &str = "pgh_service_accept";
}

pub mod db;

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
/// mode_resize(mode: i64, width, height) -> i32   // content cells, clamped
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
/// fs_list(path_ptr, path_len, flags, out_ptr, out_cap) -> i64
///                       // async; out PINNED; v0 = bytes, v1 = entries found
///                       // flags: 1 = mtime, 2 = directories only
///                       // async; out PINNED; v0 = bytes read, v1 = eof
/// fs_rename(from_ptr, from_len, to_ptr, to_len, flags) -> i64
///                       // async; atomic within one filesystem
///                       // flags: 0 = replace, 1 = no-replace, 2 = exchange
/// fs_remove(path_ptr, path_len) -> i64          // async; unlink a file
/// fs_write_sync(path_ptr, path_len, data_ptr, data_len, append) -> i64
/// fs_read_sync(path_ptr, path_len, offset: i64, out, cap,
///              len_out, eof_out) -> i32
/// fs_root(out, cap, len_out) -> i32              // the data dir's abs path
/// home_dir(out, cap, len_out) -> i32            // the server user's home
/// time_now() -> i64                             // Unix time, milliseconds
///
/// // database (capability db; the plugin's own SQLite file; SQL and
/// // blocks are raw bytes, host-consumed, no NUL rule; see `db`):
/// db_exec(sql_ptr, sql_len, params_ptr, params_len) -> i64
///                       // async; v0 = changes, v1 = last_insert_rowid
///                       // params count 0 => multi-statement script allowed
/// db_query(sql_ptr, sql_len, params_ptr, params_len) -> i64
///                       // async; data = rows block, v0 = nrows, v1 = ncols
/// db_batch(block_ptr, block_len) -> i64
///                       // async; ONE transaction; v0 = total changes,
///                       // v1 = last_insert_rowid after the last statement
/// db_exec_sync(sql_ptr, sql_len, params_ptr, params_len, out_ptr) -> i32
///                       // out_ptr -> 16-byte exec struct
/// db_query_sync(sql_ptr, sql_len, params_ptr, params_len, owned_out) -> i32
///                       // OwnedBuf = rows block
/// db_decompress(src_ptr, src_len, owned_out) -> i32
///                       // OwnedBuf = the bytes behind a stored zstd frame
///                       // (a BLOB written from a ZSTD_REF parameter)
///
/// // services (capabilities service-serve / service-call; payloads are
/// // raw bytes the plugin defines; a target is "plugin" or
/// // "plugin@server", where server is "local" or a linked server's name):
/// service_register(method_ptr, method_len) -> i32
/// service_call(target_ptr, target_len, method_ptr, method_len,
///              payload_ptr, payload_len) -> i64
///                       // async; each page completes with v0 = page
///                       // index, v1 = flags (bit 0 MORE: another page
///                       // follows and the token stays alive), data =
///                       // payload; an error reply completes with err
/// service_reply(call: i64, payload_ptr, payload_len, flags) -> i32
///                       // flags bit 0 = MORE, bit 1 = ERROR (payload is
///                       // the message)
/// service_cancel(token: i64) -> i32
/// service_emit(topic_ptr, topic_len, payload_ptr, payload_len) -> i32
/// service_subscribe(target_ptr, target_len, topic_ptr, topic_len) -> i32
/// servers(owned_out) -> i32                     // list of server records
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
    /// pane_env(pane, name_ptr, name_len, out, cap, len_out) -> i32
    ///                       // read one env var from the pane's
    ///                       // foreground process; -2 = no such var
    pub const PANE_ENV: &str = "pane_env";
    /// pane_fds(pane, out, cap, len_out) -> i32
    ///                       // the open-file paths of the pane's
    ///                       // foreground process, one per line;
    ///                       // -2 = none
    pub const PANE_FDS: &str = "pane_fds";
    /// panes_search(ids_ptr, ids_len, pat_ptr, pat_len, flags,
    ///              max_lines, owned_out) -> i32
    ///                       // grep the grids of ids_len panes for a
    ///                       // pattern; result is a u32-count list of
    ///                       // {pane:u32, line:u32, col:u32, score:u32,
    ///                       // snippet} records, one per matching pane.
    ///                       // See `search_flags` for the flags word
    ///                       // (matcher mode + case). `score` ranks fuzzy
    ///                       // hits; it is 0 for plain/regex.
    pub const PANES_SEARCH: &str = "panes_search";
    /// pane_pid(pane) -> i64  // foreground process-group pid; <=0 = gone
    pub const PANE_PID: &str = "pane_pid";
    pub const DISPLAY_MESSAGE: &str = "display_message";
    pub const TIMER_CANCEL: &str = "timer_cancel";
    pub const MODE_OPEN: &str = "mode_open";
    pub const MODE_WRITE: &str = "mode_write";
    pub const MODE_PREVIEW: &str = "mode_preview";
    pub const MODE_MOVE: &str = "mode_move";
    pub const MODE_RESIZE: &str = "mode_resize";
    pub const MODE_CLOSE: &str = "mode_close";
    pub const LAST_ERROR: &str = "last_error";
    pub const LOG: &str = "log";

    pub const RUN_JOB: &str = "run_job";
    pub const RUN_COMMAND: &str = "run_command";
    pub const TIMER_START: &str = "timer_start";

    pub const FS_WRITE: &str = "fs_write";
    pub const FS_READ: &str = "fs_read";
    pub const FS_LIST: &str = "fs_list";
    pub const FS_WRITE_SYNC: &str = "fs_write_sync";
    pub const FS_READ_SYNC: &str = "fs_read_sync";
    pub const FS_RENAME: &str = "fs_rename";
    pub const FS_REMOVE: &str = "fs_remove";
    pub const FS_ROOT: &str = "fs_root";
    pub const HOME_DIR: &str = "home_dir";
    pub const TIME_NOW: &str = "time_now";

    pub const DB_EXEC: &str = "db_exec";
    pub const DB_QUERY: &str = "db_query";
    pub const DB_BATCH: &str = "db_batch";
    pub const DB_EXEC_SYNC: &str = "db_exec_sync";
    pub const DB_QUERY_SYNC: &str = "db_query_sync";
    pub const DB_DECOMPRESS: &str = "db_decompress";

    pub const SERVICE_REGISTER: &str = "service_register";
    pub const SERVICE_CALL: &str = "service_call";
    pub const SERVICE_REPLY: &str = "service_reply";
    pub const SERVICE_CANCEL: &str = "service_cancel";
    pub const SERVICE_EMIT: &str = "service_emit";
    pub const SERVICE_SUBSCRIBE: &str = "service_subscribe";
    pub const SERVERS: &str = "servers";
}

/// Flag bits of a service reply page (`service_reply` flags, and `v1` of
/// the completion the caller receives).
pub mod service_flags {
    /// Another page follows; the caller's token stays alive.
    pub const MORE: u32 = 1 << 0;
    /// The payload is an error message; the call fails with E_HOST.
    pub const ERROR: u32 = 1 << 1;
}

/// The name of the local server in service targets and server records.
pub const LOCAL_SERVER: &str = "local";

/// Field names of the `service-request` and `service-event` events. The
/// host writes `call` FIRST in a service-request so a guest that reads the
/// raw buffer finds it at a fixed offset.
pub mod service_fields {
    pub const CALL: &str = "call";
    pub const METHOD: &str = "method";
    pub const FROM_SERVER: &str = "from_server";
    pub const FROM_PLUGIN: &str = "from_plugin";
    pub const PAYLOAD: &str = "payload";
    pub const PLUGIN: &str = "plugin";
    pub const TOPIC: &str = "topic";
    pub const SEQ: &str = "seq";
    pub const SERVER: &str = "server";
    pub const VERSION: &str = "version";
    pub const ROLE: &str = "role";
}

/// A plugin's service version: the shape of its methods and topics, as
/// `major.minor.patch`. Two copies of a plugin talk only when they accept
/// each other; the default rule is [`Version::compatible`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Version {
        Version { major, minor, patch }
    }

    /// Parse `major.minor.patch`; a missing minor or patch reads as 0, a
    /// `-pre` or `+build` tail is ignored.
    pub fn parse(text: &str) -> Option<Version> {
        let core = text.trim().split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
        let patch = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch })
    }

    /// The semver rule: the same major, and for major 0 also the same
    /// minor. Patch never matters.
    pub fn compatible(self, other: Version) -> bool {
        self.major == other.major && (self.major != 0 || self.minor == other.minor)
    }

    /// One i64 for the `pgh_service_version` export: major in the high
    /// 32 bits, minor and patch in 16 bits each.
    pub fn pack(self) -> i64 {
        (i64::from(self.major) << 32)
            | (i64::from(self.minor.min(0xffff)) << 16)
            | i64::from(self.patch.min(0xffff))
    }

    pub fn unpack(v: i64) -> Version {
        Version {
            major: (v >> 32) as u32,
            minor: ((v >> 16) & 0xffff) as u32,
            patch: (v & 0xffff) as u32,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A server as listed by the `servers` import: the local server (id 0,
/// name "local") and every linked remote server. `version` is that
/// server's service version of the calling plugin ("" when the server
/// has no copy of it); `accepted` says whether this side talks to it.
///
///   record := u32 id, str name, u32 flags, str version
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub id: u32,
    pub name: String,
    pub up: bool,
    pub local: bool,
    pub version: String,
    pub accepted: bool,
}

pub mod server_flags {
    pub const UP: u32 = 1 << 0;
    pub const LOCAL: u32 = 1 << 1;
    pub const ACCEPTED: u32 = 1 << 2;
}

impl ServerInfo {
    /// The local server's own record.
    pub fn local(version: &str) -> ServerInfo {
        ServerInfo {
            id: 0,
            name: LOCAL_SERVER.into(),
            up: true,
            local: true,
            version: version.into(),
            accepted: true,
        }
    }

    pub fn parse(c: &mut Cursor<'_>) -> Result<Self, WireError> {
        let id = c.u32()?;
        let name = c.str()?.to_string();
        let flags = c.u32()?;
        let version = c.str()?.to_string();
        Ok(Self {
            id,
            name,
            up: flags & server_flags::UP != 0,
            local: flags & server_flags::LOCAL != 0,
            version,
            accepted: flags & server_flags::ACCEPTED != 0,
        })
    }

    pub fn emit(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.to_le_bytes());
        emit_str(out, &self.name);
        let mut flags = 0u32;
        if self.up {
            flags |= server_flags::UP;
        }
        if self.local {
            flags |= server_flags::LOCAL;
        }
        if self.accepted {
            flags |= server_flags::ACCEPTED;
        }
        out.extend_from_slice(&flags.to_le_bytes());
        emit_str(out, &self.version);
    }
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
    /// The server a call or subscription names is not linked or its link
    /// is down.
    #[serde(rename = "E_UNREACHABLE")]
    Unreachable,
    /// A service call got no reply within its deadline.
    #[serde(rename = "E_TIMEOUT")]
    Timeout,
    /// The two copies of a plugin do not accept each other's service
    /// version (see `Version`).
    #[serde(rename = "E_VERSION")]
    Version,
    /// The peer is not granted to call this plugin here (see the peer
    /// grants; `plugin-peers`).
    #[serde(rename = "E_DENIED")]
    Denied,
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
            ErrorCode::Unreachable => 10,
            ErrorCode::Timeout => 11,
            ErrorCode::Version => 12,
            ErrorCode::Denied => 13,
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
            10 => ErrorCode::Unreachable,
            11 => ErrorCode::Timeout,
            12 => ErrorCode::Version,
            13 => ErrorCode::Denied,
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
            ErrorCode::Unreachable => "E_UNREACHABLE",
            ErrorCode::Timeout => "E_TIMEOUT",
            ErrorCode::Version => "E_VERSION",
            ErrorCode::Denied => "E_DENIED",
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
    #[serde(default)]
    pub role: Role,
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
    /// Raw bytes: u32 len + bytes, no UTF-8 rule. Service payloads ride
    /// in events with this tag.
    pub const BYTES: u8 = 6;
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
    Bytes(&'a [u8]),
}

impl<'a> ValueRef<'a> {
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            ValueRef::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The raw bytes of a BYTES field (a STR field also answers, as its
    /// UTF-8 bytes).
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            ValueRef::Bytes(b) => Some(b),
            ValueRef::Str(s) => Some(s.as_bytes()),
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

    pub fn bytes(&mut self, key: KeyRef<'_>, v: &[u8]) {
        self.key(key);
        self.buf.push(tag::BYTES);
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v);
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

    /// u32-length-prefixed raw bytes (no UTF-8 requirement).
    pub fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let len = self.u32()? as usize;
        self.take(len)
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
        tag::BYTES => ValueRef::Bytes(c.bytes()?),
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
    /// The pane mirrors a pane on another server (a remote link).
    pub const REMOTE: u8 = 1 << 3;
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
    /// The pane is a shadow of a pane on another server; `host` names it.
    pub remote: bool,
    /// Empty string = absent.
    pub title: String,
    pub shell: String,
    /// For a remote pane, the remote's current path (cached by the link).
    pub cwd: String,
    /// The remote host for a remote pane, else empty.
    pub host: String,
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
        let host = c.str()?.to_string();
        Ok(Self {
            id,
            window,
            width,
            height,
            active: flags & pane_flags::ACTIVE != 0,
            floating: flags & pane_flags::FLOATING != 0,
            dead: flags & pane_flags::DEAD != 0,
            remote: flags & pane_flags::REMOTE != 0,
            title,
            shell,
            cwd,
            host,
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
        if self.remote {
            flags |= pane_flags::REMOTE;
        }
        out.push(flags);
        emit_str(out, &self.title);
        emit_str(out, &self.shell);
        emit_str(out, &self.cwd);
        emit_str(out, &self.host);
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

/// `self_info` out-struct: 24 packed LE bytes
/// { scope_kind: i32, scope_id: u32, generation: u64, role: u32, pad: u32 }.
/// A guest built for the 16-byte form reads the first three fields and
/// keeps working; a new guest on an old host reads role 0 = Both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfInfo {
    pub scope_kind: i32,
    pub scope_id: u32,
    pub generation: u64,
    pub role: Role,
}

pub const SELF_INFO_LEN: usize = 24;

impl SelfInfo {
    pub fn to_bytes(self) -> [u8; SELF_INFO_LEN] {
        let mut out = [0u8; SELF_INFO_LEN];
        out[0..4].copy_from_slice(&self.scope_kind.to_le_bytes());
        out[4..8].copy_from_slice(&self.scope_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.generation.to_le_bytes());
        out[16..20].copy_from_slice(&self.role.as_num().to_le_bytes());
        out
    }

    pub fn from_bytes(buf: &[u8]) -> Result<Self, WireError> {
        let mut c = Cursor::new(buf);
        let scope_kind = c.u32()? as i32;
        let scope_id = c.u32()?;
        let generation = c.u64()?;
        let role = if c.remaining() >= 4 {
            Role::from_num(c.u32()?)
        } else {
            Role::Both
        };
        Ok(Self { scope_kind, scope_id, generation, role })
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
/// Cap on one database request: SQL plus params, or a whole batch block.
/// The host copies the request out of guest memory before the statement
/// runs on a worker thread, so this bounds that copy. A ZSTD_REF param
/// counts its 8-byte reference here, not the bytes it points at.
pub const MAX_DB_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Cap on the raw bytes behind one ZSTD_REF parameter, and on the output
/// of `db_decompress`. The bytes are read in place from pinned guest
/// memory, so this bounds the worker's compression input, not a copy.
pub const MAX_DB_ZSTD_RAW_BYTES: usize = 64 * 1024 * 1024;
/// Cap on one result set (the rows block). It is delivered into the
/// guest through `pgh_alloc`; a query that needs more should page with
/// `LIMIT`/`OFFSET`.
pub const MAX_DB_ROWS_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
mod tests {
    #[test]
    fn version_parse_pack_compat() {
        use super::Version;
        assert_eq!(Version::parse("0.1.0"), Some(Version::new(0, 1, 0)));
        assert_eq!(Version::parse("2"), Some(Version::new(2, 0, 0)));
        assert_eq!(Version::parse("1.4.7-rc1"), Some(Version::new(1, 4, 7)));
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("x"), None);
        let v = Version::new(3, 70000, 5);
        assert_eq!(Version::unpack(v.pack()), Version::new(3, 0xffff, 5));
        assert!(Version::new(1, 2, 0).compatible(Version::new(1, 9, 3)));
        assert!(!Version::new(1, 2, 0).compatible(Version::new(2, 0, 0)));
        assert!(Version::new(0, 1, 0).compatible(Version::new(0, 1, 9)));
        assert!(!Version::new(0, 1, 0).compatible(Version::new(0, 2, 0)));
        assert_eq!(Version::new(0, 1, 0).to_string(), "0.1.0");
    }

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
            remote: true,
            title: "t".into(),
            shell: "/bin/zsh".into(),
            cwd: "/home/x".into(),
            host: "devbox".into(),
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
        let si = SelfInfo { scope_kind: KIND_PANE, scope_id: 12, generation: 9, role: Role::Provider };
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
