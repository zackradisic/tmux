# tmux2 UX notes

Rough edges hit while actually living in this thing. Observations and
half-formed ideas, not designs — nothing here is fixed unless a heading
says so. Three areas so far: remote links, the agents picker, and the
session/window chooser.

- [Remote links](#remote-links) — §R1–R8
- [Agents picker](#agents-picker) — §A1–A2
- [Sessions and windows](#sessions-and-windows) — §S1

## Remote links

Hit while running `remote-attach` against four real hosts (a Tailscale box
and three GCP VMs). See `REMOTE.md` for how the feature works.

The theme running through most of them: **a remote link fails in ways that
look like nothing happening.** The local side stays plausible-looking while
the thing you wanted silently did not occur.

### R1. A server that cannot report agents looks like a server with none

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

### R2. `ssh host tmux2` does not find tmux2

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

### R3. A shadow session going away can exit the whole client

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

### R4. The disconnect notice repeats forever and corrupts the mirror

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

### R5. `remote-ssh-command` is one global option for every host

Hosts differ in where their tmux lives, so a single option becomes a
nested conditional that grows a level per host. At three remotes:

```tmux
ssh -T -o BatchMode=yes #{q:remote_host} #{?#{m:*ovh-devbox*,#{remote_host}},/home/zack/tmux2/tmux,#{?#{m:dev-4x*,#{remote_host}},/home/ubuntu/.local/bin/tmux2,tmux}} -C attach -t #{q:remote_session}
```

The `/usr/local/bin` symlink from §R2 collapses this to one special case,
but that is a workaround for the hosts, not a fix for the option. A
session creator that makes remote sessions easy to add will make people
add hosts casually, and whatever writes those entries needs somewhere
per-host to write them.

### R6. Plugins act locally on paths that only exist on the remote

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

### R7. A descriptor reaches the event loop unvalidated

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

### R8. The fleet view is one undifferentiated block of lines

Eleven agents across four servers renders as twenty-one consecutive lines
with nothing between any of them:

```
 agents (11 live, 4 servers)
  search (/ or ↑ to search)
  ────────────────────────────────────────────────────────────
▪ local
 waiting
▸ ◍ ✳ training-1-7b SSH pane recovery                        claude      11m
 working
 ●  ✳ Dmatrix agent flickering state                         claude       5s
▪ dev-8x
 waiting
 ◍  Qwen 3.5 2B Training                                    claude       2m
 ◍  ✳ GLM-5.3 data logits sync migration                    claude   11h57m
 ...
```

A server group ends and the next server header begins on the very next
line. `rebuild_lines()` (`view.rs:567`) pushes `Line::Server`, then
`Line::Header`, then items, back to back — there is no spacer line
anywhere in the enum, so the only thing separating `dev-8x`'s last row
from `dmatrix`'s header is the one-space indent difference between an
item and a server line. Scanning for "which host is this row on" means
reading upward through rows until a `▪` appears.

The band headers make it worse rather than better. `waiting` and
`working` are rendered in dim bold at the same indent as the rows
(`view.rs:1869`), and every server repeats them, so four servers with two
bands each produce eight faint header lines interleaved with eleven
content lines. Roughly half the list is chrome, and the chrome all looks
alike.

What the layout should carry, in order of how much it buys:

- **A blank line above each server header** (except the first). This is
  the whole complaint in the title — one spacer turns four runs of rows
  into four visible groups.
- **Server headers that outrank band headers visually.** Today the
  server is `▪ name` in bold colour and the band is ` name` in dim bold;
  they are one character of indent apart. A rule to the right of the
  server name, or indenting bands and rows one level under their server,
  would make the hierarchy readable without counting characters.
- **Dropping the band header when a group has one band**, which is the
  common case for small servers: `dev-8x` showing `waiting` above four
  waiting rows says nothing the badges do not.

The cost is real and should be stated: the list is at most
`min(LIST_MAX, height-5)` display lines and headers already consume
them, so spacers trade visible agents for legibility. Two mitigations
keep that honest — suppress the spacer when it would be the first line
of the scroll window (`scroll_to_selection()` already pulls headers in at
the top, `view.rs:631`, and a spacer needs the same treatment plus a rule
never to render as the top line), and skip spacers entirely while a
search filter is active, since a filtered list is short and wants
density.

Whatever is added must be a new `Line` variant, not a `\n` inside an
existing one: `lines` is the scroll coordinate space (`Line::Item`
carries a position in `view`, and `sel_line()`/`scroll_to_selection()`
count display lines), so a row that secretly renders two lines would
desynchronise the selection from the screen.

### Fixed, kept for the record

- **Pre-first-connect failures were completely invisible.** A bad session
  name, a host key failure, a wrong binary path — all showed the same
  `connecting to <host>` placeholder forever while the backoff spun,
  because `remote_link_down` reports into shadow *panes* and a link that
  never came up has none. Fixed in `bae64eef`: the placeholder window name,
  the placeholder pane, `show-messages` and `#{remote_error}` all carry the
  reason, and ssh's stderr passes through with `JOB_SHOWSTDERR`.
- **resurrect restored a linked session as a local fake.** `SavedSession`
  had no remote fields and the plugin never called `remote-attach`, so a
  shadow session was snapshotted as if it owned its panes — capturing the
  *remote's* scrollback into every autosave — and restored as local shells
  wearing the remote's name: no `remote_host`, no `⇄` tag, nothing
  connected, while the remote server still held the real state. Now the
  snapshot stores the link alone (host and remote session, `windows`
  empty) and restore runs `remote-attach`, which is what the server
  handoff already did in `server-handoff.c`. The host comes from the link,
  not the session name, which has had `.` and `:` replaced by `_`.
  `remote-attach` returns before the link is up, so an unreachable host
  leaves a disconnected session carrying the reason instead of failing the
  restore. Needed a new `#{remote_session}` format: the name only existed
  inside the throwaway tree that expands `remote-ssh-command`. Covered by
  `regress/plugin-resurrect-remote.sh`.
- **`#{remote_host}` expanded to the empty string**, so the shipped default
  `remote-ssh-command` never worked over real ssh — `format_find` consults
  the format table before the tree and returns the table callback's NULL.
  No test caught it because every regress case used only
  `#{remote_session}`. Fixed in `bae64eef`.

### Not a link bug, but adjacent

Killing a shadow session is the supported way to drop a link and nothing
says so. `x` in `choose-tree` prompts and does the right thing:
`session_destroy` → `remote_link_session_destroyed` → the deferred
`remote_link_destroy` kills the ssh job. The remote session keeps running
and re-attaches cleanly. Verified: ssh process count went 1 → 0 while the
remote session survived untouched. Worth a line in `REMOTE.md` — subject
to §R3 above, which is what makes it unpleasant today.

## Agents picker

### A1. "waiting" is two different states wearing one badge

Everything not `working` and not `needs_input` lands in `waiting`, and the
band sorts above `working` on the theory that a stopped agent may want
something (`band()`, `view.rs:1381`). Two populations share that band and
they want opposite things:

- **Actionable.** The agent stopped, I have the next prompt in my head, I
  want to send it now. These want to be at the top and want to be short-
  lived — the whole point is that I clear them.
- **Lingering.** Blocked on something else (a VM, a review, another
  agent's output), or finished-ish but holding information I still want to
  read. I cannot act on these today and I do not want to lose them.

Because both are `waiting`, the band grows until it is useless: half the
rows are a to-do list and half are a filing cabinet, and nothing in the
row distinguishes them. The band header says `waiting` over both.

`archive` was supposed to absorb the second group and does not, because
it is built for *out of sight for a long time* — an archived row leaves
the roster entirely and rejoins the pile of finished agents behind `h`
(see §A2). That is far too heavy for "blocked until the eval finishes",
so the lingering rows stay in `waiting` and the pile-up continues.

Ideas, roughly in order of how much they change:

1. **Make the archive cheap to come back from** (§A2) and the pressure
   mostly goes away — I would archive the lingering half without
   flinching. Half of this problem is really a findability problem.
   *Done first: `A` is now the archive as its own list, searchable
   (§A2, "Fixed").* Whether that is enough on its own, or (2)/(3) is
   still wanted, is a question of living with it for a while.
2. **A third life between `active` and `archived`.** `life` is already
   `active | stale | archived` (`store.rs:36`), so this is a fourth
   value, not a new axis: still in the roster, still previewable, but in
   its own band below `working` rather than above it. "idle" is the
   obvious name and a bad one — `waiting` already means idle. "parked",
   "held", "later", "on ice" are all closer to what it means: *I chose to
   set this one down.*
3. **Invert the default: `needs_input` becomes what an agent gets when it
   stops, and `waiting` becomes the state I move it to.** Then the top
   band is by construction the actionable list, and `waiting` is the
   filing cabinet — the split is mine to maintain rather than something
   the classifier guesses. Costs: `needs_input` currently means something
   specific and earned (a real prompt on screen), and this makes it mean
   "stopped"; the unread/ack machinery (`acked_ms`, the bright `◉`) is
   built around that meaning and would have to move with it.

(2) and (3) are the same decision seen from two ends — either the user
promotes rows *out* of the attention band or demotes them *into* a parked
one. Worth picking one and not shipping both.

### A2. The archive is where agents go to become unfindable

The friction is not archiving, it is everything after. Four separate
things made the archive feel like a write-only sink. The first three are
fixed (see the end of this section); the fourth is open.

**It is not a list of archived agents.** `h` folds in `store::history()`
(`store.rs:612`), which is `ended_ms IS NOT NULL OR life = 'archived'`,
capped at `HISTORY_MAX = 100` and sorted by recency. So the twelve things
I deliberately set aside arrive mixed into a hundred finished agents,
ordered by when they happened to stop. There is no view of *just* the
archive, which is the view I actually want.

**Content search cannot see it.** `^F` calls `search_local()`
(`provider.rs:642`), which starts from `store::live_agents()` — by
definition `life != 'archived' AND ended_ms IS NULL` — and searches the
live panes. Archived and finished rows are exactly the set it skips. The
one search that would justify burying something is the one that cannot
reach it.

**Previews are missing for some of them, and the rule is not obvious.**
`save_capture()` is called from one place: the `done` branch of `report()`
(`provider.rs:509`). So a capture exists only if the shim reported `done`
while the pane was still alive. An agent whose pane was killed, whose
server went away, or that was archived while alive and later retired,
never hits that branch — `get_capture` returns nothing and the preview is
`no capture`. (Rows predating the `captures` table have none either.)
Fix candidates: capture on archive as well as on done; capture
periodically for anything stopped; or stop depending on the pane
entirely, below.

**The search itself is not fzf-shaped.** Two gaps:

- *Matches are invisible in the list.* Ranking is prefix > substring >
  subsequence over a joined haystack (`view.rs:1477`), and the row shows
  the name either way — I cannot see *why* something matched, or which
  of five similar rows matched on the interesting word. fzf shows the hit
  inline; we would have to show the matched line from the pane content,
  which means carrying that line back with the hit rather than only the
  row id.
- *The hit is not reachable in the preview.* Showing a content match
  properly means the preview has to scroll to it, and the preview is a
  captured tail of a TUI — the text above the fold may never have been
  captured, because the agent's own TUI is virtually scrolled and only
  ever painted a window of itself.

That last point suggests the structural fix, which is bigger than search:
**stop previewing the pane and render our own view from the session
transcript.** `~/.claude/projects/<slug>/<id>.jsonl` (and the equivalent
for other harnesses) has the full turn history, unbounded by what was on
screen, already on disk, and readable long after the pane is gone. We
already parse these files for enrichment (`resolve.rs`), so the path is
known. A preview built from the transcript would: work identically for
live, finished and archived agents; remove the capture-on-done
dependency entirely; be searchable in full, with real line hits to show
in the list and a real position to scroll to; and be scrollable without
touching the agent's TUI. The cost is writing a renderer per harness and
keeping it honest about what the transcript does not contain (raw
terminal output, anything the agent printed outside the protocol).

**Fixed, kept for the record.** `A` in the picker (`pick_archived`) is
the archive as a list of its own: only archived rows, every one of them
rather than whatever survives `HISTORY_MAX`, from every server (the
`list` request gained `archived`; an older provider answers with capped
history, and the view still filters it). Entering it turns history on;
leaving it puts history back the way it was, and `h` off leaves it too.
The header says `archive`, an empty one says `(nothing archived)`. The
search box filters inside that set, and `^F` reaches it: a live archived
row is grepped in its grid as before, an ended one in its saved capture
(`store::archived_captures`, over the same `search` RPC for a remote
row, which gained `archived`). To make that capture exist, archiving a
live agent now saves one (`provider::capture_on_archive`), on both the
local `a` path and the remote `act archive`; `sweep_gone` also covers
archived rows now, so a lost pane ends the row and the capture takes
over. Content hits key on the row (`Agent::key`) rather than the pane,
which is what let a pane-less row carry a snippet at all; the snippet
now wins over the `archived` tag on the row. Captures are grepped in
Rust (substring, then subsequence), so a *regex* query only ever hits a
grid, never a capture. `regress/plugin-agents-archived.sh` covers it.

**Fixed, the structural half.** The transcript is now the source of the
search and of the preview for anything without a pane. The harness's
own file (Claude's `~/.claude/projects/<slug>/<id>.jsonl`, Codex's
rollout) is read when a turn ends (a `waiting`/`needs_input` report)
and when the agent ends (pane gone, `done`), condensed to prompts,
replies and one line per tool call (`Edit view.rs +12 −3`; never the
tool's output), and stored in `turns` for `history_days` (365 by
default) - past Claude's own 30-day cleanup of the file, so the
searchable history is no longer capped at what the harness keeps. The
search box searches those conversations through an inverted index held
in memory (`index.rs`; one document per user turn, bm25, paths at half
weight), so a query brings in every agent that ever talked about the
word, live or dead, history on or off, best first, with the matching
line on the row - and the preview opens on the matching turn. A
finished agent's preview is its conversation; `Tab` shows a live one's
in place of its pane; `[`/`]` scroll it. The `HISTORY_MAX` cap now only
bounds the empty-query browse. `extract.rs` (per harness),
`transcript.rs` (ingest, index lifecycle), `regress/plugin-agents-
transcript.sh`.

### A3. What the conversation search does not do yet

- **A harness with no hooks is indexed only when it ends.** The
  turn-end trigger is the shim's status report; a Claude without the
  hooks, or a pi/opencode with none, gets its transcript read when the
  pane dies (and at server start). Its live pane is still grepped by
  `^F`, so what is on screen is findable; what scrolled off is not until
  it ends. A fallback off `pane-command-changed` plus a quiet period
  would close this.
- **The current turn is not searchable until it ends.** By design (the
  file holds a half-written turn), but a long turn is invisible to the
  conversation search for its whole duration.
- **pi and opencode have no extractor.** `extract::for_kind` knows
  Claude and Codex; the others get rows and captures as before, no
  turns. The Codex extractor is written against archived rollouts, not a
  live one.
- **Tool results and full diffs are not stored.** The row keeps the
  record's byte offset in the transcript, so expanding a tool call in
  the preview could seek the original while it exists; nothing does yet.
  Once Claude deletes the file, the one-line condensation is all there
  is.
- **The snippet lands a beat after the row.** The hit is computed in the
  keystroke; its snippet (and the row itself, for an agent the roster did
  not hold) comes back from the store a moment later, so a fast typist
  sees rows appear before their matching lines do.
- **Relevance order replaces the frozen order while a query is typed.**
  Rows sort by band then score, so two keystrokes can reorder the live
  band under the cursor. The cursor follows its row; the eye may not.
- **Retention pruning rebuilds the whole index.** `prune` deleting any
  agent triggers a full rebuild from `turns` (page by page, one page per
  wake). Cheap at hundreds of sessions; at tens of thousands it would
  want tombstones and a merge instead.

## Sessions and windows

### S1. `prefix w` and `prefix s` should look like the agents picker

Stock `choose-tree` is the wrong shape for how this fork is used. What it
gives today (`window-tree.c`, `mode-tree.c`):

- The preview is a box *below* the list, full width
  (`box_x = w - 4; box_y = sy - h - 2`, `mode-tree.c:1021`), so the list
  gets the top third and the preview is short and wide.
- `/` is find-next over a prompt, and `f` is a filter written as a format
  string. Neither is incremental narrowing with the list shrinking as you
  type.
- It has no idea a session might be remote. Nothing in `window-tree.c`
  knows about links, so a shadow session is drawn as an ordinary local
  one — no host, no connection state, nothing to act on.

What it should be, which is the agents picker's shape:

- **List left, preview right**, the same split, the same scrolling, the
  same search-as-you-type.
- **Remote panes managed in place**: a shadow session shows its host and
  link state on the row, and keys act on the link — reconnect, drop,
  reattach — instead of making me remember `remote-attach -k` (and §R3,
  which is what makes killing one unpleasant today).
- **`session_creator` stays the creation path.** It already binds into
  `choose-tree` (`bind -T choose-tree S plugin-command session_creator
  new`) and already resolves the highlighted row as its target, so a
  replacement chooser has to keep delivering `plugin-command` with the
  highlighted item as target or that integration breaks.

The idea worth chasing: **the agents picker and the session chooser may be
one view.** The agents picker is already a list of panes grouped by server
with a preview, a search, marks and per-row verbs. A session chooser is a
list of panes grouped by session with a preview and per-row verbs. If the
grouping key and the row filter are parameters, "agents" is that view
filtered to panes running an agent and grouped by server, and "sessions"
is the same view unfiltered and grouped by session/window. That would get
`prefix w` the search, the layout and the remote awareness for free, and
would mean one set of keys to learn instead of two.

Unresolved, and the reason this is a note rather than a plan: the agents
picker's state (bands, unread, archive, marks) is agent-specific and does
not generalise to arbitrary panes, so "one view" probably means one
rendering engine with two configurations, not literally one picker. Worth
sketching where the seam goes before building either.
