//! The search box, parsed: free words that rank rows, and sigil tokens
//! (`@server`, `#session`, `~dir`) that narrow them. Which sigils exist
//! is the consumer's call: it passes the set to [`parse_query`] and
//! [`typing_token`], and reads the values back by sigil.
//!
//! The pure halves of the dropdown live here too: counting the values a
//! roster holds for the token being typed ([`complete_values`]), putting
//! the chosen one into the box ([`replace_typing_token`]), and toggling a
//! token on and off ([`toggle_word`]). What the values ARE (a row's
//! server, its session, its folder) stays with the consumer.

use std::collections::HashMap;

/// How many values the dropdown offers at most.
pub const COMPLETIONS_MAX: usize = 8;

/// A parsed search box: the free words, and the filter tokens by sigil
/// in the order typed. Several of one sigil are alternatives; the sigils
/// combine. A backslash escapes a sigil into an ordinary word: `\@foo`
/// is the word `@foo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub words: String,
    pub tokens: Vec<(char, String)>,
    /// The sigils the box was parsed with, so `tokens()` can spell them
    /// grouped in that order.
    sigils: Vec<char>,
}

impl Query {
    pub fn has_filters(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// The values typed for one sigil.
    pub fn values(&self, sigil: char) -> Vec<&str> {
        self.tokens.iter().filter(|(c, _)| *c == sigil).map(|(_, v)| v.as_str()).collect()
    }

    /// The active tokens, spelled as typed, grouped by sigil.
    pub fn tokens(&self) -> String {
        let mut t: Vec<String> = Vec::new();
        for s in &self.sigils {
            t.extend(self.values(*s).into_iter().map(|v| format!("{s}{v}")));
        }
        t.join(" ")
    }
}

pub fn parse_query(filter: &str, sigils: &[char]) -> Query {
    let mut q = Query { sigils: sigils.to_vec(), ..Query::default() };
    let mut words: Vec<&str> = Vec::new();
    for w in filter.split_whitespace() {
        let Some(c) = w.chars().next() else { continue };
        if c == '\\' {
            let rest = &w[1..];
            words.push(if rest.starts_with(sigils) { rest } else { w });
        } else if sigils.contains(&c) {
            let v = &w[c.len_utf8()..];
            if v.is_empty() {
                // A bare sigil is a token being typed, not a filter yet.
                continue;
            }
            q.tokens.push((c, v.to_string()));
        } else {
            words.push(w);
        }
    }
    q.words = words.join(" ");
    q
}

/// The token under the cursor: the search box ends in a word that starts
/// with a sigil. Its sigil and what is typed after it.
pub fn typing_token<'a>(filter: &'a str, sigils: &[char]) -> Option<(char, &'a str)> {
    if filter.ends_with(char::is_whitespace) {
        return None;
    }
    let last = filter.split_whitespace().last()?;
    let c = last.chars().next()?;
    if !sigils.contains(&c) {
        return None;
    }
    Some((c, &last[c.len_utf8()..]))
}

/// Prefix beats substring. No fuzzy subsequence tier: a haystack that
/// joins a name with its kind/status/session words has common letters
/// enough that a subsequence matches almost everything. Substring is the
/// right strictness for a roster.
pub fn rank(hay: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(4);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if h.starts_with(&n) {
        return Some(0);
    }
    if h.contains(&n) {
        return Some(1);
    }
    None
}

/// Does `target` pass the alternatives typed for one sigil? Empty
/// alternatives pass everything. Prefix by default; `substring` for
/// values like a directory's tail.
pub fn passes(vals: &[&str], target: &str, substring: bool) -> bool {
    if vals.is_empty() {
        return true;
    }
    let t = target.to_lowercase();
    vals.iter().any(|v| {
        let v = v.to_lowercase();
        if substring {
            t.contains(&v)
        } else {
            t.starts_with(&v)
        }
    })
}

/// The dropdown's values for a token being typed: each distinct value
/// with how many rows have it, most rows first, at most
/// [`COMPLETIONS_MAX`]. `partial` is matched by prefix, or by substring
/// when `substring` is set. The one value left that is exactly what was
/// typed is nothing to offer, so the list is then empty.
pub fn complete_values<I>(values: I, partial: &str, substring: bool) -> Vec<(String, usize)>
where
    I: IntoIterator<Item = String>,
{
    let partial = partial.to_lowercase();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in values {
        if v.is_empty() {
            continue;
        }
        let lv = v.to_lowercase();
        let ok = if substring { lv.contains(&partial) } else { lv.starts_with(&partial) };
        if ok {
            *counts.entry(v).or_insert(0) += 1;
        }
    }
    let mut list: Vec<(String, usize)> = counts.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list.truncate(COMPLETIONS_MAX);
    if list.len() == 1 && list[0].0.to_lowercase() == partial {
        list.clear();
    }
    list
}

/// Replace the token being typed with `value`, and a space so the next
/// word starts fresh. Nothing happens when no token is being typed.
pub fn replace_typing_token(filter: &mut String, sigils: &[char], value: &str) -> bool {
    let Some((sigil, plen)) = typing_token(filter, sigils).map(|(s, t)| (s, t.len())) else {
        return false;
    };
    let cut = filter.len() - sigil.len_utf8() - plen;
    filter.truncate(cut);
    filter.push(sigil);
    filter.push_str(value);
    filter.push(' ');
    true
}

/// Add the word to the box, or take it out if it is there (case-
/// insensitively). Returns whether it was removed. The box ends in a
/// space afterwards, so typing continues with a fresh word.
pub fn toggle_word(filter: &mut String, tok: &str) -> bool {
    let mut words: Vec<String> = filter.split_whitespace().map(str::to_string).collect();
    let before = words.len();
    words.retain(|w| !w.eq_ignore_ascii_case(tok));
    let removed = words.len() != before;
    if !removed {
        words.push(tok.to_string());
    }
    *filter = words.join(" ");
    if !filter.is_empty() {
        filter.push(' ');
    }
    removed
}

/// The column the dropdown opens at: under the token being typed, when
/// the box's text starts at `text_col` (1-based).
pub fn dropdown_col(filter: &str, partial: &str, text_col: usize) -> usize {
    text_col + filter.chars().count() - 1 - partial.chars().count()
}

#[cfg(test)]
mod query_tests {
    use super::*;

    const SIGILS: [char; 3] = ['@', '#', '~'];

    #[test]
    fn tokens_and_words() {
        let q = parse_query("dflash @dm #tmux2 ~cfb bench", &SIGILS);
        assert_eq!(q.words, "dflash bench");
        assert_eq!(q.values('@'), vec!["dm"]);
        assert_eq!(q.values('#'), vec!["tmux2"]);
        assert_eq!(q.values('~'), vec!["cfb"]);
        assert_eq!(q.tokens(), "@dm #tmux2 ~cfb");
        assert!(q.has_filters());
        // Grouped by sigil however they were typed.
        let q = parse_query("~x @a ~y", &SIGILS);
        assert_eq!(q.tokens(), "@a ~x ~y");
    }

    #[test]
    fn bare_sigil_and_escape() {
        let q = parse_query("@ foo", &SIGILS);
        assert_eq!(q.words, "foo");
        assert!(!q.has_filters());
        let q = parse_query("\\@alpha \\x", &SIGILS);
        assert_eq!(q.words, "@alpha \\x");
        assert!(!q.has_filters());
    }

    #[test]
    fn typing() {
        assert_eq!(typing_token("foo #al", &SIGILS), Some(('#', "al")));
        assert_eq!(typing_token("foo #al ", &SIGILS), None);
        assert_eq!(typing_token("foo \\#al", &SIGILS), None);
        assert_eq!(typing_token("@", &SIGILS), Some(('@', "")));
    }

    #[test]
    fn ranking_and_passing() {
        assert_eq!(rank("alpha beta", ""), Some(4));
        assert_eq!(rank("Alpha beta", "al"), Some(0));
        assert_eq!(rank("alpha beta", "bet"), Some(1));
        assert_eq!(rank("alpha beta", "ab"), None);
        assert!(passes(&[], "anything", false));
        assert!(passes(&["dm"], "dmatrix", false));
        assert!(!passes(&["matrix"], "dmatrix", false));
        assert!(passes(&["matrix"], "dmatrix", true));
    }

    #[test]
    fn completion_halves() {
        let vals = ["dev-8x", "dmatrix", "dmatrix", "local"].map(String::from);
        assert_eq!(
            complete_values(vals.clone(), "d", false),
            vec![("dmatrix".to_string(), 2), ("dev-8x".to_string(), 1)]
        );
        // Exactly what is typed already: nothing to offer.
        assert!(complete_values(vals, "local", false).is_empty());
        let mut f = String::from("foo @dm");
        assert!(replace_typing_token(&mut f, &SIGILS, "dmatrix"));
        assert_eq!(f, "foo @dmatrix ");
        assert!(!replace_typing_token(&mut f, &SIGILS, "x"));
        let mut f = String::from("foo ");
        assert!(!toggle_word(&mut f, "#tmux2"));
        assert_eq!(f, "foo #tmux2 ");
        assert!(toggle_word(&mut f, "#TMUX2"));
        assert_eq!(f, "foo ");
        assert_eq!(dropdown_col("foo #al", "al", 10), 14);
    }
}
