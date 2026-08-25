- Seamless server upgrade (`server-handoff`): restart the server (e.g. onto a
  freshly built binary) without killing pane processes. Panes die today
  because server exit closes every pty master (SIGHUP to the slaves) — so
  hand the masters off instead: old server serializes its world (sessions,
  windows, layouts, pane pid/cwd/size, options, plugin snapshots, ideally
  scrollback) and passes the blob + every master fd over a unix socket
  (SCM_RIGHTS) to the new server, nginx binary-upgrade style; new server
  adopts the fds instead of forkpty. Known costs: adopted panes are not our
  children (no waitpid — detect death via master EOF/EIO, no
  #{pane_dead_status}); scrollback must ride the blob or be lost; ~64K pty
  buffer while nothing reads; per-server transients (jobs, prompts, popups)
  don't survive. Prior art: reptyr -T proves the pty part. (2026-08-25)

- Probably want to have strong handles to tmux objects later at some point as I anticpiate this could come up
- At some point we may want to have separate queues. The notify_add() thing
  seems to be used for tmux internal stuff and I could imagine there could be a
  lot of contention on this queued callback thing if we have a lot of plugins
  and we probably want to prioritize tmux internal stuff first. But this is
  just a hunch and not verified.
