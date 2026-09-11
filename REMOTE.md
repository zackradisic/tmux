# Remote sessions, windows and panes over ssh

This is a design for remote objects in tmux2. The local server shows a
session, window or pane that lives on another machine. The user stays in the
local tmux. Keys, resizes and structural commands go to the remote. Output
comes back and renders in a local grid. Nothing here is built yet.

Proposed use:

```
$ tmux remote-attach host              # mirror every session on host
$ tmux remote-attach -t work host      # mirror one session as "host:work"
$ tmux new-session -H host -s deploy   # create a session on host and mirror it
$ tmux new-window -H host              # a remote window in a local session
```

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

Any tmux with control mode works on the remote. Version 3.2 or later gives
`%pause`. Floating panes and plugin modes on the remote need tmux2.

## Architecture

A new module `remote-link.c` owns one `struct remote_link` per ssh connection.

```
 local server                                   remote host
 ------------                                   -----------
 shadow session "host:work"
   shadow window  <-- %layout-change ---------- window
     shadow pane                                 pane
       wp->fd = socketpair[0]                      pty + shell
         ^  |
   bytes |  | keys
         |  v
       remote_link
         socketpair[1] per pane
         parser for %-lines
         job: ssh host tmux -CC attach -t work
              stdout --> %output, %layout-change, ...
              stdin  <-- send-keys -H, resize-window, split-window, ...
```

### Shadow objects

A shadow object is a real `struct session`, `struct window` or
`struct window_pane`. It carries a pointer to its link and the remote id:

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

`window_pane_send_resize()` in `window.c` does `TIOCSWINSZ` on `wp->fd`. That
fails on a socket. The function gets one check: if the pane is remote, it
calls `remote_link_resize()` instead, which sends `resize-window -t @n -x -y`.
This is the only hook in the pane code path.

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

1. The ssh job exits. The link marks every shadow pane `PANE_EXITED`, keeps
   the grids, and shows `[remote: host disconnected]` in the pane border.
2. The link retries with backoff. On success it runs `list-sessions`,
   `list-windows -a` and `list-panes -a` with `-F` and reconciles the tree.
3. For each pane it runs `capture-pane -p -e -J -S -` and feeds the result
   through the local parser. This refills the grid and the scrollback.
4. It subscribes to the formats it caches (see below) and resumes output.

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

### Two plugin runtimes

If the remote runs tmux2 with plugins, its plugins run there. A remote mode
pane is a floating remote pane. `layout_parse()` does not read floats, so the
link needs a separate `list-panes -F '#{pane_floating}'` pass to mirror them.
Both sides that load the same UI plugin show duplicates. Rule: UI plugins run
locally only. The remote runs plain tmux, or tmux2 with no UI plugins.

## Server restart

`restart-server` (`SERVER-RESTART.md`) can adopt the socketpair through
`adopt_fd`, but not the ssh job or the parser state. The simple path: the
handoff records `host` and `session` per shadow session, drops the links,
and the new server reconnects. The grids refill from `capture-pane`.

## Scope and phases

Remote sessions and remote windows are clean. A whole window mirrors a whole
remote window. A remote pane inside a local window is the hard case. It
breaks the rule that the remote owns the layout, and it needs a per-pane byte
forwarder on the remote with no persistence.

1. Remote sessions. `remote-attach host` mirrors sessions as `host:name`.
   Output, keys, resize, reconnect, the format cache.
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
- The ssh command. Take it from an option, `remote-ssh-command`, default
  `ssh -T %h tmux -CC attach -t %s`. Multiplex with `ControlMaster` so a
  second link to the same host is cheap.

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
