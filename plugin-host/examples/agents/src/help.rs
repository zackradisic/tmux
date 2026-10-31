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

Agent ids are durable: claude:<uuid>, codex:<id>, opencode:<id>. The i card
in the picker shows one; the store has them all:
  sqlite3 ~/.local/share/tmux/plugins/agents/store.db \\
    'select id, coalesce(user_name,name), cwd from agents where ended_ms is null'
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
        Some("message") => MESSAGE,
        Some("project") => PROJECT,
        Some("pick") | Some("open") => PICK,
        Some(v) if crate::STATUSES.contains(&v) => STATUS,
        _ => GENERAL,
    }
}
