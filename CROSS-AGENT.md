# Cross-agent messaging

An agent in one pane sends a message to an agent in another pane, on this
machine or on a linked server, with a permission gate. The message reaches
the target as a queued user message, not as keystrokes. It does not touch
a half-typed prompt and it does not commit on Enter. Zack's incident, where
`send-keys` appended to a live draft and the Enter submitted the mix, does
not happen on this path, because there are no keystrokes.

## The delivery primitive

Every Claude Code session listens on its own unix socket. Claude Code uses
it for peer messaging between sessions. The wire protocol is two lines of
JSON:

```
{ echo '{"type":"auth","token":"<TOKEN>"}';
  echo '{"type":"user","message":{"role":"user","content":"hello"}}'; } \
  | socat - UNIX-CONNECT:<SOCKET>
```

The session replies `{"delivered":true}` on the right token and
`{"error":...}` on a wrong one. The message becomes a user turn in that
session. Nothing is typed into the terminal.

- `SOCKET` is `$XDG_RUNTIME_DIR/cc-socks/<claude-pid>.sock`.
- `TOKEN` is `$CLAUDE_CODE_MESSAGING_TOKEN`. Claude sets the socket and the
  token in the environment of the processes it spawns (the Bash tool, the
  hooks), not in its own environment. So a hook inside the session can read
  the token; the pane's foreground process cannot. This decides the design:
  the target session registers its own mailbox, rather than another process
  reading it from the outside.

## The mailbox helper

`tools/cc-mailbox` carries the primitive. It has no dependency beyond
python3.

- `cc-mailbox register` runs inside a session, from a `SessionStart` hook.
  It reads `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_SOCKET` and
  `CLAUDE_CODE_MESSAGING_TOKEN` from its environment and records them under
  `$XDG_RUNTIME_DIR/cc-mailboxes/<session>.json`, mode 0600 in a 0700
  directory, because the token grants delivery into that session.
- `cc-mailbox send <session> -m <text>` delivers to a registered session.
  `--socket`/`--token` deliver without the registry.
- `cc-mailbox list` shows the mailboxes on this machine.

The token never leaves the machine that owns the session. A cross-machine
message runs `send` on the target machine.

## How the agents plugin uses it

The agents plugin already keeps a roster of every agent on every linked
server, each with a durable, server-independent id, and it already runs an
identify shim in each pane. Two additions make it a messaging surface:

1. The identify shim also runs `cc-mailbox register`, so the roster row for
   a Claude agent has a mailbox on that agent's own machine.
2. A `message` service beside `list`, `capture`, `search` and `act`. A
   sender calls `message` with a target agent id and the text. The call
   rides the plugin bridge to the target agent's server. The provider there
   runs `cc-mailbox send` for the target session. Local delivery, no
   keystrokes.

Addressing is by the durable agent id, not a pane id. "Message
training-aa" means the agent, whichever pane and server it now sits in, and
survives a restart. A delivered message also marks the row unread in the
picker, where the roster already tracks unread state; an agent with no
mailbox (no shim, or not a Claude) shows the message unread and undelivered
instead, which is a safe failure.

## The permission gate

Three layers, all where the receiving side controls them:

- `plugin-remote-caps` on the receiving server decides what a pushed plugin
  from a linked server may do at all.
- The sidecar `[caps.services] call` allowlist decides which services a
  plugin may call; empty means unrestricted.
- A receiving-server option (planned) names which servers may reach its
  agents, checked against `from_server` on the request before delivery.
  Default is the local server only. `set -sa agents-accept-from 'training-*'`
  opens one remote.

## Driving a raw pane on purpose

An agent that deliberately wants to type into a remote pane, a shell or a
TUI, still can: `tmux2 send-keys -t <shadow-pane>` forwards over the link
today. That path is destructive by construction, so it stays an explicit
choice an agent makes, never the default for a message to an agent.

## Status

Built and tested: the `cc-mailbox` helper, end to end against a
protocol-faithful stub (register, send by id, send via stdin, bad-token
refusal). Not built yet: the identify-shim registration, the `message`
service, the picker unread and send key, and the `agents-accept-from`
option.
