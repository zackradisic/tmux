# Remote links: known UX problems

Rough edges hit while running `remote-attach` against four real hosts (a
Tailscale box and three GCP VMs). These are observations, not designs —
nothing here is fixed yet. See `REMOTE.md` for how the feature works.

The theme running through most of them: **a remote link fails in ways that
look like nothing happening.** The local side stays plausible-looking while
the thing you wanted silently did not occur.

## 1. A server that cannot report agents looks like a server with none

`training-1-7b` ran seven Claude sessions for days without one of them
appearing in the agents picker. Its tmux2 was `v3.8-wasm.4`, built before
`c8171f6e` added roles, services and the bridge, so it had no
`plugin-bridge` command at all — the local server could not push `agents`
there and no provider existed to answer. Nothing said so. The picker
grouped by server and simply omitted it, which is indistinguishable from a
linked server that happens to be idle.

The only way to find it was to go looking:

```
$ ssh training-1-7b tmux2 list-commands | grep -c plugin-bridge
0
```

`d9b7f782` already built the right shape for the neighbouring case — a
copy whose *service version* is rejected shows as one yellow line in the
picker saying why. That does not cover a remote which never handshook at
all, because no handshake means no version to reject.

Two other variants of the same silence, both since fixed, show it is a
pattern rather than one bug:

- A stale local `agents.wasm` was pushed to a remote and **displaced the
  remote's own newer build**. Both reported service version `0.1.0`, so
  the version check passed. Fixed by the keep-local rule in `d9b7f782`,
  but only for a remote that loads the plugin itself.
- The sidecar `agents.toml` did not request `service-serve` /
  `service-call`, and `caps::compute()` returns `requested & granted`, so
  the caps were masked no matter what the user granted. The only symptom
  was one line in `plugin-log`. Fixed in `9398705b`.

Worth considering: every linked server gets a row in the picker
unconditionally, carrying a reason when it cannot report; a version floor
checked at link time and surfaced through `show-messages` and
`#{remote_error}`; and some fleet view (`remote-status`?) listing host,
tmux version, plugin support and link state.

## 2. `ssh host tmux2` does not find tmux2

`install-tmux2.sh` puts the wrapper in `~/.local/bin` and prints

```
note: add /home/ubuntu/.local/bin to PATH
```

which does not help the case that matters. Ubuntu's stock `~/.bashrc`
returns at line 8 for a non-interactive shell, so a `PATH` export appended
to it never runs, and `~/.profile` is not read for an ssh command at all:

```
$ ssh dev-8x 'command -v tmux2'
  NOT FOUND
$ ssh dev-8x 'bash -ic "command -v tmux2"'
/home/ubuntu/.local/bin/tmux2
```

Non-interactive ssh is exactly what `remote-ssh-command` and any probe use.
The consequence is that tmux2 looks absent on a host that has it, so you
fall back to the stock `tmux` and create a **second, parallel session on a
different socket** — which is how a fresh empty session got made on
`training-1-7b` next to the real one holding twelve windows of work.

`/usr/local/bin` *is* on the default non-interactive PATH, so
`sudo ln -sf ~/.local/bin/tmux2 /usr/local/bin/tmux2` closes it, and the
installer could do that (or warn) rather than printing advice that only
fixes interactive shells.

The structural half of this: creating a remote session and attaching one
resolve the remote binary through different code paths, so they can
disagree. `REMOTE.md` lists `new-session -H host` as planned; landing it
would put both through `remote-ssh-command` and remove the chance of drift.
Until then, probe by absolute path (`~/.local/share/tmux2/bin/tmux`) rather
than `command -v`.

## 3. A shadow session going away can exit the whole client

With a client attached to a shadow session, dropping the link takes the
client down with it. `remote_link_destroy` calls `server_destroy_session`,
and under the default `detach-on-destroy on` that path never looks for a
replacement session — it sets `CLIENT_EXIT` (`server-fn.c`). Your entire
local tmux exits, with every other session left behind.

For a session the user deliberately killed this is ordinary tmux
behaviour. For a shadow session it is worse than that, because the reason
is usually not a decision about *that* session's contents: a remote
rebooted, a VM was preempted, a link was recycled. And a shadow session
holds no local processes — destroying it loses nothing — so falling back
to another session is strictly safer than exiting.

Observed by running `remote-attach -k` against a host whose shadow session
the user was attached to.

Worth considering: shadow sessions could opt out of `detach-on-destroy`,
or `remote_link_destroy` could move attached clients to another session
before tearing down, falling back to detach only when none exists.

## 4. The disconnect notice repeats forever and corrupts the mirror

`remote_link_down` writes `[remote: <host> disconnected: <reason>]` into
every shadow pane, and it is called from `remote_link_job_complete` on
*every* failed reconnect. With backoff capped at `REMOTE_BACKOFF_MAX` (60)
that is one line per pane per minute, indefinitely. A box left down
overnight accumulates hundreds. From a stopped GCP VM:

```
[remote: dev-4x disconnected]issions on (shift+tab to cycle) · ← for agents
[remote: dev-4x disconnected: ssh: connect to host 34.31.135.33 port 22: Operation timed out]
[remote: dev-4x disconnected: ssh: connect to host 34.31.135.33 port 22: Operation timed out]
[remote: dev-4x disconnected: ssh: connect to host 34.31.135.33 port 22: Operation timed out]
```

Note the first line. It is written at wherever the cursor happens to be, so
it landed inside the mirrored Claude TUI's last line. The leading `\r\n`
does not save it, because the remote's own last write left the cursor
mid-line.

More fundamentally the shadow pane's grid is a *mirror* of the remote.
Writing local status into it desynchronises the two and leaves junk
interleaved with real content until a capture refresh replaces it —
whatever the pane is showing, a TUI, a pager, an editor, gets stomped.

The formats needed to do this properly already exist and are undocumented.
`#{session_remote_host}` is set only on a shadow session, which makes it a
clean discriminator, and `#{remote_connected}` and `#{remote_error}` carry
the rest. This in `status-left` covers all three states and writes nothing
into any pane:

```tmux
set -g status-left "#{?session_remote_host,#{?remote_connected,#[fg=colour245]⇄ #{session_remote_host}#[default] ,#[bg=red#,fg=white#,bold] ⚠ #{session_remote_host} disconnected#{?remote_error,: #{=/48/…:remote_error},} #[default] },}[#{session_name}] "
```

It also tracks state continuously, which a line in the scrollback cannot.
Worth shipping in `example_tmux.conf`, documenting the three formats, and
perhaps adding a `#{remote_state}` rendering one word rather than making
everyone compose that conditional.

## 5. `remote-ssh-command` is one global option for every host

Hosts differ in where their tmux lives, so a single option becomes a
nested conditional that grows a level per host. At three remotes:

```tmux
ssh -T -o BatchMode=yes #{q:remote_host} #{?#{m:*ovh-devbox*,#{remote_host}},/home/zack/tmux2/tmux,#{?#{m:dev-4x*,#{remote_host}},/home/ubuntu/.local/bin/tmux2,tmux}} -C attach -t #{q:remote_session}
```

The `/usr/local/bin` symlink from §2 collapses this to one special case,
but that is a workaround for the hosts, not a fix for the option. A
session creator that makes remote sessions easy to add will make people
add hosts casually, and whatever writes those entries needs somewhere
per-host to write them.

## 6. Plugins act locally on paths that only exist on the remote

A shadow pane reports the *remote* path, because that is what the link
syncs (`rl_path`). A plugin reading `#{pane_current_path}` gets it and
then does local work with it, against a path that does not exist here:

```
$ tmux2 display -p -t %276 '#{pane_current_path}'
/home/ubuntu/training
$ ls -d /home/ubuntu/training
ls: /home/ubuntu/training: No such file or directory
```

Two user-visible failures in `session_creator`, both from this. Opening
the form on a shadow pane prefills the folder field with the remote path,
so **the completion list is empty** — the directory scan that fills it
runs locally and finds nothing to scan. And accepting it to create the
folder fails, because macOS will not create anything under `/home`:

```
$ mkdir -p /home/ubuntu/training
mkdir: /home/ubuntu: Operation not supported
```

(Reported in the field as "Operation not permitted"; the exact errno
varies with the path, the cause is the same.)

This is not specific to `session_creator`. Any plugin that reads a path
format from a pane and then touches the filesystem has the same bug on a
shadow pane, and it fails silently rather than saying why. `git_status`
is `scope pane` and currently runs 52 instances against 29 shadow panes,
every one of them resolving a remote path locally.

The fix is the provider/view split these plugins never got.
`WRITING-PLUGINS.md` documents it: the provider runs on each server and
sees only that machine's panes, processes and files; the view runs
locally and owns the UI. `agents` is built this way and works across five
servers. `session_creator` references `role`, `service_register` and
`provides()` exactly zero times; `agents` references them twelve. So the
form's directory scan and its `mkdir` belong in a provider that runs on
the server owning the target pane, with the view keeping only the UI.

The machinery is not the missing part. The ABI already reports it per
pane — `pane.flags` bit `8 remote`, with `host` naming the owning server
and `cwd` documented as "the remote's cached current path", both empty
for a local pane. Neither plugin checks any of it.

Two things make this easy to walk into. First, both plugins predate
`c8171f6e`, which added roles, and were never revisited. Second,
`WRITING-PLUGINS.md` introduces the split as an opt-in for "a plugin that
wants to see every linked server" — framed as a feature for building
multi-server UIs, not as a hazard that any plugin touching a pane's path
or processes must handle or silently act on the wrong machine. A note at
the point an author would hit it would have caught both.

And `session_creator` is already pushed to every remote as `role
provider` while containing no provider code, so it runs as a live no-op
on three of Zack's boxes. The deployment assumed a split the plugin does
not have.

Half of the create path no longer needs a provider at all: `remote-attach
-t name -c dir host` (16090b66) creates the session on the remote and
mirrors it. What has no equivalent is listing *directories* on the remote
for completion — `remote-attach -L` lists sessions. That gap is the part
a provider is genuinely needed for.

## 7. A descriptor reaches the event loop unvalidated

`58eb8c66` added `handoff_fd_ok()` — `fcntl(fd, F_GETFD)` plus a
`ptsname(fd)` cross-check against the tty the pane was saved with — and
its comment states the stake exactly: "One that is not open, or not the
tty it was saved with, must not reach the event loop: select() would fail
on it for good and the server would hang at full CPU."

That check guards the handoff path only. It touched `server-handoff.c`,
`server.c`, `proc.c` and `log.c`; it did not touch `spawn.c` or
`window.c`. So every other way a pane is created still hands libevent a
number nobody verified:

```c
if (sc->adopt_fd != -1) {
        new_wp->fd = sc->adopt_fd;      /* spawn.c:481 - no check */
        new_wp->pid = sc->adopt_pid;
}
```

and two lines before the descriptor reaches libevent, `window.c` performs
the very check it needs and discards the answer:

```c
setblocking(wp->fd, 0);                 /* fcntl(F_GETFL), result dropped */
wp->event = bufferevent_new(wp->fd, ...);
```

`setblocking()` (`tmux.c`) returns void and silently does nothing when
`fcntl` fails, so a dead descriptor sails into the event loop.

`window_pane_set_event()` is the chokepoint every path funnels through -
adopt, respawn, ordinary spawn - and is where `handoff_fd_ok()`'s logic
belongs, generalised. One bad pane should cost that pane, not the server:
the fd is shared with the accept socket and libevent's own signal pipe,
so poisoning it takes down command handling and signal handling together,
leaving `kill -9` as the only exit.

macOS makes this easy to hit. `revoke(2)`/vhangup invalidates *every*
descriptor to a tty at once when its session leader goes, with no close
the owner can observe; `lsof` then shows the fd as `(revoked)`. Such a
descriptor was present on a wedged server here.

## Fixed, kept for the record

- **Pre-first-connect failures were completely invisible.** A bad session
  name, a host key failure, a wrong binary path — all showed the same
  `connecting to <host>` placeholder forever while the backoff spun,
  because `remote_link_down` reports into shadow *panes* and a link that
  never came up has none. Fixed in `bae64eef`: the placeholder window name,
  the placeholder pane, `show-messages` and `#{remote_error}` all carry the
  reason, and ssh's stderr passes through with `JOB_SHOWSTDERR`.
- **`#{remote_host}` expanded to the empty string**, so the shipped default
  `remote-ssh-command` never worked over real ssh — `format_find` consults
  the format table before the tree and returns the table callback's NULL.
  No test caught it because every regress case used only
  `#{remote_session}`. Fixed in `bae64eef`.

## Not a link bug, but adjacent

Killing a shadow session is the supported way to drop a link and nothing
says so. `x` in `choose-tree` prompts and does the right thing:
`session_destroy` → `remote_link_session_destroyed` → the deferred
`remote_link_destroy` kills the ssh job. The remote session keeps running
and re-attaches cleanly. Verified: ssh process count went 1 → 0 while the
remote session survived untouched. Worth a line in `REMOTE.md` — subject
to §3 above, which is what makes it unpleasant today.
