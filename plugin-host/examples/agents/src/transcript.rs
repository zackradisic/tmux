//! The transcript pipeline: from the harness's file to the store and the
//! search index, and back out as hits with snippets.
//!
//! Ingest runs when something happened, never on a timer:
//!
//!   * a status report that leaves `working` (the turn is over, and the
//!     transcript holds all of it);
//!   * the agent ending - its pane dying, or a `done` report - which is
//!     what makes a killed agent searchable: the file outlives the pane;
//!   * the agent being archived (set aside is when you want it findable);
//!   * `init`, for every agent whose transcript may have grown while the
//!     server was down.
//!
//! Every trigger goes through [`request`], which debounces per agent: a
//! burst (Stop and Notification hooks firing together, a pane dying as
//! its `done` lands, a sweep finding what an event already found) is one
//! read, `DEBOUNCE_MS` after the last request. A request that arrives
//! while a read is running queues exactly one more.
//!
//! Each run asks the host for the transcript's lines from the saved
//! cursor (`fs_read_lines`): the host scans the file on the fs worker,
//! keeps only the records whose head holds one of the extractor's
//! needles, and lands those - about one per cent of the bytes - in guest
//! memory with their offsets. The guest parses them and stores the turns
//! and the new cursor in one transaction. Only complete lines count: the
//! tail of a record still being written is re-read next time. Tool
//! output that runs to megabytes on one line never reaches the guest.
//!
//! The index (see `index.rs`) is fed as turns are stored, one document
//! per user turn. It is snapshotted to the store when an agent ends and
//! every so many documents, and loaded at start with a catch-up over the
//! turns stored since the snapshot.
//!
//! Cost: the scan is the host's, at native memchr speed off the main
//! thread; the guest pays for parsing the records it was handed, about
//! 2 µs per 128 KiB of transcript scanned, and a typical turn-end ingest
//! is one call and one small batch of turns.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

use crate::extract::{self, TurnKind};
use crate::index::{self, Index};
use crate::store::{self, NewTurn, TurnRow};

/// The buffer `fs_read_lines` fills per call: the records that passed
/// the host's prefilter (5-60% of a Claude transcript's bytes - the
/// assistant records carry the tool inputs). One call is one wake of the
/// guest, and the typed parse runs at roughly 0.5-1 GB/s, so 64 KiB
/// bounds a wake to about 0.1 ms. The SDK grows the buffer for a record
/// that does not fit, up to `MAX_LINE`.
pub const LINES_BUF: usize = 64 * 1024;
/// A candidate record longer than this is skipped by the host unparsed,
/// whatever its head said: nothing conversational is that long.
pub const MAX_LINE: usize = 4 * 1024 * 1024;
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
/// A read starts this long after the last request for the agent, so a
/// burst of triggers is one read.
pub const DEBOUNCE_MS: u64 = 500;
/// A read slower than this is logged with its size, so lag can be
/// attributed to a specific transcript.
const SLOW_MS: u64 = 50;

thread_local! {
    static INDEX: RefCell<Option<Index>> = const { RefCell::new(None) };
    /// Agents with an ingest in flight, and the ones asked for again
    /// meanwhile (run once more when the current one finishes).
    static INGESTING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static PENDING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Documents added since the last snapshot.
    static SINCE_SNAPSHOT: RefCell<usize> = const { RefCell::new(0) };
    /// Per agent: the serial of the latest request, and whether any
    /// request in the burst said the agent ended. The task that wakes
    /// with the latest serial does the read; the others do nothing.
    static REQUESTS: RefCell<std::collections::HashMap<String, (u64, bool)>> =
        RefCell::new(std::collections::HashMap::new());
    static SERIAL: RefCell<u64> = const { RefCell::new(0) };
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

/// Ask for the agent's transcript to be read, debounced: the read starts
/// `DEBOUNCE_MS` after the last request for it. `ended` is remembered
/// across the burst, so a pane-gone and a `done` a moment apart end in
/// one read that snapshots the index.
pub fn request(id: String, ended: bool) {
    let serial = SERIAL.with(|s| {
        let mut s = s.borrow_mut();
        *s += 1;
        *s
    });
    REQUESTS.with(|r| {
        let mut r = r.borrow_mut();
        let e = r.entry(id.clone()).or_insert((serial, ended));
        *e = (serial, e.1 || ended);
    });
    spawn(async move {
        if sleep_ms(DEBOUNCE_MS).await.is_err() {
            return;
        }
        let mine = REQUESTS.with(|r| {
            let mut r = r.borrow_mut();
            match r.get(&id) {
                Some(&(s, ended)) if s == serial => {
                    r.remove(&id);
                    Some(ended)
                }
                _ => None,
            }
        });
        if let Some(ended) = mine {
            ingest(&id, ended).await;
        }
    });
}

/// [`request`] for the live agent on `pane`, if it has a transcript.
pub async fn request_pane(pane: u32) {
    if let Ok(Some(a)) = store::live_by_pane(pane as i64).await {
        if a.transcript_path.is_some() {
            request(a.id, false);
        }
    }
}

/// Every agent whose transcript may have grown: the live ones and the
/// recently ended. Sequential, so a start with many agents is one file
/// at a time rather than all at once. First, rows that never had a
/// transcript path - written by a plugin from before the transcript
/// existed - get one found for them, so history from before the upgrade
/// is searchable too.
pub async fn catch_up() {
    backfill().await;
    let now = now_ms() as i64;
    let targets = store::ingest_targets(now, RECENT_MS).await.unwrap_or_default();
    for a in targets {
        ingest(&a.id, !a.live()).await;
    }
}

/// Find transcripts for the Claude rows that have none: one listing of
/// the project directories gives every session id on disk, and a row
/// whose id is among them gets the path and one read. Ended rows are read
/// here and now, one at a time; live ones are read on their next turn.
/// Rows whose transcript Claude has already cleaned up stay as they are.
async fn backfill() {
    let rows = store::without_transcript().await.unwrap_or_default();
    let claude: Vec<&store::Agent> =
        rows.iter().filter(|a| a.kind == "claude" && a.id.starts_with("claude:")).collect();
    if claude.is_empty() {
        return;
    }
    let Some(on_disk) = claude_transcripts().await else { return };
    let mut found = 0usize;
    for a in claude {
        let Some(sid) = a.id.strip_prefix("claude:") else { continue };
        let Some(path) = on_disk.get(sid) else { continue };
        if store::set_transcript(&a.id, path).await.is_err() {
            continue;
        }
        found += 1;
        if !a.live() {
            ingest(&a.id, false).await;
        }
    }
    if found > 0 {
        log(&format!("agents: transcripts found for {found} rows from before the upgrade"));
        snapshot().await;
    }
}

/// Every Claude transcript on disk, by session id: `~/.claude/projects/*/
/// <sid>.jsonl`. One listing per project directory.
async fn claude_transcripts() -> Option<std::collections::HashMap<String, String>> {
    let home = home_dir().ok().filter(|s| !s.is_empty())?;
    let root = format!("{home}/.claude/projects");
    let dirs = fs_list(&root).await.ok()?;
    let dirs: Vec<String> = dirs.iter().map(|e| e.name.to_string()).collect();
    let mut out = std::collections::HashMap::new();
    for d in dirs {
        let dir = format!("{root}/{d}");
        if let Ok(listing) = fs_list(&dir).await {
            for e in listing.iter() {
                if let Some(sid) = e.name.strip_suffix(".jsonl") {
                    out.insert(sid.to_string(), format!("{dir}/{}", e.name));
                }
            }
        }
    }
    Some(out)
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

/// One pass over the transcript from the cursor to EOF. The host does
/// the scan (`fs_read_lines`): only the records whose head holds one of
/// the extractor's needles come back, with their offsets, and the
/// cursor the host consumed to - kept or skipped - is what is stored.
async fn ingest_once(id: &str) -> Result<(), ()> {
    let Ok(Some(a)) = store::by_id(id).await else { return Err(()) };
    let Some(mut ex) = extract::for_kind(&a.kind) else { return Err(()) };
    let Some(mut path) = a.transcript_path.clone() else { return Err(()) };
    let started = now_ms();
    let start_cursor = a.transcript_cursor.max(0) as u64;
    let (mut calls, mut turns_stored) = (0u32, 0usize);
    let mut cursor = start_cursor;
    let mut seq = store::next_seq(id).await.map_err(|_| ())?;
    let mut grouper = Grouper::default();
    let mut first_read = true;
    let needles: Vec<LineNeedle<'static>> = ex
        .needles()
        .into_iter()
        .map(|(bytes, reject)| LineNeedle { bytes, reject })
        .collect();
    loop {
        let lines = match fs_read_lines(&path, cursor, &needles, extract::HEAD, MAX_LINE, LINES_BUF).await
        {
            Ok(l) => l,
            Err(e) => {
                // A derived path that does not exist yet (or a wrong
                // guess): for a fresh Claude transcript, look for the
                // session id under every project directory once.
                if first_read && cursor == 0 && a.kind == "claude" {
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
        calls += 1;
        if lines.cursor <= cursor {
            // Nothing consumed: a partial record at the end, or nothing new.
            break;
        }
        let mut fed = extract::Fed::default();
        for (off, record) in lines.iter() {
            ex.feed_line(record, off, &mut fed);
        }
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
        match store::insert_turns(id, &rows, lines.cursor as i64, fed.version.as_deref()).await {
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
                turns_stored += n;
            }
            Err(e) => {
                log(&format!("agents: transcript {id}: store: {}", e.message));
                return Err(());
            }
        }
        cursor = lines.cursor;
        if lines.eof {
            break;
        }
    }
    let added = with_index(|ix| grouper.flush(ix));
    SINCE_SNAPSHOT.with(|c| *c.borrow_mut() += added);
    let took = now_ms().saturating_sub(started);
    if took >= SLOW_MS || calls > 2 {
        log(&format!(
            "agents: transcript {id}: {} KB in {calls} calls, {turns_stored} turns, {took} ms",
            cursor.saturating_sub(start_cursor) / 1024
        ));
    }
    Ok(())
}

/// Find a Claude transcript by session id under every project directory:
/// the fallback when the path derived from `cwd` was wrong. Lists a few
/// dozen directories once; not a per-render cost.
async fn locate_claude(id: &str) -> Option<String> {
    let sid = id.strip_prefix("claude:")?;
    claude_transcripts().await?.remove(sid)
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
