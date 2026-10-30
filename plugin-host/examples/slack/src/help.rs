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
  v           sections / unread / all      o   open in the Slack app
  R           refresh everything           n   compose to this channel
  j/k h/l     move, fold                   /   search (# channel, @ user,
  ?           every key                        ! unread|mention|dm|muted)

A row is bold while it has unread messages, red with a count when you
were mentioned, dim when muted. The right column is how long ago the
channel last spoke. The preview shows its recent messages.";

pub const OPEN: &str = "\
open: the channel view.

  j/k  move over messages      Enter / t  open the message's thread
  J/K  page                    i          compose (Enter sends)
  g/G  top / bottom            r          reply in the message's thread
  L    load older              e          react (type :name: then Enter)
  o    open in the Slack app   y          copy the permalink (tmux buffer)
  m    mark read               Space      the action menu (every key)
  R    refetch                 Escape     back: thread, channel, sidebar
  b    back to the sidebar     q          close

Enter opens the thread of any message, with or without replies, so
Enter then i is \"reply in this thread\". Escape steps back one level:
from a thread to its channel (however the thread was opened), from
the channel to the sidebar; q closes the view outright. Space lists
every action as a menu; what does not apply is dimmed.

While composing, keys type; Enter sends, Escape drops the text, a paste
goes in as is (newlines kept).";

pub fn topic(verb: Option<&str>) -> &'static str {
    match verb {
        Some("pick") | Some("sidebar") => PICK,
        Some("open") => OPEN,
        _ => GENERAL,
    }
}
