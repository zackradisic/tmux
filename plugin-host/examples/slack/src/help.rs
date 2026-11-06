//! What `plugin-command slack help` prints.

pub const GENERAL: &str = "\
usage: tmux2 plugin-command slack '<verb> [args...]'

  pick                      the sidebar: sections, channels, DMs, badges
  open <#chan|@user|Cid> [thread_ts]
                            the channel view (a thread with its root ts)
  send <#chan|@user> <text...>
                            post a message as you (reply: the ts)
  reply <#chan> <thread_ts> <text...>
                            post into a thread
  mark <#chan>              mark the channel read
  unread                    list channels and threads with unread messages
  status                    who you are, badges, last sync
  refresh                   fetch channels, sections, users and badges now
  auth                      forget the stored session and derive it again
  bind <#chan>[/thread_ts] <agent-id>
                            push new messages there into an agent's session
                            (others' messages, once each; yours too only
                            with @slack-bridge-me 1)
  unbind <#chan>[/thread_ts]
  notify <title> | <body>   a line in the notification feed (hooks, tests)
  close                     close any open view
  help [verb]               this text

Channels are named `#name`, DMs `@user` (a display name or handle), or
by Slack id (C…, D…, G…). The plugin acts as the Slack desktop app's
signed-in user: sign in there once, nothing else to configure.
Status line: #{@slack_unread} channels with unread, #{@slack_mentions}
mentions (threads counted), #{@slack_dms} unread DMs, #{@slack_threads}
threads with unread replies.";

pub const PICK: &str = "\
pick: the sidebar, as a floating picker.

  Enter       open the channel view        m   mark read (a thread too)
  Space       the action menu              o   open in the Slack app
  v           sections / unread / all      i   compose to this channel
  j/k h       move, up a level (z: fold)   n   new agent from the preview
  l / Right   read the preview             @   go to where you were mentioned
  ?           every key (R: refresh)       /   search (# channel, @ user,
  C-o / Tab   back / forward through the        ! unread|mention|dm|thread
              conversations you opened          |pinned|recent)
  N           new agent from the preview    w   quiet this channel's toasts
  p           pin the row on top (p again:      (w again: every message)
              unpin; a thread too)

Pinned sits on top: the rows p pinned, channels and threads alike,
copies of their rows elsewhere, in the order pinned (p on a Pinned row
unpins it). Recent lists the conversations opened last, newest first,
the history C-o and Tab walk. An Unread section follows: every channel and DM
with something new, DMs first, then channels and threads in the order
of the badges; the channel rows stay in their own sections too. A
thread you follow that has replies you have not read (what Slack's
Threads view lists) is a row of its own there: `↳`, its channel, the
root's first words and `+N` new replies. Each row starts with its
badge: a red `!` block when you were mentioned (the name red, the
count after it as @3), a cyan dot while it has unread messages (the
name bold), a dim dot when read or muted. The right column is how
long ago the channel (or the thread) last spoke.

@ on a row with a mention hands the preview the keyboard with the
cursor on the message that mentions you (the first unread one), and
marks it: a message that addresses you carries a red `@` in its gutter
wherever it is drawn; @ again goes to the next. m on a thread row
marks the thread read on Slack (the channel is left alone) and the
row goes; Enter opens the thread in the channel view, i replies in it. The preview is
the channel view itself, laid out at the column's width: the channel's
recent messages, newest at the bottom, attachments under their text;
[ and ] scroll it. Space opens a menu of every action for the row,
dimmed where one does not apply; each item is the key itself.

Toasts (the notifications pane): every message of every channel you
are in, unless it is yours, the channel is muted, or it is on screen.
w on a channel row quiets it (`quiet` before its age: only DMs and
mentions there toast) until w again. `set -g @slack-notify slack` is
Slack's own rule instead (DMs, mentions, its per-channel preference);
w then watches a channel (`all`). C-o opens the conversation you opened before this one, Tab
(C-i) the one after, through a history that survives closing the
float (the sidebar opens on its newest entry, a thread in the
preview). N hands everything the preview shows to a new agent, for a
summary of the channel; n the message under its cursor or a selection.

l, Right or a click in the column hand the preview the keyboard (the
separator lights up): the channel view's keys then work there - j/k
move over the messages, J/K page, g/G, y copies the message under the
cursor (v then j/k a selection), Y its permalink, e reacts, m marks
the channel read, Enter opens the message's thread right there in the
preview (Escape brings the channel back), i and r open the full view
composing (r in the message's thread), Space its menu, ? its hints.
Escape, h or Left give the keyboard back to the list; q closes the
sidebar either way. + and - resize the float, in the channel view too;
a size set any way (keys, a drag, resize-pane) is the one it opens
with next time.";

pub const OPEN: &str = "\
open: the channel view.

  j/k  move over messages      Enter / t  open the message's thread
  J/K  page                    i          compose (Enter sends, box stays)
  g/G  top / bottom            r          reply in the message's thread
  L    load older              e          react (type :name: then Enter)
  z    the message's picture, big (a click on a picture does the same)
  o    open in the Slack app   y          copy the message (or selection)
  m    mark read               Y          copy the permalink
  p    pin / unpin this conversation (the thread shown, else the channel)
  @    next mention of you     E          edit your message (Enter saves)
  R    refetch                 v          select from here (j/k extend)
  b    back to the sidebar     n          new agent from the message(s)
  q    close                   N          new agent from everything shown
  C-o / Tab  the conversation  Space      the action menu (every key)
       opened before / after   Escape     back: thread, channel, sidebar

A message that mentions you (@you, @here, @channel) carries a red `@`
in its gutter; @ moves the cursor to the next one, round again at the
end, and the foot says which of how many. In a thread, m (and reading
it to the end) marks the thread read, not its channel.

Enter opens the thread of any message, with or without replies, so
Enter then i is \"reply in this thread\". Escape steps back one level:
from a thread to its channel (however the thread was opened), from
the channel to the sidebar; q closes the view outright. Space lists
every action as a menu; what does not apply is dimmed.

y copies the message under the cursor to the clipboard (a tmux paste
buffer, and the terminal's clipboard over OSC 52) as
`Name (2026-10-30 14:02): text`, continuation lines indented. v opens
a selection at the cursor and j/k extend it; y then copies every
message in it, one after the other, ready to hand to an agent. Y
copies the message's Slack permalink instead.

n hands the same messages to a new agent: they are written to a file
(prompts/<channel>-<ts>.md under the plugin's data directory, under a
`From Slack #channel on <date>:` line) and the new-agent form opens
over the view - an instruction to put above them (ask), the folder,
session (`new` for one of its own), window name, command and project,
and where the prompt comes from (that file, or the tmux paste buffer:
Tab toggles). C-e opens the file in $EDITOR in a popup; Enter starts
the agent through the agents plugin, in the background, and says where
it landed.

The compose box (i; r for the message's thread) opens in insert mode:
keys type, Enter sends and the box stays open, empty, for the next
message; C-j puts a newline inside a message (tmux sees Shift-Enter as
Enter). @ opens a dropdown of people as the name is typed (@te: every
handle or name with te in it, the latest speakers first; @here, @channel,
@everyone too): Tab/Down and BTab/Up walk it, Enter takes the pick into
the draft (@tejas), Escape puts it away. A @handle is sent as Slack's
mention, so the person is pinged; an unknown @word goes as typed. Escape switches the box to normal mode, where vim's keys edit
the draft - h j k l, w b e, 0 ^ $, x dd dw cw D C, yy p P, o O a A I,
r J u, v / V / C-v selections with y d c (C-v then I or A inserts on
every line), / search with n and N - and Enter still sends; Escape in
normal mode closes the box. The box grows with the draft, up to six
rows, and its right edge says -- INSERT -- or -- NORMAL --. A paste
goes in as is, newlines kept. e (a reaction) is a one-shot box: Enter
reacts and closes it.

A shared picture (PNG, JPEG, a GIF's first frame), or a /giphy post's,
is drawn under its clip or caption line in Ghostty or kitty;
`set -g @slack-images off` leaves the line alone (other terminals show
a blank block).";

pub fn topic(verb: Option<&str>) -> &'static str {
    match verb {
        Some("pick") | Some("sidebar") => PICK,
        Some("open") => OPEN,
        _ => GENERAL,
    }
}
