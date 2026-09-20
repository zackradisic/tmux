//! A completion list for one text field.
//!
//! The rows come from a [`Source`]: the directories under a base, only
//! the repos among them, a repo's existing worktrees plus the directories
//! beside them, a repo's branches, or a fixed word list. A scan is one
//! `fs_list` plus at most one shell job and never forks per entry; the
//! expensive git questions (dirty, worktree count, last commit) are asked
//! afterwards by a probe, and only for the rows on screen.
//!
//! The list itself ([`Completion`]) is plain state: rows, the filtered
//! and ranked `view`, a highlight, a scroll offset. It knows nothing about
//! the form it sits under; the form layer owns the async plumbing
//! (`crate::form::start_scan`) and hands scan results in.

use std::borrow::Cow;
use std::collections::HashMap;

use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

use crate::text::{age, basename, clip_end, quote, rank};

/// The capabilities a plugin needs granted for completion to work: the
/// scans list directories anywhere the user types a path and run one
/// shell job per scan. A plugin without them still builds and runs; the
/// list then says which one is missing (see [`Completion::denied`]).
pub const CAPS: &[&str] = &["run-process", "fs-list", "fs-read-any"];

/// Rows the list shows at once.
pub const LIST_MAX: usize = 8;

/// How many rows a scan will build.
///
/// The limit is the guest's CPU budget, not the host's: listing a huge
/// directory is cheap on the fs worker, but every row allocates inside
/// one budgeted callback. Ten thousand rows measures at about 5ms for
/// the whole scan, with no callback reaching even the 2ms soft warning.
///
/// The cut must therefore happen AFTER ranking, never before. A
/// filesystem returns entries in hash order, so keeping "the first N"
/// would rank an arbitrary subset and confidently name the wrong most
/// recent directory. `scan_dir` ranks the whole listing by time while the
/// names are still borrowed - no allocation - and only then builds rows
/// for the survivors.
pub const SCAN_MAX: usize = 10_000;

/// What a completion row stands for. The kind drives the marker, the
/// sort and whether the row can be accepted at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// A directory that is not a git repo.
    Dir,
    /// A git repo (a `.git` directory or file is present).
    Repo,
    /// A linked worktree that the chosen repo already has.
    Worktree,
    /// A branch in the chosen repo.
    Branch,
    /// An entry of a fixed word list.
    Word,
}

/// One candidate. `value` is what lands in the field; everything else is
/// display only. The `Option` columns are empty until the second probe
/// fills them, so the list can render the moment the first scan lands.
#[derive(Clone, Debug)]
pub struct Row {
    /// `None` means the value is `<base>/<label>`, which is the common
    /// case and the one worth not storing: every directory row under one
    /// base would otherwise hold its own copy of the same prefix - 600 KB
    /// of identical bytes for ten thousand rows - to serve six read sites
    /// that touch at most eight rows. `Some` is for a row whose path is
    /// not under the base: an existing worktree lives wherever git put
    /// it, and a branch row's value is its own name.
    pub value: Option<String>,
    pub label: String,
    /// Second column: the branch for a repo row, the upstream for a
    /// branch row, free text for a word.
    pub meta: String,
    /// Right-hand hint: "exists", "in ../other-tree".
    pub note: String,
    pub dirty: Option<bool>,
    /// Commit time in epoch seconds, rendered as an age.
    pub when: Option<i64>,
    pub trees: Option<u32>,
    pub kind: RowKind,
    /// False rows are drawn dim and cannot be accepted (a branch that is
    /// already checked out in another worktree).
    pub enabled: bool,
}

impl Row {
    /// A row whose value is not derivable from the base.
    pub fn new(value: String, label: String, kind: RowKind) -> Row {
        Row::of(Some(value), label, kind)
    }

    /// A row under the scan's base directory: the value is the base plus
    /// the label, so it is not stored.
    pub fn under_base(label: String, kind: RowKind) -> Row {
        Row::of(None, label, kind)
    }

    fn of(value: Option<String>, label: String, kind: RowKind) -> Row {
        Row {
            value,
            label,
            meta: String::new(),
            note: String::new(),
            dirty: None,
            when: None,
            trees: None,
            kind,
            enabled: true,
        }
    }
}

/// The full path a row stands for: stored when it had to be, derived
/// from the scan's base directory otherwise.
pub fn row_value<'a>(base: &str, row: &'a Row) -> Cow<'a, str> {
    match &row.value {
        Some(v) => Cow::Borrowed(v.as_str()),
        None => Cow::Owned(format!("{}/{}", base.trim_end_matches('/'), row.label)),
    }
}

/// What to scan for one field. Paths may start with `~`; the form layer
/// expands them against the home directory before the scan runs.
#[derive(Clone, Debug)]
pub enum Source {
    /// Every directory under `base`.
    Dirs { base: String },
    /// Only the git repos under `base`.
    Repos { base: String },
    /// The worktrees `repo` already has, then the directories under
    /// `base`: a place to put a new worktree, or an old one to reopen.
    Dests { base: String, repo: String },
    /// The branches of `repo`, most recently committed first.
    Branches { repo: String },
    /// A fixed list, e.g. the commands a field may hold. `words` are
    /// (value, meta) pairs; the meta is the second column.
    Words { title: String, words: Vec<(String, String)> },
}

impl Source {
    /// The cache key. Two focus changes with the same key reuse the rows
    /// already scanned instead of running the probe again.
    pub fn key(&self) -> String {
        match self {
            Source::Dirs { base } => format!("dirs\t{base}"),
            Source::Repos { base } => format!("repos\t{base}"),
            Source::Dests { base, repo } => format!("dests\t{base}\t{repo}"),
            Source::Branches { repo } => format!("branches\t{repo}"),
            Source::Words { title, .. } => format!("words\t{title}"),
        }
    }
}

/// The completion list attached to one field.
#[derive(Debug)]
pub struct Completion {
    /// Which field the rows belong to.
    pub field: usize,
    /// Identifies the scan the rows came from: the base directory for a
    /// path list, `repo` for a branch list. A field edit that leaves the
    /// key unchanged only re-filters; a changed key starts a new scan.
    pub key: String,
    pub rows: Vec<Row>,
    /// Indices into `rows`, filtered by the typed fragment and ranked.
    pub view: Vec<usize>,
    /// Index into `view`. `None` means focus is still in the text.
    pub sel: Option<usize>,
    /// First visible row of `view`.
    pub top: usize,
    /// Esc put the list away. The rows stay cached; the next edit of the
    /// field brings it back. While hidden, C-j/C-k move between fields.
    pub hidden: bool,
    /// What the rule above the rows says. Built with the rows, because
    /// the key alone cannot tell a directory scan from a worktree list.
    pub title: String,
    /// The directory the rows were scanned from, for resolving a row
    /// whose value is derived rather than stored.
    pub base: String,
    pub loading: bool,
    /// The scan was cut off at `SCAN_MAX`.
    pub truncated: bool,
    /// Bumped on every scan and every second probe. A completion that
    /// carries a stale generation is dropped, so results that arrive out
    /// of order cannot overwrite fresher ones.
    pub generation: u64,
    /// Parallel to `rows`: the second probe has been asked for already.
    pub probed: Vec<bool>,
    /// The scan was refused for want of a capability: what to grant.
    /// Shown in the list's rule, so the empty list explains itself.
    pub denied: Option<String>,
}

impl Completion {
    /// A list that is still scanning: the rule says so, the rows come
    /// with [`Completion::install`].
    pub fn pending(field: usize, key: String, generation: u64) -> Completion {
        Completion {
            field,
            key,
            rows: Vec::new(),
            view: Vec::new(),
            sel: None,
            top: 0,
            hidden: false,
            title: String::new(),
            base: String::new(),
            loading: true,
            truncated: false,
            generation,
            probed: Vec::new(),
            denied: None,
        }
    }

    /// Take a finished scan's rows.
    pub fn install(&mut self, scanned: Scanned) {
        self.probed = vec![false; scanned.rows.len()];
        self.rows = scanned.rows;
        self.title = scanned.title;
        self.base = scanned.base;
        self.truncated = scanned.truncated;
        self.denied = scanned.denied;
        self.loading = false;
    }

    /// The full path of row `i`.
    pub fn value_of(&self, i: usize) -> Cow<'_, str> {
        row_value(&self.base, &self.rows[i])
    }

    pub fn selected(&self) -> Option<&Row> {
        let i = self.sel?;
        self.rows.get(*self.view.get(i)?)
    }

    /// The list takes rows on screen: not put away, and either still
    /// scanning, holding at least one match, or with a refusal to show.
    pub fn shown(&self) -> bool {
        !self.hidden && (self.loading || !self.view.is_empty() || self.denied.is_some())
    }

    pub fn height(&self) -> usize {
        self.view.len().min(LIST_MAX)
    }

    /// Keep the selection inside the visible window.
    pub fn scroll_to_selection(&mut self) {
        let h = self.height();
        let Some(sel) = self.sel else { return };
        if h == 0 {
            self.top = 0;
            return;
        }
        if sel < self.top {
            self.top = sel;
        } else if sel >= self.top + h {
            self.top = sel + 1 - h;
        }
    }

    /// Move the highlight down (`+1`) or up (`-1`). From the text, down
    /// enters the list at its first row; up from the first row leaves
    /// it. Returns false when there is no list to move in.
    pub fn step(&mut self, delta: i32) -> bool {
        if self.view.is_empty() {
            return false;
        }
        if delta > 0 {
            self.sel = Some(match self.sel {
                None => 0,
                Some(i) => (i + 1).min(self.view.len() - 1),
            });
        } else {
            self.sel = match self.sel {
                Some(0) | None => None,
                Some(i) => Some(i - 1),
            };
        }
        self.scroll_to_selection();
        true
    }

    /// Re-rank the rows against the typed fragment. Keeps the highlighted
    /// row highlighted when it survives the new filter.
    pub fn refilter(&mut self, frag: &str) {
        let keep = self
            .sel
            .and_then(|i| self.view.get(i).copied())
            .map(|i| self.value_of(i).into_owned());
        // Safe even on a truncated scan: scan_dir ranked the whole listing
        // before cutting, so the rows in hand are genuinely the most recent.
        let mut scored: Vec<(u8, i64, usize)> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(r) = rank(&row.label, frag) {
                // Newest first inside a rank, so the repo you touched last
                // is the one you reach first.
                scored.push((r, -row.when.unwrap_or(0), i));
            }
        }
        // Integers only. The rows arrive from scan_dir already in time
        // order, so a stable sort resolves every tie to that order for
        // free - and a name tiebreak here would be the same 200x cliff as
        // in scan_dir, because with an empty filter every rank is equal
        // and a directory written in one go gives every row the same
        // time. It also keeps `rows[..]` out of the comparator, which is
        // a bounds check and two derefs per comparison.
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        self.view = scored.into_iter().map(|(_, _, i)| i).collect();
        self.sel = keep.and_then(|v| self.view.iter().position(|&i| self.value_of(i) == v));
        self.top = 0;
        self.scroll_to_selection();
    }

    /// The highlighted row's value and kind, if it may be taken. A
    /// disabled row answers with the reason instead.
    pub fn accept(&self) -> Result<Option<(String, RowKind)>, String> {
        let Some(row) = self.selected() else { return Ok(None) };
        if !row.enabled {
            return Err(format!("{} is checked out {}", row.label, row.note));
        }
        Ok(Some((row_value(&self.base, row).into_owned(), row.kind)))
    }

    /// The rows on screen that nobody has asked git about yet, marked as
    /// asked, with the generation the answer must carry. `None` when
    /// there is nothing to do.
    pub fn probe_batch(&mut self) -> Option<(Vec<(usize, String)>, u64)> {
        // The row index travels with the path. Finding it again afterwards
        // would mean walking every row and building its path to compare,
        // which is a string allocation per row per answer.
        let mut paths: Vec<(usize, String)> = Vec::new();
        for vi in self.top..(self.top + self.height()).min(self.view.len()) {
            let ri = self.view[vi];
            if self.probed[ri] {
                continue;
            }
            // Only a repo has anything to ask git about.
            if !matches!(self.rows[ri].kind, RowKind::Repo | RowKind::Worktree) {
                continue;
            }
            self.probed[ri] = true;
            paths.push((ri, self.value_of(ri).into_owned()));
        }
        if paths.is_empty() {
            return None;
        }
        Some((paths, self.generation))
    }

    /// Fold one probe's output back into the rows it asked about.
    pub fn probe_apply(&mut self, batch: &[(usize, String)], output: &str) {
        // Keyed on the batch we asked about, which is one screenful, rather
        // than on every row: the answers come back in the order they were
        // asked, but a lookup does not depend on that staying true.
        let asked: HashMap<&str, usize> =
            batch.iter().map(|(i, path)| (path.as_str(), *i)).collect();
        for line in output.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 6 || f[0] != "P" {
                continue;
            }
            let Some(&i) = asked.get(f[1]) else { continue };
            let Some(row) = self.rows.get_mut(i) else { continue };
            row.dirty = Some(!f[2].trim().is_empty());
            row.trees = f[3].trim().parse().ok();
            row.when = f[4].trim().parse().ok();
            if row.meta.is_empty() {
                row.meta = f[5].trim().to_string();
            }
        }
    }

    /// Draw the list: a rule that names the source, then the visible
    /// rows. `w` is the panel width; `now` the wall clock from the last
    /// scan, for the age column (0 hides it). Nothing is drawn while the
    /// list is not [`shown`](Completion::shown).
    pub fn render(&self, out: &mut String, w: usize, now: i64) {
        if !self.shown() {
            return;
        }
        let title = if self.loading {
            " scanning… ".to_string()
        } else if let Some(why) = &self.denied {
            format!(" {why} ")
        } else if self.truncated {
            format!(" {} of many in {} — keep typing ", self.view.len(), self.title)
        } else {
            format!(" {} of {} in {} ", self.view.len(), self.rows.len(), self.title)
        };
        let title = clip_end(&title, w.saturating_sub(8));
        let rule = "─".repeat(w.saturating_sub(title.chars().count() + 4));
        let colour = if self.denied.is_some() { "\x1b[33m" } else { "\x1b[2m" };
        out.push_str(&format!("  {colour}──{title}{rule}\x1b[0m\r\n"));

        if self.loading {
            return;
        }

        // Columns scale with the panel so a wide float shows more of a
        // long branch name instead of padding.
        let namew = (w * 30 / 100).clamp(10, 30);
        let metaw = (w * 26 / 100).clamp(8, 26);

        for vi in self.top..(self.top + self.height()).min(self.view.len()) {
            let row = &self.rows[self.view[vi]];
            let cur = self.sel == Some(vi);
            let marker = match (cur, row.kind) {
                (true, _) => "▸",
                (false, RowKind::Worktree) => "·",
                _ => " ",
            };
            let dirty = match row.dirty {
                Some(true) => "*",
                _ => " ",
            };
            let when = match row.when {
                Some(t) if now > 0 => age(now, t),
                _ => String::new(),
            };
            let trees = match row.trees {
                Some(n) if n > 1 => format!("{}wt", n - 1),
                _ => String::new(),
            };
            let mut line = format!(
                " {marker} {:<namew$} {:<metaw$} {dirty} {:>4} {:<4}",
                clip_end(&row.label, namew),
                clip_end(&row.meta, metaw),
                when,
                trees,
            );
            if !row.note.is_empty() {
                line.push_str(&format!(" {}", row.note));
            }
            let line = clip_end(line.trim_end(), w.saturating_sub(2));
            if cur {
                out.push_str(&format!("\x1b[7m {line:<pad$}\x1b[0m\r\n", pad = w - 1));
            } else if !row.enabled || row.kind == RowKind::Dir {
                out.push_str(&format!(" \x1b[2m{line}\x1b[0m\r\n"));
            } else {
                out.push_str(&format!(" {line}\r\n"));
            }
        }
    }
}

/// A finished scan, ready for [`Completion::install`].
#[derive(Debug, Default)]
pub struct Scanned {
    pub rows: Vec<Row>,
    /// Wall clock at scan time (epoch seconds), 0 if unknown.
    pub now: i64,
    pub truncated: bool,
    pub title: String,
    /// The directory the rows hang off; empty for a list whose rows all
    /// carry their own value.
    pub base: String,
    pub denied: Option<String>,
}

// ---------------------------------------------------------------------
// Probes
//
// Both probes are one `run_job` each, and neither forks per entry. The
// first is deliberately cheap and best-effort: shell builtins list the
// directory, one `[ -e ]` per entry marks the repos, and a single `awk`
// reads every `.git/HEAD` in one pass. Nothing shells out to git. The
// second probe is the expensive one — it does run git, three times per
// repo — so it runs only for the rows on screen, and only after the
// keyboard goes quiet.
// ---------------------------------------------------------------------

/// Which directories under `base` are repos, and what branch each is on,
/// in one job.
///
/// The `G` loop is shell builtins only (one `[ -e ]` per entry, no fork),
/// and it marks a repo whether `.git` is a directory or, as in a linked
/// worktree, a file. The `B` pass is a single `awk` over every
/// `.git/HEAD`, which is a one-line file, so no `git` process runs at
/// all. A worktree's `.git` is a file, so its branch stays empty here and
/// the second probe fills it in.
fn branches_command(base: &str) -> String {
    let b = quote(base);
    format!(
        "B={b}; \
         printf 'T\\t%s\\n' \"$(date +%s)\"; \
         for g in \"$B\"/*/.git; do [ -e \"$g\" ] && \
           {{ g=${{g%/.git}}; printf 'G\\t%s\\n' \"${{g##*/}}\"; }}; done; \
         awk 'FNR==1{{n=split(FILENAME,p,\"/\"); s=$0; \
           sub(/^ref: refs\\/heads\\//,\"\",s); print \"B\\t\" p[n-2] \"\\t\" s}}' \
           \"$B\"/*/.git/HEAD 2>/dev/null; \
         true"
    )
}

/// Branch list for one repo, plus the worktree that holds each branch.
/// `git worktree add` refuses a branch that is already checked out, so
/// the row is marked and blocked before Enter rather than after.
fn branch_command(repo: &str) -> String {
    format!(
        "printf 'T\\t%s\\n' \"$(date +%s)\"; \
         git -C {} for-each-ref --sort=-committerdate \
           --format='R%09%(refname:short)%09%(committerdate:unix)%09%(worktreepath)' \
           refs/heads 2>/dev/null; \
         true",
        quote(repo)
    )
}

/// The worktrees a repo already has, so `dest` can open one instead of
/// creating another.
fn worktree_command(repo: &str) -> String {
    format!(
        "git -C {} worktree list --porcelain 2>/dev/null | \
         awk '/^worktree /{{print \"W\\t\" substr($0,10)}}'; true",
        quote(repo)
    )
}

/// The expensive probe, for the rows now on screen only: dirty state,
/// linked worktree count, last commit time, and the branch for any repo
/// the cheap scan could not read.
pub fn detail_command(paths: &[String]) -> String {
    let list = paths.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ");
    format!(
        "for d in {list}; do \
           printf 'P\\t%s\\t%s\\t%s\\t%s\\t%s\\n' \"$d\" \
             \"$(git -C \"$d\" status --porcelain 2>/dev/null | head -1)\" \
             \"$(git -C \"$d\" worktree list --porcelain 2>/dev/null | \
                grep -c '^worktree ')\" \
             \"$(git -C \"$d\" log -1 --format=%ct 2>/dev/null)\" \
             \"$(git -C \"$d\" symbolic-ref --short -q HEAD 2>/dev/null || \
                git -C \"$d\" rev-parse --short HEAD 2>/dev/null)\"; \
         done; true"
    )
}

/// The hint for a refused host call, or `None` for any other failure.
fn denied_hint(e: &HostError, cap: &str) -> Option<String> {
    (e.code == ErrorCode::CapDenied).then(|| format!("completion needs the {cap} capability"))
}

/// List `base` through the host and turn it into rows.
///
/// The directory itself never touches a shell: `fs_list` walks it on the
/// fs worker and hands back packed records whose names borrow the
/// listing buffer, each already carrying its `d_type`. One shell job
/// follows, and only to read the `.git/HEAD` of every repo at once.
///
/// `repos_only` drops plain directories instead of showing them dim.
async fn scan_dir(base: &str, repos_only: bool) -> Scanned {
    // Directories only, with their modification times: the host skips
    // everything else before paying a stat for it.
    let opts = ListOpts { mtime: true, dirs_only: true };
    let listing = match fs_list_with(base, opts).await {
        Ok(l) => l,
        Err(e) => {
            return Scanned {
                denied: denied_hint(&e, "fs-list and fs-read-any"),
                base: base.to_string(),
                ..Default::default()
            }
        }
    };

    // Rank first, while the names still borrow the listing buffer and
    // nothing has been allocated, then build rows only for the survivors.
    // Doing it the other way round would either trap on the guest's CPU
    // budget or rank an arbitrary hash-ordered subset.
    let mut ranked: Vec<(i64, &str)> = listing
        .iter()
        .filter(|e| !e.name.starts_with('.'))
        .map(|e| (e.mtime, e.name))
        .collect();
    let overflowed = ranked.len() > SCAN_MAX;
    // A stable sort with no name tiebreak. Equal times are common - a
    // clone, a checkout and an unpacked tarball all stamp many entries
    // the same second - and falling through to a string compare then
    // costs 200x: 433us against 2us over ten thousand entries. Stability
    // keeps equal rows in listing order for free, so every comparison
    // stays a single integer compare.
    ranked.sort_by(|a, b| b.0.cmp(&a.0));
    ranked.truncate(SCAN_MAX);

    let mut rows: Vec<Row> = Vec::with_capacity(ranked.len());
    for (mtime, name) in ranked {
        // No path is built here: it is `base` plus this name, and `base`
        // is the same for every row. row_value derives it for the handful
        // of rows that are ever asked.
        let mut row = Row::under_base(name.to_string(), RowKind::Dir);
        // Enough to rank by on the first frame. The exact commit time
        // replaces it for the rows the second probe reaches.
        row.when = (mtime > 0).then_some(mtime);
        rows.push(row);
    }
    // Only the display is short, and only of the oldest entries: the
    // ranking above saw everything, so the rows kept really are the most
    // recently touched. Filtering searches these.
    let truncated = listing.truncated() || overflowed;

    // One job for every .git/HEAD under base. A directory that answers
    // is a repo; the rest stay plain directories.
    let mut now = 0i64;
    let mut denied = None;
    match run_job(&branches_command(base), None).await {
        Ok(out) => {
            // Index the rows by name first. The walk this replaces was
            // O(rows) per line, and both counts grow with the directory:
            // a scan of /tmp answered 1364 lines against 10000 rows,
            // which is 13.6 million string compares in one callback and
            // a guest that trapped on its CPU budget at 13.77ms. The
            // names are borrowed, so the index costs no allocation per
            // row.
            let index: HashMap<&str, usize> =
                rows.iter().enumerate().map(|(i, r)| (r.label.as_str(), i)).collect();
            // Collected rather than applied as we go: `index` borrows the
            // rows it points into, so the writes wait until the reads are
            // finished with them.
            let mut hits: Vec<(usize, Option<String>)> = Vec::new();
            for line in out.output.lines() {
                let mut it = line.split('\t');
                match it.next() {
                    Some("T") => now = it.next().unwrap_or("").trim().parse().unwrap_or(0),
                    // A .git of any shape: this is a repo, branch unknown
                    // for now (a worktree's .git is a file, not a dir).
                    Some("G") => {
                        let name = it.next().unwrap_or("");
                        if let Some(&i) = index.get(name) {
                            hits.push((i, None));
                        }
                    }
                    Some("B") => {
                        let name = it.next().unwrap_or("");
                        let br = it.next().unwrap_or("").trim();
                        let Some(&i) = index.get(name) else { continue };
                        // A detached HEAD leaves the raw object id; shorten it.
                        let meta =
                            if br.len() == 40 { br[..7].to_string() } else { br.to_string() };
                        hits.push((i, Some(meta)));
                    }
                    _ => {}
                }
            }
            drop(index);
            for (i, meta) in hits {
                rows[i].kind = RowKind::Repo;
                if let Some(meta) = meta {
                    rows[i].meta = meta;
                }
            }
        }
        Err(e) => denied = denied_hint(&e, "run-process"),
    }
    if repos_only {
        rows.retain(|r| r.kind == RowKind::Repo);
    }
    Scanned { rows, now, truncated, title: base.to_string(), base: base.to_string(), denied }
}

/// Parse `for-each-ref` output into branch rows.
fn parse_branches(out: &str) -> (Vec<Row>, i64) {
    let mut rows = Vec::new();
    let mut now = 0i64;
    for line in out.lines() {
        let mut it = line.split('\t');
        match it.next() {
            Some("T") => now = it.next().unwrap_or("").trim().parse().unwrap_or(0),
            Some("R") => {
                let name = it.next().unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let when = it.next().unwrap_or("").trim().parse::<i64>().ok();
                let tree = it.next().unwrap_or("").trim().to_string();
                let mut row = Row::new(name.clone(), name, RowKind::Branch);
                row.when = when;
                if !tree.is_empty() {
                    row.note = format!("in {}", basename(&tree));
                    row.enabled = false;
                }
                rows.push(row);
            }
            _ => {}
        }
    }
    (rows, now)
}

/// Run one source's scan. Paths in the source must already be expanded
/// (no `~`). The result is plain data; install it with
/// [`Completion::install`] after checking it is still wanted.
pub async fn scan(source: Source) -> Scanned {
    match source {
        Source::Dirs { base } => scan_dir(&base, false).await,
        Source::Repos { base } => scan_dir(&base, true).await,
        Source::Dests { base, repo } => {
            let mut rows = Vec::new();
            if !repo.is_empty() {
                if let Ok(out) = run_job(&worktree_command(&repo), None).await {
                    for line in out.output.lines() {
                        if let Some(path) = line.strip_prefix("W\t") {
                            let path = path.trim();
                            if path.is_empty() || path == repo {
                                continue; // the main working tree
                            }
                            let mut row =
                                Row::new(path.to_string(), basename(path), RowKind::Worktree);
                            row.note = "exists".into();
                            rows.push(row);
                        }
                    }
                }
            }
            let worktrees = rows.len();
            let mut dirs = scan_dir(&base, false).await;
            if worktrees > 0 {
                dirs.title = format!("{worktrees} worktrees + {base}");
            }
            for d in dirs.rows.drain(..) {
                if !rows.iter().any(|r: &Row| row_value(&base, r) == row_value(&base, &d)) {
                    rows.push(d);
                }
            }
            dirs.rows = rows;
            dirs
        }
        Source::Branches { repo } => {
            let title = format!("branches in {}", basename(&repo));
            match run_job(&branch_command(&repo), None).await {
                Ok(out) => {
                    let (rows, now) = parse_branches(&out.output);
                    Scanned { rows, now, title, ..Default::default() }
                }
                Err(e) => Scanned {
                    title,
                    denied: denied_hint(&e, "run-process"),
                    ..Default::default()
                },
            }
        }
        Source::Words { title, words } => {
            let rows = words
                .into_iter()
                .map(|(value, meta)| {
                    let mut row = Row::new(value.clone(), value, RowKind::Word);
                    row.meta = meta;
                    row
                })
                .collect();
            Scanned { rows, title, ..Default::default() }
        }
    }
}
