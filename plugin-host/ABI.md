# tmux plugin ABI

A tmux plugin is a core WebAssembly module (`wasm32-unknown-unknown`). It
has no WASI and no ambient authority: every effect goes through typed host
imports, gated by capabilities. The ABI is C-like: one import per method,
scalars only, strings and buffers as (ptr, len) pairs, binary event
buffers. There is no JSON and no base64 anywhere on the wire.

Plugins run inside the tmux server on its event loop. Every entry into the
guest runs to completion under a CPU budget (epoch interruption, ~2 ms soft
warning, ~8 ms hard trap). There is no stack suspension: async host
operations complete via a callback export. The Rust SDK
(`plugin-host/sdk`, crate `tmux-plugin-sdk`) hides all of the below.

## Buffer taxonomy

Every value crossing the boundary has one of five shapes:

| shape      | direction  | wire form               | contract |
|------------|------------|-------------------------|----------|
| `Str`      | guest→host | `ptr, len` scalars      | borrowed for the call; **NUL byte at `data[len]`**, no interior NUL. The host validates and passes `base+ptr` straight into C — zero copies. `ptr 0, len 0` = absent (optional params). |
| `Bytes`    | guest→host | `ptr, len` scalars      | borrowed for the call; raw bytes, no NUL (mode_write). Zero copies. |
| pinned     | guest→host | `ptr, len` scalars      | async input (future fs calls): the SDK future owns the buffer until the completion arrives; the host/worker may read it after the call returns. |
| `OutBuf`   | host→guest | `out_ptr, out_cap, len_out_ptr` | caller-provided output. The host writes the data and stores the length (u32 LE) at `len_out_ptr`. Does not fit → `-E_LIMIT`, with the NEEDED size in `len_out` (grow and retry). |
| `OwnedBuf` | host→guest | out-ptr to 8 bytes `{ptr: u32, len: u32}` LE | host allocates exactly `len` via `pgh_alloc`, guest frees `(ptr, len)` via `pgh_free` (RAII in the SDK). |

## Filesystem reach

`fs_read`, `fs_write` and `fs_list` resolve a path the way a process
resolves against its cwd, with the plugin's data directory
(`$XDG_DATA_HOME|~/.local/share` + `tmux/plugins/<plugin>/`) standing in
for the cwd.

By default the reach is **contained**: the path must be relative, `..` is
refused syntactically, and the OS enforces containment during the walk
(`openat2` with `RESOLVE_BENEATH` on Linux 5.6+, canonicalize-and-compare
elsewhere).

The `fs-read-any` and `fs-write-any` capabilities widen the reach to
**anywhere**: an absolute path means what it says, and `..` may walk out.
A relative path still resolves against the data directory. Containment is
then deliberately not enforced, so `RESOLVE_BENEATH` is dropped for those
opens - leaving is the point.

Neither grant gives a plugin power it could not already reach through
`run-process`, which runs an arbitrary shell. They exist so a plugin can
read a directory *without* reaching for a shell.

## Directory listing

`fs_list` runs on the fs executor and writes packed records straight
into the guest's pinned buffer - one copy for the whole directory, and
no allocation per entry on either side:

```
record: u16 namelen | u8 kind | u8 reserved | i64 mtime | u8 name[namelen]
kind:   0 unknown, 1 dir, 2 file, 3 symlink, 4 other      (little-endian)
flags:  1 = fetch mtime, 2 = directories only
```

`mtime` costs one `statx` per entry, so a listing that asks for it
hands each batch of 250 to the executor as soon as those names are
packed, and keeps reading. The times for entries already read are then
fetched while the rest of the directory is still being read. Over ten
thousand entries a listing with times takes 13.7 ms one entry at a
time, 6.2 ms with the batches run after the walk, and 3.3 ms with them
run during it.

Batches, not one task per entry: a task costs about 970 ns to dispatch
and a `statx` on a warm cache costs 552 ns to run, so per-entry tasks
lose to doing the work in place. Below one batch there is no dispatch
at all - a directory of 200 entries is stat-ed on the spot, because
across threads it costs 0.10 ms and 0.22 ms once the hand-off is
counted.

The counts ride on the completion: `v0` is the bytes written, `v1` the
number of entries the directory holds. Fewer records than `v1` means the
buffer was too small; retry with a larger one. There is no buffer header
and no padding between records.

`kind` comes from `d_type`, which arrives with the directory entry, so
telling a directory from a file costs no `stat`. A filesystem that
answers `DT_UNKNOWN` falls back to one `stat` for that entry alone.

`mtime` (seconds) is the one field that is not free, so it is opt-in:
`getdents64` carries no timestamp, so it costs one `fstatat` per name,
measured at 0.8us. `flags` bit 2 narrows the walk to directories first,
which matters in a directory of ten thousand files and five
subdirectories. A platform with a bulk metadata call - `getattrlistbulk`
on macOS, `NtQueryDirectoryFile` on Windows - returns names and times in
one syscall and deserves its own backend; Linux has none, so per-entry
`statx` is the floor there.

Order is whatever the filesystem returns, which on a hashed directory
index (ext4's default) is neither creation nor name order. Sorting
belongs to the guest - and a guest that ranks the result must rank the
WHOLE listing, because a truncated one is an arbitrary subset.

Copy floor: guest→host strings/bytes cross with zero copies (the guest is
frozen during the call; C consumes before returning). Results cost exactly
one copy (into guest memory). tmux copies internally only where it takes
ownership (e.g. `options_set_string`).

Memory rules: the host never caches a raw guest pointer across a guest
call; the engine additionally pins linear memories so growth can only
extend, never relocate. Debug builds of the host replace every borrowed
pointer handed to C with a call-lifetime copy, so C code that stashes one
becomes an ASAN-visible use-after-free in CI.

## Interning

Event names and payload field keys are u32 ids, interned in one host table
shared by both sides: the C bridge interns through `pgh_intern` when
building event buffers; guests intern through the `intern` import to
subscribe and to compare keys. Ids start at 1 (0 marks an inline key), are
stable for the server lifetime, and are never reused. `intern_name` is the
reverse lookup. Routing compares integers, never strings.

## Guest exports

Required:

| export | signature | notes |
|---|---|---|
| `memory` | memory | single exported linear memory |
| `pgh_abi_version` | `() -> i32` | must return `1` |
| `pgh_alloc` | `(size: i32) -> i32` | 8-aligned; 0 = OOM (treated as failure) |
| `pgh_free` | `(ptr: i32, size: i32)` | size is echoed back exactly |
| `pgh_init` | `(cfg_ptr: i32, cfg_len: i32) -> i32` | config field block; nonzero = init failed |
| `pgh_on_event` | `(ptr: i32, len: i32)` | one binary event buffer |

Optional:

| export | signature | notes |
|---|---|---|
| `pgh_on_async_complete` | `(token: i64, err: i32, v0: i64, v1: i64, ptr: i32, len: i32)` | see Async |
| `pgh_on_unload` | `()` | tiny budget; best-effort |
| `pgh_state_version` | `() -> i32` | schema version of snapshot bytes |
| `pgh_snapshot` | `(out_ptr_ptr: i32, out_len_ptr: i32) -> i32` | 0 = wrote {ptr,len}; nonzero = stateless |
| `pgh_migrate` | `(old_version: i32, ptr: i32, len: i32) -> i32` | nonzero refuses (old code keeps running) |
| `pgh_on_config_changed` | `(ptr: i32, len: i32) -> i32` | 1 = absorbed, 0 = restart me |

Snapshot/migrate bytes are opaque to the host (the SDK uses JSON there;
that is plugin-internal, not ABI).

## Host imports (module `"tmux"`)

Errors: sync imports return `0` or `-code`; value-returning imports
(`intern`, `mode_open`, the async starters) return a positive value or
`-code`. The message for the most recent error is fetched with
`last_error` (an OutBuf). Codes: `E_BAD_REQUEST`(1), `E_UNKNOWN_METHOD`(2),
`E_CAP_DENIED`(3), `E_NO_SUCH_OBJECT`(4), `E_OUT_OF_SCOPE`(5),
`E_LIMIT`(6), `E_HOST`(7), `E_CANCELLED`(8), `E_UNSUPPORTED`(9).

`kind` values: -1 server/global, 0 session, 1 window, 2 pane, 3 client.

| import | signature | capability |
|---|---|---|
| `intern` | `(ptr, len) -> i64` — raw UTF-8, host-consumed (no NUL needed); id > 0 | none |
| `intern_name` | `(id, out, cap, len_out) -> i32` | none |
| `subscribe` / `unsubscribe` | `(event_id) -> i32` | read-state |
| `list` | `(kind, owned_out) -> i32` — object list buffer | read-state |
| `resolve` | `(kind, id, owned_out) -> i32` — one object record | read-state |
| `self_info` | `(out) -> i32` — 16-byte `{scope_kind: i32, scope_id: u32, generation: u64}` | read-state |
| `get_option` | `(kind, id, name Str, out, cap, len_out) -> i32` | read-state |
| `set_option` | `(kind, id, name Str, value Str) -> i32` (@-options only) | write-options |
| `format_expand` | `(kind, id, fmt Str, out, cap, len_out) -> i32` — `#{...}` against the scope; `#()` disabled | read-state |
| `send_keys` | `(pane, keys Str, literal) -> i32` | send-keys |
| `capture_pane` | `(pane, start, end, escapes, out, cap, len_out) -> i32` (≤2000 lines/call) | capture-pane |
| `display_message` | `(client /* -1 = all */, msg Str) -> i32` | display-message |
| `timer_cancel` | `(token: i64) -> i32` | timers |
| `mode_open` | `(window /* -1 = default */, width, height, x, y, title Str?) -> i64` (mode id) | mode |
| `mode_write` | `(mode: i64, data Bytes) -> i32` (≤256 KiB; raw ANSI, zero-copy) | mode |
| `mode_preview` | `(mode: i64, pane: i64 /* -1 = clear */, x, y, w, h) -> i32` | mode |
| `mode_move` | `(mode: i64, window /* -1 = default */, x, y) -> i32` | mode |
| `mode_resize` | `(mode: i64, width, height) -> i32` (content cells, clamped) | mode |
| `mode_close` | `(mode: i64) -> i32` | mode |
| `last_error` | `(out, cap, len_out) -> i32` | none |
| `fs_root` | `(out, cap, len_out) -> i32` — the plugin data dir's absolute path | none |
| `home_dir` | `(out, cap, len_out) -> i32` — the server user's home directory | none |
| `fs_write_sync` | `(path, data Bytes, append) -> i64` (bytes written) | fs-write |
| `fs_read_sync` | `(path, offset: i64, out, cap, len_out, eof_out) -> i32` | fs-read |
| `log` | `(level, ptr, len)` — raw UTF-8; 0=debug 1=info 2=warn 3=error | none |

fs paths are raw UTF-8 (host-consumed - the NUL rule does not apply);
they resolve inside the plugin's sandboxed data directory. That root is
resolved once per plugin and held as a directory descriptor. Containment
is checked twice: the host rejects absolute paths and any `..` component
on the string, then the OS enforces it on the open - `openat2` with
`RESOLVE_BENEATH` on Linux 5.6+, which is atomic and also refuses to
follow a symlink out of the tree, and canonicalize-the-parent elsewhere.
`fs_root` reports the canonical path. fs transfers
have no per-call byte cap: each one names a buffer in the guest's own
linear memory, so the instance memory limit is the ceiling, and no fs
path copies through a host allocation. Two costs stay with the caller -
a long sync call burns the instance's CPU budget, and an in-flight async
call holds off instance teardown.

### Asynchronous imports

Return a token > 0 (or `-code` on synchronous rejection); the result
arrives later via `pgh_on_async_complete(token, err, v0, v1, ptr, len)`.
`err` = 0 on success or an error code; `(ptr, len)` is an OwnedBuf the
guest frees (the error message bytes when `err != 0`; empty = none).

| import | signature | completion | capability |
|---|---|---|---|
| `run_job` | `(cmd Str, cwd Str?) -> i64` | v0 = exit status (or signal), v1 = signalled, data = combined output (≤256 KiB, truncated) | run-process |
| `run_command` | `(cmd Str) -> i64` | nothing; parse errors arrive as error completions (a command that runs and fails still completes Ok) | run-command |
| `timer_start` | `(ms: i64) -> i64` | nothing | timers |
| `fs_write` | `(path, data, append) -> i64` | v0 = bytes written | fs-write |
| `fs_read` | `(path, offset: i64, out_ptr, out_cap) -> i64` | v0 = bytes read, v1 = eof | fs-read |
| `fs_list` | `(path Str, out_ptr, out_cap) -> i64` (async; v0 = bytes, v1 = entries) | fs-list |

The async fs calls run on the host's fs executor (the tmux loop never
blocks) with ZERO copies: a runner reads `fs_write`'s data and fills
`fs_read`'s out-buffer directly in plugin memory. The buffers are
pinned by the SDK future until the completion arrives; awaited fs ops
are fully ordered, but two in-flight ops on the SAME file are not -
await each before the next. Completion means the data reached the page
cache (survives kill-server; not a power loss).

## Object records (list / resolve results)

Sequential little-endian records with inline `str` = u32 length + UTF-8
bytes (no NUL; empty = absent). `list` prefixes `u32 count`; `resolve`
returns one bare record. `NONE` = 0xffffffff.

```
session := u32 id, u8 attached, u32 current_window(NONE),
           str name, u32 nwindows, nwindows * { u32 index, u32 id }
window  := u32 id, u32 width, u32 height, u32 active_pane(NONE),
           str name, u32 nsessions, nsessions * u32,
           u32 npanes, npanes * u32          (pane ids in window order)
pane    := u32 id, u32 window, u32 width, u32 height,
           u8 flags(1 active | 2 floating | 4 dead),
           str title, str shell, str cwd
client  := u32 id, u32 session(NONE), u8 flags(1 attached | 2 control),
           str name
```

## Events

A binary buffer: fixed header + field block, all little-endian, packed.

```
event  := u32 event_id            (interned)
          u64 seq                 (host-assigned delivery order)
          u32 client, session, window, pane    (scope; NONE = absent)
          field block
fields := u16 count, count * field
field  := u32 key_id              (0 = inline key: u32 len + bytes follow)
          u8 tag
          value
tags   := 0 null | 1 bool (u8) | 2 i64 (8B) | 3 f64 (8B)
          | 4 str (u32 len + bytes) | 5 json (u32 len + bytes; config only)
```

Object names travel as flat fields (`session_name`, `window_name`,
`client_name`); the notification/command text as `text`; extra bus payload
items keep their names (`window_index`, `exit_status`, `command_duration`,
`old_pane`, ...) as i64 or str fields.

Scoped instances receive events touching their object; server-scoped
instances receive everything. `*-created` / `*-destroyed` events (plus
`session-closed`) are delivered without subscription; everything else
requires `subscribe`. The bridge registers an event-bus sink for every
hookable tmux event; synthesized object-lifecycle events replace the bus
versions for creation (`window-created`, `pane-created`, `client-created`)
and destruction. `pane-notification` (OSC 9;message or OSC
777;notify;title;body — the message travels in the `text` field, ≤512
bytes, valid UTF-8) has no bus equivalent and is delivered directly.
`plugin-command` (from the tmux command of the same name) is targeted:
only the plugin named in its `plugin` field receives it (subscription
still required); the command string is the `text` field and the target
pane/window/session form the scope.

Config (`pgh_init` / `pgh_on_config_changed`) is a bare field block with
inline string keys; scalar values map directly, nested values (arrays /
tables from a sync-plugins manifest) use the `json` tag.

## UI modes

A mode is an interactive panel owned by one plugin instance: a freshly
spawned **empty floating pane** (no process) running a dedicated window
mode, opened with `mode_open` (capability `mode`). The pane is focused on
open so keys flow to it immediately; ids are monotonic and never reused.
(Entering a mode on an *existing* pane — tmux's own copy-mode pattern —
is not offered; the design for it is in [MODE-ATTACH.md](MODE-ATTACH.md).)

- **Target window**: scope-implied. Pane- and window-scoped instances may
  only open in their own window (pass -1); session-scoped in windows
  linked to their session (default: the session's current window);
  server-scoped anywhere (window required). `cross-scope` relaxes the
  checks.
- **Rendering**: `mode_write` sends raw ANSI bytes (a borrowed `Bytes`,
  parsed straight out of plugin memory by the full tmux escape parser)
  into the mode's screen — cursor addressing, SGR, clears, alternate
  charsets all work, so ratatui-style TUI libraries can render
  unmodified. At most 256 KiB per call. Note the screen is not a
  terminal: nothing echoes back, and replies that would go to a terminal
  (OSC 52 queries etc.) are dropped.
- **Preview**: `mode_preview` declares one retained rect `(pane, x, y, w,
  h)` mirroring the source pane's live grid (grid-cell blit, no escape
  reparsing), refreshed ~every 500 ms until cleared (pane -1), the
  source pane dies, or the mode closes. The rect must fit the mode
  screen; scope-implied pane targeting applies as for `capture_pane`.
- **Events** (delivered only to the owning instance, no subscription
  needed; the mode id arrives in the `mode` field):
  - `mode-key` — fields `mode`, `key` (a tmux key name: "q", "Enter",
    "Escape", "MouseDown1Pane", ...), `client` (the pressing client),
    and for mouse keys `mouse_x`, `mouse_y`, `mouse_b` (pane-relative
    cell coordinates).
  - `mode-resize` — fields `mode`, `width`, `height`; the float was
    resized, redraw.
  - `mode-closed` — fields `mode`, `reason`; terminal. `reason` is
    `"closed"` (the plugin called `mode_close`) or `"killed"` (anything
    else: the user killed the pane, the window died, the plugin was
    reloaded or unloaded). The id is dead afterwards.
- **Resize**: `mode_resize` sets the float's size. `width` and `height`
  are content cells, exactly as in `mode_open`, and the border sits
  outside them. The host clamps the size to the window and keeps the
  float's top-left corner, so a panel that grows expands down and right
  instead of jumping. The resize delivers a `mode-resize` event with the
  size actually given: treat the event as the truth and the call as a
  request, and only call it when the size you want differs from the size
  the last event reported. Refused with `E_LIMIT` when the window is too
  small to hold a float at all.
- **Move**: `mode_move` relocates the float to another window, join-pane
  style: the same pane is relinked, so the mode id, the pane id, the
  rendered screen and the event stream all survive - at most a
  `mode-resize` follows if the destination clamps the size. Target
  window rules and `x`/`y` as for `mode_open` (default: re-centered).
  Refused with `E_LIMIT` when the move would leave the source window
  empty (close instead; a paneless window cannot be left behind). This
  is the primitive for follow-the-user panels.
- **Close**: `mode_close` returns immediately; the pane teardown happens
  at the next event-loop iteration and delivers `mode-closed`. Instance
  teardown (unload, reload, scope-object death) force-closes all modes the
  instance owns; stale events from a previous generation are dropped.

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
instance and counts one failure. A trapped guest's memory is frozen
wherever the deadline landed, so the instance is never reused: the host
starts a fresh one for the same scope, with fresh state, if the scope
still exists. Three failures within five minutes disable the plugin until
`reload-plugin`, which also starts an instance for any scope that has
none. Host API misuse returns structured errors and never counts. Guests never see raw host pointers: all handles are ids,
validated on every call (`E_NO_SUCH_OBJECT` after death).

## Capabilities

Granted with `load-plugin -c <name>`; requested (optionally) by a TOML
sidecar `<stem>.toml` next to the `.wasm` — effective = requests ∩ grants.
Defaults always granted: `read-state`, `display-message`, `timers`. Others:
`write-options`, `send-keys`, `capture-pane`, `run-process` (with optional
`[caps.run-process] argv0 = [...]` allowlist), `run-command`,
`cross-scope`, `mode` (UI modes), `fs-read`, `fs-write` (sandboxed to the
plugin's data directory: `$XDG_DATA_HOME|~/.local/share` +
`tmux/plugins/<name>/`; relative paths only, no `..`, symlink escapes
rejected), and reserved: `popup`, `menu`.
Scope-implied targeting is enforced on top: a pane-scoped instance may only
target its own pane, window-scoped its window's panes, session-scoped its
session's panes; `cross-scope` lifts this.

## tmux commands

```
load-plugin [-n name] [-s server|session|window|pane] [-c cap]...
            [-o key=value]... path.wasm
sync-plugins manifest.toml
unload-plugin name
reload-plugin [-a] [name]
enable-plugin name / disable-plugin name
show-plugins [-v]
plugin-log [-n lines] [name]
plugin-command [-t target] plugin command
```

`sync-plugins` reconciles the *managed* plugin pool against a TOML
manifest (declarative loading): new entries load, changed ones follow the
load-plugin reconcile rules, and managed plugins the manifest no longer
names are unloaded. Identity is the `[plugins.NAME]` key; entry paths
resolve relative to the manifest. Interactive `load-plugin` definitions
are unmanaged and never swept (a manifest entry of the same name adopts
them). A manifest that fails to parse or validate changes nothing. See
WRITING-PLUGINS.md for the manifest format.

Build tmux with `./configure --enable-plugins` (requires cargo; links
`plugin-host/target/release/libplugin_host.a`).
