# Accounts: several Claude Code logins per machine

Claude Code keeps one login per machine, per user. The credential (an
OAuth access and refresh token pair) sits in the macOS Keychain under
`Claude Code-credentials`, or in `~/.claude/.credentials.json` elsewhere;
the identity (`oauthAccount`: email, organization, account uuid) sits in
`~/.claude.json`. Every claude on the machine reads those, and a claude
re-reads the credential whenever it refreshes its access token.

The `accounts` plugin keeps more of them. Each stored login is a numbered
slot in the plugin's store, with its credential blob and identity; the
panel shows every slot's rate-limit windows (5-hour, 7-day, per-model) and
makes any slot the login. The provider half runs on every server a
`remote-attach` link pushes it to, so the workstation's panel lists each
linked server's slots and switches a login over there with nothing
installed on it beyond tmux2 and curl. That is the point of doing this in
a plugin instead of running cswap on every box: the wasm travels over the
bridge.

## What a switch does

A switch changes the machine's login, nothing less: it is what `/login`
would do with the other account. A claude already running there keeps its
in-memory access token until that expires, then re-reads the credential
and continues as the new account. So:

- a switch is global to the server; claudes running there move at their
  next refresh (within the hour, typically);
- `n` in the panel is the per-window alternative: a claude started as a
  slot through `CLAUDE_CONFIG_DIR`, which stays that account whatever the
  machine's login does (below).

### Under claude's own locks

Claude guards its token refresh with proper-lockfile directories:
`~/.claude/.oauth_refresh.lock` then `~/.claude.lock` (stale after 60 s, a
live holder touches every 5 s), and `~/.claude.json` with
`~/.claude.json.lock` (stale after 10 s). A credential swap that landed
inside a refresh would be overwritten by the refreshed old token. So the
plugin holds the same directories the same way, in `claude.rs`:

1. One shell job takes both credential locks (`mkdir` is the mutex; a
   directory older than its stale time is removed and retried, a live one
   is waited on for ~9 s then refused) and prints the live credential as
   it is under them.
2. The plugin backs that credential up into the slot it belongs to (so a
   rotation claude did is not lost), composes the target - the slot's
   blob with the machine-wide keys (`mcpOAuth` and friends) taken from
   the live one - and stages it in its own data directory (0700).
3. A second job installs it (Keychain through `security -i` so the
   secret stays off argv, or the file via `cp` under `umask 077` and
   `mv`) and removes the lock directories.
4. The same two steps for `~/.claude.json`: lock, splice `oauthAccount`
   into the whole file, install, unlock.

If the plugin dies between the two jobs the directory goes stale and
claude steals it: the protocol working as designed. Network never
happens under a lock; a slot whose access token has expired is refreshed
through the token endpoint before the lock is taken.

### The store

`store.db` holds the slots, their credential blobs, the cached usage and
a few settings. The host creates files 0644, so `init` makes the plugin's
data directory 0700. Nothing outside that directory is written by the
host: every install goes through the shell as above. There is no second
copy of a credential anywhere else; `remove` forgets a slot and leaves the
machine's login alone.

## Usage

`oauth.rs` calls `GET /api/oauth/usage` with the slot's access token
(bearer, plus the `oauth-2025-04-20` beta header) and normalises the
answer: `five_hour` and `seven_day` carry `utilization` and `resets_at`,
the `limits[]` entries with a `scope.model.display_name` are the per-model
weekly windows ("Fable"), `extra_usage` is the pay-as-you-go spend. A
window's expected spend by now is the elapsed fraction of it; 15 points
over that reads "(ahead of pace)" (cswap's threshold), 100 % reads "(!)".

The endpoint budgets non-first-party clients at roughly thirty requests
an hour per identity, so a slot is refetched at most every three minutes,
every ten once it has nothing left or after a 429, and the panel's timer
only asks the providers, which only hit the API when a slot is due. A
slot whose token has expired while it sat idle is refreshed first
(`POST /v1/oauth/token`, a refresh-token grant with Claude Code's client
id) and the rotated blob stored; for the live slot that refresh happens
under the locks and is installed, so claude never finds its token pulled
out from under it.

All of it goes through curl in `run_job`: the guest has no sockets. The
bearer token travels in a staging file (`-H @file`), never on a command
line.

## Per-window logins: `n`

`profile <slot>` makes a directory under the plugin's data dir
(`profiles/<slot>-<email>/`) seeded with the slot's credential, a
`.claude.json` carrying its identity plus the machine config's theme,
MCP servers and per-project entries (folder trust above all, or the new
claude stops at "do you trust this folder?"), and symlinks to the shared
customisations in `~/.claude` (settings, keybindings, CLAUDE.md, skills,
commands, agents) - cswap's session-mode layout. A claude started with
`CLAUDE_CONFIG_DIR` pointing there runs as that account and keeps its own
history. On a Mac claude migrates the seeded file into a Keychain item of
its own (`Claude Code-credentials-<sha256(dir)[:8]>`).

The panel's `n` asks the provider for the profile, then opens the agents
plugin's new-agent form (`plugin-command agents 'new [@server] --client
<name> --profile <dir>'`) with the launch line prefixed
`CLAUDE_CONFIG_DIR='<dir>'`, so the agent starts where and how the picker
would start it. The agents roster reads `CLAUDE_CONFIG_DIR` from the
pane's environment and looks for the session file (and the transcript)
under it instead of `~/.claude`; its sidecar allows that variable and the
profiles directory.

## Adding a login: `+`

`+` (or `plugin-command accounts login [server]`) makes a profile with no
credential in it (`profiles/login-<n>/`, seeded like the others) and
opens a claude under it in a popup on the pressing client (a window on a
remote, or with no client). That claude has no login, so it asks for one;
the machine's own login is never replaced, and every other claude keeps
running as it was. The provider watches the profile for twenty minutes
(every two seconds: the hashed Keychain item on a Mac, the file
elsewhere) and stores what lands as a slot, with the identity claude
writes into the profile's config, or from the profile endpoint when that
is missing. A status line says which slot. The popup can then be left
with `/exit`, or kept: it is a claude running as the new account. The
`claude` config key names what the popup runs (a test stands `sleep` in).

`a` is the other way in: it stores whatever the machine is logged in as,
for a login made with `/login` the usual way.

## Across a link

The panel calls `list`, `switch`, `best`, `add`, `profile`, ... on
`accounts@<server>`. An initiator's calls are never gated, so no
`plugin-peers` grant is needed. The provider's sidecar lists nothing in
`serve_remote`: a server that linked IN cannot switch the login here.

A pushed copy runs with the receiving server's `plugin-remote-caps`,
which by default leaves out `run-process`, `fs-write` and `fs-read-any`;
the provider needs all three (the shell, the staging files, the files
under `~`). So on each remote, once:

```
tmux2 set -s plugin-remote-caps "$(tmux2 show -sv plugin-remote-caps),run-process,fs-write,fs-read-any"
```

(in that server's tmux.conf, to survive a restart), or an entry for the
plugin in that server's own manifest, which grants from its sidecar. A
pushed copy without the shell says so in its block.

## The panel

`plugin-command accounts pick [server] [slot]`; the agents picker's `@`
opens it for the row's server. One block per server: the local one, then
each linked server that is up. The cursor walks the slots; `Enter`
switches (asks), `b` best pick (the slot with the most room before its
binding window, asks), `+` logs in to another account, `a` adds the
server's login, `n` starts a claude as
the slot, `r` refetches, `d` holds a slot out of best pick, `x` removes
(asks), `i` imports cswap's accounts from the Keychain, `c` compact rows,
`?` help. A mode takes keys only from an attached client, which is why
the regress test attaches a control one.

## Testing

`regress/plugin-accounts.sh`: against a fake home (`-o home=...`) with
file credentials (`-o credentials=file`), so the real login and Keychain
are untouched: two logins captured, a switch under the locks with the
shared keys kept and the outgoing rotation backed up, a stale lock
stolen and a live one refused, and over a link a remote server's slots
listed and switched. `-o keychain_service=<name>` rehearses the Keychain
path against a throwaway item. `PLUGIN_OUT` names the wasm build
directory when the tmux under test is not the tree's own.

## Not built yet

- The band header in the agents picker does not show each server's
  login; the panel does.
- A slot's usage is not pushed into the picker (an "about to hit the
  limit" badge on an agent row).
- Linux import from cswap's file store is untested.
- A profile claude's transcripts are found for the roster but the
  conversation search's disk scan still covers `~/.claude/projects` only.
