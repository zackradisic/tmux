- DONE (2026-08-27): seamless server upgrade, as `restart-server`. The design
  seed below turned out to be the wrong shape: the server replaces its own
  image with `execve` instead of forking and passing the pty masters over
  SCM_RIGHTS, which keeps the pid and so keeps the panes as our children, and
  with them `waitpid`, `remain-on-exit` and `#{pane_dead_status}`. See
  `SERVER-RESTART.md`, `server-handoff.c`, `SPAWN_ADOPT` in `spawn.c`, and
  `regress/restart-server.sh`.
- Original seed, kept for the record: restart the server (e.g. onto a
  freshly built binary) without killing pane processes. Panes die today
  because server exit closes every pty master (SIGHUP to the slaves) - so
  hand the masters off instead: old server serializes its world (sessions,
  windows, layouts, pane pid/cwd/size, options, plugin snapshots, ideally
  scrollback) and passes the blob + every master fd over a unix socket
  (SCM_RIGHTS) to the new server, nginx binary-upgrade style; new server
  adopts the fds instead of forkpty. Known costs: adopted panes are not our
  children (no waitpid - detect death via master EOF/EIO, no
  #{pane_dead_status}); scrollback must ride the blob or be lost; ~64K pty
  buffer while nothing reads; per-server transients (jobs, prompts, popups)
  don't survive. Prior art: reptyr -T proves the pty part. (2026-08-25)
- Probably want to have strong handles to tmux objects later at some point as I anticpiate this could come up
- At some point we may want to have separate queues. The notify_add() thing
  seems to be used for tmux internal stuff and I could imagine there could be a
  lot of contention on this queued callback thing if we have a lot of plugins
  and we probably want to prioritize tmux internal stuff first. But this is
  just a hunch and not verified.
