//! The notifications pane's picture of a conversation.
//!
//! A toast the plugin posts names itself as the row's preview source,
//! and the toast plugin's chooser calls `preview` back with the row's
//! key and the column it has to fill. The answer is what the sidebar's
//! preview would draw for that conversation: the last messages laid out
//! at that width, the day labels, the `new messages` rule and the
//! unread bar, with the thread's own read mark when the row is a
//! thread. Rows of styled text, nothing positional, so the caller puts
//! them where its column is.

use std::rc::Rc;

use tmux_plugin_sdk::prelude::*;

use crate::store::Message;
use crate::{api, chat, store, Shared};

/// What the toast plugin asks with: the row's key as the toast was
/// posted (`slack:<channel>[/<thread>]`) and the column's size.
#[derive(serde::Deserialize)]
struct Req {
    #[serde(default)]
    key: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(serde::Serialize)]
struct Reply {
    lines: Vec<String>,
}

/// The messages the cache must hold for a channel before it stands in
/// for Slack: fewer and the picture is only what arrived while the
/// feed was up, with nothing older to read it against.
const CACHE_ENOUGH: usize = 10;

/// A `dismissed` call: the key of the row the user dropped.
#[derive(serde::Deserialize)]
struct Dismissed {
    key: String,
}

/// Answer a `dismissed` call: the user dropped this conversation's row
/// from the notifications pane, so it is read for them. A channel is
/// marked read at its latest message, a thread at its last reply, on
/// Slack and here (the sidebar's badges follow). The reply goes out
/// at once; the mark runs on its own.
pub fn dismissed(st: &Shared, req: ServiceRequest) {
    let key = match req.json::<Dismissed>() {
        Ok(r) => r.key,
        Err(e) => {
            let _ = req.fail(&format!("dismissed: {}", e.message));
            return;
        }
    };
    let Some((channel, thread)) = conversation_of(&key) else {
        let _ = req.fail(&format!("dismissed: not a conversation key: {key:?}"));
        return;
    };
    let _ = req.reply_json(&serde_json::json!({ "ok": true }));
    let st = Rc::clone(st);
    spawn(async move {
        const WHY: &str = "dismissed in the notifications pane";
        match thread {
            Some(root) => crate::mark_thread_read(&st, &channel, &root, WHY).await,
            None => crate::mark_read(&st, &channel, WHY).await,
        }
    });
}

/// How much history a preview shows at most.
const PAGE: usize = 25;

/// Answer a `preview` call; the reply goes out from a task once the
/// messages are in.
pub fn serve(st: &Shared, req: ServiceRequest) {
    let r: Req = match req.json() {
        Ok(r) => r,
        Err(e) => {
            let _ = req.fail(&format!("preview: {}", e.message));
            return;
        }
    };
    let Some((channel, thread)) = conversation_of(&r.key) else {
        let _ = req.fail(&format!("preview: not a conversation key: {:?}", r.key));
        return;
    };
    if r.width < 10 || r.height == 0 {
        let _ = req.reply_json(&Reply { lines: Vec::new() });
        return;
    }
    let st = Rc::clone(st);
    spawn(async move {
        match render(&st, &channel, thread.as_deref(), r.width, r.height).await {
            Ok(lines) => {
                let _ = req.reply_json(&Reply { lines });
            }
            Err(e) => {
                log(&format!("slack: preview {}: {e}", r.key));
                let _ = req.fail(&e);
            }
        }
    });
}

/// `slack:C1/1.2` -> (C1, Some(1.2)); `slack:C1` -> (C1, None); None
/// for anything else.
pub fn conversation_of(key: &str) -> Option<(String, Option<String>)> {
    let rest = key.strip_prefix("slack:")?;
    let (channel, thread) = match rest.split_once('/') {
        Some((c, t)) => (c, Some(t.to_string()).filter(|t| !t.is_empty())),
        None => (rest, None),
    };
    (!channel.is_empty()).then(|| (channel.to_string(), thread))
}

/// The conversation's rows for a `width` x `height` column.
async fn render(st: &Shared, channel: &str, thread: Option<&str>, width: u32, height: u32) -> Result<Vec<String>, String> {
    let (id, ch) = {
        let s = st.borrow();
        let id = s.ident.clone();
        let ch = s.channel(channel).cloned().ok_or_else(|| format!("unknown channel {channel}"))?;
        (id, ch)
    };
    let msgs = match thread {
        Some(root) => thread_messages(st, id.as_ref(), channel, root).await,
        None => channel_messages(st, id.as_ref(), channel).await,
    };
    let (names, tz, me, last_read) = {
        let s = st.borrow();
        let me = s.ident.as_ref().map(|i| i.user_id.clone()).unwrap_or_default();
        // A thread reads against the Threads view's mark for it when
        // there is one; the channel's mark says nothing about a thread.
        let last_read = match thread {
            Some(root) => s.threads.iter().find(|t| t.channel == channel && t.root.ts == root).and_then(|t| t.last_read.clone()),
            None => ch.last_read.clone(),
        };
        (Rc::clone(&s.names), s.tz(), me, last_read)
    };
    let now = (now_ms() / 1000) as i64;
    let mut c = chat::embedded(ModeId(0), ch, None, None, msgs, width, height, 0);
    c.thread = thread.map(str::to_string);
    c.last_read = last_read;
    c.me = me.clone();
    // No cursor: the column is read, not walked.
    c.cursor = None;
    chat::layout_chat(&mut c, &names, tz, &me, now, None);
    Ok(chat::render_lines(&c))
}

/// The channel's last messages: the cache when it holds enough, else
/// Slack's (kept), else whatever the cache has.
async fn channel_messages(st: &Shared, id: Option<&store::Identity>, channel: &str) -> Vec<Message> {
    let cached = store::messages(channel, PAGE).await;
    if cached.len() >= CACHE_ENOUGH {
        return cached;
    }
    let Some(id) = id else { return cached };
    match api::history(id, channel, None, None, PAGE as u32).await {
        Ok((mut msgs, _)) => {
            msgs.reverse();
            crate::absorb(st, channel, &msgs).await;
            msgs
        }
        Err(e) => {
            log(&format!("slack: preview {channel}: {e}"));
            cached
        }
    }
}

/// A thread: Slack's replies (kept), else the cached ones.
async fn thread_messages(st: &Shared, id: Option<&store::Identity>, channel: &str, root: &str) -> Vec<Message> {
    if let Some(id) = id {
        match api::replies(id, channel, root).await {
            Ok(msgs) => {
                crate::absorb(st, channel, &msgs).await;
                return msgs;
            }
            Err(e) => log(&format!("slack: preview {channel}/{root}: {e}")),
        }
    }
    store::thread(channel, root).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_name_a_channel_or_a_thread() {
        assert_eq!(conversation_of("slack:C1"), Some(("C1".into(), None)));
        assert_eq!(conversation_of("slack:C1/1700000022.000100"), Some(("C1".into(), Some("1700000022.000100".into()))));
        assert_eq!(conversation_of("slack:C1/"), Some(("C1".into(), None)));
        assert_eq!(conversation_of("slack:"), None);
        assert_eq!(conversation_of("other:C1"), None);
        assert_eq!(conversation_of(""), None);
    }
}
