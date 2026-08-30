//! SDK example plugin: create tmux sessions — plain or worktree-backed.
//!
//! One form, two kinds, toggled with `C-t`:
//!
//! - **plain**: a folder and a name; Enter runs `new-session -c folder`
//!   and switches the client there. A missing folder asks for a second
//!   Enter, then is created with `mkdir -p`.
//! - **worktree**: repo, name, dest, branch; Enter runs
//!   `git worktree add` (checkout if the branch exists, else `-b`),
//!   then creates a session rooted in the new worktree and switches.
//!   Picking an existing worktree in `dest` skips the add and opens a
//!   session on it.
//!
//! Every path and branch field completes. The list below the fields
//! fills from a scan of the directory you are typing in, and shows the
//! branch, a dirty marker, the age of the last commit and the number of
//! linked worktrees. `C-j` steps into the list and `C-j`/`C-k` move in
//! it; `Tab` completes the field with the highlighted row (or the first
//! one) and leaves the cursor at the end. `Esc` hides the list; after
//! that `C-j`/`C-k` move between fields, and a second `Esc` closes the
//! form.
//!
//! Wire keys to it in ~/.tmux.conf:
//!
//! ```tmux
//! bind S plugin-command session_creator new        # plain, folder = pane cwd
//! bind W plugin-command session_creator worktree   # repo from pane cwd
//! bind -T choose-tree W plugin-command session_creator worktree
//! bind -T choose-tree S plugin-command session_creator new
//! ```
//!
//! From the chooser (`prefix w` / `prefix s`) the key closes it and
//! delivers a `plugin-command` event whose target is the *highlighted*
//! item (a session row resolves to its active pane); from a plain
//! binding the target is the current pane. The form opens on the window
//! the pressing client is actually looking at. Every field is editable —
//! detected values are just prefills (repo shows ✓ while unchanged, C-u
//! clears a field, C-j/C-k move between fields when no list shows).
//!
//! Load server-scoped with caps `mode`, `run-process`, `run-command`.
//!
//! Build: cargo build -p session_creator --target wasm32-unknown-unknown --release

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tmux_plugin_sdk::executor::{
    cancel as cancel_task, spawn as spawn_task, TaskId,
};
use tmux_plugin_sdk::prelude::*;

/// Form geometry (cells). The height is the closed form; an open list
/// adds a rule and up to `LIST_MAX` rows through `mode_resize`.
const FORM_WIDTH: u32 = 76;
const FORM_HEIGHT: u32 = 12;
const LIST_MAX: usize = 8;

/// How many rows the picker will build.
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
const SCAN_MAX: usize = 10_000;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Worktree,
}

struct Field {
    label: &'static str,
    value: String,
    /// Once the user edits a field it stops mirroring its source.
    touched: bool,
}

fn field(label: &'static str, value: String) -> Field {
    Field { label, value, touched: false }
}

/// What a completion row stands for. The kind drives the marker, the
/// sort and whether the row can be accepted at all.
#[derive(Clone, Copy, PartialEq)]
enum RowKind {
    /// A directory that is not a git repo.
    Dir,
    /// A git repo (a `.git` directory or file is present).
    Repo,
    /// A linked worktree that the chosen repo already has.
    Worktree,
    /// A branch in the chosen repo.
    Branch,
}

/// One candidate. `value` is what lands in the field; everything else is
/// display only. The `Option` columns are empty until the second probe
/// fills them, so the list can render the moment the first scan lands.
#[derive(Clone)]
struct Row {
    /// `None` means the value is `<base>/<label>`, which is the common
    /// case and the one worth not storing: every directory row under one
    /// base would otherwise hold its own copy of the same prefix - 600 KB
    /// of identical bytes for ten thousand rows - to serve six read sites
    /// that touch at most eight rows. `Some` is for a row whose path is
    /// not under the base: an existing worktree lives wherever git put
    /// it, and a branch row's value is its own name.
    value: Option<String>,
    label: String,
    /// Second column: the branch for a repo row, the upstream for a
    /// branch row.
    meta: String,
    /// Right-hand hint: "exists", "in ../other-tree".
    note: String,
    dirty: Option<bool>,
    /// Commit time in epoch seconds, rendered as an age.
    when: Option<i64>,
    trees: Option<u32>,
    kind: RowKind,
    /// False rows are drawn dim and cannot be accepted (a branch that is
    /// already checked out in another worktree).
    enabled: bool,
}

impl Row {
    /// A row whose value is not derivable from the base.
    fn new(value: String, label: String, kind: RowKind) -> Row {
        Row::of(Some(value), label, kind)
    }

    /// A row under the scan's base directory: the value is the base plus
    /// the label, so it is not stored.
    fn under_base(label: String, kind: RowKind) -> Row {
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
fn row_value<'a>(base: &str, row: &'a Row) -> Cow<'a, str> {
    match &row.value {
        Some(v) => Cow::Borrowed(v.as_str()),
        None => Cow::Owned(format!("{}/{}", base.trim_end_matches('/'), row.label)),
    }
}

/// The completion list attached to one field.
struct Picker {
    /// Which field the rows belong to.
    field: usize,
    /// Identifies the scan the rows came from: the base directory for a
    /// path list, `repo` for a branch list. A field edit that leaves the
    /// key unchanged only re-filters; a changed key starts a new scan.
    key: String,
    rows: Vec<Row>,
    /// Indices into `rows`, filtered by the typed fragment and ranked.
    view: Vec<usize>,
    /// Index into `view`. `None` means focus is still in the text.
    sel: Option<usize>,
    /// First visible row of `view`.
    top: usize,
    /// Esc put the list away. The rows stay cached; the next edit of the
    /// field brings it back. While hidden, C-j/C-k move between fields.
    hidden: bool,
    /// What the rule above the rows says. Built with the rows, because
    /// the key alone cannot tell a directory scan from a worktree list.
    title: String,
    /// The directory the rows were scanned from, for resolving a row
    /// whose value is derived rather than stored.
    base: String,
    loading: bool,
    /// The scan was cut off at `SCAN_MAX`.
    truncated: bool,
    /// Bumped on every scan and every second probe. A completion that
    /// carries a stale generation is dropped, so results that arrive out
    /// of order cannot overwrite fresher ones.
    generation: u64,
    /// Parallel to `rows`: the second probe has been asked for already.
    probed: Vec<bool>,
}

impl Picker {
    /// The full path of row `i`.
    fn value_of(&self, i: usize) -> Cow<'_, str> {
        row_value(&self.base, &self.rows[i])
    }

    fn selected(&self) -> Option<&Row> {
        let i = self.sel?;
        self.rows.get(*self.view.get(i)?)
    }

    /// The list takes rows on screen: not put away, and either still
    /// scanning or holding at least one match.
    fn shown(&self) -> bool {
        !self.hidden && (self.loading || !self.view.is_empty())
    }

    fn height(&self) -> usize {
        self.view.len().min(LIST_MAX)
    }

    /// Keep the selection inside the visible window.
    fn scroll_to_selection(&mut self) {
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
}

struct Form {
    mode: ModeId,
    width: u32,
    height: u32,
    /// The size the host last confirmed through `mode-resize`. A resize
    /// is requested only when the wanted size differs from this, so the
    /// request and the event cannot chase each other.
    sized: (u32, u32),
    kind: Kind,
    /// Detected repo root, kept to mark the repo field with ✓ while its
    /// value still matches the detection. The field itself is always
    /// editable; `git worktree add` uses whatever it holds on submit.
    detected: Option<String>,
    /// Pressing client (name, current session) for the final
    /// switch-client; resolved once when the form opens.
    client_name: Option<String>,
    /// The home directory, read once when the form opens, so a `~` in a
    /// path field completes like any other directory.
    client_home: Option<String>,
    /// Plain: folder, name. Worktree: repo, name, dest, branch.
    fields: Vec<Field>,
    focused: usize,
    error: Option<String>,
    /// Plain kind: the folder was missing on the last Enter; the next
    /// Enter creates it with `mkdir -p`. Any edit disarms this.
    confirm_create: bool,
    /// A creation pipeline is in flight; input is ignored until it
    /// resolves (failure re-enables the form, success closes it).
    busy: bool,
    /// The completion list for the focused field, when it has one.
    picker: Option<Picker>,
    /// Wall clock from the last scan, used to render commit ages. The
    /// guest has no clock of its own.
    now: i64,
    /// `dest` holds a worktree that already exists, so submit must not
    /// run `git worktree add`. Any edit of repo or dest clears it.
    reuse: bool,
    /// Bumped whenever a scan is started or invalidated.
    generation: u64,
    /// The scan task, so a newer one can stop it. A scan does real work
    /// after each await - list, rank, build rows - and the staleness
    /// check used to run only at the very end, so typing a path left
    /// several full scans racing and threw away all but the last.
    scan_task: Option<TaskId>,
}

impl Form {
    fn idx(&self, label: &str) -> usize {
        self.fields.iter().position(|f| f.label == label).unwrap()
    }

    fn value(&self, label: &str) -> String {
        self.fields[self.idx(label)].value.trim().to_string()
    }

    /// Mirror rules, per kind: worktree — dest/branch follow name (dest
    /// base from repo); plain — name follows basename(folder). A touched
    /// field stops following.
    fn sync_mirrors(&mut self) {
        match self.kind {
            Kind::Worktree => {
                let name = self.value("name");
                let base = dest_base(&self.value("repo"));
                let di = self.idx("dest");
                if !self.fields[di].touched {
                    self.fields[di].value = format!("{base}{name}");
                }
                let bi = self.idx("branch");
                if !self.fields[bi].touched {
                    self.fields[bi].value = name;
                }
            }
            Kind::Plain => {
                let folder = self.value("folder");
                let ni = self.idx("name");
                if !self.fields[ni].touched {
                    self.fields[ni].value = basename(&folder);
                }
            }
        }
    }

    /// Switch plain ↔ worktree in place, carrying name across and
    /// mapping folder ↔ repo. Returns the folder to run repo detection
    /// on when entering the worktree kind.
    fn toggle(&mut self) -> Option<String> {
        let focus_label = self.fields[self.focused].label;
        let ni = self.idx("name");
        let name = Field {
            label: "name",
            value: self.fields[ni].value.clone(),
            touched: self.fields[ni].touched,
        };
        let detect = match self.kind {
            Kind::Plain => {
                let folder = self.value("folder");
                self.kind = Kind::Worktree;
                self.fields = vec![
                    field("repo", folder.clone()),
                    name,
                    field("dest", String::new()),
                    field("branch", String::new()),
                ];
                (!folder.is_empty()).then_some(folder)
            }
            Kind::Worktree => {
                let repo = self.value("repo");
                self.kind = Kind::Plain;
                self.fields = vec![field("folder", repo), name];
                None
            }
        };
        self.focused = self
            .fields
            .iter()
            .position(|f| f.label == focus_label)
            .unwrap_or_else(|| self.idx("name"));
        self.error = None;
        self.confirm_create = false;
        self.reuse = false;
        self.picker = None;
        self.sync_mirrors();
        detect
    }

    /// The completion source for a field, or `None` for `name`.
    fn source(&self, i: usize) -> Option<Source> {
        match self.fields[i].label {
            "folder" => Some(Source::Dirs { base: scan_base(&self.fields[i].value) }),
            "repo" => Some(Source::Repos { base: scan_base(&self.fields[i].value) }),
            "dest" => {
                let repo = self.value("repo");
                Some(Source::Dests { base: scan_base(&self.fields[i].value), repo })
            }
            "branch" => {
                let repo = self.value("repo");
                (!repo.is_empty()).then_some(Source::Branches { repo })
            }
            _ => None,
        }
    }

    /// The fragment the list filters on: the last path component for a
    /// path field, the whole value for a branch field.
    fn fragment(&self, i: usize) -> String {
        let v = &self.fields[i].value;
        match self.fields[i].label {
            "branch" => v.trim().to_string(),
            _ => match v.rfind('/') {
                Some(p) => v[p + 1..].to_string(),
                None => v.clone(),
            },
        }
    }

    /// Wanted outer size: the closed form, plus a rule and the visible
    /// rows when a list is up.
    fn wanted_size(&self) -> (u32, u32) {
        let extra = match &self.picker {
            Some(p) if p.shown() => 1 + p.height().max(1) as u32,
            _ => 0,
        };
        (self.width, FORM_HEIGHT + extra)
    }
}

/// What to scan for one field.
enum Source {
    Dirs { base: String },
    Repos { base: String },
    Dests { base: String, repo: String },
    Branches { repo: String },
}

impl Source {
    /// The cache key. Two focus changes with the same key reuse the rows
    /// already scanned instead of running the probe again.
    fn key(&self) -> String {
        match self {
            Source::Dirs { base } => format!("dirs\t{base}"),
            Source::Repos { base } => format!("repos\t{base}"),
            Source::Dests { base, repo } => format!("dests\t{base}\t{repo}"),
            Source::Branches { repo } => format!("branches\t{repo}"),
        }
    }
}

/// Last non-empty path component ("" for "/" or "").
fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// The directory a path field completes in: everything up to the last
/// `/`. A value with no `/` completes in the home directory.
fn scan_base(value: &str) -> String {
    let v = value.trim();
    match v.rfind('/') {
        Some(0) => "/".to_string(),
        Some(p) => v[..p].to_string(),
        None => "~".to_string(),
    }
}

/// Sibling scheme: /path/to/repo -> /path/to/repo-worktrees/<name>.
fn dest_base(repo: &str) -> String {
    let repo = repo.trim_end_matches('/');
    if repo.is_empty() {
        return String::new();
    }
    format!("{repo}-worktrees/")
}

/// Single-quote for sh and for tmux command strings.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// tmux session names may not contain '.' or ':'; spaces are legal but
/// unpleasant in targets.
fn session_name(name: &str) -> String {
    name.chars()
        .map(|c| if c == '.' || c == ':' || c == ' ' { '-' } else { c })
        .collect()
}

/// run_command reports parse errors only — a failed command (e.g.
/// "duplicate session") completes as success — so duplicates must be
/// caught before new-session runs.
fn session_exists(name: &str) -> bool {
    list_sessions()
        .map(|sessions| sessions.iter().any(|s| s.name == name))
        .unwrap_or(false)
}

/// Rank a candidate against the typed fragment: 0 is the best match, and
/// `None` rejects the row. Prefix beats substring beats subsequence, so
/// the row you are spelling out stays at the top.
fn rank(hay: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(4);
    }
    if hay.starts_with(needle) {
        return Some(0);
    }
    let h = hay.to_lowercase();
    let n = needle.to_lowercase();
    if h.starts_with(&n) {
        return Some(1);
    }
    if h.contains(&n) {
        return Some(2);
    }
    let mut chars = h.chars();
    if n.chars().all(|c| chars.any(|x| x == c)) {
        return Some(3);
    }
    None
}

/// Short age for a commit time, e.g. `4m`, `2h`, `9d`.
fn age(now: i64, then: i64) -> String {
    let d = (now - then).max(0);
    if d < 90 {
        format!("{d}s")
    } else if d < 5400 {
        format!("{}m", d / 60)
    } else if d < 172800 {
        format!("{}h", d / 3600)
    } else if d < 63072000 {
        format!("{}d", d / 86400)
    } else {
        format!("{}y", d / 31536000)
    }
}

struct Shared {
    form: Option<Form>,
    /// A probe worker is alive. The only thing stopping a burst of key
    /// presses from starting a job each.
    probing: bool,
}

type State = Rc<RefCell<Shared>>;

struct SessionCreator {
    state: State,
}

/// Resolve the git repo for a directory: the main working tree when the
/// directory sits inside a linked worktree (common dir minus "/.git"),
/// so worktrees don't nest inside worktrees.
async fn git_root(dir: &str) -> Option<String> {
    match run_job(
        "git rev-parse --show-toplevel --path-format=absolute --git-common-dir",
        Some(dir),
    )
    .await
    {
        Ok(out) if out.status == 0 => {
            let mut lines = out.output.lines();
            let toplevel = lines.next().unwrap_or("").trim().to_string();
            let common = lines.next().unwrap_or("").trim();
            let root = match common.strip_suffix("/.git") {
                Some(main) if !main.is_empty() => main.to_string(),
                _ => toplevel,
            };
            (!root.is_empty()).then_some(root)
        }
        _ => None,
    }
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
/// The `G` loop is shell builtins only - one `[ -e ]` per entry, no fork
/// - and it marks a repo whether `.git` is a directory or, as in a linked
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
fn detail_command(paths: &[String]) -> String {
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

/// List `base` through the host and turn it into rows.
///
/// The directory itself never touches a shell: `fs_list` walks it on the
/// fs worker and hands back packed records whose names borrow the
/// listing buffer, each already carrying its `d_type`. One shell job
/// follows, and only to read the `.git/HEAD` of every repo at once.
///
/// `repos_only` drops plain directories instead of showing them dim.
async fn scan_dir(base: &str, repos_only: bool) -> (Vec<Row>, i64, bool) {
    // Directories only, with their modification times: the host skips
    // everything else before paying a stat for it.
    let opts = ListOpts { mtime: true, dirs_only: true };
    let Ok(listing) = fs_list_with(base, opts).await else {
        return (Vec::new(), 0, false);
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
        // No path is built here: it is `root` plus this name, and `root`
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
    if let Ok(out) = run_job(&branches_command(base), None).await {
        // Index the rows by name first. The walk this replaces was
        // O(rows) per line, and both counts grow with the directory: a
        // scan of /tmp answered 1364 lines against 10000 rows, which is
        // 13.6 million string compares in one callback and a guest that
        // trapped on its CPU budget at 13.77ms. The names are borrowed,
        // so the index costs no allocation per row.
        let index: HashMap<&str, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.label.as_str(), i))
            .collect();
        // Collected rather than applied as we go: `index` borrows the
        // rows it points into, so the writes wait until the reads are
        // finished with them.
        let mut hits: Vec<(usize, Option<String>)> = Vec::new();
        for line in out.output.lines() {
            let mut it = line.split('\t');
            match it.next() {
                Some("T") => {
                    now = it.next().unwrap_or("").trim().parse().unwrap_or(0)
                }
                // A .git of any shape: this is a repo, branch unknown
                // for now (a worktree's .git is a file, not a directory).
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
                    let meta = if br.len() == 40 {
                        br[..7].to_string()
                    } else {
                        br.to_string()
                    };
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
    if repos_only {
        rows.retain(|r| r.kind == RowKind::Repo);
    }
    (rows, now, truncated)
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

/// Run the scan for a field and install the rows, unless the form moved
/// on while the job was in flight.
async fn scan(state: State, mode: ModeId, generation: u64, field: usize, source: Source) {
    let key = source.key();
    let mut title;
    // The directory the rows hang off, so a row can derive its path
    // instead of storing one. Empty for a branch list, whose rows all
    // carry their own value.
    let mut scan_base = String::new();
    let (rows, now, truncated) = match source {
        Source::Dirs { base } | Source::Repos { base } => {
            let repos_only = key.starts_with("repos\t");
            let base = expand(&base, &state);
            title = base.clone();
            scan_base = base.clone();
            scan_dir(&base, repos_only).await
        }
        Source::Dests { base, repo } => {
            let base = expand(&base, &state);
            title = base.clone();
            scan_base = base.clone();
            let mut rows = Vec::new();
            if !repo.is_empty() {
                if let Ok(out) = run_job(&worktree_command(&repo), None).await {
                    for line in out.output.lines() {
                        if let Some(path) = line.strip_prefix("W\t") {
                            let path = path.trim();
                            if path.is_empty() || path == repo {
                                continue; // the main working tree
                            }
                            let mut row = Row::new(
                                path.to_string(),
                                basename(path),
                                RowKind::Worktree,
                            );
                            row.note = "exists".into();
                            rows.push(row);
                        }
                    }
                }
            }
            let worktrees = rows.len();
            if worktrees > 0 {
                title = format!("{worktrees} worktrees + {base}");
            }
            let (dirs, now, truncated) = scan_dir(&base, false).await;
            for d in dirs {
                if !rows
                    .iter()
                    .any(|r: &Row| row_value(&base, r) == row_value(&base, &d))
                {
                    rows.push(d);
                }
            }
            (rows, now, truncated)
        }
        Source::Branches { repo } => match {
            title = format!("branches in {}", basename(&repo));
            run_job(&branch_command(&repo), None).await
        } {
            Ok(out) => {
                let (r, n) = parse_branches(&out.output);
                (r, n, false)
            }
            Err(_) => (Vec::new(), 0, false),
        },
    };

    let mut st = state.borrow_mut();
    let Some(form) = st.form.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
    if form.generation != generation || form.focused != field {
        return; // a newer scan owns the list
    }
    if now != 0 {
        form.now = now;
    }
    let frag = form.fragment(field);
    let probed = vec![false; rows.len()];
    let picker = Picker {
        field,
        key,
        title,
        base: scan_base,
        rows,
        view: Vec::new(),
        sel: None,
        top: 0,
        hidden: false,
        loading: false,
        truncated,
        generation,
        probed,
    };
    form.picker = Some(picker);
    refilter(form, &frag);
    render(form);
    drop(st);

    // The rows are on screen now, so fill in what git knows about them.
    kick_probe(&state, mode);
}

/// Fill dirty, worktree count, commit age and any missing branch for the
/// rows now on screen. Each row is asked about once for the life of the
/// form.
///
/// Start a probe worker unless one is already running.
///
/// This is the whole rate limit. A key press does not start work; it only
/// makes sure a worker exists. A burst of presses therefore lands inside
/// the job the running worker is already awaiting, and costs nothing.
/// There is no timer, so a single press starts immediately, and the
/// pacing comes from how long git actually takes.
fn kick_probe(state: &State, mode: ModeId) {
    {
        let mut st = state.borrow_mut();
        if st.probing {
            return;
        }
        st.probing = true;
    }
    spawn_task(probe_worker(Rc::clone(state), mode));
}

/// The one worker. It loops until the rows on screen have nothing left to
/// ask about, so anything that scrolled into view while a job was running
/// is picked up by the next turn. "Is there work" is read off the rows
/// themselves rather than tracked in a flag.
async fn probe_worker(state: State, mode: ModeId) {
    loop {
        let Some((batch, generation)) = probe_batch(&state, mode) else {
            break;
        };
        let paths: Vec<String> =
            batch.iter().map(|(_, p)| p.clone()).collect();
        let Ok(out) = run_job(&detail_command(&paths), None).await else {
            break;
        };
        if !probe_apply(&state, mode, generation, &batch, &out.output) {
            break;
        }
    }
    state.borrow_mut().probing = false;
}

/// The rows on screen that nobody has asked git about yet, marked as
/// asked. `None` when there is nothing to do.
fn probe_batch(
    state: &State,
    mode: ModeId,
) -> Option<(Vec<(usize, String)>, u64)> {
    let mut st = state.borrow_mut();
    let form = st.form.as_mut().filter(|f| f.mode.0 == mode.0)?;
    let p = form.picker.as_mut()?;
    // The row index travels with the path. Finding it again afterwards
    // would mean walking every row and building its path to compare,
    // which is a string allocation per row per answer.
    let mut paths: Vec<(usize, String)> = Vec::new();
    for vi in p.top..(p.top + p.height()).min(p.view.len()) {
        let ri = p.view[vi];
        if p.probed[ri] || p.rows[ri].kind == RowKind::Branch {
            continue;
        }
        if p.rows[ri].kind == RowKind::Dir {
            continue; // not a repo; nothing to ask git
        }
        p.probed[ri] = true;
        paths.push((ri, p.value_of(ri).into_owned()));
    }
    (!paths.is_empty()).then(|| (paths, p.generation))
}

/// Fold one probe's output back into the rows. False means the form or
/// the list is gone and the worker should stop.
fn probe_apply(
    state: &State,
    mode: ModeId,
    generation: u64,
    batch: &[(usize, String)],
    output: &str,
) -> bool {
    let mut st = state.borrow_mut();
    let Some(form) = st.form.as_mut().filter(|f| f.mode.0 == mode.0) else {
        return false;
    };
    let Some(p) = form.picker.as_mut() else { return false };
    // A newer scan owns the list now. Its own rows are unprobed, so the
    // next turn of the loop will pick them up; only this result is stale.
    if p.generation != generation {
        return true;
    }
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
        let Some(row) = p.rows.get_mut(i) else { continue };
        row.dirty = Some(!f[2].trim().is_empty());
        row.trees = f[3].trim().parse().ok();
        row.when = f[4].trim().parse().ok();
        if row.meta.is_empty() {
            row.meta = f[5].trim().to_string();
        }
    }
    render(form);
    true
}

/// `~` and `~/x` against the cached home directory.
fn expand(path: &str, state: &State) -> String {
    let home = state
        .borrow()
        .form
        .as_ref()
        .and_then(|f| f.client_home.clone())
        .unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home;
    }
    match path.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => path.to_string(),
    }
}

/// Context gathering + form open, run asynchronously after the
/// plugin-command event. Detected values only prefill (always editable)
/// fields.
async fn open_form(
    state: State,
    kind: Kind,
    target_pane: Option<u64>,
    client: Option<u64>,
) {
    // Folder/repo from the target pane's cwd (highlighted item in the
    // chooser, current pane otherwise).
    let cwd = target_pane
        .and_then(|p| resolve_pane(PaneId(p as u32)).ok())
        .and_then(|p| (!p.cwd.is_empty()).then_some(p.cwd));
    let repo = match kind {
        Kind::Worktree => match &cwd {
            Some(dir) => git_root(dir).await,
            None => None,
        },
        Kind::Plain => None,
    };

    // The form must open where the user is looking: the pressing
    // client's current window (the target may be in another session).
    let client_info = client.and_then(|cid| {
        list_clients().ok()?.iter().find_map(|c| {
            (u64::from(c.id) == cid)
                .then(|| (Some(c.name.clone()), c.session))
        })
    });
    let window = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window);
    let Some(window) = window else {
        log("session_creator: cannot resolve the client's current window");
        let _ = display_message("session_creator: no client to open the form for");
        return;
    };

    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("new session".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("session_creator: mode_open failed: {}", e.message));
            return;
        }
    };

    let fields = match kind {
        Kind::Worktree => vec![
            field("repo", repo.clone().unwrap_or_default()),
            field("name", String::new()),
            field("dest", String::new()),
            field("branch", String::new()),
        ],
        Kind::Plain => vec![
            field("folder", cwd.clone().unwrap_or_default()),
            field("name", String::new()),
        ],
    };

    let mut form = Form {
        mode,
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        sized: (FORM_WIDTH, FORM_HEIGHT),
        kind,
        detected: repo,
        client_name: client_info.and_then(|(name, _)| name),
        // Synchronous, so `~` is expandable before the first scan runs.
        client_home: home_dir().ok().filter(|h| !h.is_empty()),
        // A prefilled first field is usually accepted as-is: start on
        // name (Up selects it to change it). An empty one must be
        // filled first: start there.
        focused: if fields[0].value.is_empty() { 0 } else { 1 },
        fields,
        error: None,
        confirm_create: false,
        busy: false,
        picker: None,
        now: 0,
        reuse: false,
        generation: 0,
        scan_task: None,
    };
    form.sync_mirrors();
    render(&mut form);
    state.borrow_mut().form = Some(form);

    start_scan(&state, mode);
}

/// Start (or reuse) the completion list for the focused field.
fn start_scan(state: &State, mode: ModeId) {
    let (generation, field, source, frag) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.form.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        let field = form.focused;
        let Some(source) = form.source(field) else {
            form.picker = None;
            render(form);
            return;
        };
        let key = source.key();
        let frag = form.fragment(field);
        // Same directory, same rows: filter what is already there.
        if let Some(p) = form.picker.as_mut() {
            if p.key == key {
                p.field = field;
                p.hidden = false;
                refilter(form, &frag);
                render(form);
                drop(st);
                kick_probe(state, mode);
                return;
            }
        }
        form.generation += 1;
        let generation = form.generation;
        // Stop the previous scan before it finishes work nobody wants.
        if let Some(old) = form.scan_task.take() {
            cancel_task(old);
        }
        form.picker = Some(Picker {
            field,
            key,
            title: String::new(),
            base: String::new(),
            rows: Vec::new(),
            view: Vec::new(),
            sel: None,
            top: 0,
            hidden: false,
            loading: true,
            truncated: false,
            generation,
            probed: Vec::new(),
        });
        render(form);
        (generation, field, source, frag)
    };
    let _ = frag;
    let task = spawn_task(scan(Rc::clone(state), mode, generation, field, source));
    if let Some(form) = state.borrow_mut().form.as_mut() {
        form.scan_task = Some(task);
    }
}

/// Re-rank the rows against the typed fragment. Keeps the highlighted
/// row highlighted when it survives the new filter.
fn refilter(form: &mut Form, frag: &str) {
    let Some(p) = form.picker.as_mut() else { return };
    let keep = p.sel.and_then(|i| p.view.get(i).copied()).map(|i| {
        p.value_of(i).into_owned()
    });
    // Safe even on a truncated scan: scan_dir ranked the whole listing
    // before cutting, so the rows in hand are genuinely the most recent.
    let by_time = true;
    let mut scored: Vec<(u8, i64, usize)> = Vec::new();
    for (i, row) in p.rows.iter().enumerate() {
        if let Some(r) = rank(&row.label, frag) {
            // Newest first inside a rank, so the repo you touched last
            // is the one you reach first.
            let when = if by_time { -row.when.unwrap_or(0) } else { 0 };
            scored.push((r, when, i));
        }
    }
    // Integers only. The rows arrive from scan_dir already in time
    // order, so a stable sort resolves every tie to that order for free -
    // and a name tiebreak here would be the same 200x cliff as in
    // scan_dir, because with an empty filter every rank is equal and a
    // directory written in one go gives every row the same time. It also
    // keeps `p.rows[..]` out of the comparator, which is a bounds check
    // and two derefs per comparison.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    p.view = scored.into_iter().map(|(_, _, i)| i).collect();
    p.sel = keep.and_then(|v| {
        p.view.iter().position(|&i| p.value_of(i) == v)
    });
    p.top = 0;
    p.scroll_to_selection();
}

/// Take the highlighted row into its field.
fn accept(form: &mut Form) -> bool {
    let Some(p) = form.picker.as_ref() else { return false };
    let Some(row) = p.selected() else { return false };
    if !row.enabled {
        form.error = Some(format!("{} is checked out {}", row.label, row.note));
        return false;
    }
    let field = p.field;
    let value = row_value(&p.base, row).into_owned();
    let kind = row.kind;
    form.fields[field].value = value;
    form.fields[field].touched = !matches!(form.fields[field].label, "repo" | "folder");
    form.error = None;
    form.confirm_create = false;
    // Picking an existing worktree turns Enter into "open it".
    form.reuse = kind == RowKind::Worktree;
    form.sync_mirrors();
    true
}

fn render(form: &mut Form) {
    // Ask for the size the content wants; the mode-resize event that
    // follows carries the size the window actually allowed.
    let want = form.wanted_size();
    if want != form.sized {
        if mode_resize(form.mode, want.0, want.1).is_ok() {
            form.sized = want;
        }
    }

    let w = form.width as usize;
    let (tag, other) = match form.kind {
        Kind::Plain => ("[plain]", "worktree"),
        Kind::Worktree => ("[worktree]", "plain"),
    };
    let mut out = String::from("\x1b[2J\x1b[H");
    out.push_str(&format!(
        "\x1b[2m{:>width$}\x1b[0m\r\n",
        tag,
        width = w.saturating_sub(2)
    ));
    if form.fields[0].value.trim().is_empty() {
        let what = match form.kind {
            Kind::Plain => "no folder detected — enter one:",
            Kind::Worktree => "no git repo detected — enter one:",
        };
        out.push_str(&format!("  \x1b[33m{what}\x1b[0m\r\n\r\n"));
    } else {
        out.push_str("\r\n\r\n");
    }
    for (i, f) in form.fields.iter().enumerate() {
        let focused = i == form.focused && !form.busy;
        // ✓ while the repo field still holds the detected root.
        let mut mark = if f.label == "repo" && form.detected.as_ref() == Some(&f.value) {
            " \x1b[32m✓\x1b[0m".to_string()
        } else {
            String::new()
        };
        if f.label == "dest" && form.reuse {
            mark = " \x1b[36m(exists)\x1b[0m".to_string();
        }
        let val = clip(&f.value, w.saturating_sub(20));
        if focused {
            // The cursor block sits in the text only while the list has
            // no selection; once you are in the list the field is quiet.
            let in_list = form
                .picker
                .as_ref()
                .is_some_and(|p| p.field == i && p.sel.is_some());
            if in_list {
                out.push_str(&format!(
                    "  \x1b[1m{:<7}\x1b[0m \x1b[4m{val}\x1b[0m{mark}\r\n",
                    f.label
                ));
            } else {
                out.push_str(&format!(
                    "  \x1b[1m{:<7}\x1b[0m \x1b[7m{val}\x1b[27m\x1b[7m \x1b[0m{mark}\r\n",
                    f.label
                ));
            }
        } else {
            out.push_str(&format!("  {:<7} {val}{mark}\r\n", f.label));
        }
    }

    render_list(form, &mut out, w);

    out.push_str("\r\n");
    if let Some(err) = &form.error {
        out.push_str(&format!("  \x1b[31m{}\x1b[0m\r\n", clip(err, w - 4)));
    } else if form.busy {
        let doing = match form.kind {
            Kind::Plain => "creating session...",
            Kind::Worktree if form.reuse => "opening worktree...",
            Kind::Worktree => "creating worktree...",
        };
        out.push_str(&format!("  \x1b[33m{doing}\x1b[0m\r\n"));
    } else if form.confirm_create {
        out.push_str(
            "  \x1b[33mfolder does not exist — Enter again to create it\x1b[0m\r\n",
        );
    } else {
        out.push_str("\r\n");
    }
    let in_list = form.picker.as_ref().is_some_and(|p| p.sel.is_some());
    let listed = form.picker.as_ref().is_some_and(|p| p.shown());
    let hint = if in_list {
        "Tab accept · C-j/C-k move · Enter create · Esc hide list"
    } else if listed {
        "C-t swap · C-j list · Tab accept · Enter create · Esc hide list"
    } else {
        "C-t swap · C-j/C-k field · Enter create · Esc cancel"
    };
    let hint = hint.replacen("swap", other, 1);
    out.push_str(&format!("\r\n  \x1b[2m{hint}\x1b[0m"));
    let _ = mode_write(form.mode, out.as_bytes());
}

/// Draw the completion list under the fields: a rule that names the
/// source, then the visible rows.
fn render_list(form: &Form, out: &mut String, w: usize) {
    let Some(p) = form.picker.as_ref().filter(|p| p.shown()) else { return };

    let title = if p.loading {
        " scanning… ".to_string()
    } else if p.truncated {
        format!(" {} of many in {} — keep typing ", p.view.len(), p.title)
    } else {
        format!(" {} of {} in {} ", p.view.len(), p.rows.len(), p.title)
    };
    let title = clip(&title, w.saturating_sub(8));
    let rule = "─".repeat(w.saturating_sub(title.chars().count() + 4));
    out.push_str(&format!("  \x1b[2m──{title}{rule}\x1b[0m\r\n"));

    if p.loading {
        return;
    }

    // Columns scale with the form so a wide float shows more of a long
    // branch name instead of padding.
    let namew = (w * 30 / 100).clamp(10, 30);
    let metaw = (w * 26 / 100).clamp(8, 26);

    for vi in p.top..(p.top + p.height()).min(p.view.len()) {
        let row = &p.rows[p.view[vi]];
        let cur = p.sel == Some(vi);
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
            Some(t) if form.now > 0 => age(form.now, t),
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

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .rev()
            .take(max.saturating_sub(1))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Like `clip`, but keeps the head — names read left to right, paths do
/// not.
fn clip_end(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Repo detection after a plain → worktree toggle. Applies only if the
/// form still shows the same untouched folder value in the same mode.
async fn detect_repo(state: State, mode: ModeId, folder: String) {
    let Some(root) = git_root(&folder).await else { return };
    let mut st = state.borrow_mut();
    let Some(form) = st.form.as_mut() else { return };
    if form.mode.0 != mode.0 || form.kind != Kind::Worktree {
        return;
    }
    let ri = form.idx("repo");
    if form.fields[ri].value != folder {
        return;
    }
    form.fields[ri].value = root.clone();
    form.detected = Some(root);
    form.sync_mirrors();
    render(form);
}

/// Validate and run the creation pipeline for the current kind. On
/// failure the form stays up with the error rendered; on success the
/// mode closes and the client is switched to the new session.
async fn submit(state: State) {
    let (mode, kind, values, client_name, confirmed, reuse) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.form.as_mut() else { return };
        let mut err = match form.kind {
            Kind::Plain => {
                if form.value("folder").is_empty() {
                    Some("folder is required".to_string())
                } else if form.value("name").is_empty() {
                    Some("name is required".to_string())
                } else {
                    None
                }
            }
            Kind::Worktree => {
                if form.value("repo").is_empty() {
                    Some("repo is required".to_string())
                } else if form.value("name").is_empty() {
                    Some("name is required".to_string())
                } else if form.value("dest").is_empty()
                    || (!form.reuse && form.value("branch").is_empty())
                {
                    Some("dest and branch are required".to_string())
                } else {
                    None
                }
            }
        };
        if err.is_none() {
            let sess = session_name(&form.value("name"));
            if session_exists(&sess) {
                err = Some(format!("session '{sess}' already exists"));
            }
        }
        if let Some(err) = err {
            form.error = Some(err);
            render(form);
            return;
        }
        let values: Vec<String> =
            form.fields.iter().map(|f| f.value.trim().to_string()).collect();
        let confirmed = form.confirm_create;
        let reuse = form.reuse;
        form.busy = true;
        form.error = None;
        form.picker = None;
        render(form);
        (form.mode, form.kind, values, form.client_name.clone(), confirmed, reuse)
    };

    let (dir, name) = match kind {
        Kind::Plain => {
            let folder = values[0].clone();
            let name = values[1].clone();
            let exists = run_job(&format!("test -d {}", quote(&folder)), None)
                .await
                .map(|o| o.status == 0)
                .unwrap_or(false);
            if !exists {
                if !confirmed {
                    // Ask for one more Enter before creating the folder.
                    let mut st = state.borrow_mut();
                    if let Some(form) =
                        st.form.as_mut().filter(|f| f.mode.0 == mode.0)
                    {
                        form.busy = false;
                        form.confirm_create = true;
                        render(form);
                    }
                    return;
                }
                match run_job(&format!("mkdir -p {}", quote(&folder)), None).await {
                    Ok(out) if out.status == 0 => {}
                    Ok(out) => {
                        let last = out
                            .output
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("mkdir failed")
                            .to_string();
                        fail(&state, mode, last);
                        return;
                    }
                    Err(e) => {
                        fail(&state, mode, format!("job failed: {}", e.message));
                        return;
                    }
                }
            }
            (folder, name)
        }
        Kind::Worktree => {
            let (repo, name, dest, branch) = (
                values[0].clone(),
                values[1].clone(),
                values[2].clone(),
                values[3].clone(),
            );
            // A worktree picked from the list already exists: open it.
            if !reuse {
                // Existing branch -> check it out; otherwise create with -b.
                let branch_exists = run_job(
                    &format!(
                        "git -C {} show-ref --verify --quiet {}",
                        quote(&repo),
                        quote(&format!("refs/heads/{branch}"))
                    ),
                    None,
                )
                .await
                .map(|o| o.status == 0)
                .unwrap_or(false);
                let add = if branch_exists {
                    format!(
                        "git -C {} worktree add {} {}",
                        quote(&repo),
                        quote(&dest),
                        quote(&branch)
                    )
                } else {
                    format!(
                        "git -C {} worktree add -b {} {}",
                        quote(&repo),
                        quote(&branch),
                        quote(&dest)
                    )
                };
                match run_job(&add, None).await {
                    Ok(out) if out.status == 0 => {}
                    Ok(out) => {
                        let last = out
                            .output
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("git worktree add failed")
                            .to_string();
                        fail(&state, mode, last);
                        return;
                    }
                    Err(e) => {
                        fail(&state, mode, format!("job failed: {}", e.message));
                        return;
                    }
                }
            }
            (dest, name)
        }
    };

    // The session first, the close second: a duplicate name (or any
    // new-session error) renders in the still-open form.
    let sess = session_name(&name);
    if let Err(e) = run_command(&format!(
        "new-session -d -s {} -c {}",
        quote(&sess),
        quote(&dir)
    ))
    .await
    {
        fail(&state, mode, e.message.clone());
        return;
    }
    let _ = mode_close(mode);
    state.borrow_mut().form = None;

    if let Some(client) = client_name {
        if let Err(e) = run_command(&format!(
            "switch-client -c {} -t {}",
            quote(&client),
            quote(&sess)
        ))
        .await
        {
            log(&format!("session_creator: switch-client failed: {}", e.message));
        }
    }
    let ready = match kind {
        Kind::Plain => format!("session ready: {sess}"),
        Kind::Worktree if reuse => format!("worktree opened: {dir}"),
        Kind::Worktree => format!("worktree ready: {dir}"),
    };
    let _ = display_message(&ready);
}

/// Re-enable the form with an error, unless it was closed meanwhile.
fn fail(state: &State, mode: ModeId, error: String) {
    let mut st = state.borrow_mut();
    if let Some(form) = st.form.as_mut().filter(|f| f.mode.0 == mode.0) {
        form.busy = false;
        form.confirm_create = false;
        form.error = Some(error);
        render(form);
    }
}

impl Plugin for SessionCreator {
    const NAME: &'static str = "session_creator";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        Ok(SessionCreator {
            state: Rc::new(RefCell::new(Shared { form: None, probing: false })),
        })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => {
                let kind = match event.get_str("text") {
                    Some("new") => Kind::Plain,
                    Some("worktree") => Kind::Worktree,
                    _ => return,
                };
                if self.state.borrow().form.is_some() {
                    let _ = display_message("session_creator: form already open");
                    return;
                }
                let target_pane = event.scope.pane.map(u64::from);
                let client = event.scope.client.map(u64::from);
                let state = Rc::clone(&self.state);
                ctx.spawn(open_form(state, kind, target_pane, client));
            }
            "mode-key" => {
                let mut st = self.state.borrow_mut();
                let Some(form) = st.form.as_mut() else { return };
                if event.get_i64("mode") != Some(form.mode.0 as i64) {
                    return;
                }
                let Some(key) = event.get_str("key") else {
                    return;
                };
                if form.busy {
                    return;
                }
                let mode = form.mode;
                // What the key did, decided inside the borrow and acted
                // on after it: rescan the focused field, probe the rows
                // now on screen, or nothing.
                enum After {
                    None,
                    Rescan,
                    Probe,
                    Submit,
                    Detect(String),
                }
                let mut after = After::None;

                match key {
                    "Escape" => {
                        // The list first, the form second. Hiding the
                        // list frees C-j/C-k to move between fields.
                        if form.picker.as_ref().is_some_and(|p| p.shown()) {
                            if let Some(p) = form.picker.as_mut() {
                                p.sel = None;
                                p.hidden = true;
                            }
                            form.error = None;
                            render(form);
                        } else {
                            let _ = mode_close(form.mode);
                        }
                    }
                    "Enter" => after = After::Submit,
                    "C-t" => {
                        let detect = form.toggle();
                        render(form);
                        after = match detect {
                            Some(folder) => After::Detect(folder),
                            None => After::Rescan,
                        };
                    }
                    "Down" | "C-n" | "C-j" => {
                        let listed = form
                            .picker
                            .as_ref()
                            .is_some_and(|p| p.shown() && !p.view.is_empty());
                        if listed {
                            let p = form.picker.as_mut().unwrap();
                            p.sel = Some(match p.sel {
                                None => 0,
                                Some(i) => (i + 1).min(p.view.len() - 1),
                            });
                            p.scroll_to_selection();
                            form.error = None;
                            render(form);
                            after = After::Probe;
                        } else {
                            // No list to step into: the key keeps its
                            // old meaning rather than going dead.
                            form.focused = (form.focused + 1) % form.fields.len();
                            after = After::Rescan;
                        }
                    }
                    "Up" | "C-p" | "C-k" => {
                        // Leaving the top row puts the cursor back in the
                        // text; a second Up then moves to the field above.
                        let inlist = form
                            .picker
                            .as_ref()
                            .is_some_and(|p| p.sel.is_some());
                        if inlist {
                            let p = form.picker.as_mut().unwrap();
                            match p.sel {
                                Some(0) | None => p.sel = None,
                                Some(i) => p.sel = Some(i - 1),
                            }
                            p.scroll_to_selection();
                            form.error = None;
                            render(form);
                            after = After::Probe;
                        } else {
                            form.focused =
                                (form.focused + form.fields.len() - 1)
                                    % form.fields.len();
                            after = After::Rescan;
                        }
                    }
                    "Tab" => {
                        // Complete like a shell: take the highlighted row,
                        // or the first row when none is highlighted, and
                        // stay in the field with the cursor at the end.
                        // The list re-filters on the completed value.
                        let listed = form
                            .picker
                            .as_ref()
                            .is_some_and(|p| p.shown() && !p.view.is_empty());
                        if listed {
                            let p = form.picker.as_mut().unwrap();
                            if p.sel.is_none() {
                                p.sel = Some(0);
                            }
                            if accept(form) {
                                form.picker.as_mut().unwrap().sel = None;
                                after = After::Rescan;
                            } else {
                                render(form);
                            }
                        }
                    }
                    "BTab" => {
                        form.focused =
                            (form.focused + form.fields.len() - 1) % form.fields.len();
                        after = After::Rescan;
                    }
                    "BSpace" => {
                        let i = form.focused;
                        form.fields[i].value.pop();
                        form.fields[i].touched = !form.fields[i].value.is_empty()
                            && form.fields[i].touched;
                        form.error = None;
                        form.confirm_create = false;
                        form.reuse = false;
                        form.sync_mirrors();
                        after = After::Rescan;
                    }
                    "C-u" => {
                        // Clear to type fresh: touched keeps the mirror
                        // from instantly refilling the field (BSpace past
                        // empty is the resume-the-mirror gesture).
                        let i = form.focused;
                        form.fields[i].value.clear();
                        form.fields[i].touched = true;
                        form.error = None;
                        form.confirm_create = false;
                        form.reuse = false;
                        form.sync_mirrors();
                        after = After::Rescan;
                    }
                    "Space" => {
                        let i = form.focused;
                        form.fields[i].value.push(' ');
                        if !matches!(form.fields[i].label, "repo" | "folder") {
                            form.fields[i].touched = true;
                        }
                        form.error = None;
                        form.confirm_create = false;
                        form.sync_mirrors();
                        after = After::Rescan;
                    }
                    k if k.chars().count() == 1
                        && !k.chars().next().unwrap().is_control() =>
                    {
                        let i = form.focused;
                        form.fields[i].value.push_str(k);
                        if !matches!(form.fields[i].label, "repo" | "folder") {
                            form.fields[i].touched = true;
                        }
                        form.error = None;
                        form.confirm_create = false;
                        form.reuse = false;
                        form.sync_mirrors();
                        after = After::Rescan;
                    }
                    _ => {}
                }

                drop(st);
                match after {
                    After::None => {}
                    After::Rescan => start_scan(&self.state, mode),
                    After::Probe => kick_probe(&self.state, mode),
                    After::Submit => {
                        ctx.spawn(submit(Rc::clone(&self.state)));
                    }
                    After::Detect(folder) => {
                        ctx.spawn(detect_repo(
                            Rc::clone(&self.state),
                            mode,
                            folder,
                        ));
                        start_scan(&self.state, mode);
                    }
                }
            }
            "mode-resize" => {
                let mut st = self.state.borrow_mut();
                let Some(form) = st.form.as_mut() else { return };
                if event.get_i64("mode") != Some(form.mode.0 as i64) {
                    return;
                }
                if let Some(w) = event.get_i64("width") {
                    form.width = w as u32;
                }
                if let Some(h) = event.get_i64("height") {
                    form.height = h as u32;
                }
                // The host has spoken: this is the size we really have,
                // so a render must not ask for the old one again.
                form.sized = (form.width, form.height);
                render(form);
            }
            "mode-closed" => {
                let mut st = self.state.borrow_mut();
                if st
                    .form
                    .as_ref()
                    .is_some_and(|f| {
                        event.get_i64("mode") == Some(f.mode.0 as i64)
                    })
                {
                    st.form = None;
                }
            }
            _ => {}
        }
    }
}

tmux_plugin!(SessionCreator);
