# Cross-agent messaging

An agent in one pane sends a message to an agent in another pane, on this
machine or on a linked server, with a permission gate. The message is data
in a plugin's store until the reader pulls it. It is never typed into a
pane, so it does not touch a half-typed prompt and it does not commit on
Enter. Zack's incident, where `send-keys` appended to a live draft and the
Enter submitted the mix, does not happen on this path, because there are no
keystrokes.

It is a wasm plugin. No Python, no external program. An agent talks to it
with `plugin-command`, and the plugin's own services carry a message
between servers over the tmux2 bridge. The last hop, from the mailbox
into the reader's live Claude session, is one line on the inbox socket
that Claude Code binds for every session (see "Push" below).

## The mailbox plugin

`plugin-host/examples/mailbox`. Role both, server scope. It keeps a
SQLite table of messages and registers one service, `deliver`.

Commands, through `plugin-command mailbox ...`:

```
send <box>[@server] <text...>   leave a message
inbox <box> [-a]                write this box's messages to the option
                                @mailbox_<box> as JSON and mark them read
                                (-a keeps the read ones too)
list                            unread counts per box, on the status line
```

A `<box>` is any name. The agents plugin will use the durable agent id, so
"message training-aa" addresses the agent, not a pane. `@server` is a name
from `service::servers()`; without it the local server.

The reader consumes its inbox from the shell:

```
tmux2 plugin-command mailbox 'inbox training-aa'
tmux2 show-options -s -v @mailbox_training-aa   # the JSON messages
```

The option round trip is v1. A command that writes the messages to the
caller's stdout is the ergonomic follow-up; it needs the host to let a
plugin-command return output to its client, which it cannot yet.

## Across a link

`send beta@dev-4x <text>` calls `deliver` on `mailbox@dev-4x`. The call
rides the plugin bridge that a `remote-attach` link already carries, and
the mailbox plugin on dev-4x stores the message locally. The sender is
stamped with the sending server, so the reader sees which machine it came
from. Nothing types into a pane on either side, and ssh carries only the
bridge frames, not the message as text.

The plugin must run on the receiving server. A link pushes it there as a
provider, or the server loads it from its own manifest. A server that
loads it keeps its own copy (the keep-local rule), so a workstation that is
also a remote is not disturbed.

## The permission gate

The bridge is bidirectional, but the two directions are not equal.

- **Initiator to remote: always allowed.** When you `remote-attach` to a
  server you ssh'd in, so it already trusts you at the OS level. Your
  calls into it (the roster fetch, delivering a message there) are never
  gated. This is why the picker and outbound messaging just work.
- **Remote to initiator: declared and granted.** A server you linked to
  calling back into yours is gated twice. First the callee plugin must
  have opted the method in with `[caps.services] serve_remote = [...]` in
  its sidecar; a call for any other method is denied outright with
  `E_DENIED` (no row, no menu). Second, you must have allowed the
  (server, plugin) pair. When a link comes up, the client that ran
  `remote-attach` gets a menu of the peer's plugins that declare a
  remote-callable method; until you allow one, its calls fail `E_DENIED`.

For Zack today that is one line: mailbox declares `serve_remote =
["deliver"]`, agents declares nothing, so linking a server offers only
"allow this server to deliver messages to agents here". Nothing can read
your roster or capture your panes from a linked server.

`plugin-peers list|allow|deny|revoke|menu` edits the table (stored at
`<data>/tmux/plugin-host/peers.db`, keyed (server, plugin), `*` for every
plugin). `plugin-remote-caps` (what a pushed plugin may do at all) and the
caller-side `[caps.services] call` list (what a plugin reaches out to) are
separate and unchanged.

## Driving a raw pane on purpose

An agent that deliberately wants to type into a remote pane, a shell or a
TUI, still can: `tmux2 send-keys -t <shadow-pane>` forwards over the link
today. That path is destructive by construction, so it stays an explicit
choice an agent makes, never the default for a message to an agent.

## Push: the reader wakes

Delivery is push. A message left for a Claude agent reaches its session
at once, so the agent wakes with it instead of finding it on its next
`inbox`.

Every Claude Code session binds an inbox socket and names it in its
session file (`~/.claude/sessions/<pid>.json`, `messagingSocketPath`).
A line `{"type":"user","message":{"role":"user","content":...}}` on that
socket is one queued user turn: Claude reads it between tool calls, or
starts a new turn with it when idle. It is not keystrokes, so a
half-typed prompt in that pane is untouched. Claude Code documents this
socket for scripts to post into a session; on Linux and macOS the OS user
permission on the socket is the guard and no token is needed.

The chain, all inside the tmux event loop:

1. The mailbox stores the message and emits its `mailbox` topic with the
   message in it.
2. The agents plugin (the provider half, on the reader's own server)
   follows that topic. When the box is a live Claude agent here, it reads
   the agent's session file for the socket path.
3. It calls the host builtin `claude_notify(path, text)`, which writes the
   line. The text names the sender (`<id>` or `<id>@<server>`).
4. On success it asks the mailbox to `mark_read` the message. The picker
   badge clears.

Across a link nothing is added: the message rides the bridge to the
reader's server as before, and that server's agents plugin does the push
locally. A box that is not a Claude agent here (a plain box, a codex or
pi agent, a Claude with no socket) is left unread for `inbox`.

`claude_notify` needs the `claude-notify` capability. The agents sidecar
requests it; it is never in the default grants for a plugin a remote
pushed here, so a linked server cannot speak into your sessions through
its own copy.

### One setting on the reader's machine

Claude Code holds a peer message for approval when the receiving session
bypasses permission prompts and the sender does not identify as bypassing
too. A push from tmux2 identifies as nothing, so in a bypass session it
would pop an approval dialog instead of waking the agent. Set this in
`~/.claude/settings.json` on each machine whose agents should wake:

```json
{ "crossSessionInbound": "accept" }
```

## The agents picker

The agents plugin addresses a message by the durable agent id, not a box
name, and knows which server hosts each id from its roster. In the picker,
`m` on a row opens a compose line; Enter sends. `plugin-command agents
message <id> <text>` does the same without the picker, so a script or
another agent can send too. A local agent's mailbox is on this server; a
remote agent's is on its own, reached over the bridge. When the id is in
no roster yet (a message right after linking) the view fetches the rosters
once, then routes. Each server's mailbox is asked for its unread counts,
and a row with unread shows an envelope badge with the number.

## Status

Built and tested. `regress/plugin-mailbox.sh` runs the mailbox alone
across two (then three) servers, including a plugin loaded after a link is
already up. `regress/plugin-agents-message.sh` runs the whole feature: a
message to a local agent id stored here, and a message to a remote agent
id routed over the bridge into that agent's server, sender qualified. The
release bundles `mailbox.wasm`.

`regress/plugin-mailbox-push.sh` runs the push: a stub Claude listens on
an inbox socket, a local message lands on it as one user turn and is
marked read, a plain box is left alone, and a message from the remote
crosses the link and lands on the same socket once allowed.

Not built yet: the stdout-returning read command.
