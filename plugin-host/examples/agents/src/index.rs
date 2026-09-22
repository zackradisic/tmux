//! The search index: an inverted index over the conversation, in memory.
//!
//! A document is one user turn - the prompt and everything the agent
//! said and did until the next prompt. Terms come from the prose and,
//! at half weight, from the paths the tools touched, so "the agent that
//! edited view.rs" finds the session through the edit, not only through
//! whoever happened to type the file name. A query is scored with bm25
//! and answered with the best turn of each agent, so a hit says not just
//! which agent but where in its conversation to open the preview.
//!
//! It lives here rather than in SQLite because a search box wants an
//! answer inside the keystroke: a lookup in a map and a scan over a few
//! hundred postings is microseconds, and it needs no host call, no
//! worker-thread hop and no reply to wait for. FTS5 through `db_query`
//! measured 1-6 ms at a few thousand sessions, plus the round trip; this
//! measured 20-500 microseconds on the same corpus.
//!
//! Persistence is one snapshot blob in the store (see `transcript`),
//! rewritten when an agent ends: postings delta-encoded as varints, about
//! 1-2 KB per session. On start the snapshot is loaded and the turns
//! added since it was taken (by rowid) are indexed on top. There are no
//! segments and no merges: at the sizes the roster sees, a full rebuild
//! from the stored turns is cheap, so the snapshot is an optimisation,
//! not the source of truth.

use std::collections::{BTreeMap, HashMap};

/// The best turn of one agent for a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: String,
    /// The `seq` of the user turn the document started with.
    pub seq: i64,
    pub score: f32,
}

#[derive(Clone, Copy, Debug)]
struct Posting {
    doc: u32,
    /// Occurrences in the prose, saturating.
    tf: u8,
    /// Occurrences in a tool path, saturating.
    tf_path: u8,
}

#[derive(Clone, Copy, Debug)]
struct Doc {
    agent: u32,
    seq: i64,
    /// Tokens in the document, for length normalisation.
    len: u32,
}

#[derive(Default)]
pub struct Index {
    dict: BTreeMap<String, u32>,
    postings: Vec<Vec<Posting>>,
    docs: Vec<Doc>,
    agents: Vec<String>,
    agent_of: HashMap<String, u32>,
    /// Tombstones: an agent whose id was merged into another's. Its
    /// postings stay until the next rebuild; a query skips it.
    removed: Vec<bool>,
    total_len: u64,
    /// The store rowid of the last turn this index holds, so a load can
    /// pick up where the snapshot stopped.
    pub max_rowid: i64,
    /// Scratch for a query: one score per document. Kept between queries
    /// so a keystroke never allocates for it.
    score: Vec<f32>,
}

const K1: f32 = 1.2;
const B: f32 = 0.75;
/// A path token counts this much of a prose token.
const PATH_WEIGHT: f32 = 0.5;
/// How many dictionary terms the last, half-typed word may expand to.
const PREFIX_MAX: usize = 32;
const MIN_TOKEN: usize = 2;
const MAX_TOKEN: usize = 48;

/// Words that carry no signal on their own. Dropped from a query unless
/// they are all of it; never dropped from a document (they cost little
/// and a phrase check in the guest may want them).
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
/// same rule for documents and queries, so they always agree.
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
/// is still being typed and should match dictionary terms it begins.
pub fn query_terms(query: &str) -> (Vec<String>, bool) {
    let all: Vec<String> = tokenize(query).collect();
    let kept: Vec<String> = all.iter().filter(|t| !is_stop(t)).cloned().collect();
    let terms = if kept.is_empty() { all } else { kept };
    let prefix = !query.ends_with(char::is_whitespace);
    (terms, prefix)
}

impl Index {
    pub fn new() -> Index {
        Index::default()
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn term_count(&self) -> usize {
        self.dict.len()
    }

    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// A rough size, for a log line: bytes of postings plus dictionary.
    pub fn approx_bytes(&self) -> usize {
        self.postings.iter().map(|p| p.len() * 6).sum::<usize>()
            + self.dict.keys().map(|k| k.len() + 8).sum::<usize>()
            + self.docs.len() * 16
    }

    fn agent_idx(&mut self, id: &str) -> u32 {
        if let Some(&i) = self.agent_of.get(id) {
            return i;
        }
        let i = self.agents.len() as u32;
        self.agents.push(id.to_string());
        self.removed.push(false);
        self.agent_of.insert(id.to_string(), i);
        i
    }

    fn term_id(&mut self, term: &str) -> u32 {
        if let Some(&t) = self.dict.get(term) {
            return t;
        }
        let t = self.postings.len() as u32;
        self.postings.push(Vec::new());
        self.dict.insert(term.to_string(), t);
        t
    }

    /// Add one document: the prose of a user turn and the paths its tools
    /// touched. Returns the document id. An agent that was tombstoned
    /// comes back alive when it gains a document (a resumed session).
    pub fn add_doc(&mut self, agent_id: &str, seq: i64, prose: &str, paths: &[&str]) -> u32 {
        let agent = self.agent_idx(agent_id);
        self.removed[agent as usize] = false;
        let doc = self.docs.len() as u32;
        let mut tf: HashMap<u32, (u8, u8)> = HashMap::new();
        let mut len = 0u32;
        for tok in tokenize(prose) {
            let t = self.term_id(&tok);
            let e = tf.entry(t).or_insert((0, 0));
            e.0 = e.0.saturating_add(1);
            len += 1;
        }
        for p in paths {
            for tok in tokenize(p) {
                let t = self.term_id(&tok);
                let e = tf.entry(t).or_insert((0, 0));
                e.1 = e.1.saturating_add(1);
                len += 1;
            }
        }
        for (t, (f, fp)) in tf {
            self.postings[t as usize].push(Posting { doc, tf: f, tf_path: fp });
        }
        self.docs.push(Doc { agent, seq, len });
        self.total_len += len as u64;
        doc
    }

    /// The agent's id was migrated (provisional to durable): its
    /// documents follow.
    pub fn rename_agent(&mut self, old: &str, new: &str) {
        let Some(i) = self.agent_of.remove(old) else { return };
        if let Some(&j) = self.agent_of.get(new) {
            // Both exist (a merge): point old's docs at new.
            for d in &mut self.docs {
                if d.agent == i {
                    d.agent = j;
                }
            }
            self.removed[i as usize] = true;
            return;
        }
        self.agents[i as usize] = new.to_string();
        self.agent_of.insert(new.to_string(), i);
    }

    /// The best document per agent for `query`, best first, at most
    /// `limit` agents. Empty for a query with no usable terms.
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<Hit> {
        let (terms, prefix) = query_terms(query);
        if terms.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        // Expand the last term to what it begins, when it is still being
        // typed. An exact term that exists keeps its own posting list too.
        let mut term_ids: Vec<u32> = Vec::new();
        let last = terms.len() - 1;
        for (i, t) in terms.iter().enumerate() {
            if let Some(&id) = self.dict.get(t) {
                term_ids.push(id);
            }
            if prefix && i == last && t.len() >= MIN_TOKEN {
                let mut n = 0;
                for (k, &id) in self.dict.range(t.clone()..) {
                    if !k.starts_with(t.as_str()) || n >= PREFIX_MAX {
                        break;
                    }
                    if k != t {
                        term_ids.push(id);
                        n += 1;
                    }
                }
            }
        }
        if term_ids.is_empty() {
            return Vec::new();
        }
        if self.score.len() < self.docs.len() {
            self.score.resize(self.docs.len(), 0.0);
        }
        let n_docs = self.docs.len() as f32;
        let avg = (self.total_len as f32 / n_docs).max(1.0);
        let mut touched: Vec<u32> = Vec::new();
        for &t in &term_ids {
            let pl = &self.postings[t as usize];
            let df = pl.len() as f32;
            let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
            for p in pl {
                let d = &self.docs[p.doc as usize];
                if self.removed[d.agent as usize] {
                    continue;
                }
                let f = p.tf as f32 + PATH_WEIGHT * p.tf_path as f32;
                let dl = d.len as f32 / avg;
                let s = idf * (f * (K1 + 1.0)) / (f + K1 * (1.0 - B + B * dl));
                let slot = &mut self.score[p.doc as usize];
                if *slot == 0.0 {
                    touched.push(p.doc);
                }
                *slot += s;
            }
        }
        // Best document per agent.
        let mut best: HashMap<u32, (u32, f32)> = HashMap::new();
        for &d in &touched {
            let sc = self.score[d as usize];
            let a = self.docs[d as usize].agent;
            let e = best.entry(a).or_insert((d, 0.0));
            if sc > e.1 {
                *e = (d, sc);
            }
        }
        for &d in &touched {
            self.score[d as usize] = 0.0;
        }
        let mut hits: Vec<Hit> = best
            .into_iter()
            .map(|(a, (d, sc))| Hit {
                id: self.agents[a as usize].clone(),
                seq: self.docs[d as usize].seq,
                score: sc,
            })
            .collect();
        hits.sort_by(|x, y| y.score.partial_cmp(&x.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);
        hits
    }

    // -----------------------------------------------------------------------
    // snapshot
    // -----------------------------------------------------------------------

    /// Serialize to one blob. Terms in dictionary order, postings as
    /// document deltas; all integers as varints.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.approx_bytes() / 2 + 64);
        out.extend_from_slice(b"AGIX");
        out.push(1);
        put_i64(&mut out, self.max_rowid);
        put_u64(&mut out, self.total_len);
        put_u64(&mut out, self.agents.len() as u64);
        for (i, a) in self.agents.iter().enumerate() {
            put_bytes(&mut out, a.as_bytes());
            out.push(u8::from(self.removed[i]));
        }
        put_u64(&mut out, self.docs.len() as u64);
        for d in &self.docs {
            put_u64(&mut out, d.agent as u64);
            put_i64(&mut out, d.seq);
            put_u64(&mut out, d.len as u64);
        }
        put_u64(&mut out, self.dict.len() as u64);
        for (term, &tid) in &self.dict {
            put_bytes(&mut out, term.as_bytes());
            let pl = &self.postings[tid as usize];
            put_u64(&mut out, pl.len() as u64);
            let mut prev = 0u32;
            for p in pl {
                put_u64(&mut out, (p.doc - prev) as u64);
                out.push(p.tf);
                out.push(p.tf_path);
                prev = p.doc;
            }
        }
        out
    }

    /// The inverse of [`serialize`]. `None` for anything that is not a
    /// snapshot this version wrote.
    pub fn deserialize(buf: &[u8]) -> Option<Index> {
        let mut c = Cursor { buf, pos: 0 };
        if c.take(4)? != b"AGIX" || c.u8()? != 1 {
            return None;
        }
        let mut ix = Index { max_rowid: c.i64()?, total_len: c.u64()?, ..Index::default() };
        let n_agents = c.u64()? as usize;
        for i in 0..n_agents {
            let id = String::from_utf8(c.bytes()?.to_vec()).ok()?;
            let removed = c.u8()? != 0;
            ix.agents.push(id.clone());
            ix.removed.push(removed);
            ix.agent_of.insert(id, i as u32);
        }
        let n_docs = c.u64()? as usize;
        for _ in 0..n_docs {
            let agent = c.u64()? as u32;
            let seq = c.i64()?;
            let len = c.u64()? as u32;
            if agent as usize >= n_agents {
                return None;
            }
            ix.docs.push(Doc { agent, seq, len });
        }
        let n_terms = c.u64()? as usize;
        for _ in 0..n_terms {
            let term = String::from_utf8(c.bytes()?.to_vec()).ok()?;
            let n = c.u64()? as usize;
            let mut pl = Vec::with_capacity(n);
            let mut prev = 0u32;
            for _ in 0..n {
                let doc = prev + c.u64()? as u32;
                let tf = c.u8()?;
                let tf_path = c.u8()?;
                if doc as usize >= n_docs {
                    return None;
                }
                pl.push(Posting { doc, tf, tf_path });
                prev = doc;
            }
            let tid = ix.postings.len() as u32;
            ix.postings.push(pl);
            ix.dict.insert(term, tid);
        }
        Some(ix)
    }
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

// ---------------------------------------------------------------------------
// varints
// ---------------------------------------------------------------------------

fn put_u64(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_i64(out: &mut Vec<u8>, v: i64) {
    put_u64(out, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    fn u64(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Some(v);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    fn i64(&mut self) -> Option<i64> {
        let u = self.u64()?;
        Some(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u64()? as usize;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut ix = Index::new();
        ix.add_doc("claude:a", 0, "benchmark the dflash2 drafter on the h100", &[]);
        ix.add_doc("claude:a", 5, "now compare acceptance length", &[]);
        ix.add_doc("claude:b", 0, "fix the picker preview", &["/x/view.rs"]);
        ix.add_doc("claude:c", 0, "remote attach keeps dropping", &["/x/remote.c"]);
        ix
    }

    #[test]
    fn finds_best_turn_per_agent() {
        let mut ix = sample();
        let hits = ix.search("dflash2", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "claude:a");
        assert_eq!(hits[0].seq, 0);
        let hits = ix.search("acceptance", 10);
        assert_eq!(hits[0].seq, 5);
    }

    #[test]
    fn paths_count_at_half_weight() {
        let mut ix = sample();
        let hits = ix.search("view", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "claude:b");
        // A word in prose outranks the same word in a path.
        let mut ix2 = Index::new();
        ix2.add_doc("p", 0, "remote", &[]);
        ix2.add_doc("q", 0, "other words here", &["/remote"]);
        let hits = ix2.search("remote", 10);
        assert_eq!(hits[0].id, "p");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn stop_words_and_prefix() {
        let mut ix = sample();
        // "the agent that was working on the picker": only "agent", "working", "picker" survive.
        let (terms, prefix) = query_terms("the agent that was working on the picker");
        assert_eq!(terms, vec!["agent", "working", "picker"]);
        assert!(prefix);
        let hits = ix.search("the picker", 10);
        assert_eq!(hits[0].id, "claude:b");
        // Half-typed last word expands.
        let hits = ix.search("dfla", 10);
        assert_eq!(hits.len(), 1);
        // A trailing space means the word is complete: no expansion.
        assert!(ix.search("dfla ", 10).is_empty());
        // All stop words: they are searched as themselves.
        let (terms, _) = query_terms("the");
        assert_eq!(terms, vec!["the"]);
        assert!(!ix.search("the", 10).is_empty());
    }

    #[test]
    fn rename_and_merge() {
        let mut ix = sample();
        ix.rename_agent("claude:b", "claude:bb");
        assert_eq!(ix.search("picker", 10)[0].id, "claude:bb");
        // A merge: the old agent's documents answer under the new id,
        // and the old id stops answering.
        ix.rename_agent("claude:c", "claude:a");
        let hits = ix.search("remote", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "claude:a");
        assert!(ix.search("nothing", 10).is_empty());
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut ix = sample();
        ix.max_rowid = 77;
        // A merged agent's tombstone survives the round trip.
        ix.rename_agent("claude:c", "claude:a");
        let blob = ix.serialize();
        let mut back = Index::deserialize(&blob).expect("deserialize");
        assert_eq!(back.max_rowid, 77);
        assert_eq!(back.doc_count(), 4);
        assert_eq!(back.term_count(), ix.term_count());
        let a = ix.search("dflash2 drafter", 10);
        let b = back.search("dflash2 drafter", 10);
        assert_eq!(a, b);
        assert_eq!(back.search("remote", 10)[0].id, "claude:a");
        assert!(Index::deserialize(b"nope").is_none());
        assert!(Index::deserialize(&blob[..blob.len() - 3]).is_none());
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
