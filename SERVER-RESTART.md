# Restarting the server without killing the panes

`restart-server` replaces the running server with a new one and keeps every
pane process alive. Use it to move onto a freshly built binary without losing
any work.

```
$ tmux restart-server            # the running binary
$ tmux restart-server -b ./tmux  # a specific binary
```

## Why execve, and not fork

A pane dies on server exit for one reason: the server closes its pty master
(`spawn.c`, `window.c`, `server-fn.c`). tmux sends no signal. The slave side
gets EOF, and the shell exits on its own. So a pane survives as long as its
master stays open.

That leaves two ways to restart. The server takes the second.

| | fork + exec | in-place `execve` |
|---|---|---|
| Pane processes | orphans of a sibling | stay our children |
| `waitpid` / SIGCHLD | gone | works |
| `#{pane_dead_status}` | gone | works |
| Descriptor transport | `SCM_RIGHTS` over a socket | free |

`execve` keeps the pid, the child list and every open descriptor. Only a
parent can read a child's exit status, and `server_child_exited()` is the only
thing that sets `wp->status`. A sibling server would lose `pane_dead_status`,
`pane_dead_signal`, `remain-on-exit`, the `pane-died` hook, and the `SIGCONT`
that revives a stopped pane. `PR_SET_CHILD_SUBREAPER` cannot help, because it
only adopts descendants; `pidfd` can, but only with `PIDFD_INFO_EXIT` on Linux
6.15 and newer.

## The sequence

1. `restart-server` checks the binary is executable, then sends every attached
   client `MSG_EXEC` with the command that brings it back:
   `exec tmux -S <socket> attach -t $<id>`. A control mode client only
   detaches. A client running a command is left alone.
2. The server loop waits for the clients to go, or five seconds, whichever
   comes first.
3. The server writes its state to `<socket>.handoff`.
4. Every descriptor except stdio, the listening socket and the pty masters
   becomes close-on-exec.
5. `execve(binary, {binary, "-S", socket, "-Z", statefile, ...})`. This fails
   atomically, so a failure leaves the old image and every pane intact.
6. The new image sees `-Z` and calls `server_resume()` instead of the client
   code. It blocks every signal, restores the state, then drains `waitpid()`
   once for anything that died in the gap and unblocks.
7. Each pane is rebuilt with `spawn_pane()` and the new `SPAWN_ADOPT` flag,
   which takes the inherited pty instead of calling `forkpty()`.
8. The clients reconnect to the same socket.

## What comes back

- Pane processes, with the same pids, still our children.
- `waitpid`, SIGCHLD, `#{pane_dead_status}` and `remain-on-exit`.
- Sessions, windows, panes, and window links across sessions.
- Layouts, window sizes and the zoomed flag.
- Session, window and pane ids, so `$2`, `@7` and `%13` still mean the same
  objects. Each object takes its saved id at creation, and the id counters go
  back afterwards.
- Options at every scope, including hooks, plus the global and session
  environments.
- Key bindings in every table. The saved set is the whole set, so a key the
  user unbound at runtime stays unbound.
- Paste buffers, by name.
- Pane scrollback, cursor position, title, and the terminal modes a program
  turned on, such as bracketed paste, mouse reporting and the application
  keypad.
- Floating panes, at the same size, position and z-order.
- Dead panes, with their exit status.
- Plugins, reloaded from the commands that loaded them.
- The server start time, so `#{start_time}` does not jump.

## What does not

- **Pane modes.** Copy mode, `choose-tree` and plugin modes drop; panes come
  back in normal mode.
- **Jobs.** `#()` format jobs and any in-flight `run-shell`.
- **`pipe-pane`.** The pipe closes and its command sees EOF.
- **Popups, menus and `wait-for` channels.**
- **Client tty state.** Clients detach and reattach.
- **Plugin guest memory.** Plugins are loaded again from scratch.
- **The scrollback behind an alternate screen.** A pane running a full-screen
  program keeps only what is on screen.
- **Session groups.**
- **The place a floating cell holds in the cell list.** Every pane comes back
  the same size in the same place, and the z-order is the same, but a restart
  moves the floating cells to the end of the list, so `#{window_layout}`
  reads differently. Nothing depends on that order: `layout_fix_offsets()`
  skips floating cells, and `layout_parse()` cannot read them at all.
- **Automatic paste buffers.** They come back named, but as ordinary buffers,
  so `buffer-limit` no longer trims them.

## The state file

`<socket>.handoff`, mode 0600, deleted as soon as the new image has read it. A
failed restore leaves it behind for a look.

It is line-oriented and tab-separated. Each field escapes `\\`, tab, newline
and carriage return, so a value can hold any of them. Records are ordered:
`session`, `window` and `pane` open a context, and `sessionend`, `windowend`
and the next `pane` record close one.

```
version   1
socketfd  9
starttime 1787870086  250168
nextids   2  3  5
opt-server  status-left  LEFT#{session_name}
arr-server  command-alias  0  split-pane=split-window
env-global  PATH  /usr/bin  0
bind        root  F5  0  a note  display-message "a binding"
buffer      handoffbuf  3  YnVmZmVyIGNvbnRlbnRz
plugin      load-plugin -c read-state ticker.wasm
session     0  alpha  /home/zack  1787870086
opt-session history-limit  12345
window      0  0  sleep  80  20  0  0  0
opt-window  window-status-format  WSF
layout      72ca,80x20,0,0[80x10,0,0,0,80x9,0,11,1]
wactive     1
pane        0  10  3769224  /dev/pts/22  /home  /bin/bash  0  0  1  sleep 600
panescreen  8  9  1  a title
paneline    0  line 1 ^[[31mRED^[[39m ...
windowend
scurw       0
slastw      1
sessionend
```

Notes on the format:

- `socketfd` is the listening socket. Keeping it means there is no window in
  which the socket does not exist, so a client can connect throughout.
- A window that more than one session links appears once, under the first
  session that links it. The others carry a `link` record instead.
- `layout` holds the pre-zoom layout, because `layout_parse()` needs the real
  cell tree; the `window` record's last field re-zooms afterwards. It also
  leaves out the floating cells, through the new `layout_dump_part()`:
  `layout_parse()` rejects the `<...>` section that `layout_dump()` writes,
  and a floating cell left in the tiled list makes the cell count wrong.
- The tiled panes of a window arrive first, then `tiledend`, then the floating
  ones. `layout_parse()` wants exactly as many panes as the layout has cells
  and assigns them in list order, so `SPAWN_ADOPT` appends each tiled pane and
  leaves the layout alone until `tiledend`. Each floating pane then brings its
  own cell, made with `layout_floating_pane()` from the saved geometry.
- If `layout_parse()` still refuses the layout, the window is rebuilt by
  splitting one pane at a time. That path exists so that no pane is ever left
  without a cell: `layout_set_tiled()` and the redraw code both read
  `wp->layout_cell` without checking it.
- `paneline` records carry a wrapped flag. An unwrapped line loses its
  trailing spaces; a wrapped one keeps every cell, so that the replay wraps at
  the same column. The replay goes back through `input_parse_buffer()`, and
  the lines scroll off the top into the history exactly as they did the first
  time.
- Key bindings are installed from a command queue callback, behind the default
  bindings that `key_bindings_init()` queues.
- `cfg_finished` is set to 1 on resume. The configuration already ran in the
  image we replaced, and its result is in the file; reading it again would
  double every binding.

## Rollback

Write your resurrect file first. `execve` failing is safe, but a state file
the new image cannot parse is not: the new image exits, and every pane dies
with it.

## Tests

`regress/restart-server.sh` builds a scene with splits, a custom layout, a
linked window, a zoomed pane, a dead pane, options at four scopes, custom key
bindings, an unbound default, a paste buffer, a floating pane, a pane with
mouse reporting on, and 40 lines of wrapped scrollback. It restarts, then
asserts that a full snapshot is byte-identical and that
`#{pane_dead_signal}` still reports a signal sent afterwards.
