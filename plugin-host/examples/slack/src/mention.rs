//! `@` in the compose box: a dropdown of people as the name is typed,
//! like the agents picker's project prompt. `@te` shows everyone whose
//! handle or name has `te` in it, those who spoke in the conversation
//! most recently first; Tab/Down and BTab/Up walk it, Enter takes the
//! highlighted one (`@tejas ` goes into the draft), Escape puts it away
//! until the token changes. `@here`, `@channel` and `@everyone` are
//! offered too. On send, `@handle` becomes Slack's `<@U…>` (and the
//! three keywords `<!here>`-style), so Slack pings the person; a word
//! that is nobody's handle is left as typed.

use std::collections::HashMap;

use crate::store::User;

/// Someone the dropdown can name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// `U…`, or `!here` for a keyword.
    pub id: String,
    /// What goes into the draft after the `@`.
    pub handle: String,
    /// Shown beside the handle: the display or full name; empty when it
    /// is the handle again.
    pub label: String,
    /// The full name, matched but not shown when the label is the
    /// display name.
    pub real: String,
}

/// The dropdown while a `@` token is being typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropdown {
    /// The text after the `@`.
    pub partial: String,
    /// Indexes into the people list, best first.
    pub hits: Vec<usize>,
    pub idx: usize,
}

/// Most people the dropdown shows.
pub const MAX_HITS: usize = 8;

/// The keywords that address a room.
const KEYWORDS: [&str; 3] = ["here", "channel", "everyone"];

/// The people the box can offer: every known member who is not deleted
/// (bots included, they can be pinged), the room keywords, `me` last.
pub fn people(users: &HashMap<String, User>, me: &str) -> Vec<Person> {
    let mut v: Vec<Person> = users
        .values()
        .filter(|u| !u.deleted && !u.name.trim().is_empty())
        .map(|u| {
            let label = u.label();
            Person {
                id: u.id.clone(),
                handle: u.name.trim().to_string(),
                label: if label == u.name.trim() { String::new() } else { label.to_string() },
                real: u.real_name.trim().to_string(),
            }
        })
        .collect();
    v.sort_by(|a, b| (a.id == me).cmp(&(b.id == me)).then_with(|| a.handle.to_lowercase().cmp(&b.handle.to_lowercase())));
    v.extend(KEYWORDS.iter().map(|k| Person { id: format!("!{k}"), handle: k.to_string(), label: String::new(), real: String::new() }));
    v
}

/// A handle's characters: what a Slack handle may hold.
fn is_handle_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The `@` token the cursor sits at the end of, in `line` up to
/// `col` (characters): the part after the `@`, which may be empty.
/// None when the cursor is not right after such a token (the `@` must
/// start the line or follow a blank, so `a@b.com` is not one).
pub fn typing(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().take(col).collect();
    let mut i = chars.len();
    while i > 0 && is_handle_char(chars[i - 1]) {
        i -= 1;
    }
    if i == 0 || chars[i - 1] != '@' {
        return None;
    }
    if i >= 2 && !chars[i - 2].is_whitespace() {
        return None;
    }
    Some(chars[i..].iter().collect())
}

/// The people for `partial`: handle, display name or full name contains it (any case);
/// a handle that starts with it ranks first, then whoever spoke latest
/// (`recent`: user ids, latest first), then the handle. At most
/// [`MAX_HITS`]. An empty partial offers everyone, in that order.
pub fn matches(people: &[Person], partial: &str, recent: &[String]) -> Vec<usize> {
    let p = partial.to_lowercase();
    let rank = |id: &str| recent.iter().position(|r| r == id).unwrap_or(usize::MAX);
    let mut hits: Vec<(usize, usize, usize)> = people
        .iter()
        .enumerate()
        .filter(|(_, x)| p.is_empty() || x.handle.to_lowercase().contains(&p) || x.label.to_lowercase().contains(&p) || x.real.to_lowercase().contains(&p))
        .map(|(i, x)| {
            let prefix = if !p.is_empty() && x.handle.to_lowercase().starts_with(&p) { 0 } else { 1 };
            (prefix, rank(&x.id), i)
        })
        .collect();
    hits.sort();
    hits.into_iter().map(|(_, _, i)| i).take(MAX_HITS).collect()
}

/// The user ids of the messages, latest first, each once.
pub fn recent_speakers<'a>(msgs: impl DoubleEndedIterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in msgs.rev() {
        if !u.is_empty() && !out.iter().any(|x| x == u) {
            out.push(u.to_string());
        }
    }
    out
}

/// `@handle` tokens (at the start or after a blank) that name a known
/// handle become `<@U…>`; the room keywords `<!here>` and friends. Any
/// other `@word` stays. A handle match ignores case.
pub fn encode(text: &str, users: &HashMap<String, User>) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            let mut j = i + 1;
            while j < chars.len() && is_handle_char(chars[j]) {
                j += 1;
            }
            if j > i + 1 {
                let word: String = chars[i + 1..j].iter().collect();
                let lower = word.to_lowercase();
                if KEYWORDS.contains(&lower.as_str()) {
                    out.push_str(&format!("<!{lower}>"));
                    i = j;
                    continue;
                }
                if let Some(u) = users.values().find(|u| !u.deleted && u.name.eq_ignore_ascii_case(&word)) {
                    out.push_str(&format!("<@{}>", u.id));
                    i = j;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> HashMap<String, User> {
        [
            ("U1", "zack", "Zack R", "zack"),
            ("U2", "tejas", "Tejas K", "Tejas"),
            ("U3", "gregor", "", "gregor"),
            ("U4", "old", "Old Hand", ""),
        ]
        .into_iter()
        .map(|(id, name, real, disp)| {
            (
                id.to_string(),
                User {
                    id: id.into(),
                    name: name.into(),
                    real_name: real.into(),
                    display_name: disp.into(),
                    deleted: id == "U4",
                    ..Default::default()
                },
            )
        })
        .collect()
    }

    #[test]
    fn people_are_members_then_keywords_me_last() {
        let p = people(&users(), "U1");
        let handles: Vec<&str> = p.iter().map(|x| x.handle.as_str()).collect();
        assert_eq!(handles, vec!["gregor", "tejas", "zack", "here", "channel", "everyone"]);
        assert_eq!(p[1].label, "Tejas");
        assert_eq!(p[0].label, "", "a label equal to the handle is not repeated");
    }

    #[test]
    fn the_token_under_the_cursor() {
        assert_eq!(typing("hey @te", 7), Some("te".into()));
        assert_eq!(typing("@", 1), Some("".into()));
        assert_eq!(typing("hey @te more", 7), Some("te".into()));
        assert_eq!(typing("hey @te more", 12), None);
        assert_eq!(typing("a@b.com", 7), None, "an address is not a mention");
        assert_eq!(typing("hey te", 6), None);
    }

    #[test]
    fn matches_rank_prefix_then_recent() {
        let p = people(&users(), "U1");
        let recent = vec!["U3".to_string(), "U2".to_string()];
        let hit = |partial: &str| -> Vec<&str> { matches(&p, partial, &recent).iter().map(|&i| p[i].handle.as_str()).collect() };
        assert_eq!(hit("te"), vec!["tejas"]);
        assert_eq!(hit("e"), vec!["everyone", "gregor", "tejas", "here", "channel"], "prefix first, then the latest speaker");
        assert_eq!(hit(""), vec!["gregor", "tejas", "zack", "here", "channel", "everyone"]);
        assert_eq!(hit("K"), vec!["tejas", "zack"], "a name matches too, any case");
        assert!(hit("nobody").is_empty());
    }

    #[test]
    fn speakers_latest_first_once() {
        let r = recent_speakers(["U1", "U2", "U1", "U3"].into_iter());
        assert_eq!(r, vec!["U3", "U1", "U2"]);
    }

    #[test]
    fn encoding_handles_and_keywords() {
        let u = users();
        assert_eq!(encode("hey @tejas, look @Zack", &u), "hey <@U2>, look <@U1>");
        assert_eq!(encode("@here ship it", &u), "<!here> ship it");
        assert_eq!(encode("mail a@b.com or @nobody", &u), "mail a@b.com or @nobody");
        assert_eq!(encode("@old is gone", &u), "@old is gone", "a deleted member is not pinged");
        assert_eq!(encode("<@U2> already", &u), "<@U2> already");
    }
}
