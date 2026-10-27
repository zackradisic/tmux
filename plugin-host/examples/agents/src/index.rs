//! Text helpers for search: tokenizing, stop words, the query-term split
//! and the snippet.
//!
//! The search index itself is no longer here. It used to be a hand-rolled
//! inverted index held in the wasm guest heap; at a few hundred thousand
//! postings it grew past the guest's hard memory ceiling and trapped the
//! plugin. Search now lives on disk, as a SQLite FTS5 table over the
//! stored turns (see `store::search_fts` and `transcript`). What stays
//! here is the small, pure text logic shared by the query path and the
//! snippet: tokenizing a query the same way SQLite's `unicode61` tokenizes
//! the prose, dropping stop words, and clipping a matching line.

const MIN_TOKEN: usize = 2;
const MAX_TOKEN: usize = 48;

/// Words that carry no signal on their own. Dropped from a query unless
/// they are all of it; the stored prose keeps them (FTS indexes it whole).
const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "is", "it", "this",
    "that", "with", "i", "we", "you", "be", "as", "at", "by", "was", "are", "not", "do",
    "does", "can", "my", "me", "our", "what", "how", "which", "who", "one", "from", "so",
    "if", "but", "its", "into", "up", "out", "about", "just", "like", "then", "than",
    "also", "have", "has", "had", "will", "would", "should", "could", "there", "here",
    "they", "them", "their", "he", "she", "his", "her", "am", "been", "being", "did",
    "let", "lets", "please", "want", "need", "make", "get", "go", "ok", "okay", "yes",
    "no", "yeah", "hmm", "some", "any", "all", "more", "most", "other", "such", "when",
    "where", "why", "thing", "things", "something", "use", "using", "used",
];

/// Lowercased runs of alphanumerics, `MIN_TOKEN..=MAX_TOKEN` chars. The
/// rule a query is broken on; the stored prose is tokenized host-side by
/// FTS5's `unicode61`, which splits on the same non-alphanumeric
/// boundaries, so the two agree.
pub fn tokenize(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| {
            let n = t.chars().count();
            n >= MIN_TOKEN && n <= MAX_TOKEN
        })
        .map(|t| t.to_lowercase())
}

fn is_stop(t: &str) -> bool {
    STOP.contains(&t)
}

/// The terms a query is answered with: tokens minus stop words (unless
/// nothing would be left), in order. `prefix` says whether the last term
/// is still being typed and should match terms it begins.
pub fn query_terms(query: &str) -> (Vec<String>, bool) {
    let all: Vec<String> = tokenize(query).collect();
    let kept: Vec<String> = all.iter().filter(|t| !is_stop(t)).cloned().collect();
    let terms = if kept.is_empty() { all } else { kept };
    let prefix = !query.ends_with(char::is_whitespace);
    (terms, prefix)
}

// ---------------------------------------------------------------------------
// snippets
// ---------------------------------------------------------------------------

/// The line of `text` that best shows why it matched `terms`: the first
/// line holding any term, clipped to `width` around the first match.
/// Falls back to the first non-empty line.
pub fn snippet(text: &str, terms: &[String], width: usize) -> String {
    let lower_terms: Vec<&str> = terms.iter().map(|s| s.as_str()).collect();
    let mut best: Option<(&str, usize)> = None;
    for line in text.lines() {
        let l = line.to_lowercase();
        if let Some(pos) = lower_terms.iter().filter_map(|t| l.find(t)).min() {
            best = Some((line, pos));
            break;
        }
    }
    let (line, pos) = match best {
        Some(b) => b,
        None => (text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""), 0),
    };
    let line = line.trim();
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= width {
        return line.to_string();
    }
    // Centre the window on the match, in chars.
    let pos_chars = line[..pos.min(line.len())].chars().count();
    let start = pos_chars.saturating_sub(width / 3).min(chars.len().saturating_sub(width));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(chars[start..(start + width).min(chars.len())].iter());
    if start + width < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_words_and_prefix() {
        // "the agent that was working on the picker": only the content
        // words survive, in order.
        let (terms, prefix) = query_terms("the agent that was working on the picker");
        assert_eq!(terms, vec!["agent", "working", "picker"]);
        assert!(prefix);
        // A trailing space means the last word is complete: no prefix.
        let (_, prefix) = query_terms("dfla ");
        assert!(!prefix);
        // All stop words: they are searched as themselves.
        let (terms, _) = query_terms("the");
        assert_eq!(terms, vec!["the"]);
        // One-character runs are dropped; short real words are kept.
        let (terms, _) = query_terms("a x dflash2");
        assert_eq!(terms, vec!["dflash2"]);
    }

    #[test]
    fn snippets() {
        let terms = vec!["dflash2".to_string()];
        assert_eq!(snippet("first line\nthe DFlash2 bench\nlast", &terms, 40), "the DFlash2 bench");
        let long = format!("{} dflash2 {}", "x".repeat(100), "y".repeat(100));
        let s = snippet(&long, &terms, 30);
        assert!(s.contains("dflash2"), "{s}");
        assert!(s.starts_with('…') && s.ends_with('…'));
        assert_eq!(snippet("\n\nplain\n", &[], 40), "plain");
    }
}
