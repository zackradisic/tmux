//! The live feed: Slack's RTM websocket, which the desktop client itself
//! uses. `rtm.connect` hands a `wss://` URL for the session; the host
//! holds the socket (capability `net`) and every frame arrives as a
//! `ws-message` event. The feed carries what the poll cannot give at
//! once: a message the moment it is posted, a channel read on another
//! device, the notification Slack decided the user should see, a
//! reaction. The badge poll stays as the safety net.
//!
//! Reconnection: a close (Slack's `goodbye`, a network drop) schedules a
//! fresh `rtm.connect` after a backoff that doubles to a minute. A ping
//! every 30 s keeps the socket alive through idle periods.
//!
//! The agent bridge lives here too: a message in a bound channel or
//! thread is handed to the mailbox plugin for the bound agent, which
//! pushes it into that Claude session as a user turn. The text names the
//! command that answers in the same thread.

use serde_json::Value;
use tmux_plugin_sdk::prelude::*;

use crate::store::{self, Kind};
use crate::{api, chat, mrkdwn, sidebar, Shared};

const PING_MS: u64 = 30_000;
const FIRST_BACKOFF_MS: u64 = 1_000;
const MAX_BACKOFF_MS: u64 = 60_000;
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Open the socket, unless one is open or being opened.
pub async fn connect(st: Shared) {
    let id = {
        let mut s = st.borrow_mut();
        if s.feed.is_some() || s.feed_connecting {
            return;
        }
        let Some(id) = s.ident.clone() else { return };
        s.feed_connecting = true;
        id
    };
    let result = async {
        let url = api::rtm_connect(&id).await?;
        ws_open(&url, &[("Cookie", &format!("d={}", id.cookie)), ("User-Agent", USER_AGENT)])
            .await
            .map_err(|e| format!("rtm socket: {}", e.message))
    }
    .await;
    let wait = {
        let mut s = st.borrow_mut();
        s.feed_connecting = false;
        match result {
            Ok(ws) => {
                s.feed = Some(ws);
                s.feed_since_ms = now_ms();
                s.feed_backoff_ms = FIRST_BACKOFF_MS;
                s.feed_error = None;
                log("slack: feed connected");
                None
            }
            Err(e) => {
                log(&format!("slack: feed: {e}"));
                let wait = s.feed_backoff_ms.max(FIRST_BACKOFF_MS);
                s.feed_backoff_ms = (wait * 2).min(MAX_BACKOFF_MS);
                s.feed_error = Some(e);
                Some(wait)
            }
        }
    };
    match wait {
        None => {
            let ws = st.borrow().feed.unwrap();
            spawn(keepalive(st, ws));
        }
        Some(ms) => spawn_reconnect(st, ms),
    }
}

fn spawn_reconnect(st: Shared, after_ms: u64) {
    spawn(async move {
        if sleep_ms(after_ms).await.is_err() {
            return;
        }
        connect(st).await;
    });
}

async fn keepalive(st: Shared, ws: WsId) {
    let mut n: u64 = 0;
    loop {
        if sleep_ms(PING_MS).await.is_err() {
            return;
        }
        if st.borrow().feed != Some(ws) {
            return;
        }
        n += 1;
        if ws_send_text(ws, &format!(r#"{{"type":"ping","id":{n}}}"#)).is_err() {
            return;
        }
    }
}

pub fn close(st: &Shared) {
    if let Some(ws) = st.borrow_mut().feed.take() {
        let _ = ws_close(ws);
    }
}

/// The socket went away: forget it, come back after the backoff.
pub fn on_closed(st: &Shared, c: &WsClosed) {
    let wait = {
        let mut s = st.borrow_mut();
        if s.feed != Some(c.socket) {
            return;
        }
        s.feed = None;
        let wait = s.feed_backoff_ms.max(FIRST_BACKOFF_MS);
        s.feed_backoff_ms = (wait * 2).min(MAX_BACKOFF_MS);
        s.feed_error = Some(if c.reason.is_empty() { format!("closed ({})", c.code) } else { c.reason.clone() });
        wait
    };
    log(&format!("slack: feed closed ({} {}), reconnecting in {}s", c.code, c.reason, wait / 1000));
    spawn_reconnect(st.clone(), wait);
}

pub fn on_message(st: &Shared, m: &WsMessage) {
    if st.borrow().feed != Some(m.socket) {
        return;
    }
    let Ok(v) = m.json::<Value>() else { return };
    let st2 = st.clone();
    spawn(async move {
        handle(&st2, v).await;
    });
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

async fn handle(st: &Shared, v: Value) {
    let kind = s(&v, "type").unwrap_or_default();
    match kind.as_str() {
        "message" => on_chat_message(st, &v).await,
        "channel_marked" | "im_marked" | "group_marked" | "mpim_marked" => {
            let (Some(channel), Some(ts)) = (s(&v, "channel"), s(&v, "ts")) else { return };
            let mentions = v.get("mention_count_display").and_then(Value::as_i64).unwrap_or(0);
            let unread = v.get("unread_count_display").and_then(Value::as_i64).unwrap_or(0);
            store::mark_read(&channel, &ts).await;
            {
                let mut s = st.borrow_mut();
                if let Some(c) = s.channels.iter_mut().find(|c| c.id == channel) {
                    c.last_read = Some(ts);
                    c.has_unreads = unread > 0;
                    c.mentions = mentions;
                }
                crate::publish(&s);
            }
            sidebar::refresh(st);
        }
        "desktop_notification" => {
            // What Slack itself would have shown, already filtered by the
            // user's notification preferences: title is the sender (and
            // the channel), content the text.
            let title = s(&v, "title").unwrap_or_default();
            let content = s(&v, "content").unwrap_or_default();
            let subtitle = s(&v, "subtitle").unwrap_or_default();
            let title = if subtitle.is_empty() || title.contains(&subtitle) { title } else { format!("{title} ({subtitle})") };
            let channel = s(&v, "channel").unwrap_or_default();
            let thread = s(&v, "thread_ts").filter(|t| !t.is_empty());
            let action = if channel.is_empty() {
                None
            } else {
                Some(match &thread {
                    Some(t) => format!("plugin-command slack 'open {channel} {t}'"),
                    None => format!("plugin-command slack 'open {channel}'"),
                })
            };
            let key = format!("slack:{channel}{}", thread.map(|t| format!("/{t}")).unwrap_or_default());
            crate::notify(&title, &content, Some(&key), action.as_deref()).await;
        }
        "reaction_added" | "reaction_removed" => {
            let Some(item) = v.get("item") else { return };
            let (Some(channel), Some(ts), Some(name)) = (s(item, "channel"), s(item, "ts"), s(&v, "reaction")) else { return };
            // Keep the cached copy current; the open view redraws from it.
            let mut msgs = store::thread(&channel, &ts).await;
            if let Some(m) = msgs.iter_mut().find(|m| m.ts == ts) {
                let add = kind == "reaction_added";
                match m.reactions.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, count)) => *count = if add { *count + 1 } else { (*count - 1).max(0) },
                    None if add => m.reactions.push((name, 1)),
                    None => {}
                }
                m.reactions.retain(|(_, c)| *c > 0);
                let _ = store::upsert_messages(std::slice::from_ref(m)).await;
                chat::on_changed(st, m.clone());
            }
        }
        "goodbye" => {
            // Slack is about to close the socket: be first.
            if let Some(ws) = st.borrow_mut().feed.take() {
                let _ = ws_close(ws);
            }
            spawn_reconnect(st.clone(), FIRST_BACKOFF_MS);
        }
        _ => {}
    }
}

/// A `message` event: a new message, an edit, a deletion, or a thread
/// root's reply count changing.
async fn on_chat_message(st: &Shared, v: &Value) {
    let Some(channel) = s(v, "channel") else { return };
    let subtype = s(v, "subtype").unwrap_or_default();
    match subtype.as_str() {
        "message_changed" | "message_replied" => {
            let Some(inner) = v.get("message") else { return };
            let Some(m) = api::message_from(&channel, inner) else { return };
            let _ = store::upsert_messages(std::slice::from_ref(&m)).await;
            crate::resolve_message_users(st, std::slice::from_ref(&m));
            chat::on_changed(st, m);
        }
        "message_deleted" => {
            let Some(ts) = s(v, "deleted_ts") else { return };
            store::delete_message(&channel, &ts).await;
            chat::on_deleted(st, &channel, &ts);
        }
        _ => {
            let Some(m) = api::message_from(&channel, v) else { return };
            let (me, label, who, is_dm, muted) = {
                let s = st.borrow();
                let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
                let ch = s.channel(&channel).cloned();
                let label = ch.as_ref().map(|c| s.channel_label(c)).unwrap_or_else(|| channel.clone());
                let who = m.user.as_deref().map(|u| s.user_label(u)).or_else(|| m.username.clone()).unwrap_or_else(|| "someone".into());
                (me, label, who, ch.as_ref().is_some_and(|c| c.kind == Kind::Im), ch.as_ref().is_some_and(|c| c.is_muted))
            };
            let ours = m.user.as_deref() == Some(me.as_str());
            let mention = !ours && (m.text.contains(&format!("<@{me}>")) || m.text.contains("<!here") || m.text.contains("<!channel") || m.text.contains("<!everyone"));
            crate::absorb(st, &channel, std::slice::from_ref(&m)).await;
            // A reply does not change the channel's unread state; a channel
            // message does (unless it is ours).
            if !m.is_reply() {
                store::note_message(&channel, &m.ts, ours, mention).await;
                let mut s = st.borrow_mut();
                if let Some(c) = s.channels.iter_mut().find(|c| c.id == channel) {
                    c.latest = Some(m.ts.clone());
                    if ours {
                        c.last_read = Some(m.ts.clone());
                        c.has_unreads = false;
                        c.mentions = 0;
                    } else {
                        c.has_unreads = true;
                        if mention {
                            c.mentions += 1;
                        }
                    }
                }
                crate::publish(&s);
            }
            sidebar::on_arrived(st, &m);
            sidebar::refresh(st);
            chat::on_arrived(st, m.clone());
            let _ = (is_dm, muted, who, label);
            bridge(st, &m).await;
        }
    }
}

/// Hand a message in a bound conversation to the bound agent's mailbox.
async fn bridge(st: &Shared, m: &store::Message) {
    let (targets, text) = {
        let s = st.borrow();
        // The user's own words count (a thread answered from a phone is
        // the point); what this plugin posted for the agent does not.
        if s.own_posts.contains(&m.ts) {
            return;
        }
        let root = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
        let targets: Vec<String> = s
            .bindings
            .iter()
            .filter(|b| b.channel == m.channel && (b.thread_ts.is_empty() || b.thread_ts == root))
            .map(|b| b.agent.clone())
            .collect();
        if targets.is_empty() {
            return;
        }
        let ch = s.channel(&m.channel).cloned();
        let label = ch.as_ref().map(|c| s.channel_label(c)).unwrap_or_else(|| m.channel.clone());
        let who = m.user.as_deref().map(|u| s.user_label(u)).or_else(|| m.username.clone()).unwrap_or_else(|| "someone".into());
        let body = s.names.with(|n| mrkdwn::plain(&m.text, n));
        let text = format!(
            "Slack {label} · {who}: {body}\n(answer in the thread with: tmux2 plugin-command slack 'reply {label} {root} <text>')"
        );
        (targets, text)
    };
    #[derive(serde::Serialize)]
    struct Deliver<'a> {
        to: &'a str,
        from: String,
        body: &'a str,
    }
    for agent in targets {
        let req = Deliver { to: &agent, from: format!("slack:{}", m.channel), body: &text };
        if let Err(e) = service::call_json::<_, Value>("mailbox", "deliver", &req).await {
            log(&format!("slack: bridge to {agent}: {}", e.message));
        }
    }
}
