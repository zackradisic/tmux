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

/// Slack's placeholder for an attachment it could not summarise.
const NO_PREVIEW: &str = "[no preview available]";

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|t| !t.trim().is_empty())
}

/// A legacy attachment (a link unfurl, an integration card) as the lines
/// the views draw: pretext, author/title, text, each field as
/// `title: value`, footer. The `fallback` only when nothing else is
/// there, and never Slack's "[no preview available]".
fn attachment_text(at: &Value) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    fn push(lines: &mut Vec<String>, t: Option<String>) {
        if let Some(t) = nonempty(t) {
            lines.push(t.trim_end().to_string());
        }
    }
    push(&mut lines, s(at, "pretext"));
    let text = nonempty(s(at, "text"));
    let head = nonempty(s(at, "author_name"));
    let title = nonempty(s(at, "title"));
    let head = match (head, title) {
        (Some(a), Some(t)) => Some(format!("{a}: {t}")),
        (a, t) => a.or(t),
    };
    if let Some(h) = head {
        match nonempty(s(at, "title_link")).filter(|_| text.is_none()) {
            Some(link) => push(&mut lines, Some(format!("{h} <{link}>"))),
            None => push(&mut lines, Some(h)),
        }
    }
    push(&mut lines, text);
    for f in at.get("fields").and_then(Value::as_array).into_iter().flatten() {
        let Some(value) = nonempty(s(f, "value")) else { continue };
        match nonempty(s(f, "title")) {
            Some(t) => push(&mut lines, Some(format!("{t}: {value}"))),
            None => push(&mut lines, Some(value)),
        }
    }
    if let Some(blocks) = at.get("blocks") {
        push(&mut lines, Some(blocks_text(blocks)));
    }
    push(&mut lines, s(at, "footer"));
    if lines.is_empty() {
        push(&mut lines, nonempty(s(at, "fallback")).filter(|f| f.trim() != NO_PREVIEW));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// A Block Kit `blocks` array as mrkdwn text, one block per line
/// (`header`, `section` with its fields, `context`, `rich_text`,
/// `image`, a `divider` as a rule). The angle-bracket tokens stay as
/// mrkdwn so the renderer resolves them.
pub fn blocks_text(blocks: &Value) -> String {
    let mut out: Vec<String> = Vec::new();
    for blk in blocks.as_array().into_iter().flatten() {
        let kind = blk.get("type").and_then(Value::as_str).unwrap_or("");
        let mut lines: Vec<String> = Vec::new();
        match kind {
            "header" | "section" => {
                if let Some(t) = nonempty(blk.get("text").and_then(|t| s(t, "text"))) {
                    lines.push(t);
                }
                for f in blk.get("fields").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(t) = nonempty(s(f, "text")) {
                        lines.push(t);
                    }
                }
            }
            "context" => {
                let parts: Vec<String> = blk
                    .get("elements")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| nonempty(s(e, "text")).or_else(|| nonempty(s(e, "alt_text"))))
                    .collect();
                if !parts.is_empty() {
                    lines.push(parts.join("  "));
                }
            }
            "image" => {
                if let Some(t) = nonempty(s(blk, "alt_text")).or_else(|| nonempty(blk.get("title").and_then(|t| s(t, "text")))) {
                    lines.push(format!("[image: {t}]"));
                }
            }
            "divider" => lines.push("──".to_string()),
            "rich_text" => rich_text_lines(blk, "", &mut lines),
            _ => {}
        }
        out.extend(lines.into_iter().filter(|l| !l.trim().is_empty()));
    }
    out.join("\n")
}

/// The lines of a `rich_text` block (or one of its containers): each
/// `rich_text_section` is a line, a list's items get a bullet, a quote
/// a `>`, preformatted text a fence.
fn rich_text_lines(blk: &Value, prefix: &str, out: &mut Vec<String>) {
    for el in blk.get("elements").and_then(Value::as_array).into_iter().flatten() {
        match el.get("type").and_then(Value::as_str).unwrap_or("") {
            "rich_text_section" => out.push(format!("{prefix}{}", rich_text_inline(el))),
            "rich_text_list" => {
                let ordered = el.get("style").and_then(Value::as_str) == Some("ordered");
                for (n, item) in el.get("elements").and_then(Value::as_array).into_iter().flatten().enumerate() {
                    let bullet = if ordered { format!("{}. ", n + 1) } else { "• ".to_string() };
                    out.push(format!("{prefix}{bullet}{}", rich_text_inline(item)));
                }
            }
            "rich_text_quote" => out.push(format!("{prefix}&gt; {}", rich_text_inline(el))),
            "rich_text_preformatted" => {
                out.push("```".to_string());
                out.extend(rich_text_inline(el).lines().map(str::to_string));
                out.push("```".to_string());
            }
            _ => {}
        }
    }
}

/// One rich_text section's elements as mrkdwn: text, `<url|label>`,
/// `<@U..>`, `<#C..>`, `:emoji:`, `<!here>`.
fn rich_text_inline(section: &Value) -> String {
    let mut out = String::new();
    for e in section.get("elements").and_then(Value::as_array).into_iter().flatten() {
        match e.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => out.push_str(&s(e, "text").unwrap_or_default()),
            "link" => {
                let url = s(e, "url").unwrap_or_default();
                match nonempty(s(e, "text")).filter(|t| *t != url) {
                    Some(t) => out.push_str(&format!("<{url}|{t}>")),
                    None => out.push_str(&format!("<{url}>")),
                }
            }
            "user" => out.push_str(&format!("<@{}>", s(e, "user_id").unwrap_or_default())),
            "usergroup" => out.push_str(&format!("<!subteam^{}>", s(e, "usergroup_id").unwrap_or_default())),
            "channel" => out.push_str(&format!("<#{}>", s(e, "channel_id").unwrap_or_default())),
            "emoji" => {
                // A unicode emoji arrives with its codepoints; Slack's own
                // custom ones only by name.
                let shown = s(e, "unicode")
                    .and_then(|u| u.split('-').map(|h| u32::from_str_radix(h, 16).ok().and_then(char::from_u32)).collect::<Option<String>>())
                    .unwrap_or_else(|| format!(":{}:", s(e, "name").unwrap_or_default()));
                out.push_str(&shown);
            }
            "broadcast" => out.push_str(&format!("<!{}>", s(e, "range").unwrap_or_else(|| "here".into()))),
            "date" => out.push_str(&s(e, "fallback").unwrap_or_default()),
            _ => {}
        }
    }
    out
}

/// The user ids a message names: its author and every `<@U..>` mention
/// in its text (what the views need resolved to draw it).
pub fn user_ids_of(m: &Message) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(u) = m.user.as_deref().filter(|u| u.starts_with('U') || u.starts_with('W')) {
        out.push(u.to_string());
    }
    let mut rest = m.text.as_str();
    while let Some(i) = rest.find("<@") {
        rest = &rest[i + 2..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
        if (id.starts_with('U') || id.starts_with('W')) && id.len() > 1 && !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

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
    let attachments: Vec<String> = m
        .get("attachments")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(attachment_text).collect())
        .unwrap_or_default();
    // A message with no text draws its blocks (a bot's header/section
    // card, an app's rich_text). One with text has the same words in its
    // blocks, so those are left alone.
    let mut text = s(m, "text").unwrap_or_default();
    if text.trim().is_empty() {
        text = m.get("blocks").map(blocks_text).unwrap_or_default();
    }
    Some(Message {
        channel: channel.to_string(),
        ts,
        thread_ts: s(m, "thread_ts"),
        user: s(m, "user"),
        username,
        text,
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
    fn flattens_a_fields_only_attachment() {
        // A bot card with no text: fields, and Slack's useless fallback.
        let v: Value = serde_json::from_str(
            r#"{"type":"message","subtype":"bot_message","bot_id":"B1","username":"Demo App","text":"","ts":"1.1",
                "attachments":[{"fallback":"[no preview available]","id":1,"mrkdwn_in":["fields"],
                  "fields":[{"title":"Status","value":"deploy green","short":true},{"title":"","value":"3 pods","short":true},
                             {"title":"Empty","value":"","short":false}]}]}"#,
        )
        .unwrap();
        let m = message_from("C1", &v).unwrap();
        assert_eq!(m.text, "");
        assert_eq!(m.attachments, vec!["Status: deploy green\n3 pods".to_string()]);
        // Pretext, author + title with its link, text, footer, in that order;
        // the title link is dropped once there is a text.
        let v: Value = serde_json::from_str(
            r#"{"type":"message","text":"","ts":"1.2","attachments":[
                {"pretext":"New PR","author_name":"octocat","title":"Fix it","title_link":"https://x/1","footer":"GitHub","fallback":"f"},
                {"title":"Only a title","title_link":"https://x/2","text":"body"},
                {"fallback":"just the fallback"},
                {"fallback":"[no preview available]"}]}"#,
        )
        .unwrap();
        let m = message_from("C1", &v).unwrap();
        assert_eq!(
            m.attachments,
            vec!["New PR\noctocat: Fix it <https://x/1>\nGitHub".to_string(), "Only a title\nbody".to_string(), "just the fallback".to_string()]
        );
    }

    #[test]
    fn renders_blocks_when_text_is_empty() {
        let v: Value = serde_json::from_str(
            r#"{"type":"message","bot_id":"B1","text":"","ts":"1.3","blocks":[
                {"type":"header","text":{"type":"plain_text","text":"Deploy finished"}},
                {"type":"section","text":{"type":"mrkdwn","text":"*prod* is on <https://x/r/42|r42>"},
                 "fields":[{"type":"mrkdwn","text":"*Region*\nus-east"},{"type":"mrkdwn","text":"*By*\n<@U7>"}]},
                {"type":"divider"},
                {"type":"context","elements":[{"type":"image","image_url":"https://x/i.png","alt_text":"ok"},{"type":"mrkdwn","text":"took 3m"}]}]}"#,
        )
        .unwrap();
        let m = message_from("C1", &v).unwrap();
        assert_eq!(m.text, "Deploy finished\n*prod* is on <https://x/r/42|r42>\n*Region*\nus-east\n*By*\n<@U7>\n──\nok  took 3m");
        assert!(m.attachments.is_empty());
        // With a text, the blocks are its duplicate and stay out.
        let v: Value = serde_json::from_str(
            r#"{"type":"message","user":"U1","text":"hello","ts":"1.4","blocks":[{"type":"rich_text","elements":[{"type":"rich_text_section","elements":[{"type":"text","text":"hello"}]}]}]}"#,
        )
        .unwrap();
        assert_eq!(message_from("C1", &v).unwrap().text, "hello");
        // An attachment's own blocks are its lines.
        let v: Value = serde_json::from_str(
            r#"{"type":"message","text":"","ts":"1.5","attachments":[{"fallback":"[no preview available]",
                "blocks":[{"type":"section","text":{"type":"mrkdwn","text":"from the attachment"}}]}]}"#,
        )
        .unwrap();
        assert_eq!(message_from("C1", &v).unwrap().attachments, vec!["from the attachment".to_string()]);
    }

    #[test]
    fn renders_rich_text_with_a_mention() {
        let v: Value = serde_json::from_str(
            r#"{"type":"message","bot_id":"B2","text":"","ts":"1.6","blocks":[{"type":"rich_text","elements":[
                {"type":"rich_text_section","elements":[
                    {"type":"text","text":"cc "},{"type":"user","user_id":"U0854E66JD8"},{"type":"text","text":" see "},
                    {"type":"link","url":"https://x/doc","text":"the doc"},{"type":"text","text":" in "},
                    {"type":"channel","channel_id":"C9"},{"type":"text","text":" "},{"type":"emoji","name":"tada"},
                    {"type":"text","text":" "},{"type":"broadcast","range":"here"}]},
                {"type":"rich_text_list","style":"bullet","elements":[
                    {"type":"rich_text_section","elements":[{"type":"text","text":"one"}]},
                    {"type":"rich_text_section","elements":[{"type":"text","text":"two"}]}]},
                {"type":"rich_text_quote","elements":[{"type":"text","text":"quoted"}]},
                {"type":"rich_text_preformatted","elements":[{"type":"text","text":"let x = 1;"}]}]}]}"#,
        )
        .unwrap();
        let m = message_from("C1", &v).unwrap();
        assert_eq!(
            m.text,
            "cc <@U0854E66JD8> see <https://x/doc|the doc> in <#C9> :tada: <!here>\n• one\n• two\n&gt; quoted\n```\nlet x = 1;\n```"
        );
        assert_eq!(user_ids_of(&m), vec!["U0854E66JD8".to_string()]);
        let m = Message { user: Some("U1".into()), text: "hi <@U2> and <@U1> and <@W3>, not <#C1> nor <!here>".into(), ..Default::default() };
        assert_eq!(user_ids_of(&m), vec!["U1".to_string(), "U2".to_string(), "W3".to_string()]);
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
