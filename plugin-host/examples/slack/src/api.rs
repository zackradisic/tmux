//! The Slack methods the plugin uses, typed. Each function is one web
//! API call (or one paginated series) and returns store types; nothing
//! here touches the store or the screen.

use serde_json::Value;

use crate::http;
use crate::store::{Channel, Count, Identity, Kind, Message, Section, User};

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}
fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}
fn i(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}
fn cursor(v: &Value) -> Option<String> {
    v.get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

/// Everything `client.counts` says about badges, in one call.
#[derive(Debug, Default)]
pub struct Counts {
    pub channels: Vec<Count>,
    pub threads_unread: bool,
    pub thread_mentions: i64,
}

pub async fn counts(id: &Identity) -> Result<Counts, String> {
    let v = http::call(id, "client.counts", &[]).await?;
    let mut out = Counts::default();
    for key in ["channels", "mpims", "ims"] {
        for c in v.get(key).and_then(Value::as_array).into_iter().flatten() {
            let Some(cid) = s(c, "id") else { continue };
            out.channels.push(Count {
                id: cid,
                last_read: s(c, "last_read"),
                latest: s(c, "latest"),
                has_unreads: b(c, "has_unreads"),
                mentions: i(c, "mention_count"),
                is_muted: c.get("is_muted").and_then(Value::as_bool),
                is_open: c.get("is_open").and_then(Value::as_bool),
            });
        }
    }
    if let Some(t) = v.get("threads") {
        out.threads_unread = b(t, "has_unreads");
        out.thread_mentions = i(t, "mention_count");
    }
    Ok(out)
}

fn channel_from(c: &Value) -> Option<Channel> {
    let id = s(c, "id")?;
    let kind = if b(c, "is_im") {
        Kind::Im
    } else if b(c, "is_mpim") {
        Kind::Mpim
    } else if b(c, "is_private") || b(c, "is_group") {
        Kind::Private
    } else {
        Kind::Public
    };
    let user_id = s(c, "user");
    let name = match kind {
        Kind::Im => user_id.clone().unwrap_or_else(|| id.clone()),
        _ => s(c, "name").unwrap_or_else(|| id.clone()),
    };
    let field = |k: &str| c.get(k).and_then(|t| t.get("value")).and_then(Value::as_str).unwrap_or("").to_string();
    Some(Channel {
        id,
        name,
        kind,
        user_id,
        topic: field("topic"),
        purpose: field("purpose"),
        is_member: c.get("is_member").and_then(Value::as_bool).unwrap_or(true),
        is_muted: false,
        is_starred: b(c, "is_starred"),
        is_archived: b(c, "is_archived"),
        is_open: c.get("is_open").and_then(Value::as_bool).unwrap_or(true),
        last_read: s(c, "last_read"),
        latest: None,
        has_unreads: false,
        mentions: 0,
        updated_ms: 0,
    })
}

/// Every conversation the user is in.
pub async fn conversations(id: &Identity) -> Result<Vec<Channel>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        let mut form = vec![
            ("types", "public_channel,private_channel,mpim,im"),
            ("exclude_archived", "true"),
            ("limit", "200"),
        ];
        let c = cur.clone().unwrap_or_default();
        if !c.is_empty() {
            form.push(("cursor", &c));
        }
        let v = http::call(id, "users.conversations", &form).await?;
        for ch in v.get("channels").and_then(Value::as_array).into_iter().flatten() {
            if let Some(c) = channel_from(ch) {
                out.push(c);
            }
        }
        cur = cursor(&v);
        if cur.is_none() {
            break;
        }
    }
    Ok(out)
}

/// The user's sidebar sections.
pub async fn sections(id: &Identity) -> Result<Vec<Section>, String> {
    let v = http::call(id, "users.channelSections.list", &[]).await?;
    let mut out = Vec::new();
    for sec in v.get("channel_sections").and_then(Value::as_array).into_iter().flatten() {
        let Some(sid) = s(sec, "channel_section_id") else { continue };
        let ids = sec
            .get("channel_ids_page")
            .and_then(|p| p.get("channel_ids"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        out.push(Section {
            id: sid,
            name: s(sec, "name").unwrap_or_default(),
            kind: s(sec, "type").unwrap_or_default(),
            emoji: s(sec, "emoji").unwrap_or_default(),
            channel_ids: ids,
        });
    }
    Ok(out)
}

fn user_from(u: &Value) -> Option<User> {
    let id = s(u, "id")?;
    let profile = u.get("profile");
    let p = |k: &str| profile.and_then(|p| p.get(k)).and_then(Value::as_str).unwrap_or("").to_string();
    Some(User {
        id,
        name: s(u, "name").unwrap_or_default(),
        real_name: s(u, "real_name").unwrap_or_else(|| p("real_name")),
        display_name: p("display_name"),
        is_bot: b(u, "is_bot"),
        deleted: b(u, "deleted"),
        tz_offset: i(u, "tz_offset"),
    })
}

pub async fn users(id: &Identity) -> Result<Vec<User>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        let mut form = vec![("limit", "200")];
        let c = cur.clone().unwrap_or_default();
        if !c.is_empty() {
            form.push(("cursor", &c));
        }
        let v = http::call(id, "users.list", &form).await?;
        for u in v.get("members").and_then(Value::as_array).into_iter().flatten() {
            if let Some(u) = user_from(u) {
                out.push(u);
            }
        }
        cur = cursor(&v);
        if cur.is_none() {
            break;
        }
    }
    Ok(out)
}

pub async fn user_info(id: &Identity, user: &str) -> Result<User, String> {
    let v = http::call(id, "users.info", &[("user", user)]).await?;
    v.get("user").and_then(user_from).ok_or_else(|| "users.info: no user".into())
}

/// Subtypes that are housekeeping, not conversation.
const NOISE: &[&str] = &[
    "channel_join",
    "channel_leave",
    "group_join",
    "group_leave",
    "channel_purpose",
    "channel_topic",
    "channel_name",
    "bot_add",
    "bot_remove",
    "pinned_item",
    "joiner_notification",
    "joiner_notification_for_inviter",
];

/// One message record as the views keep it. `None` for noise.
pub fn message_from(channel: &str, m: &Value) -> Option<Message> {
    if m.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let ts = s(m, "ts")?;
    let subtype = s(m, "subtype");
    if subtype.as_deref().is_some_and(|st| NOISE.contains(&st)) {
        return None;
    }
    let username = s(m, "username")
        .or_else(|| m.get("bot_profile").and_then(|p| p.get("name")).and_then(Value::as_str).map(str::to_string));
    let reactions = m
        .get("reactions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|r| Some((s(r, "name")?, i(r, "count")))).collect())
        .unwrap_or_default();
    let files = m
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| s(f, "title").filter(|t| !t.is_empty()).or_else(|| s(f, "name")))
                .collect()
        })
        .unwrap_or_default();
    let attachments = m
        .get("attachments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|at| {
                    let title = s(at, "title").unwrap_or_default();
                    let text = s(at, "text").or_else(|| s(at, "fallback")).unwrap_or_default();
                    let line = match (title.is_empty(), text.is_empty()) {
                        (true, true) => return None,
                        (false, true) => title,
                        (true, false) => text,
                        (false, false) => format!("{title}\n{text}"),
                    };
                    Some(line)
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Message {
        channel: channel.to_string(),
        ts,
        thread_ts: s(m, "thread_ts"),
        user: s(m, "user"),
        username,
        text: s(m, "text").unwrap_or_default(),
        subtype,
        reply_count: i(m, "reply_count"),
        latest_reply: s(m, "latest_reply"),
        reactions,
        files,
        edited: m.get("edited").is_some(),
        attachments,
        deleted: false,
    })
}

/// A page of a channel's history, newest first as Slack returns it.
/// `oldest` / `latest` are exclusive bounds.
pub async fn history(
    id: &Identity,
    channel: &str,
    oldest: Option<&str>,
    latest: Option<&str>,
    limit: u32,
) -> Result<(Vec<Message>, bool), String> {
    let lim = limit.to_string();
    let mut form = vec![("channel", channel), ("limit", lim.as_str()), ("inclusive", "false")];
    if let Some(o) = oldest {
        form.push(("oldest", o));
    }
    if let Some(l) = latest {
        form.push(("latest", l));
    }
    let v = http::call(id, "conversations.history", &form).await?;
    let msgs = v
        .get("messages")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|m| message_from(channel, m)).collect())
        .unwrap_or_default();
    Ok((msgs, b(&v, "has_more")))
}

/// A whole thread: root first.
pub async fn replies(id: &Identity, channel: &str, thread_ts: &str) -> Result<Vec<Message>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    loop {
        let mut form = vec![("channel", channel), ("ts", thread_ts), ("limit", "200")];
        let c = cur.clone().unwrap_or_default();
        if !c.is_empty() {
            form.push(("cursor", &c));
        }
        let v = http::call(id, "conversations.replies", &form).await?;
        for m in v.get("messages").and_then(Value::as_array).into_iter().flatten() {
            if let Some(m) = message_from(channel, m) {
                out.push(m);
            }
        }
        cur = cursor(&v);
        if cur.is_none() {
            break;
        }
    }
    Ok(out)
}

pub async fn mark(id: &Identity, channel: &str, ts: &str) -> Result<(), String> {
    http::call(id, "conversations.mark", &[("channel", channel), ("ts", ts)]).await.map(|_| ())
}

/// Post as the user. Returns the message Slack stored.
pub async fn post(id: &Identity, channel: &str, text: &str, thread_ts: Option<&str>) -> Result<Message, String> {
    let mut form = vec![("channel", channel), ("text", text), ("as_user", "true")];
    if let Some(t) = thread_ts {
        form.push(("thread_ts", t));
    }
    let v = http::call(id, "chat.postMessage", &form).await?;
    let ch = s(&v, "channel").unwrap_or_else(|| channel.to_string());
    v.get("message")
        .and_then(|m| message_from(&ch, m))
        .or_else(|| {
            Some(Message {
                channel: ch,
                ts: s(&v, "ts")?,
                thread_ts: thread_ts.map(str::to_string),
                user: Some(id.user_id.clone()),
                text: text.to_string(),
                ..Default::default()
            })
        })
        .ok_or_else(|| "chat.postMessage: no message".into())
}

pub async fn react(id: &Identity, channel: &str, ts: &str, name: &str) -> Result<(), String> {
    let name = name.trim_matches(':');
    match http::call(id, "reactions.add", &[("channel", channel), ("timestamp", ts), ("name", name)]).await {
        Ok(_) => Ok(()),
        Err(e) if e.ends_with("already_reacted") => {
            http::call(id, "reactions.remove", &[("channel", channel), ("timestamp", ts), ("name", name)]).await.map(|_| ())
        }
        Err(e) => Err(e),
    }
}

pub async fn permalink(id: &Identity, channel: &str, ts: &str) -> Result<String, String> {
    let v = http::call(id, "chat.getPermalink", &[("channel", channel), ("message_ts", ts)]).await?;
    s(&v, "permalink").ok_or_else(|| "chat.getPermalink: no permalink".into())
}

/// A search hit: where it is, and the message.
pub struct Hit {
    pub channel_name: String,
    pub message: Message,
}

pub async fn search(id: &Identity, query: &str, count: u32) -> Result<Vec<Hit>, String> {
    let n = count.to_string();
    let v = http::call(id, "search.messages", &[("query", query), ("count", &n), ("sort", "timestamp"), ("sort_dir", "desc")]).await?;
    let mut out = Vec::new();
    for m in v.get("messages").and_then(|m| m.get("matches")).and_then(Value::as_array).into_iter().flatten() {
        let ch = m.get("channel");
        let cid = ch.and_then(|c| c.get("id")).and_then(Value::as_str).unwrap_or("");
        let cname = ch.and_then(|c| c.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
        let mut mm = Message {
            channel: cid.to_string(),
            ts: s(m, "ts").unwrap_or_default(),
            user: s(m, "user"),
            username: s(m, "username"),
            text: s(m, "text").unwrap_or_default(),
            ..Default::default()
        };
        // A hit inside a thread carries the root in its permalink.
        if let Some(p) = s(m, "permalink") {
            if let Some(i) = p.find("thread_ts=") {
                let t: String = p[i + 10..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                if !t.is_empty() {
                    mm.thread_ts = Some(t);
                }
            }
        }
        out.push(Hit { channel_name: cname, message: mm });
    }
    Ok(out)
}

pub async fn rtm_connect(id: &Identity) -> Result<String, String> {
    let v = http::call(id, "rtm.connect", &[("batch_presence_aware", "1"), ("presence_sub", "1")]).await?;
    s(&v, "url").ok_or_else(|| "rtm.connect: no url".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_message() {
        let v: Value = serde_json::from_str(
            r#"{"type":"message","user":"U1","text":"hi <@U2>","ts":"1700000000.000100","thread_ts":"1700000000.000100",
                "reply_count":2,"latest_reply":"1700000500.000200","reactions":[{"name":"+1","count":3,"users":["U2"]}],
                "files":[{"name":"a.png","title":""}],"edited":{"user":"U1","ts":"1"},
                "attachments":[{"title":"A link","text":"its text"}]}"#,
        )
        .unwrap();
        let m = message_from("C1", &v).unwrap();
        assert_eq!(m.user.as_deref(), Some("U1"));
        assert_eq!(m.reply_count, 2);
        assert_eq!(m.reactions, vec![("+1".to_string(), 3)]);
        assert_eq!(m.files, vec!["a.png".to_string()]);
        assert!(m.edited);
        assert_eq!(m.attachments, vec!["A link\nits text".to_string()]);
        assert!(!m.is_reply());
    }

    #[test]
    fn drops_join_noise() {
        let v: Value = serde_json::from_str(r#"{"type":"message","subtype":"channel_join","user":"U1","text":"joined","ts":"1.2"}"#).unwrap();
        assert!(message_from("C1", &v).is_none());
    }

    #[test]
    fn names_a_dm_by_its_peer() {
        let v: Value = serde_json::from_str(r#"{"id":"D1","is_im":true,"user":"U9","is_open":true}"#).unwrap();
        let c = channel_from(&v).unwrap();
        assert_eq!(c.kind, Kind::Im);
        assert_eq!(c.name, "U9");
        let v: Value = serde_json::from_str(r#"{"id":"C1","name":"general","is_channel":true,"is_private":false,"topic":{"value":"t"}}"#).unwrap();
        let c = channel_from(&v).unwrap();
        assert_eq!(c.kind, Kind::Public);
        assert_eq!(c.topic, "t");
    }
}
