# Remote sessions, windows and panes over ssh

This is the design for remote objects in tmux2. The local server shows a
session, window or pane that lives on another machine. The user stays in the
local tmux. Keys, resizes and structural commands go to the remote. Output
comes back and renders in a local grid.

Status: phase 1 (remote sessions) is built. `remote-attach` mirrors one
remote session as `host/session`, with output, keys, resize, command
forwarding, reconnect, the format cache and server restart. The files are
`remote-parse.c` (the control mode line parser), `remote-link.c` (the link)
and `cmd-remote-attach.c`. The tests are `regress/remote-*.sh`.

Use:

```
$ tmux remote-attach host              # mirror the current session on host
$ tmux remote-attach -t work host      # mirror one session as "host/work"
$ tmux remote-attach -k host           # drop the link(s) to host
```

Not built yet: `new-session -H host`, `new-window -H host`, remote panes
inside local windows, mouse forwarding, remote floating panes.

The local name is `host/session`, not `host:session`: tmux forbids `:` and
`.` in session names, and `:` separates the window part of a target.

## Why the client protocol does not help

The tmux client is dumb. It passes its tty descriptor to the server with
`MSG_IDENTIFY_STDIN` (`server-client.c`), and the server draws on that
descriptor itself. The messages in `tmux-protocol.h` cover identify, resize,
exit and file transfer. There is no screen protocol between client and server.
So a local server cannot attach to a remote server as a normal client. An ssh
channel cannot carry a tty descriptor.

## The seam: control mode

A server started with `tmux -CC attach` speaks a line protocol on stdout.
iTerm2 uses it to show remote tmux windows as native tabs and splits. That is
the feature described here. The remote side already sends everything the
local side needs (`control.c`, `control-notify.c`):

| Message | Meaning |
|---|---|
| `%output %<pane> <bytes>` | raw pty bytes, octal escaped |
| `%extended-output %<pane> <age> : <bytes>` | the same, with latency |
| `%pause %<pane>` / `%continue %<pane>` | flow control, `pause-after` |
| `%layout-change @<win> <layout> <visible> <flags>` | the tiled layout |
| `%window-add`, `%window-close`, `%window-renamed` | window lifecycle |
| `%window-pane-changed`, `%session-window-changed` | focus |
| `%session-changed`, `%sessions-changed`, `%session-renamed` | sessions |
| `%subscription-changed <name> $<s> @<w> <idx> %<p> : <value>` | formats |
| `%begin` / `%end` / `%error` | command replies |

The local side already has the matching parts:

- `layout_parse()` in `layout-custom.c` rebuilds a window from the layout
  string.
- `input_parse_pane()` turns raw bytes into a grid. A pane does not care
  where the bytes come from.
- `adopt_fd` in `spawn.c` (from the server-restart work) builds a pane around
  any descriptor.
- `window_pane_key()` and `window_pane_paste()` write keys to `wp->event`,
  so key input already flows through the pane descriptor.

Any tmux with control mode works on the remote. Version 3.3 or later gives
`pause-after` and `refresh-client -C`, both of which the link needs.
Floating panes and plugin modes on the remote need tmux2.

Facts about the protocol that shaped the parser (`remote-parse.c`):

- Events fire synchronously on the remote, so a notification such as
  `%window-add` or `%layout-change` can land inside the `%begin`/`%end`
  block of the command that caused it. The parser dispatches known
  notification lines wherever they appear and keeps every other line as
  body (`list-panes -F '#{pane_id}: ...'` prints body lines that start with
  `%`).
- The `flags` field of `%begin` is 1 for a command the control client sent
  and 0 for everything else (the initial `attach-session`, hooks). The link
  matches reply blocks to its request queue by that flag, so it needs no
  guesswork about unsolicited replies.
- `spawn_pane()` makes a new pane active before it reports the layout, so
  `%window-pane-changed` for a new pane arrives before the `%layout-change`
  that creates it. The link keeps the wanted pane and applies it after the
  layout.
- `#{window_layout}` from tmux2 lists a floating pane's cell inline in the
  tiled tree *and* in the `<...>` part. `layout_parse()` rejects both, so
  the link cuts the `<...>` part and every inline leaf whose id it names,
  then recomputes the checksum.

## Architecture

A new module `remote-link.c` owns one `struct remote_link` per ssh connection.

```
 local server                                   remote host
 ------------                                   -----------
 shadow session "host/work"
   shadow window  <-- %layout-change ---------- window
     shadow pane                                 pane
       wp->fd = socketpair[0]                      pty + shell
         ^  |
   bytes |  | keys
         |  v
       remote_link
         socketpair[1] per pane
         parser for %-lines
         job: ssh host tmux -C attach -t work
              stdout --> %output, %layout-change, ...
              stdin  <-- send-keys -H, resize-window, split-window, ...
```

### Shadow objects

A shadow object is a real `struct session`, `struct window` or
`struct window_pane`. It carries a pointer to its link and the remote id.
A shadow window keeps the remote window's index and name. The pane inventory
comes from the layout string: each leaf cell carries the pane id, in the order
`layout_parse()` assigns cells to the window's pane list, so the link orders
the panes like the leaves before every parse. A local floating pane over a
shadow window (a popup or a plugin mode) is taken out of the list for the
parse and given back its cell afterwards.

```c
struct remote_ref {
	struct remote_link	*link;
	u_int			 remote_id;	/* $n, @n or %n on the remote */
};
```

The link keeps two maps, remote id to local object and local id to remote id.
Ids are never reused on either side, so the maps are stable.

### Output path

`window_pane_read_callback()` and `input_parse_pane()` run unchanged. The
link decodes each `%output` line, looks up the shadow pane, and writes the
bytes to `socketpair[1]`. libevent wakes the pane on `socketpair[0]` like a
pty. The local grid is a second copy of the remote grid. This doubles the
parsing work. iTerm2 does the same, and the cost is small.

The link enables `pause-after` on the remote client. When a pane pauses, the
link sends `refresh-client -A %n:continue` once the local grid catches up.

### Input path

Keys reach `bufferevent_write(wp->event, ...)` as today. The link reads
`socketpair[1]` and sends `send-keys -H -t %n <hex bytes>`. Mouse events need
extra work: control mode has no mouse message. The first version forwards the
encoded mouse sequence as bytes when the remote pane has mouse mode on.

### Resize

`window_pane_send_resize()` in `window.c` does `TIOCSWINSZ` on `wp->fd`, and
calls `fatal()` when the ioctl fails. A shadow pane returns before the ioctl.
Window sizing funnels through `resize_window()` in `resize.c`; for a shadow
window it sends `refresh-client -C @n:WxH` to the remote instead, and the
remote answers with `%layout-change`, which resizes the local window. The
link sends one size per window and repeats it only when it changes.

## Command routing

The remote owns the tiled layout. The local server never edits it. Every
command that targets a shadow object falls into one of two classes.

| Local only | Forwarded to the remote |
|---|---|
| copy mode, scrollback, search | `split-window`, `new-window`, `kill-*` |
| `select-pane`, `select-window` | `resize-pane`, `resize-window` |
| `resize-pane -Z` (zoom, view only) | `rename-window`, `rename-session` |
| plugin modes, popups, menus | `send-keys`, `paste-buffer` |
| local options and hooks | `respawn-pane`, `respawn-window` |
| `display-message`, `list-*` | `swap-pane`, `move-window`, `join-pane` |

Forwarding lives in one place. `cmdq` gets a check before it runs a command:
if the command has the `CMD_REMOTE` flag and its target is a shadow object,
the link sends the command text and the reply block comes back as the local
command's output. The remote then emits `%layout-change` or `%window-add`,
and the local tree follows.

`select-pane` and `select-window` stay local so that focus does not round
trip. The link sends the new focus to the remote after the fact, so a
reconnect restores it.

## Reconnect and persistence

The sessions live on the remote. The local objects are a view.

1. The ssh job exits. The link keeps every shadow pane, its grid and its
   socketpair (closing the socketpair end would destroy the pane), and
   writes `[remote: host disconnected]` into each pane.
2. The link retries with backoff (1, 2, 4 .. 60 s). On success it runs
   `list-windows -F` and reconciles the tree: new windows are built, gone
   windows killed, the rest get the `%layout-change` treatment.
3. For each pane it runs `capture-pane -p -e -J -S -` followed by a cursor
   query, clears the grid and writes the result through the local parser.
   Output for the pane is dropped until the cursor reply, which is exact
   because the remote emits blocks in order.
4. It subscribes to the formats it caches (see below) and resumes output.

`%pause` gets the same treatment: `refresh-client -A %n:continue` followed by
a capture, so a pane that fell behind snaps to the current remote grid.

The local scrollback depth after reconnect is what `capture-pane` returned.

## Plugins

Plugins run in the local server only. A shadow object is a real local object
with a real grid. So most of the plugin API works with no change. Three
classes remain.

### Works as is

- Object tree: `list_objects`, `resolve_object`, `obj_relation`, the
  `emit_*` records. Add `PLUGIN_PANE_REMOTE` (and the window and session
  equivalents) plus a `host` string to the records, so a plugin can tell.
- Events: `*-created`, `*-destroyed`, `window-pane-changed`, layout, focus
  and mode events. The link mutates the tree through the same functions a
  local command uses, so the same events fire.
- `capture_pane` and `panes_search`: they read the local grid.
- `send_keys`: it writes to `wp->event`, and the link forwards the bytes.
- Plugin modes: a mode pane is a floating pane. Floating panes are not part
  of the tiled layout, so they stay local. Toasts and pickers work over a
  remote window with no extra code. `layout_parse()` ignores the floating
  part of a layout string already.
- OSC 9 and 777 notifications: the bytes come through `%output` into the
  local parser and `plugin_notify()` fires locally.
- Options, the sqlite layer, cron and timers: all local.

### Needs a cache from the remote

The format callbacks for `pane_current_command`, `pane_current_path`,
`pane_pid` and `pane_tty` read the local descriptor or pid. On a shadow pane
the pid is unset and the descriptor is a socket. The link subscribes with
`refresh-client -B` to these formats per pane, stores the values on the
shadow pane, and the callbacks return the cached value when the pane is
remote. `format_expand` then works for plugins with no ABI change. The
`plugin_vtable_emit_pane()` cwd field uses the same cache.

### Needs a proxy or an honest failure

These host calls inspect a local process or the local filesystem:

- `pane_env`, `pane_pid`, `pane_fds`: the process is on the other machine.
  Step one returns "unavailable" for remote panes. Step two proxies them
  with `run-shell` on the remote, which returns its output in the reply
  block.
- `run-process`, `fs_read`, `fs_write`: they run on the local machine. Add a
  host call that runs a process on the pane's host through the link.

### Effect on the example plugins

| Plugin | Effect |
|---|---|
| agents | command detection works; env checks need the `pane_env` proxy |
| notify-toast | works; remote processes can still send OSC 9 |
| resurrect | must save the link (`host:session`), not the panes |
| git-status | needs the remote `run-process` call |
| session_creator | needs a host target for `new-session` |
| cron, ticker, hello-raw | no effect |

### Plugin records

`PaneInfo` carries `remote: bool` and `host: String` (flag bit 8 in the
record). `cwd` for a remote pane is the remote's cached current path.

### Roles: provider and view

Machine facts enter plugins through a few doors only: `pane_env`,
`pane_fds`, `pane_pid`, the fs calls, `run_job`, the process-derived
formats and the clock. Everything else is tmux state, and the shadow
tree carries that. So a plugin splits into two halves:

- the *provider* runs on every server and sees only its own machine; it
  detects, reads processes and files, keeps the store and answers
  service calls;
- the *view* runs on the local server, merges what the providers report
  and owns the UI.

One crate, two roles: `load-plugin -r` picks `both` (the local default),
`view` or `provider`. `Ctx::role()` tells the plugin which half to run.
The host refuses `mode_open` for a provider; nothing else is gated.

### The bridge

The link carries a plugin bridge between the two hosts. Version 1 rides
the control mode connection: the local side sends a frame as a
`plugin-bridge <base64>` command, the remote answers with `%bridge
<base64>` lines through `control_write()`, so a frame never splits an
output block. The Rust host (`plugin-host/host/src/bridge.rs`) frames,
compresses (zstd above 4 KiB) and interprets; C moves opaque bytes
(`plugin-bridge.c`, `cmd-plugin-bridge.c`). A remote running plain tmux
answers `plugin-bridge` with an error and the link stops sending.

Frames: `hello` (ABI version, host name, providers, capability ceiling),
`push` (a plugin's descriptor, sidecar and wasm bytes), `call`, `reply`
(paged, with MORE and ERROR flags), `cancel`, `subscribe`,
`unsubscribe`, `event` (a topic payload with a host-stamped sequence),
`ping`, `pong`.

### Push

When the link comes up the hosts exchange `hello`. The local server then
pushes every plugin with role `both` or `provider` to the remote, which
writes it under `<data>/tmux/plugin-cache/<host>/` and loads it in role
`provider` through the ordinary path-based load. The wasm is
byte-portable (no `cfg(target_os)`, no WASI), so the local build runs on
the remote as it is. The remote decides the grants with its
`plugin-remote-caps` server option; the default leaves out
`run-process`, `fs-write`, `fs-read-any` and `fs-write-any`. A remote
tmux2 older than the pushed ABI refuses the load and the link logs
"remote tmux2 is older; run tmux update there". Pushed plugins are
unloaded ten minutes after their peer stays down.

### Services

Providers register methods (`service_register`) and publish topics
(`service_emit`); views call them (`service_call` with a target `plugin`
or `plugin@server`, `service_subscribe`) and list the servers
(`servers`). Delivery is queued in the host and never re-entrant. A call
for a plugin that is loaded but not yet registered waits up to ten
seconds (a pushed plugin's init may still be queued); a call to a down
server fails at once with `E_UNREACHABLE`; a call nobody answers fails
after thirty seconds with `E_TIMEOUT`. The SDK adds `service::call`,
`call_all`, `call_stream`, `emit`, `subscribe`, `ServiceRequest` with
`reply`/`reply_page`/`fail`, and `Replica<T>` for a view's per-server
copy with gap detection. `plugin-host/examples/services-probe` is the
smallest provider/view pair; `regress/plugin-services-remote.sh` drives
it across two servers.

Two links to one host are two peers with one name; service targets reach
the first one that is up.

## Server restart

`restart-server` (`SERVER-RESTART.md`) can adopt the socketpair through
`adopt_fd`, but not the ssh job or the parser state. The handoff writes a
`remote host session id` record per shadow session and skips its windows
and panes; the new server makes the link again with the same session id,
so a client that reattaches to `$id` lands on the shadow session while it
syncs. Window and pane ids are new after a restart.

## Scope and phases

Remote sessions and remote windows are clean. A whole window mirrors a whole
remote window. A remote pane inside a local window is the hard case. It
breaks the rule that the remote owns the layout, and it needs a per-pane byte
forwarder on the remote with no persistence.

1. Remote sessions. `remote-attach host` mirrors a session as `host/name`.
   Output, keys, resize, reconnect, the format cache. Built.
2. Remote windows. `new-window -H host` links a remote window into a local
   session. The remote holds it in a hidden session.
3. Command forwarding for the full table above. Mouse forwarding.
4. Plugin flags, `pane_env` proxy, remote `run-process`.
5. Remote panes in local windows, if still wanted.

## Open questions

- Bandwidth and latency. Output costs the same as a plain ssh pane. A key
  costs one ssh round trip. `pause-after` protects slow links.
- Id collisions in the UI. Local pane `%5` and remote pane `%5` are different
  panes. Formats show the local id. Add `#{pane_remote_id}` for scripts.
- Remote `set-option` on shadow objects. The first version keeps options
  local. They vanish on reconnect unless the link stores them.
- Clipboard. `set-clipboard` OSC 52 from the remote arrives as bytes and
  works. Remote `paste-buffer` needs the local buffer sent with `load-buffer`.
- The ssh command comes from the `remote-ssh-command` server option, a
  format with `remote_host` and `remote_session`. Multiplex with
  `ControlMaster` so a second link to the same host is cheap.

## Work items

| Item | Files | Size |
|---|---|---|
| `%`-line parser, link state, shadow maps | `remote-link.c` (new) | large |
| `remote_ref` on session, window, pane | `tmux.h` | small |
| resize hook | `window.c` | small |
| `CMD_REMOTE` flag and forwarding | `cmd-queue.c`, ~12 `cmd-*.c` | medium |
| format cache and callbacks | `format.c` | small |
| reconnect, `capture-pane` refill | `remote-link.c` | medium |
| `remote-attach`, `-H` on new-session/window | `cmd-*.c` | medium |
| plugin flags, `host`, `pane_env` proxy | `plugin-vtable.c`, host | medium |
| pane border and status formats | `screen-redraw.c`, `format.c` | small |
| handoff record and reconnect | `server-handoff.c` | small |

Estimate for phases 1 to 3: 2500 to 3500 lines of C, most of it in
`remote-link.c`.
