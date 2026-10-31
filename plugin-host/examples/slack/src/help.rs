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
  unread                    list channels with unread messages (one per line)
  status                    who you are, badges, last sync
  refresh                   fetch channels, sections, users and badges now
  auth                      forget the stored session and derive it again
  bind <#chan>[/thread_ts] <agent-id>
                            push new messages there into an agent's session
  unbind <#chan>[/thread_ts]
  notify <title> | <body>   a line in the notification feed (hooks, tests)
  close                     close any open view
  help [verb]               this text

Channels are named `#name`, DMs `@user` (a display name or handle), or
by Slack id (C…, D…, G…). The plugin acts as the Slack desktop app's
signed-in user: sign in there once, nothing else to configure.
Status line: #{@slack_unread} channels with unread, #{@slack_mentions}
mentions, #{@slack_dms} unread DMs.";

pub const PICK: &str = "\
pick: the sidebar, as a floating picker.

  Enter       open the channel view        m   mark read
  Space       the action menu              o   open in the Slack app
  v           sections / unread / all      n   compose to this channel
  j/k h/l     move, fold (z: a section)    /   search (# channel, @ user,
  ?           every key (R: refresh)           ! unread|mention|dm|muted)

Each row starts with its badge: a red `!` block when you were mentioned
(the name red, the count after it as @3), a cyan dot while it has
unread messages (the name bold), a dim dot when read or muted. The
right column is how long ago the channel last spoke. The preview shows
the channel's recent messages, newest at the bottom, attachments under
their text; [ and ] scroll it. Space opens a menu of every action for
the row, dimmed where one does not apply; each item is the key itself.";

pub const OPEN: &str = "\
open: the channel view.

  j/k  move over messages      Enter / t  open the message's thread
  J/K  page                    i          compose (Enter sends)
  g/G  top / bottom            r          reply in the message's thread
  L    load older              e          react (type :name: then Enter)
  o    open in the Slack app   y          copy the message (or selection)
  m    mark read               Y          copy the permalink
  R    refetch                 v          select from here (j/k extend)
  b    back to the sidebar     Space      the action menu (every key)
  q    close                   Escape     back: thread, channel, sidebar

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

While composing, keys type; Enter sends, Escape drops the text, a paste
goes in as is (newlines kept).

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
