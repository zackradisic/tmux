# Cross-agent messaging

An agent in one pane sends a message to an agent in another pane, on this
machine or on a linked server, with a permission gate. The message is data
in a plugin's store until the reader pulls it. It is never typed into a
pane, so it does not touch a half-typed prompt and it does not commit on
Enter. Zack's incident, where `send-keys` appended to a live draft and the
Enter submitted the mix, does not happen on this path, because there are no
keystrokes.

It is a wasm plugin. No Python, no external program, no Claude socket. An
agent talks to it with `plugin-command`, and the plugin's own services
carry a message between servers over the tmux2 bridge.

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

The bridge is bidirectional: once a link is up, either side can call the
other's services. A grant table the host owns decides who may. It is keyed
by (server, plugin); `plugin = "*"` means every plugin; the local server is
always allowed.

- A plugin a peer **pushed** to a server is auto-allowed for that peer to
  call: it provided the code and means to use it. This is why the roster
  and mailbox flows work with no manual step, since a workstation pushes
  its plugins to the servers it links.
- A plugin a server **loaded itself** is gated. When a link comes up, the
  initiator's client gets a menu naming the peer's plugins it also serves;
  an inbound peer's pairs default to deny. Until a pair is `allow`, a call
  for it fails with `E_DENIED` and a message naming the fix.
- `plugin-peers list|allow|deny|revoke|menu` edits the table; the menu
  reopens with `plugin-peers menu <server>`.

Two older layers still stand and are different things: `plugin-remote-caps`
caps what a pushed plugin may do at all, and the sidecar `[caps.services]
call` list is a plugin author narrowing what their own plugin reaches out
to.

## Driving a raw pane on purpose

An agent that deliberately wants to type into a remote pane, a shell or a
TUI, still can: `tmux2 send-keys -t <shadow-pane>` forwards over the link
today. That path is destructive by construction, so it stays an explicit
choice an agent makes, never the default for a message to an agent.

## An RPC step later

Delivery is pull today: the reader runs `inbox` when it wants its
messages. A push, where the plugin wakes the reader on a new message, is
the natural next step. Inside one Claude Code session that is its own peer
messaging; between agents on different servers it would be a topic the
reader's harness follows. The store and the `deliver` service do not
change for it.

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

Not built yet: the stdout-returning read command, and the RPC push that
wakes a reader instead of it pulling.
