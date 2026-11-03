//! What `plugin-command agents help` and `<verb> --help` print on the
//! caller's terminal (through the SDK's `reply`). The skill an agent reads
//! points here instead of repeating the flags, so this is the text that
//! has to match the code.

pub const GENERAL: &str = "\
agents - the roster of the agent sessions running in your panes
usage: tmux2 plugin-command [-t <pane>] agents '<verb> [args...]'

  pick                        open the picker (the prefix-a key)
  pick here                   open on this pane's agent, wherever its row is
  pick id <agent-id>          open on that agent (any server, history, archive)
  pick ids                    open on the agent ids on this pane's screen
  open <agent-id>             the picker on that agent, on the user's screen
  ls [--project <p>] [--all]  the live agents, every server, one per line
  show <agent-id>             one agent's card: where, project, cwd, status…
  transcript <agent-id> [-n N]
                              its conversation's last N turns (default 30)
  search <words...>           the agents whose conversations match
  whoami                      this pane's agent: its id and address
  new [flags]                 start an agent: a form, or at once with flags
  message <agent-id> <text>   leave a message in that agent's mailbox
  project [-w|-s] <name>      tag this pane's agent / its window / its session
  working | needs_input | waiting | done [task]
                              report this pane's agent's state (a shim's wire)
  identify <session_id> [session_file]
                              bind this pane's row to its durable id (a hook)
  help [verb], <verb> --help  this text, or a verb's own

The whole verb is ONE shell argument: quote it, and quote inside it for a
value with spaces (agents \"new --project 'Ooga booga'\").

Pass -t \"$TMUX_PANE\" from an agent's own pane: it is the sender of a
message, what a new agent inherits from (folder, session, project), the
agent a project or state applies to.

Agent ids are durable: claude:<uuid>, codex:<id>, opencode:<id>. `ls`
prints them, `whoami` yours, the i card in the picker shows one.
";

pub const LS: &str = "\
agents ls - the agents, one per line

  plugin-command agents ls                         the live agents, every server
  plugin-command agents 'ls --project <name>'      those of one project
  plugin-command agents 'ls --all'                 ended and archived ones too
  plugin-command agents 'ls --json'                one JSON object per line

Columns: id, state (working, needs_input, waiting, done; ended for a
finished one), project (as the picker groups: the tag, else the @project
option, else the session, else the directory), name, directory, and the
server for an agent on a linked host (its id takes @<server> in a message
address). Sorted by project, then by state.

  plugin-command agents 'show <agent-id>'          one agent, every field
  plugin-command -t \"$TMUX_PANE\" agents whoami    the agent in your pane

show prints what the picker's i card shows: name, harness and model,
where it runs (session:window, pane, server), project and where that
came from, directory and branch, when it started and was last active,
its state and task, how many prompts, replies and tool calls it has
made and the files it touched most, its transcript file, and the line
that resumes it. whoami prints your id, name, project, and your address
for a reply from another server (<id>@<this server's peer name>).

A Claude agent's inbox line is the socket a message is pushed through.
GONE means the socket was removed under the running session (macOS
prunes /tmp entries older than three days when the disk is full): the
agent still works, but a message waits in its box until its next
`mailbox inbox`; `claude --resume <uuid>` gives it a new socket. ls marks
such a row \"⚠ inbox gone\" (--json: \"inbox\": \"gone\"). Launching
claude with CLAUDE_CODE_TMPDIR set to a directory of your own keeps the
socket out of /tmp.
";

pub const TRANSCRIPT: &str = "\
agents transcript - read another agent's conversation

  plugin-command agents 'transcript <agent-id>'          its last 30 turns
  plugin-command agents 'transcript <agent-id> -n 200'   its last 200
  plugin-command agents 'transcript <agent-id> --all'    all of it
  plugin-command agents 'search <words...>'              who talked about this

The roster reads each agent's transcript as its turns end (Claude's and
Codex's session files, opencode's database) and keeps the prompts, the
replies and one line per tool call (`Edit run.py +12 −3`); tool results
are not kept. transcript prints those, oldest first: `## user`, `## agent`
and `· tool` blocks, each with its age. The turns are the ones stored at
the agent's last turn end; the call also reads the file again, so a
second call a moment later is fresher. A remote agent's turns live on
its host: run the same verb there.

search matches whole words against every stored conversation (the same
index the picker's search box uses), best first: the id, the name and
the line that matched, `-n` for more than 10.
";

pub const NEW: &str = "\
agents new - start an agent

  plugin-command -t \"$TMUX_PANE\" agents 'new --resume <id> --project <tag>'
  plugin-command -t \"$TMUX_PANE\" agents 'new --command claude --name reviewer'

With no flag below, the new-agent form opens for the user. With any of
them the agent starts at once, in the background, and the result is left
in the caller pane's @agents_new option (a plugin's work is asynchronous):
  pending, then  %pane @window session,  or  error: <why>
  tmux2 show-options -pqv -t \"$TMUX_PANE\" @agents_new

  --resume <id>      resume that harness session: claude:<uuid>, codex:<id>,
                     opencode:<id>, or the bare id; a roster row behind it
                     lends its folder, name and tag as defaults
  --fork             as a copy of it (claude --fork-session; opencode forks
                     in its service and the window opens the copy)
  --command <line>   what to run: a launcher's name (see `launch` in the
                     plugin's config) or a whole line
                     default: the harness, through its launcher
  --folder <dir>     where it runs      default: the row's folder, else the
                     caller pane's
  --session <name>   which session      default: the caller pane's
  --new-session      a session of its own, named after --name
  --name <name>      the window's name  default: the row's name, else the
                     folder's
  --project <tag>    its @project       default: the caller pane's, else the
                     row's tag
  --split            a pane beside the caller instead of a window
  --window <w>       with --split: in that window of the session (index,
                     name or @id)
  --focus            switch the user to it (default: made in the background)
  --prompt <text>    its first prompt (one word: quote it); claude and codex
                     take it as the argument, opencode as --prompt
  --prompt-file <path>
                     its first prompt read from that file, as
                     \"$(cat '<path>')\" on the command line - the shape for
                     a long prompt, or one with quotes or newlines in it
  --profile <dir>    CLAUDE_CONFIG_DIR for it (a per-window login)
  --client <name>    form only: the client to open it on
  @<host>            on that linked server (the session must be mirrored
                     here as <host>/<name>)

A resumed agent keeps its durable id: the roster re-points the old row at
the new pane, and a message to that id reaches it.
";

pub const MESSAGE: &str = "\
agents message - leave a message in another agent's mailbox

  plugin-command -t \"$TMUX_PANE\" agents 'message <agent-id> <text...>'

The text is stored, then pushed into that agent's session as one queued
user turn, prefixed \"Message from <sender> via the tmux2 mailbox\", with
the reply command in the prefix. It is never typed into the pane. The
sender is the agent in the -t pane (so the reply comes back to you); with
no -t it is the pane the user looks at.

A remote agent's id carries its server: claude:<uuid>@<host>. The message
rides the remote-attach link; the reader's server does the push. If the
receiver's server does not grant claude-notify to its agents plugin the
message is stored unread and nothing visible happens.

The result goes to the status line and the server log:
  tmux2 show-messages | grep 'plugin agents:' | tail -1

One case is answered on your terminal at once: a local Claude agent whose
inbox socket is gone (`agents show <id>` says GONE on its inbox line).
The message is stored, but it cannot be pushed; the agent reads it on its
next `mailbox inbox`, or is restarted with `claude --resume <uuid>`.
";

pub const PROJECT: &str = "\
agents project - the project a pane's agent is grouped under

  plugin-command -t \"$TMUX_PANE\" agents 'project <name>'      tag the agent
  plugin-command -t \"$TMUX_PANE\" agents 'project -w <name>'   the window's @project
  plugin-command -t \"$TMUX_PANE\" agents 'project -s <name>'   the session's @project
  plugin-command -t \"$TMUX_PANE\" agents 'project -'           clear the tag

A row's project is the first of: the tag on the agent, the @project option
its pane resolves to (pane, window, session), the session's name unless it
is a number, the last part of its directory. The picker groups by it.
";

pub const PICK: &str = "\
agents pick - the picker

  plugin-command agents pick                 open it (bind to a key)
  plugin-command -t <pane> agents 'pick here'   open on that pane's agent,
                                             in the history or archive if that
                                             is where its row went
  plugin-command agents 'pick id <agent-id>' open on that agent anywhere
  plugin-command -t <pane> agents 'pick ids' open on the ids on that screen
  plugin-command agents 'open <agent-id>'    the same, but on the screen the
                                             user looks at when this server is
                                             linked into from a workstation

Inside: ? shows the key reference in the preview column.
";

pub const STATUS: &str = "\
agents <state> - report a pane's agent's state

  plugin-command -t \"$TMUX_PANE\" agents 'working [task...]'
  plugin-command -t \"$TMUX_PANE\" agents 'needs_input [why...]'
  plugin-command -t \"$TMUX_PANE\" agents 'waiting [on what...]'
  plugin-command -t \"$TMUX_PANE\" agents 'done [summary...]'

The roster reads Claude's, Codex's and opencode's own session files, so
none of this is needed for them; it is the wire for a harness that has no
session file the roster can read (a pi hook), and a way to correct a guess.
";

/// The text for `help [topic]` and `<verb> --help`.
pub fn topic(verb: Option<&str>) -> &'static str {
    match verb {
        Some("new") => NEW,
        Some("ls") | Some("list") | Some("show") | Some("whoami") => LS,
        Some("transcript") | Some("search") => TRANSCRIPT,
        Some("message") => MESSAGE,
        Some("project") => PROJECT,
        Some("pick") | Some("open") => PICK,
        Some(v) if crate::STATUSES.contains(&v) => STATUS,
        _ => GENERAL,
    }
}
