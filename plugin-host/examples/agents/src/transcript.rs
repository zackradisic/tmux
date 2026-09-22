//! The transcript pipeline: from the harness's file to the store and the
//! search index, and back out as hits with snippets.
//!
//! Ingest runs when something happened, never on a timer:
//!
//!   * a status report that leaves `working` (the turn is over, and the
//!     transcript holds all of it);
//!   * the agent ending - its pane dying, or a `done` report - which is
//!     what makes a killed agent searchable: the file outlives the pane;
//!   * `init`, for every agent whose transcript may have grown while the
//!     server was down.
//!
//! Each run reads the transcript from the saved cursor in 128 KiB chunks
//! (`fs_read`, straight into guest memory from the fs worker), hands the
//! complete lines to the harness's extractor, and stores the turns and
//! the new cursor in one transaction. Only complete lines count: the
//! tail of a record still being written is re-read next time. A line the
//! prefilter rejects is never buffered past one chunk: tool output can
//! run to megabytes on one line, and it is skipped, not carried.
//!
//! The index (see `index.rs`) is fed as turns are stored, one document
//! per user turn. It is snapshotted to the store when an agent ends and
//! every so many documents, and loaded at start with a catch-up over the
//! turns stored since the snapshot.
//!
//! Cost, measured on real transcripts: 1.3 GB/s through the prefilter
//! under wasm, worst chunk 0.8 ms; a typical turn-end ingest reads a few
//! KB and takes microseconds.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

use crate::extract::{self, TurnKind};
use crate::index::{self, Index};
use crate::store::{self, NewTurn, TurnRow};

/// Bytes read per `fs_read`. One chunk is one wake of the guest; the
/// worst chunk seen (a large candidate record) parsed in under a
/// millisecond, inside the budget.
pub const CHUNK: usize = 128 * 1024;
/// A record still without its newline after this many bytes is dropped
/// unparsed, whatever its head said: nothing conversational is that
/// long, and carrying it would hold megabytes for nothing.
pub const MAX_CARRY: usize = 4 * 1024 * 1024;
/// An agent that ended within this long may still have unread tail (the
/// server was down when it died); older ones stopped writing long ago.
pub const RECENT_MS: i64 = 24 * 3600 * 1000;
/// A snapshot is written after this many documents since the last one,
/// so a catch-up at start stays short even if no agent has ended.
const SNAPSHOT_EVERY: usize = 200;
/// Turns loaded per page while catching up. One page is one wake of the
/// guest; 500 turns tokenize in a few milliseconds under wasm.
const CATCH_UP_PAGE: i64 = 500;
/// How many hits a search returns at most.
pub const HITS_MAX: usize = 50;
/// How many turns after the hit's user turn a snippet may look through
/// (the document is the user turn plus the agent's reply; this bounds
/// the window when the reply was long).
pub const DOC_WINDOW: i64 = 64;
/// Snippet width, in characters.
const SNIPPET_W: usize = 100;

thread_local! {
    static INDEX: RefCell<Option<Index>> = const { RefCell::new(None) };
    /// Agents with an ingest in flight, and the ones asked for again
    /// meanwhile (run once more when the current one finishes).
    static INGESTING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static PENDING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Documents added since the last snapshot.
    static SINCE_SNAPSHOT: RefCell<usize> = const { RefCell::new(0) };
}

/// A transcript hit as it travels to a view: the agent, the turn to open
/// the preview on, its score, and one line that shows the match.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscriptHit {
    pub id: String,
    pub seq: i64,
    pub score: f32,
    pub snippet: String,
}

/// Run `f` on the index, creating an empty one if none is loaded yet.
pub fn with_index<R>(f: impl FnOnce(&mut Index) -> R) -> R {
    INDEX.with(|cell| {
        let mut b = cell.borrow_mut();
        let ix = b.get_or_insert_with(Index::new);
        f(ix)
    })
}

/// Search this server's index. Synchronous: this is the keystroke path.
pub fn search(query: &str, limit: usize) -> Vec<index::Hit> {
    with_index(|ix| ix.search(query, limit))
}

/// Search, then fetch one snippet per hit from the stored turns. The
/// document is the user turn at `seq` and the agent's reply after it, so
/// the snippet comes from whichever of those turns holds a term.
pub async fn hits_with_snippets(query: &str, limit: usize) -> Vec<TranscriptHit> {
    let hits = search(query, limit);
    if hits.is_empty() {
        return Vec::new();
    }
    let (terms, _) = index::query_terms(query);
    let windows: Vec<(String, i64, i64)> =
        hits.iter().map(|h| (h.id.clone(), h.seq, h.seq + DOC_WINDOW)).collect();
    let turns = store::turns_windows(&windows).await.unwrap_or_default();
    hits.into_iter()
        .map(|h| {
            let snippet = snippet_for(&turns, &h.id, h.seq, &terms);
            TranscriptHit { id: h.id, seq: h.seq, score: h.score, snippet }
        })
        .collect()
}

/// The snippet for one hit out of the turns fetched for all of them: the
/// first turn of the document (by seq, stopping at the next user turn)
/// that holds a term, else the user turn itself.
pub fn snippet_for(turns: &[TurnRow], id: &str, seq: i64, terms: &[String]) -> String {
    let mut doc = turns.iter().filter(|t| t.id == id && t.seq >= seq).peekable();
    let mut fallback: Option<&TurnRow> = None;
    while let Some(t) = doc.next() {
        if t.seq > seq && t.kind == "user" {
            break;
        }
        if fallback.is_none() {
            fallback = Some(t);
        }
        let lower = t.text.to_lowercase();
        if terms.iter().any(|term| lower.contains(term.as_str())) {
            return index::snippet(&t.text, terms, SNIPPET_W);
        }
    }
    fallback.map(|t| index::snippet(&t.text, terms, SNIPPET_W)).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// index lifecycle
// ---------------------------------------------------------------------------

/// Load the snapshot and catch up on the turns stored since. Runs once,
/// at provider start, before anything is ingested.
pub async fn load() {
    let ix = match store::load_index().await {
        Ok(Some(blob)) => Index::deserialize(&blob).unwrap_or_else(|| {
            log("agents: search index snapshot unreadable; rebuilding");
            Index::new()
        }),
        _ => Index::new(),
    };
    fill(ix).await;
}

/// Throw the index away and build it again from every stored turn: after
/// retention pruned agents (their documents must stop answering), or a
/// snapshot that would not load.
pub async fn rebuild() {
    fill(Index::new()).await;
}

/// Add the turns stored after `ix.max_rowid` to `ix`, page by page, then
/// install it and snapshot it if anything was added.
async fn fill(mut ix: Index) {
    let mut rowid = ix.max_rowid;
    let mut added = 0usize;
    loop {
        let page = match store::turns_after(rowid, CATCH_UP_PAGE).await {
            Ok(p) => p,
            Err(_) => break,
        };
        if page.is_empty() {
            break;
        }
        rowid = page.last().map(|t| t.rowid).unwrap_or(rowid);
        let mut grouper = Grouper::default();
        for t in &page {
            let Some(kind) = TurnKind::parse(&t.kind) else { continue };
            added += grouper.push(&mut ix, &t.id, t.seq, kind, &t.text, t.path.as_deref());
        }
        added += grouper.flush(&mut ix);
        if (page.len() as i64) < CATCH_UP_PAGE {
            break;
        }
    }
    ix.max_rowid = rowid;
    log(&format!(
        "agents: search index: {} docs, {} terms, {} agents, ~{} KB (+{added} caught up)",
        ix.doc_count(),
        ix.term_count(),
        ix.agent_count(),
        ix.approx_bytes() / 1024
    ));
    INDEX.with(|c| *c.borrow_mut() = Some(ix));
    if added > 0 {
        snapshot().await;
    }
}

/// Write the index to the store.
pub async fn snapshot() {
    let (blob, max_rowid) = with_index(|ix| (ix.serialize(), ix.max_rowid));
    if let Err(e) = store::save_index(&blob, max_rowid).await {
        log(&format!("agents: search index snapshot: {}", e.message));
    }
    SINCE_SNAPSHOT.with(|c| *c.borrow_mut() = 0);
}

/// An agent's id moved (provisional to durable): its documents follow.
pub fn on_rename(old: &str, new: &str) {
    with_index(|ix| ix.rename_agent(old, new));
}

/// Groups consecutive turns of one agent into documents: a user turn
/// starts one; the assistant's text and tool lines join the open one.
#[derive(Default)]
struct Grouper {
    cur: Option<(String, i64, String, Vec<String>)>,
}

impl Grouper {
    fn push(&mut self, ix: &mut Index, id: &str, seq: i64, kind: TurnKind, text: &str, path: Option<&str>) -> usize {
        let mut added = 0;
        let open_for_other = self.cur.as_ref().is_some_and(|(cid, ..)| cid != id);
        if kind == TurnKind::User || open_for_other || self.cur.is_none() {
            added += self.flush(ix);
            self.cur = Some((id.to_string(), seq, String::new(), Vec::new()));
        }
        let Some((_, _, prose, paths)) = self.cur.as_mut() else { return added };
        match kind {
            TurnKind::Tool => {
                if let Some(p) = path {
                    paths.push(p.to_string());
                }
                // The tool line's words (the tool name, a command's
                // description) are prose too.
                prose.push('\n');
                prose.push_str(text);
            }
            _ => {
                prose.push('\n');
                prose.push_str(text);
            }
        }
        added
    }

    fn flush(&mut self, ix: &mut Index) -> usize {
        let Some((id, seq, prose, paths)) = self.cur.take() else { return 0 };
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        ix.add_doc(&id, seq, &prose, &refs);
        1
    }
}

// ---------------------------------------------------------------------------
// ingest
// ---------------------------------------------------------------------------

/// Ingest the transcript of the live agent on `pane`, if it has one.
pub async fn ingest_pane(pane: u32) {
    if let Ok(Some(a)) = store::live_by_pane(pane as i64).await {
        ingest(&a.id, false).await;
    }
}

/// Every agent whose transcript may have grown: the live ones and the
/// recently ended. Sequential, so a start with many agents is one file
/// at a time rather than all at once.
pub async fn catch_up() {
    let now = now_ms() as i64;
    let targets = store::ingest_targets(now, RECENT_MS).await.unwrap_or_default();
    for a in targets {
        ingest(&a.id, !a.live()).await;
    }
}

/// Read what the transcript has past the cursor, store the turns, feed
/// the index. `ended` says the agent is done, which is when the index is
/// snapshotted. Re-entrant per agent: a second call while one runs is
/// queued and runs once after it.
pub async fn ingest(id: &str, ended: bool) {
    let first = INGESTING.with(|s| s.borrow_mut().insert(id.to_string()));
    if !first {
        PENDING.with(|s| s.borrow_mut().insert(id.to_string()));
        return;
    }
    let mut ended = ended;
    loop {
        let r = ingest_once(id).await;
        INGESTING.with(|s| s.borrow_mut().remove(id));
        let again = PENDING.with(|s| s.borrow_mut().remove(id));
        if r.is_err() || !again {
            break;
        }
        INGESTING.with(|s| s.borrow_mut().insert(id.to_string()));
        ended = ended || store::by_id(id).await.ok().flatten().is_some_and(|a| !a.live());
    }
    let due = SINCE_SNAPSHOT.with(|c| *c.borrow() >= SNAPSHOT_EVERY);
    if ended || due {
        snapshot().await;
    }
}

/// One pass over the transcript from the cursor to EOF.
async fn ingest_once(id: &str) -> Result<(), ()> {
    let Ok(Some(a)) = store::by_id(id).await else { return Err(()) };
    let Some(mut ex) = extract::for_kind(&a.kind) else { return Err(()) };
    let Some(mut path) = a.transcript_path.clone() else { return Err(()) };
    let mut base = a.transcript_cursor.max(0) as u64;
    let mut seq = store::next_seq(id).await.map_err(|_| ())?;
    let mut carry: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut grouper = Grouper::default();
    let mut pos = base;
    let mut first_read = true;
    loop {
        let (bytes, eof) = match fs_read(&path, pos, CHUNK).await {
            Ok(r) => r,
            Err(e) => {
                // A derived path that does not exist yet (or a wrong
                // guess): for a fresh Claude transcript, look for the
                // session id under every project directory once.
                if first_read && base == 0 && a.kind == "claude" {
                    if let Some(found) = locate_claude(&a.id).await {
                        if found != path {
                            let _ = store::set_transcript(id, &found).await;
                            path = found;
                            first_read = false;
                            continue;
                        }
                    }
                }
                if e.code != tmux_plugin_sdk::abi::ErrorCode::NoSuchObject {
                    log(&format!("agents: transcript {path}: {}", e.message));
                }
                return Err(());
            }
        };
        first_read = false;
        if bytes.is_empty() {
            break;
        }
        pos += bytes.len() as u64;
        let chunk: &[u8] = if skipping {
            // Drop the rest of the over-long line; resume after it.
            match bytes.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    skipping = false;
                    base = pos - (bytes.len() - i - 1) as u64;
                    &bytes[i + 1..]
                }
                None => {
                    if eof {
                        break;
                    }
                    continue;
                }
            }
        } else {
            &bytes[..]
        };
        carry.extend_from_slice(chunk);
        match carry.iter().rposition(|&b| b == b'\n') {
            Some(cut) => {
                let complete = &carry[..=cut];
                let fed = ex.feed(complete, base);
                base += complete.len() as u64;
                let n = fed.turns.len();
                let mut rows: Vec<NewTurn<'_>> = Vec::with_capacity(n);
                for (i, t) in fed.turns.iter().enumerate() {
                    rows.push(NewTurn {
                        seq: seq + i as i64,
                        kind: t.kind.as_str(),
                        ts_ms: t.ts_ms,
                        text: &t.text,
                        path: t.path.as_deref(),
                        offset: t.offset as i64,
                        len: t.len as i64,
                    });
                }
                match store::insert_turns(id, &rows, base as i64, fed.version.as_deref()).await {
                    Ok(max_rowid) => {
                        let added = with_index(|ix| {
                            let mut added = 0;
                            for (i, t) in fed.turns.iter().enumerate() {
                                added += grouper.push(ix, id, seq + i as i64, t.kind, &t.text, t.path.as_deref());
                            }
                            if max_rowid > ix.max_rowid {
                                ix.max_rowid = max_rowid;
                            }
                            added
                        });
                        SINCE_SNAPSHOT.with(|c| *c.borrow_mut() += added);
                        seq += n as i64;
                    }
                    Err(e) => {
                        log(&format!("agents: transcript {id}: store: {}", e.message));
                        return Err(());
                    }
                }
                carry.drain(..=cut);
            }
            None => {
                // No complete line in the carry. A candidate record is
                // kept until its newline arrives (up to MAX_CARRY); a
                // record the prefilter rejects is dropped as soon as it
                // outgrows one chunk.
                let head = &carry[..carry.len().min(extract::HEAD)];
                if carry.len() > MAX_CARRY || (carry.len() > CHUNK && !ex.candidate(head)) {
                    carry.clear();
                    skipping = true;
                }
            }
        }
        if eof {
            break;
        }
    }
    let added = with_index(|ix| grouper.flush(ix));
    SINCE_SNAPSHOT.with(|c| *c.borrow_mut() += added);
    Ok(())
}

/// Find a Claude transcript by session id under every project directory:
/// the fallback when the path derived from `cwd` was wrong. Lists a few
/// dozen directories once; not a per-render cost.
async fn locate_claude(id: &str) -> Option<String> {
    let sid = id.strip_prefix("claude:")?;
    let home = home_dir().ok().filter(|s| !s.is_empty())?;
    let root = format!("{home}/.claude/projects");
    let name = format!("{sid}.jsonl");
    let dirs = fs_list(&root).await.ok()?;
    let dirs: Vec<String> = dirs.iter().map(|e| e.name.to_string()).collect();
    for d in dirs {
        let dir = format!("{root}/{d}");
        if let Ok(listing) = fs_list(&dir).await {
            if listing.iter().any(|e| e.name == name) {
                return Some(format!("{dir}/{name}"));
            }
        }
    }
    None
}

/// The Claude transcript path for a session started in `cwd`: Claude
/// keeps `~/.claude/projects/<cwd with every non-alphanumeric character
/// as '-'>/<session id>.jsonl`.
pub fn claude_transcript_path(home: &str, cwd: &str, sid: &str) -> String {
    let slug: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{home}/.claude/projects/{slug}/{sid}.jsonl")
}

/// Ingest in the background. For call sites that must not wait on a file
/// read (a status report, a pane closing).
pub fn ingest_later(id: String, ended: bool) {
    spawn(async move {
        ingest(&id, ended).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_path_slug() {
        assert_eq!(
            claude_transcript_path("/Users/z", "/Users/z/Code/tmux2", "abc"),
            "/Users/z/.claude/projects/-Users-z-Code-tmux2/abc.jsonl"
        );
        assert_eq!(
            claude_transcript_path("/Users/z", "/Users/z/.config", "abc"),
            "/Users/z/.claude/projects/-Users-z--config/abc.jsonl"
        );
    }

    fn row(id: &str, seq: i64, kind: &str, text: &str) -> TurnRow {
        TurnRow {
            rowid: seq,
            id: id.into(),
            seq,
            kind: kind.into(),
            ts_ms: None,
            text: text.into(),
            path: None,
            offset: 0,
            len: 0,
        }
    }

    #[test]
    fn snippet_prefers_the_matching_turn() {
        let turns = vec![
            row("a", 0, "user", "please run the benchmark"),
            row("a", 1, "tool", "Bash Run the drafter bench"),
            row("a", 2, "assistant", "The DFlash2 numbers are in."),
            row("a", 3, "user", "next question about dflash2"),
        ];
        let terms = vec!["dflash2".to_string()];
        // Stops at the next user turn: seq 3 is another document.
        assert_eq!(snippet_for(&turns, "a", 0, &terms), "The DFlash2 numbers are in.");
        let terms = vec!["nothing".to_string()];
        assert_eq!(snippet_for(&turns, "a", 0, &terms), "please run the benchmark");
    }

    #[test]
    fn grouper_documents() {
        let mut ix = Index::new();
        let mut g = Grouper::default();
        let mut n = 0;
        n += g.push(&mut ix, "a", 0, TurnKind::User, "first prompt", None);
        n += g.push(&mut ix, "a", 1, TurnKind::Tool, "Read x", Some("/x/view.rs"));
        n += g.push(&mut ix, "a", 2, TurnKind::Assistant, "done with view", None);
        n += g.push(&mut ix, "a", 3, TurnKind::User, "second prompt", None);
        // Another agent's turn closes the open document.
        n += g.push(&mut ix, "b", 0, TurnKind::Assistant, "orphan reply", None);
        n += g.flush(&mut ix);
        assert_eq!(n, 3);
        assert_eq!(ix.doc_count(), 3);
        let hits = ix.search("view", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].id.as_str(), hits[0].seq), ("a", 0));
        assert_eq!(ix.search("orphan", 10)[0].id, "b");
    }
}
