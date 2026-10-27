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
//! Each run asks the host for the transcript's conversation from the
//! saved cursor (`transcript_extract`): the host scans the file on the fs
//! worker, parses the records that are conversation, condenses them, and
//! hands back finished turns - a few KB per call. This side stores them
//! and the new cursor in one transaction. Only complete records count:
//! the tail of one still being written is re-read next time. No transcript
//! byte is parsed on the main thread; the formats live in the host
//! (`host/src/transcript.rs`).
//!
//! The search index is fed as turns are stored, one document per user
//! turn, written straight to the on-disk FTS5 table (see
//! `store::add_document`). There is no in-memory index to load or
//! snapshot; a fresh upgrade backfills the table once from the stored
//! turns (`backfill_docs`).
//!
//! Cost on the main thread: decoding the turn block and encoding the
//! store batch, microseconds per call; a turn-end read is one call.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

use crate::index;
use crate::store::{self, NewTurn, TurnRow};

/// The harnesses the host can extract a transcript for (see
/// `transcript_extract` in the ABI). Any other kind has rows and
/// captures as before, no turns.
pub const HARNESSES: &[&str] = &["claude", "codex", "opencode"];

/// What a turn is. Mirrors the store's `kind` column and the ABI's kind
/// byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    User,
    Assistant,
    Tool,
}

impl TurnKind {
    pub fn parse(s: &str) -> Option<TurnKind> {
        match s {
            "user" => Some(TurnKind::User),
            "assistant" => Some(TurnKind::Assistant),
            "tool" => Some(TurnKind::Tool),
            _ => None,
        }
    }

    fn from_abi(k: TranscriptKind) -> TurnKind {
        match k {
            TranscriptKind::User => TurnKind::User,
            TranscriptKind::Assistant => TurnKind::Assistant,
            TranscriptKind::Tool => TurnKind::Tool,
        }
    }
}
/// An agent that ended within this long may still have unread tail (the
/// server was down when it died); older ones stopped writing long ago.
pub const RECENT_MS: i64 = 24 * 3600 * 1000;
/// Turns read per page while backfilling the FTS table from the stored
/// turns. One page is one wake of the guest.
const BACKFILL_PAGE: i64 = 500;
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
    /// Agents with an ingest in flight, and the ones asked for again
    /// meanwhile (run once more when the current one finishes).
    static INGESTING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static PENDING: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
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

/// The best document of one agent for a query: which agent, which turn to
/// open the preview on, and how well it matched.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub seq: i64,
    pub score: f32,
}

/// The FTS5 MATCH expression for a query, or `None` when the query has no
/// usable terms. Each term is a quoted string (so a token can never be
/// read as an FTS operator), the terms AND together, and the last one is
/// a prefix (`"term"*`) while it is still being typed.
fn fts_match(query: &str) -> Option<String> {
    let (terms, prefix) = index::query_terms(query);
    if terms.is_empty() {
        return None;
    }
    let last = terms.len() - 1;
    let mut out = String::new();
    for (i, t) in terms.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push('"');
        // Tokens are alphanumeric runs (see `index::tokenize`): no quote
        // to escape.
        out.push_str(t);
        out.push('"');
        if prefix && i == last {
            out.push('*');
        }
    }
    Some(out)
}

/// Search this server's on-disk index for the best document of each
/// matching agent. One FTS5 query on the host's worker pool.
pub async fn search(query: &str, limit: usize) -> Vec<Hit> {
    let Some(m) = fts_match(query) else { return Vec::new() };
    // FTS5 ranks per document, not per agent. Fetch more raw rows than the
    // agent limit (one chatty agent can own many matching documents), then
    // keep the best document of each agent: the rows arrive best-first, so
    // the first time an id is seen is its top-scoring document.
    let raw_cap = limit.saturating_mul(40).clamp(limit, 4000);
    let rows = store::search_fts(&m, raw_cap).await.unwrap_or_default();
    let mut seen = HashSet::new();
    let mut hits = Vec::with_capacity(limit);
    for (id, seq, score) in rows {
        if seen.insert(id.clone()) {
            hits.push(Hit { id, seq, score });
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// Search, then fetch one snippet per hit from the stored turns. The
/// document is the user turn at `seq` and the agent's reply after it, so
/// the snippet comes from whichever of those turns holds a term.
pub async fn hits_with_snippets(query: &str, limit: usize) -> Vec<TranscriptHit> {
    let hits = search(query, limit).await;
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
// index backfill
// ---------------------------------------------------------------------------

/// A finished document, as the [`Grouper`] hands it over: the agent, the
/// user turn it started at, its prose, and the paths its tools touched.
type Doc = (String, i64, String, Vec<String>);

/// Build the on-disk FTS index from the stored turns, once. Runs at
/// start; a no-op unless `docs` is empty while `turns` is not - i.e. the
/// first start after the v14 upgrade (the old in-memory index left no
/// `docs` behind). Idempotent: `add_document` replaces per `(id, seq)`,
/// so a restart mid-backfill simply resumes.
pub async fn backfill_docs() {
    if !store::docs_is_empty().await.unwrap_or(false) {
        return;
    }
    if !store::turns_any().await.unwrap_or(false) {
        return;
    }
    let mut rowid = 0i64;
    let mut docs = 0usize;
    let mut grouper = Grouper::default();
    loop {
        let page = match store::turns_after(rowid, BACKFILL_PAGE).await {
            Ok(p) => p,
            Err(_) => break,
        };
        if page.is_empty() {
            break;
        }
        rowid = page.last().map(|t| t.rowid).unwrap_or(rowid);
        for t in &page {
            let Some(kind) = TurnKind::parse(&t.kind) else { continue };
            grouper.push(&t.id, t.seq, kind, &t.text, t.path.as_deref());
        }
        docs += write_docs(grouper.drain_done()).await;
        if (page.len() as i64) < BACKFILL_PAGE {
            break;
        }
    }
    docs += write_docs(grouper.take()).await;
    if docs > 0 {
        log(&format!("agents: search index backfilled {docs} documents from stored turns"));
    }
}

/// Write finished documents to the FTS index; returns how many landed.
async fn write_docs(docs: Vec<Doc>) -> usize {
    let mut n = 0;
    for (id, seq, prose, paths) in docs {
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        if store::add_document(&id, seq, &prose, &refs).await.is_ok() {
            n += 1;
        }
    }
    n
}

/// Groups consecutive turns of one agent into documents: a user turn
/// starts one; the assistant's text and tool lines join the open one.
/// Finished documents collect in `done` until drained.
#[derive(Default)]
struct Grouper {
    cur: Option<Doc>,
    done: Vec<Doc>,
}

impl Grouper {
    fn push(&mut self, id: &str, seq: i64, kind: TurnKind, text: &str, path: Option<&str>) {
        let open_for_other = self.cur.as_ref().is_some_and(|(cid, ..)| cid != id);
        if kind == TurnKind::User || open_for_other || self.cur.is_none() {
            self.close();
            self.cur = Some((id.to_string(), seq, String::new(), Vec::new()));
        }
        let Some((_, _, prose, paths)) = self.cur.as_mut() else { return };
        if kind == TurnKind::Tool {
            if let Some(p) = path {
                paths.push(p.to_string());
            }
        }
        // The tool line's words (the tool name, a command's description)
        // are prose too.
        prose.push('\n');
        prose.push_str(text);
    }

    /// Close the open document into `done`.
    fn close(&mut self) {
        if let Some(doc) = self.cur.take() {
            self.done.push(doc);
        }
    }

    /// Take the documents finished so far, leaving the open one pending.
    fn drain_done(&mut self) -> Vec<Doc> {
        std::mem::take(&mut self.done)
    }

    /// Close the open document and take every finished one.
    fn take(&mut self) -> Vec<Doc> {
        self.close();
        std::mem::take(&mut self.done)
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
    backfill_facts().await;
    // After the v14 upgrade, build the FTS index from the stored turns
    // once before anything new is ingested on top.
    backfill_docs().await;
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
    }
}

/// Rows whose transcript was read by a plugin from before the facts
/// (cwd, branch, model) were kept: one read of the transcript's first
/// block gives them - Claude stamps cwd and branch on every record and
/// the model on the first reply - with the turns it returns ignored.
async fn backfill_facts() {
    let rows = store::without_facts().await.unwrap_or_default();
    let mut filled = 0usize;
    for a in rows {
        let Some(path) = a.transcript_path.as_deref() else { continue };
        // opencode's facts (model, directory) come with its messages.
        if !HARNESSES.contains(&a.kind.as_str()) || a.kind == "opencode" {
            continue;
        }
        let Ok(got) = transcript_extract(path, 0, &a.kind).await else { continue };
        if got.cwd.is_none() && got.branch.is_none() && got.model.is_none() {
            continue;
        }
        let facts = store::Facts {
            version: got.version.as_deref(),
            cwd: got.cwd.as_deref(),
            branch: got.branch.as_deref(),
            model: got.model.as_deref(),
        };
        if store::set_facts(&a.id, &facts).await.is_ok() {
            filled += 1;
        }
    }
    if filled > 0 {
        log(&format!("agents: cwd/branch/model found for {filled} rows from before v10"));
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
/// the FTS index. `_ended` is kept for the caller's intent but no longer
/// drives anything (there is no snapshot to write). Re-entrant per agent:
/// a second call while one runs is queued and runs once after it.
pub async fn ingest(id: &str, _ended: bool) {
    let first = INGESTING.with(|s| s.borrow_mut().insert(id.to_string()));
    if !first {
        PENDING.with(|s| s.borrow_mut().insert(id.to_string()));
        return;
    }
    loop {
        let r = ingest_once(id).await;
        INGESTING.with(|s| s.borrow_mut().remove(id));
        let again = PENDING.with(|s| s.borrow_mut().remove(id));
        if r.is_err() || !again {
            break;
        }
        INGESTING.with(|s| s.borrow_mut().insert(id.to_string()));
    }
}

/// One pass over the transcript from the cursor to EOF. The host does
/// the reading, the scan and the parse (`transcript_extract`); what
/// comes back is finished turns and the cursor the host consumed to -
/// kept or skipped - which is what is stored.
async fn ingest_once(id: &str) -> Result<(), ()> {
    let Ok(Some(a)) = store::by_id(id).await else { return Err(()) };
    if !HARNESSES.contains(&a.kind.as_str()) {
        return Err(());
    }
    let Some(mut path) = a.transcript_path.clone() else { return Err(()) };
    let started = now_ms();
    let start_cursor = a.transcript_cursor.max(0) as u64;
    // A read from the very start re-reads the whole transcript. Drop any
    // turns and documents already stored for this agent first, so `seq`
    // restarts at 0 and the re-read replaces rather than duplicates - the
    // bug that let one transcript pile up hundreds of thousands of turns.
    if start_cursor == 0 {
        let _ = store::delete_turns(id).await;
        let _ = store::delete_docs(id).await;
    }
    let (mut calls, mut turns_stored) = (0u32, 0usize);
    let mut cursor = start_cursor;
    let mut seq = store::next_seq(id).await.map_err(|_| ())?;
    let mut grouper = Grouper::default();
    let mut first_read = true;
    loop {
        let got = if a.kind == "opencode" {
            // No file: the messages come out of opencode's database, the
            // cursor a sequence number (see `opencode_extract`).
            match opencode_extract(&a.id, cursor).await {
                Some(g) => g,
                None => return Err(()),
            }
        } else {
            match transcript_extract(&path, cursor, &a.kind).await {
            Ok(g) => g,
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
            }
        };
        first_read = false;
        calls += 1;
        if got.cursor <= cursor {
            // Nothing consumed: a partial record at the end, or nothing new.
            break;
        }
        let n = got.turns.len();
        let mut rows: Vec<NewTurn<'_>> = Vec::with_capacity(n);
        for (i, t) in got.turns.iter().enumerate() {
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
        let facts = store::Facts {
            version: got.version.as_deref(),
            cwd: got.cwd.as_deref(),
            branch: got.branch.as_deref(),
            model: got.model.as_deref(),
        };
        if let Err(e) = store::insert_turns(id, &rows, got.cursor as i64, &facts).await {
            log(&format!("agents: transcript {id}: store: {}", e.message));
            return Err(());
        }
        for (i, t) in got.turns.iter().enumerate() {
            grouper.push(
                id,
                seq + i as i64,
                TurnKind::from_abi(t.kind),
                &t.text,
                t.path.as_deref(),
            );
        }
        write_docs(grouper.drain_done()).await;
        seq += n as i64;
        turns_stored += n;
        cursor = got.cursor;
        if got.eof {
            break;
        }
    }
    write_docs(grouper.take()).await;
    let took = now_ms().saturating_sub(started);
    if took >= SLOW_MS || calls > 2 {
        log(&format!(
            "agents: transcript {id}: {} KB in {calls} calls, {turns_stored} turns, {took} ms",
            cursor.saturating_sub(start_cursor) / 1024
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// opencode: the conversation out of its database
// ---------------------------------------------------------------------------

/// Messages read per sqlite3 run. A session's whole history is a few
/// runs; a turn is one.
const OC_BATCH: usize = 200;

/// The opencode harness has no transcript file: its messages are rows
/// in its database (see `resolve::opencode`), and the cursor is a
/// message sequence number rather than a byte offset. One sqlite3 run
/// reads the messages past the cursor, with the parts the roster shows
/// picked out in SQL - a user message's text; an assistant message's
/// text and tool parts, less each tool's output - so the bulk of the
/// database (tool output, reasoning, attachments) never crosses the
/// pipe.
///
/// A message is written once and then updated in place while it streams
/// (its tool parts land one by one, the text last), so the cursor never
/// passes an assistant message without its `time.completed`: the read
/// stops there and the next one, after the turn's `idle`, takes it
/// whole. A user or idle message is complete when written.
async fn opencode_extract(id: &str, cursor: u64) -> Option<Extracted> {
    let sid = id.strip_prefix("opencode:")?;
    if !sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return None;
    }
    let sql = format!(
        "SELECT seq, type, time_created AS ts, \
            CASE type \
              WHEN 'user' THEN json_extract(data, '$.text') \
              WHEN 'assistant' THEN (SELECT json_group_array(json(json_remove(c.value, \
                  '$.state.output', '$.state.metadata', '$.state.content', '$.state.time'))) \
                FROM json_each(json_extract(data, '$.content')) c \
                WHERE json_extract(c.value, '$.type') IN ('text', 'tool')) \
            END AS body, \
            json_extract(data, '$.model.id') AS model, \
            (type <> 'assistant' OR json_extract(data, '$.time.completed') IS NOT NULL) AS complete \
         FROM session_message WHERE session_id = {} AND seq > {cursor} \
         ORDER BY seq LIMIT {OC_BATCH}",
        crate::resolve::sql_quote(sid)
    );
    let rows = crate::resolve::opencode_query(&sql).await?;
    let mut out = Extracted {
        turns: Vec::new(),
        version: None,
        cwd: None,
        branch: None,
        model: None,
        cursor,
        eof: rows.len() < OC_BATCH,
    };
    for row in &rows {
        if row.get("complete").and_then(|v| v.as_i64()) == Some(0) {
            // Still streaming: nothing past it is consumed either.
            out.eof = true;
            break;
        }
        let seq = row.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
        if seq > out.cursor {
            out.cursor = seq;
        }
        let ts_ms = row.get("ts").and_then(|v| v.as_i64()).filter(|&t| t > 0);
        if let Some(m) = row.get("model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) {
            out.model = Some(m.to_string());
        }
        let body = row.get("body");
        match row.get("type").and_then(|v| v.as_str()) {
            Some("user") => {
                let text = body.and_then(|v| v.as_str()).unwrap_or("").trim();
                if !text.is_empty() {
                    out.turns.push(turn(TranscriptKind::User, text.to_string(), None, ts_ms));
                }
            }
            Some("assistant") => {
                // The parts come back as a JSON text of an array.
                let parts: Vec<serde_json::Value> = body
                    .and_then(|v| v.as_str())
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or_default();
                let mut texts: Vec<&str> = Vec::new();
                let mut tools: Vec<(String, Option<String>, Option<i64>)> = Vec::new();
                for p in &parts {
                    match p.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                                if !t.trim().is_empty() {
                                    texts.push(t);
                                }
                            }
                        }
                        Some("tool") => {
                            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                            let input = p
                                .get("state")
                                .and_then(|s| s.get("input"))
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let at = p
                                .get("time")
                                .and_then(|t| t.get("created"))
                                .and_then(|v| v.as_i64())
                                .filter(|&t| t > 0)
                                .or(ts_ms);
                            let (text, path) = condense_opencode(name, &input);
                            tools.push((text, path, at));
                        }
                        _ => {}
                    }
                }
                if !texts.is_empty() {
                    let text = texts.join("\n").trim().to_string();
                    out.turns.push(turn(TranscriptKind::Assistant, text, None, ts_ms));
                }
                for (text, path, at) in tools {
                    out.turns.push(turn(TranscriptKind::Tool, text, path, at));
                }
            }
            _ => {}
        }
    }
    Some(out)
}

fn turn(kind: TranscriptKind, text: String, path: Option<String>, ts_ms: Option<i64>) -> TranscriptTurn {
    TranscriptTurn { kind, text, path, ts_ms, offset: 0, len: 0 }
}

/// A tool call as one line, the way the host condenses Claude's: the
/// tool and what it touched, an edit's line counts, never the content.
/// opencode's tools are lower-case (`write`, `edit`, `shell`, ...); the
/// line capitalises them so the preview reads the same for every
/// harness.
pub fn condense_opencode(name: &str, input: &serde_json::Value) -> (String, Option<String>) {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty());
    let lines = |k: &str| s(k).map(|v| v.lines().count()).unwrap_or(0);
    let path = s("filePath").or_else(|| s("path")).or_else(|| s("file")).map(str::to_string);
    let shown = path.as_deref().map(oc_short_path).unwrap_or_default();
    let mut cap = String::new();
    let mut chars = name.chars();
    if let Some(c) = chars.next() {
        cap.extend(c.to_uppercase());
        cap.push_str(chars.as_str());
    }
    let line = match name {
        "read" | "list" | "ls" => format!("{cap} {shown}"),
        "glob" | "grep" => format!(
            "{cap} {}{}",
            s("pattern").unwrap_or(""),
            if shown.is_empty() { String::new() } else { format!(" in {shown}") }
        ),
        "write" => format!("{cap} {shown} +{}", lines("content")),
        "edit" => format!("{cap} {shown} +{} −{}", lines("newString"), lines("oldString")),
        "bash" | "shell" => format!(
            "{cap} {}",
            s("description")
                .map(|d| oc_first_line(d, 120))
                .unwrap_or_else(|| oc_first_line(s("command").unwrap_or(""), 120))
        ),
        "task" => format!(
            "{cap} {}: {}",
            oc_first_line(s("description").unwrap_or(""), 60),
            oc_first_line(s("prompt").unwrap_or(""), 120)
        ),
        "webfetch" => format!("{cap} {}", s("url").unwrap_or("")),
        "websearch" => format!("{cap} {}", oc_first_line(s("query").unwrap_or(""), 120)),
        "todowrite" | "todoread" | "question" => cap.clone(),
        _ => {
            let compact = if input.is_null() { String::new() } else { input.to_string() };
            let mut head: String = compact.chars().take(120).collect();
            if compact.chars().count() > 120 {
                head.push('…');
            }
            format!("{cap} {head}")
        }
    };
    (line.trim_end().to_string(), path)
}

/// The last two components of a path: enough to tell files apart on a
/// line, as the host's Claude condenser shows them.
fn oc_short_path(p: &str) -> String {
    let parts: Vec<&str> = p.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn oc_first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
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
    claude_transcript_path_in(&format!("{home}/.claude"), cwd, sid)
}

/// The same under any config home (`CLAUDE_CONFIG_DIR`).
pub fn claude_transcript_path_in(config_home: &str, cwd: &str, sid: &str) -> String {
    let slug: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{config_home}/projects/{slug}/{sid}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_tool_lines() {
        let j = |t: &str| serde_json::from_str::<serde_json::Value>(t).unwrap();
        assert_eq!(
            condense_opencode("write", &j(r#"{"content":"hi\nthere\n","path":"/a/b/work/hello.txt"}"#)),
            ("Write work/hello.txt +2".into(), Some("/a/b/work/hello.txt".into()))
        );
        assert_eq!(
            condense_opencode("edit", &j(r#"{"filePath":"/a/x.rs","oldString":"a\nb","newString":"c"}"#)),
            ("Edit a/x.rs +1 −2".into(), Some("/a/x.rs".into()))
        );
        assert_eq!(
            condense_opencode("shell", &j(r#"{"command":"wc -c hello.txt\nmore","workdir":"/w"}"#)).0,
            "Shell wc -c hello.txt"
        );
        assert_eq!(condense_opencode("bash", &j(r#"{"command":"ls","description":"List files"}"#)).0, "Bash List files");
        assert_eq!(condense_opencode("grep", &j(r#"{"pattern":"foo","path":"/p/q"}"#)).0, "Grep foo in p/q");
        assert_eq!(condense_opencode("read", &j(r#"{"filePath":"/p/q.txt"}"#)).0, "Read p/q.txt");
        assert_eq!(condense_opencode("todowrite", &j(r#"{"todos":[]}"#)).0, "Todowrite");
        assert_eq!(condense_opencode("mystery", &j(r#"{"k":"v"}"#)).0, "Mystery {\"k\":\"v\"}");
        assert_eq!(condense_opencode("mystery", &serde_json::Value::Null).0, "Mystery");
    }

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
        let mut g = Grouper::default();
        g.push("a", 0, TurnKind::User, "first prompt", None);
        g.push("a", 1, TurnKind::Tool, "Read x", Some("/x/view.rs"));
        g.push("a", 2, TurnKind::Assistant, "done with view", None);
        g.push("a", 3, TurnKind::User, "second prompt", None);
        // Another agent's turn closes the open document.
        g.push("b", 0, TurnKind::Assistant, "orphan reply", None);
        let docs = g.take();
        assert_eq!(docs.len(), 3);
        // Document starts: agent `a` at seq 0 and 3, agent `b` at seq 0.
        let starts: Vec<(&str, i64)> = docs.iter().map(|(id, seq, ..)| (id.as_str(), *seq)).collect();
        assert_eq!(starts, vec![("a", 0), ("a", 3), ("b", 0)]);
        // The first document carries the tool's path and the prose of all
        // its turns.
        let (_, _, prose, paths) = &docs[0];
        assert_eq!(paths, &vec!["/x/view.rs".to_string()]);
        assert!(prose.contains("first prompt") && prose.contains("done with view"));
    }

    #[test]
    fn fts_match_builds_and_prefixes() {
        // Content words, quoted and AND-ed, last one a prefix while typing.
        assert_eq!(fts_match("the working picker").as_deref(), Some("\"working\" \"picker\"*"));
        // A trailing space completes the last word: no prefix star.
        assert_eq!(fts_match("working picker ").as_deref(), Some("\"working\" \"picker\""));
        // Nothing usable.
        assert_eq!(fts_match("  "), None);
    }
}
