# tmux plugin ABI, version 1

A tmux plugin is a core WebAssembly module (`wasm32-unknown-unknown`). It has
no WASI and no ambient authority: every effect goes through host imports,
gated by capabilities. Payloads are UTF-8 JSON in the guest's linear memory.

Plugins run inside the tmux server on its event loop. Every entry into the
guest runs to completion under a CPU budget (epoch interruption, ~2 ms soft
warning, ~8 ms hard trap). There is no stack suspension: async host
operations complete via a callback export. The Rust SDK
(`plugin-host/sdk`, crate `tmux-plugin-sdk`) builds async/await on top and
hides all of the below.

## Guest exports

Required:

| export | signature | notes |
|---|---|---|
| `memory` | memory | single exported linear memory |
| `pgh_abi_version` | `() -> i32` | must return `1` |
| `pgh_alloc` | `(size: i32) -> i32` | 8-aligned; 0 = OOM (treated as failure) |
| `pgh_free` | `(ptr: i32, size: i32)` | size is echoed back exactly |
| `pgh_init` | `(cfg_ptr: i32, cfg_len: i32) -> i32` | config JSON; nonzero = init failed |
| `pgh_on_event` | `(ptr: i32, len: i32)` | one event JSON |

Optional:

| export | signature | notes |
|---|---|---|
| `pgh_on_async_complete` | `(token: i64, ptr: i32, len: i32, is_error: i32)` | async results |
| `pgh_on_unload` | `()` | tiny budget; best-effort |
| `pgh_state_version` | `() -> i32` | schema version of snapshot bytes |
| `pgh_snapshot` | `(out_ptr_ptr: i32, out_len_ptr: i32) -> i32` | 0 = wrote {ptr,len}; nonzero = stateless |
| `pgh_migrate` | `(old_version: i32, ptr: i32, len: i32) -> i32` | nonzero refuses (old code keeps running) |
| `pgh_on_config_changed` | `(ptr: i32, len: i32) -> i32` | 1 = absorbed, 0 = restart me |

## Host imports (module `"tmux"`)

```
host_call(req_ptr, req_len, out_ptr, out_len_ptr) -> i32
    Synchronous request/response. The host writes the response into a
    buffer obtained from the guest's pgh_alloc and stores {ptr,len} into
    the two 4-byte little-endian out-slots. Returns 0 (ok: response is the
    result), 1 (structured error: response is the error), or 2 (ABI
    failure: nothing written). The guest frees the response buffer.

host_request(req_ptr, req_len) -> i64
    Asynchronous request. Returns a token > 0; the result arrives later
    via pgh_on_async_complete. Negative return = -ErrorCode (rejected
    synchronously, e.g. capability denied).

host_log(level, ptr, len)
    0=debug 1=info 2=warn 3=error. Always permitted. Feeds `plugin-log`.
```

Memory rules: request buffers are guest-owned (host copies out before
returning); host-written payloads (events, responses, config, migrate
state) are allocated with `pgh_alloc` and freed by the guest. `pgh_alloc`
may grow/move memory; the host never caches raw pointers across guest
calls.

## Requests and responses

Request: `{"method": "...", "params": {...}}`
Response: `{"ok": <value>}` or
`{"err": {"code": "E_...", "message": "...", "data": ...}}`

Error codes (numeric value used by negative `host_request` returns):
`E_BAD_REQUEST`(1), `E_UNKNOWN_METHOD`(2), `E_CAP_DENIED`(3),
`E_NO_SUCH_OBJECT`(4), `E_OUT_OF_SCOPE`(5), `E_LIMIT`(6), `E_HOST`(7),
`E_CANCELLED`(8), `E_UNSUPPORTED`(9).

### Synchronous methods (host_call)

| method | params | result | capability |
|---|---|---|---|
| `subscribe` / `unsubscribe` | `{events: [..]}` | `{}` | read-state |
| `list_sessions/windows/panes/clients` | `{}` | array | read-state |
| `resolve` | `{kind, id}` | object | read-state |
| `self` | `{}` | plugin/scope/generation | read-state |
| `get_option` | `{scope?, name}` | `{value}` | read-state |
| `set_option` | `{scope?, name, value}` | `{}` | write-options (@-options only) |
| `send_keys` | `{pane, keys, literal?}` | `{}` | send-keys |
| `capture_pane` | `{pane, start?, end?, escapes?}` | `{text}` | capture-pane (≤2000 lines, ≤256 KiB) |
| `display_message` | `{client?, message}` | `{}` | display-message |
| `timer_cancel` | `{token}` | `{}` | timers |

`scope` is `{"type":"server"}` (default) or
`{"type":"session"|"window"|"pane","id":n}`.

### Asynchronous methods (host_request)

| method | params | completion payload | capability |
|---|---|---|---|
| `run_job` | `{cmd, cwd?}` | `{status, signalled, output}` | run-process |
| `run_command` | `{command}` | `{}` (parse errors as error completion) | run-command |
| `timer_start` | `{ms}` | `{}` on fire | timers |

## Events

```json
{"event": "pane-focus-in", "seq": 42,
 "scope": {"client": 1, "session": 0, "window": 3, "pane": 7},
 "data": {"session_name": "main", ...}}
```

Scoped instances receive events touching their object; server-scoped
instances receive everything. `*-created` / `*-destroyed` events (plus
`session-closed`) are delivered without subscription; everything else
requires `subscribe`. All tmux notifications are bridged (session-created,
window-linked/unlinked/renamed, pane-focus-in/out, ...), plus synthesized
`window-created`, `pane-created`, `session-destroyed`, `window-destroyed`,
`pane-destroyed`, `client-destroyed`.

## Scopes, lifecycle, reload

One instance per (plugin, scope object): `server`, `session`, `window`, or
`pane` scope (`load-plugin -s`). Instances are created eagerly for existing
objects and on object creation, and torn down (with `pgh_on_unload`, tiny
budget) when their object dies. Pending async completions for dead or
reloaded instances are dropped by a generation check; their timers are
cancelled.

`load-plugin` is an upsert: unchanged code+config+caps keeps instances
running; changed code triggers a per-instance transaction (snapshot v1 →
instantiate+init v2 → migrate → swap; failure keeps v1); changed config
calls `pgh_on_config_changed`, restarting instances that refuse; changed
caps or scope restarts. `reload-plugin` forces the transaction.

## Failure policy

A trap (including the hard CPU budget and guest panics) tears down the
instance and counts one failure; three consecutive failures disable the
plugin until an explicit reload. Host API misuse returns structured errors
and never counts. Guests never see raw pointers: all handles are ids,
validated on every call (`E_NO_SUCH_OBJECT` after death).

## Capabilities

Granted with `load-plugin -c <name>`; requested (optionally) by a TOML
sidecar `<stem>.toml` next to the `.wasm` — effective = requests ∩ grants.
Defaults always granted: `read-state`, `display-message`, `timers`. Others:
`write-options`, `send-keys`, `capture-pane`, `run-process` (with optional
`[caps.run-process] argv0 = [...]` allowlist), `run-command`,
`cross-scope`, and reserved: `popup`, `menu`, `fs-read`, `fs-write`.
Scope-implied targeting is enforced on top: a pane-scoped instance may only
target its own pane, window-scoped its window's panes, session-scoped its
session's panes; `cross-scope` lifts this.

## tmux commands

```
load-plugin [-n name] [-s server|session|window|pane] [-c cap]...
            [-o key=value]... path.wasm
unload-plugin name
reload-plugin [-a] [name]
enable-plugin name / disable-plugin name
show-plugins [-v]
plugin-log [-n lines] [name]
```

Build tmux with `./configure --enable-plugins` (requires cargo; links
`plugin-host/target/release/libplugin_host.a`).
