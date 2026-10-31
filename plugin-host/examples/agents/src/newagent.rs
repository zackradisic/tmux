//! The new-agent form: start another agent from the picker, prefilled
//! from the row under the cursor.
//!
//! `n` opens it over the picker. Three kinds, `C-t` cycles them:
//!
//! - **window** (the default): `session`, `folder`, `name`, `command`.
//!   Enter runs `new-window` in that session, rooted in the folder,
//!   running the command. The session is the selected row's; for a row
//!   on a linked server that is the link's local session (`host/name`),
//!   and `new-window` on it runs on the remote, so the new agent lands on
//!   the box you were looking at.
//! - **session**: `folder`, `name`, `command`. A new session instead.
//! - **worktree**: `repo`, `name`, `dest`, `branch`, `session`,
//!   `command`. `git worktree add` first, then a window in `session`
//!   rooted in the new tree - or a new session when `session` is empty.
//!
//! `folder` is the row's pane cwd, `command` its harness, `name` follows
//! the folder's basename until edited. The command field completes from
//! the configured harness list but takes anything. The harness runs as
//! the window's command, so the roster sees the pane the moment it
//! appears and retires the row when the agent exits.
//!
//! `resume` (window and session kinds) holds a harness session id, and
//! is what brings a finished or killed agent back: opened from a row
//! with no live pane, the form prefills it from the row's durable id
//! (`claude:<uuid>` gives `<uuid>`, `codex:<id>` its id), `folder` from
//! the directory the transcript recorded (Claude keeps sessions per
//! project directory, so that is where `--resume` finds it), and the
//! kind is window in the row's session when that still exists, else a
//! session named after the old one. Enter on such a row opens the form
//! the same way, so the form is the "recreate it?" question: what it
//! shows is what happens, Esc says no. On Enter the command gains
//! `--resume <id>` (Claude, and anything unknown) or `resume <id>`
//! (Codex). The new pane carries the same durable id, so the roster
//! re-points the old row at it, turns and all, rather than making a new
//! one. Never the old pane: whatever runs there now is somebody's.
//!
//! `fork` (a yes/no field, `C-f` flips it; `f` in the picker opens the
//! form with it on, for a live row too) adds `--fork-session`: Claude
//! copies the conversation into a new session instead of continuing the
//! old one. An opencode row forks through the service that holds its
//! session (`resolve::opencode_fork`, before the window opens) and the
//! new window runs `opencode -s <copy>`. Either copy would carry the
//! original's name and be indistinguishable in the roster, so the form
//! names it - the old name plus `fork`, numbered when that is taken -
//! and leaves that name for the agent that appears in the new window
//! (`name_window_when_seen`), the way the rename key would.
//!
//! Built on `formkit`; this file is what the fields mean and what Enter
//! does. The scans run in this plugin, so it needs
//! `formkit::complete::CAPS` (run-process, fs-list, fs-read-any) on top
//! of the picker's own grants.

use std::cell::RefCell;
use std::rc::Rc;

use formkit::complete::{RowKind, Source};
use formkit::form::{self, field, Action, Field, Form, Model, Shared};
use formkit::git::{self, Ensured};
use formkit::text::{basename, dest_base, quote, scan_base};
use tmux_plugin_sdk::prelude::*;

use crate::view::Picker;

const FORM_WIDTH: u32 = 76;
const FORM_HEIGHT: u32 = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Window,
    Session,
    Worktree,
}

impl Kind {
    fn next(self) -> Kind {
        match self {
            Kind::Window => Kind::Session,
            Kind::Session => Kind::Worktree,
            Kind::Worktree => Kind::Window,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Kind::Window => "window",
            Kind::Session => "session",
            Kind::Worktree => "worktree",
        }
    }
}

/// What the new-agent form knows about its fields.
pub struct NewAgent {
    kind: Kind,
    /// What the `command` field completes from: (name, shell line). A
    /// configured launcher's line is what its name runs; a bare harness
    /// runs as itself. See `Config::launchers`.
    launchers: Vec<(String, String)>,
    /// The session the selected row lives in (a link's local name for a
    /// remote row), kept across the session kind, which has no field
    /// for it, so cycling back does not lose it.
    session: String,
    /// The row lives on a linked server: its paths are remote paths, so
    /// the local filesystem is not asked about them.
    remote: Option<String>,
    /// Detected repo root, for the ✓ on the repo field.
    detected: Option<String>,
    /// Pressing client, for the final switch-client.
    client_name: Option<String>,
    /// The folder was missing on the last Enter; the next Enter creates it.
    confirm_create: bool,
    /// `dest` holds a worktree that already exists: skip the add.
    reuse: bool,
    /// The picker's mode, closed together with the form on success so
    /// the new agent's pane is what you see.
    picker_mode: Option<ModeId>,
    /// Shell assignments put before the command (`CLAUDE_CONFIG_DIR='…'`):
    /// a per-window login from the accounts plugin.
    env: String,
    /// The row's harness, for the shape of the resume flag.
    harness: String,
    /// The resume id, kept across the worktree kind, which has no field.
    resume: String,
    /// The fork flag, kept the same way.
    fork: String,
    /// The projects on the roster, (name, "n agents"), for the project
    /// field's dropdown.
    projects: Vec<(String, String)>,
}

/// Is the fork field on?
fn fork_on(v: &str) -> bool {
    matches!(v.trim().to_lowercase().as_str(), "yes" | "y" | "on" | "1" | "true")
}

fn value_of(fields: &[Field], label: &str) -> String {
    fields
        .iter()
        .find(|f| f.label == label)
        .map(|f| f.value.trim().to_string())
        .unwrap_or_default()
}

fn idx(fields: &[Field], label: &str) -> Option<usize> {
    fields.iter().position(|f| f.label == label)
}

impl Model for NewAgent {
    fn source(&self, fields: &[Field], i: usize) -> Option<Source> {
        let f = &fields[i];
        match f.label {
            // A remote row's paths are on the other machine; listing the
            // local disk under them would only mislead.
            "folder" | "repo" | "dest" | "branch" if self.remote.is_some() => None,
            "folder" => Some(Source::Dirs { base: scan_base(&f.value) }),
            "repo" => Some(Source::Repos { base: scan_base(&f.value) }),
            "dest" => Some(Source::Dests {
                base: scan_base(&f.value),
                repo: value_of(fields, "repo"),
            }),
            "branch" => {
                let repo = value_of(fields, "repo");
                (!repo.is_empty()).then_some(Source::Branches { repo })
            }
            "session" => Some(Source::Words {
                title: "sessions".into(),
                words: list_sessions()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| (s.name, format!("{} windows", s.windows.len())))
                    .collect(),
            }),
            "project" => Some(Source::Words { title: "projects".into(), words: self.projects.clone() }),
            "command" => Some(Source::Words {
                title: "launchers".into(),
                words: self
                    .launchers
                    .iter()
                    .map(|(n, line)| {
                        // The second column is the line a name runs; a
                        // harness that runs as itself says so.
                        let meta = if n == line { "harness".to_string() } else { line.clone() };
                        (n.clone(), meta)
                    })
                    .collect(),
            }),
            _ => None,
        }
    }

    fn mirror(&mut self, fields: &mut [Field]) {
        match self.kind {
            Kind::Window | Kind::Session => {
                let folder = value_of(fields, "folder");
                if let Some(ni) = idx(fields, "name") {
                    if !fields[ni].touched {
                        fields[ni].value = basename(&folder);
                    }
                }
            }
            Kind::Worktree => {
                let name = value_of(fields, "name");
                let base = dest_base(&value_of(fields, "repo"));
                if let Some(di) = idx(fields, "dest") {
                    if !fields[di].touched {
                        fields[di].value = format!("{base}{name}");
                    }
                }
                if let Some(bi) = idx(fields, "branch") {
                    if !fields[bi].touched {
                        fields[bi].value = name;
                    }
                }
            }
        }
    }

    fn accepted(&mut self, _fields: &[Field], _i: usize, kind: RowKind) {
        self.confirm_create = false;
        self.reuse = kind == RowKind::Worktree;
    }

    fn edited(&mut self, _fields: &[Field], _i: usize) {
        self.confirm_create = false;
        self.reuse = false;
    }

    fn title(&self) -> String {
        "New Agent".into()
    }

    fn kinds(&self) -> Option<(Vec<&'static str>, usize)> {
        let active = match self.kind {
            Kind::Window => 0,
            Kind::Session => 1,
            Kind::Worktree => 2,
        };
        Some((vec!["window", "session", "worktree"], active))
    }

    fn toggle_hint(&self) -> Option<String> {
        Some(self.kind.next().word().into())
    }

    fn banner(&self, fields: &[Field]) -> Option<String> {
        if let Some(server) = &self.remote {
            return Some(format!("on {server}: paths are that machine's, no completion"));
        }
        if fields[0].value.trim().is_empty() {
            return Some(
                match self.kind {
                    Kind::Worktree => "no git repo detected — enter one:",
                    _ => "no folder detected — enter one:",
                }
                .into(),
            );
        }
        None
    }

    fn mark(&self, fields: &[Field], i: usize) -> Option<String> {
        let f = &fields[i];
        if f.label == "repo" && self.detected.as_ref() == Some(&f.value) {
            return Some("\x1b[32m✓\x1b[0m".into());
        }
        if f.label == "dest" && self.reuse {
            return Some("\x1b[36m(exists)\x1b[0m".into());
        }
        None
    }

    fn status(&self, fields: &[Field], busy: bool) -> Option<String> {
        if busy {
            return Some(
                match self.kind {
                    Kind::Worktree if !self.reuse => "creating worktree...",
                    _ if value_of(fields, "session").is_empty() => "creating session...",
                    _ => "creating window...",
                }
                .into(),
            );
        }
        if self.confirm_create {
            return Some("folder does not exist — Enter again to create it".into());
        }
        // A command that starts with a launcher's name says what it
        // really runs, so there is no surprise on Enter.
        let command = value_of(fields, "command");
        let expanded = expand_command(&self.launchers, &command);
        if !command.is_empty() && expanded != command {
            return Some(format!("runs: {expanded}"));
        }
        None
    }

    fn extra_hint(&self) -> Option<String> {
        matches!(self.kind, Kind::Window | Kind::Session).then(|| "C-f fork".to_string())
    }

    fn submit_label(&self) -> &'static str {
        "start"
    }
}

type State = Shared<NewAgent>;

thread_local! {
    /// The one form. A detached task (the open, the submit) finds it
    /// here; the plugin's event handlers ask [`owns`] whether a mode
    /// event is the form's.
    static FORM: State = Rc::new(RefCell::new(None));
}

pub fn state() -> State {
    FORM.with(Rc::clone)
}

/// Whether a mode event belongs to the open form.
pub fn owns(mode: Option<i64>) -> bool {
    state().borrow().as_ref().is_some_and(|f| mode == Some(f.mode.0 as i64))
}

/// The fields of a kind, from the values that travel between kinds.
fn fields_for(
    kind: Kind,
    session: &str,
    folder: &str,
    name: Field,
    command: Field,
    resume: Field,
    fork: Field,
    project: Field,
) -> Vec<Field> {
    match kind {
        Kind::Window => vec![
            field("session", session.to_string()),
            field("folder", folder.to_string()),
            name,
            command,
            resume,
            fork,
            project,
        ],
        Kind::Session => vec![field("folder", folder.to_string()), name, command, resume, fork, project],
        Kind::Worktree => vec![
            field("repo", folder.to_string()),
            name,
            field("dest", String::new()),
            field("branch", String::new()),
            field("session", session.to_string()),
            command,
            project,
        ],
    }
}

/// The shell line a command field runs: its first word, when it names a
/// launcher, is replaced by that launcher's line and the rest follows -
/// so `claude-opus --resume <id>` is the opus launcher with a flag. Any
/// other command runs as typed. A harness with no launcher of its own
/// is listed as itself, so this is the identity for it.
pub fn expand_command(launchers: &[(String, String)], command: &str) -> String {
    let command = command.trim();
    let (head, rest) = match command.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim_start()),
        None => (command, ""),
    };
    match launchers.iter().find(|(n, _)| n == head) {
        Some((_, line)) if rest.is_empty() => line.clone(),
        Some((_, line)) => format!("{line} {rest}"),
        None => command.to_string(),
    }
}

/// The harness session id a row's durable id carries, when the harness
/// can resume it: `claude:<uuid>` and `codex:<id>`. A provisional id
/// (`prov-...`) names nothing to resume.
pub fn resumable_id(id: &str) -> Option<String> {
    let (kind, rest) = id.split_once(':')?;
    if rest.is_empty() {
        return None;
    }
    match kind {
        "claude" | "codex" | "opencode" => Some(rest.to_string()),
        _ => None,
    }
}

/// The command line with the harness's resume flag: `claude --resume
/// <id>` (Claude, and any harness without a known form), `codex resume
/// <id>` (a subcommand; global flags in the launcher line stay ahead of
/// it), `opencode -s <id>`.
pub fn resume_command(harness: &str, command: &str, resume: &str) -> String {
    match harness {
        "codex" => format!("{command} resume {resume}"),
        "opencode" => format!("{command} -s {resume}"),
        _ => format!("{command} --resume {resume}"),
    }
}

/// The command line with the agent's first prompt, when there is one:
/// `claude 'text'` / `codex 'text'` (a positional argument), `opencode
/// --prompt 'text'`. A file is read by the shell the window runs,
/// `"$(cat '<path>')"`, so quotes and newlines in the prompt never meet
/// a tmux command line; the file wins when both are given. An empty
/// prompt is no prompt.
pub fn prompt_command(harness: &str, command: &str, prompt: Option<&str>, file: Option<&str>) -> String {
    let arg = match (file.filter(|f| !f.trim().is_empty()), prompt.filter(|p| !p.is_empty())) {
        (Some(f), _) => format!("\"$(cat {})\"", quote(f.trim())),
        (None, Some(p)) => quote(p),
        (None, None) => return command.to_string(),
    };
    match harness {
        "opencode" => format!("{command} --prompt {arg}"),
        _ => format!("{command} {arg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harnesses_with_a_session_id_resume() {
        assert_eq!(resumable_id("claude:1b2c"), Some("1b2c".to_string()));
        assert_eq!(resumable_id("codex:abc"), Some("abc".to_string()));
        assert_eq!(resumable_id("opencode:ses_e9d2"), Some("ses_e9d2".to_string()));
        assert_eq!(resumable_id("opencode:"), None);
        assert_eq!(resumable_id("prov-opencode-12"), None);
        assert_eq!(resumable_id("pi:abc"), None);
    }

    #[test]
    fn verb_words_split_like_a_shell() {
        let w = |s: &str| words(s);
        assert_eq!(
            w("new --resume abc --project 'Ooga booga'"),
            vec!["new", "--resume", "abc", "--project", "Ooga booga"]
        );
        assert_eq!(w(r#"new --command "sh -c 'exec cat'" -n x"#), vec!["new", "--command", "sh -c 'exec cat'", "-n", "x"]);
        assert_eq!(w("new --name a\\ b ''"), vec!["new", "--name", "a b", ""]);
        assert_eq!(w("  new   "), vec!["new"]);
        assert!(is_direct(&w("new --project x")));
        assert!(is_direct(&w("new --resume=abc")));
        assert!(is_direct(&w("new --prompt-file /tmp/p.md")));
        assert!(!is_direct(&w("new @boat --client c0 --profile /tmp/p")));
    }

    #[test]
    fn prompt_per_harness_and_from_a_file() {
        assert_eq!(prompt_command("claude", "claude", Some("fix it"), None), "claude 'fix it'");
        assert_eq!(prompt_command("codex", "codex --yolo", Some("it's"), None), "codex --yolo 'it'\\''s'");
        assert_eq!(prompt_command("opencode", "opencode", Some("go"), None), "opencode --prompt 'go'");
        assert_eq!(
            prompt_command("claude", "claude", None, Some("/tmp/p.md")),
            "claude \"$(cat '/tmp/p.md')\""
        );
        assert_eq!(
            prompt_command("opencode", "opencode", Some("ignored"), Some("/tmp/p.md")),
            "opencode --prompt \"$(cat '/tmp/p.md')\""
        );
        assert_eq!(prompt_command("claude", "claude", Some(""), None), "claude");
        assert_eq!(prompt_command("claude", "claude", None, Some(" ")), "claude");
    }

    #[test]
    fn resume_flag_per_harness() {
        assert_eq!(resume_command("claude", "claude", "x"), "claude --resume x");
        assert_eq!(resume_command("codex", "codex --yolo", "x"), "codex --yolo resume x");
        assert_eq!(resume_command("opencode", "opencode", "ses_1"), "opencode -s ses_1");
    }
}

/// tmux session names may not contain '.' or ':'; spaces are legal but
/// unpleasant in targets.
fn session_name(name: &str) -> String {
    name.chars()
        .map(|c| if c == '.' || c == ':' || c == ' ' { '-' } else { c })
        .collect()
}

fn session_exists(name: &str) -> bool {
    list_sessions().map(|s| s.iter().any(|x| x.name == name)).unwrap_or(false)
}

/// Cycle the kind in place. Name and command travel; the session is
/// remembered across the kind that has no field for it; folder and repo
/// are the same value. Returns the folder to detect a repo root in when
/// entering the worktree kind.
fn toggle(form: &mut Form<NewAgent>) -> Option<String> {
    let name = form.fields[form.idx("name")].clone();
    let command = form.fields[form.idx("command")].clone();
    // The worktree kind has no resume field; the value survives the trip
    // through it the way the session does, in a fresh field.
    let resume = idx(&form.fields, "resume")
        .map(|i| form.fields[i].clone())
        .unwrap_or_else(|| field("resume", form.model.resume.clone()));
    form.model.resume = resume.value.trim().to_string();
    let fork = idx(&form.fields, "fork")
        .map(|i| form.fields[i].clone())
        .unwrap_or_else(|| field("fork", form.model.fork.clone()));
    form.model.fork = fork.value.trim().to_string();
    let project = idx(&form.fields, "project")
        .map(|i| form.fields[i].clone())
        .unwrap_or_else(|| field("project", String::new()));
    let folder = idx(&form.fields, "folder")
        .or_else(|| idx(&form.fields, "repo"))
        .map(|i| form.fields[i].value.trim().to_string())
        .unwrap_or_default();
    if let Some(i) = idx(&form.fields, "session") {
        form.model.session = form.fields[i].value.trim().to_string();
    }
    let next = form.model.kind.next();
    form.model.kind = next;
    let session = form.model.session.clone();
    form.replace_fields(fields_for(next, &session, &folder, name, command, resume, fork, project), "name");
    form.model.confirm_create = false;
    form.model.reuse = false;
    (next == Kind::Worktree && !folder.is_empty() && form.model.remote.is_none())
        .then_some(folder)
}

/// Open the form over the picker, prefilled from its highlighted row.
/// `picker` is read for the row and the picker's mode; the form is its
/// own float.
/// What a caller other than the picker's `n` hands the form: the
/// accounts plugin's `n` (a profile directory to run the harness under,
/// on a server), with the launchers the picker would have had.
#[derive(Debug, Clone, Default)]
pub struct Extra {
    pub env: String,
    pub remote: Option<String>,
    pub launchers: Vec<(String, String)>,
}

pub async fn open(
    picker: Rc<RefCell<Option<Picker>>>,
    client: Option<u64>,
    fork: bool,
    extra: Option<Extra>,
) {
    if state().borrow().is_some() {
        return;
    }
    let extra = extra.unwrap_or_default();
    // The pressing client: its session and window are where the form
    // opens, and the defaults when no row is behind it.
    let client_info = client.and_then(|cid| {
        list_clients().ok()?.iter().find_map(|c| {
            (u64::from(c.id) == cid).then(|| (c.name.clone(), c.session))
        })
    });
    let client_session = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s)).ok());
    // What the row gives us, read under one borrow. With no picker open
    // (the accounts panel's n) the form starts from the extra and the
    // client: a window in the client's session (or the first mirrored
    // session of the named server), in its pane's folder.
    let (picker_mode, launchers, session, remote, local_pane, harness, revive, fork_name) = if picker.borrow().is_none() {
        let remote = extra.remote.clone();
        let session = match &remote {
            Some(host) => list_sessions()
                .ok()
                .and_then(|v| v.into_iter().map(|s| s.name).find(|n| n.starts_with(&format!("{host}/"))))
                .unwrap_or_default(),
            None => client_session.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
        };
        (None, extra.launchers.clone(), session, remote, None, "claude".to_string(), None, None)
    } else {
        let b = picker.borrow();
        let Some(p) = b.as_ref() else { return };
        let row = p.selected();
        let local_pane = row.and_then(|a| p.local_pane_of(a));
        let remote = row.filter(|a| !a.is_local()).map(|a| a.server.clone());
        // A remote row's session is mirrored here as "host/name"; only
        // offer it when the link is actually up (the session exists).
        let old_session = row.and_then(|a| a.session.clone()).map(|s| match &remote {
            Some(host) => format!("{host}/{s}"),
            None => s,
        });
        let session = old_session.clone().filter(|s| session_exists(s)).unwrap_or_default();
        let harness = row.map(|a| a.kind.clone()).unwrap_or_default();
        // A row with no live pane: bring that agent back rather than
        // start a fresh one - its harness session id, the directory its
        // transcript recorded, and the old session's name for when that
        // session is gone and a new one has to carry it. A row that is
        // not ended but has no pane (a roster repaired after a sweep, a
        // handoff that lost the pane) is just as gone: the picker's Enter
        // sends it here for the same reason. A fork resumes a live row's
        // session the same way, as a copy.
        let revive = row.filter(|a| fork || !a.live() || a.pane.is_none()).map(|a| {
            (
                resumable_id(&a.id).unwrap_or_default(),
                a.cwd.clone().unwrap_or_default(),
                old_session.clone().unwrap_or_default(),
            )
        });
        // The fork's own name: the row's, plus "fork", numbered past the
        // names already on the roster.
        let fork_name = row.filter(|_| fork).map(|a| {
            let base = format!("{} fork", crate::view::display_name(a).trim());
            let taken = |n: &str| {
                p.rows.iter().any(|r| crate::view::display_name(r).trim() == n)
            };
            let mut name = base.clone();
            let mut k = 2;
            while taken(&name) {
                name = format!("{base} {k}");
                k += 1;
            }
            name
        });
        (Some(p.engine.mode), p.launchers.clone(), session, remote, local_pane, harness, revive, fork_name)
    };
    // The folder: the row's pane cwd (a shadow's is the remote's cached
    // path), else the pressing client's pane, like `prefix S`. The
    // active pane may be a float with no directory (the panel that
    // asked for this form, not yet torn down): the first pane of the
    // window with one stands in then.
    let client_pane = client_session
        .as_ref()
        .and_then(|v| v.current_window)
        .and_then(|w| resolve_window(WindowId(w)).ok())
        .and_then(|w| {
            let has_cwd = |p: &u32| resolve_pane(PaneId(*p)).ok().is_some_and(|pi| !pi.cwd.is_empty());
            w.active_pane.filter(has_cwd).or_else(|| w.panes.iter().copied().find(has_cwd))
        });
    // The project the new agent joins: the row's (or, with the cursor on
    // a project header, that project's), else the one the pressing
    // client's pane resolves to. And every project on the roster, for
    // the field's dropdown.
    let (project, projects) = {
        let b = picker.borrow();
        match b.as_ref() {
            Some(p) => {
                let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                for a in &p.rows {
                    *counts.entry(crate::view::project_of(a)).or_insert(0) += 1;
                }
                let mut list: Vec<(String, String)> = counts
                    .into_iter()
                    .map(|(n, c)| (n, format!("{c} agent{}", if c == 1 { "" } else { "s" })))
                    .collect();
                list.sort();
                let own = match p.selected() {
                    Some(a) => crate::view::project_of(a),
                    None => p
                        .engine
                        .selected_key()
                        .as_deref()
                        .and_then(|k| k.strip_prefix("proj\u{1}"))
                        .unwrap_or_default()
                        .to_string(),
                };
                (own, list)
            }
            None => (String::new(), Vec::new()),
        }
    };
    let project = if project.is_empty() {
        client_pane
            .and_then(|p| format_expand(OptionTarget::Pane(PaneId(p)), "#{@project}").ok())
            .unwrap_or_default()
    } else {
        project
    };
    let folder = revive
        .as_ref()
        .map(|(_, cwd, _)| cwd.clone())
        .filter(|c| !c.is_empty())
        .or_else(|| {
            local_pane
                .or(client_pane)
                .and_then(|p| resolve_pane(PaneId(p)).ok())
                .map(|p| p.cwd)
                .filter(|c| !c.is_empty())
        })
        .unwrap_or_default();
    let window = client_session.as_ref().and_then(|v| v.current_window);
    let Some(window) = window else {
        let _ = display_message("agents: no client to open the form for");
        return;
    };
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("new agent".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("agents: cannot open the form: {}", e.message));
            return;
        }
    };
    // The row's harness by name: a launcher of that name (the user's
    // own flags for it) or the bare harness. No row: the first launcher.
    let command = if harness.is_empty() {
        launchers.first().map(|(n, _)| n.clone()).unwrap_or_default()
    } else {
        harness.clone()
    };
    // Bringing one back: a window in its session when that exists, else
    // a session named after the old one; the resume id in its field.
    let (resume, old_session) = revive
        .map(|(id, _, s)| (id, s))
        .unwrap_or_default();
    let kind = if !resume.is_empty() && session.is_empty() { Kind::Session } else { Kind::Window };
    // The old session's name for the new session, or the fork's own
    // name, held as if typed so the name's mirror (the folder's
    // basename) leaves it be.
    let mut name = field("name", String::new());
    if let Some(fname) = &fork_name {
        name.value = fname.clone();
        name.touched = true;
    } else if kind == Kind::Session && !old_session.is_empty() {
        name.value = session_name(&old_session);
        name.touched = true;
    }
    let fork_value = if fork { "yes" } else { "no" }.to_string();
    let fields = fields_for(
        kind,
        &session,
        &folder,
        name,
        field("command", command),
        field("resume", resume.clone()),
        field("fork", fork_value.clone()),
        field("project", project),
    );
    let model = NewAgent {
        kind,
        launchers,
        session,
        remote,
        detected: None,
        client_name: client_info.map(|(n, _)| n),
        confirm_create: false,
        reuse: false,
        picker_mode,
        env: extra.env.clone(),
        harness,
        resume,
        fork: fork_value,
        projects,
    };
    let mut form = Form::new(mode, FORM_WIDTH, FORM_HEIGHT, fields, model);
    // Start on the first empty field; with everything prefilled, on the
    // name, which is the one worth changing before Enter.
    form.focused = form
        .fields
        .iter()
        .position(|f| f.label != "resume" && f.value.trim().is_empty())
        .unwrap_or_else(|| form.idx("name"));
    form::render(&mut form);
    *state().borrow_mut() = Some(form);
    form::start_scan(&state(), mode, false);
}

/// Repo detection after a swap into the worktree kind; applies only if
/// the form still shows the same repo value in the same kind.
async fn detect_repo(mode: ModeId, folder: String) {
    let Some(root) = git::root(&folder).await else { return };
    let st = state();
    let mut b = st.borrow_mut();
    let Some(form) = b.as_mut() else { return };
    if form.mode.0 != mode.0 || form.model.kind != Kind::Worktree {
        return;
    }
    let Some(ri) = idx(&form.fields, "repo") else { return };
    if form.fields[ri].value != folder {
        return;
    }
    form.fields[ri].value = root.clone();
    form.model.detected = Some(root);
    form.run_mirror();
    form::render(form);
}

/// A key for the form. Called only when [`owns`] said so.
pub fn on_key(ctx: &Ctx, event: &Event) {
    let Some(key) = event.get_str("key") else { return };
    let st = state();
    let (mode, action, detect) = {
        let mut b = st.borrow_mut();
        let Some(form) = b.as_mut() else { return };
        let mode = form.mode;
        if key == "C-f" {
            if let Some(i) = idx(&form.fields, "fork") {
                let on = fork_on(&form.fields[i].value);
                form.fields[i].value = if on { "no" } else { "yes" }.to_string();
                form.fields[i].touched = true;
                form::render(form);
            }
            return;
        }
        let mut action = form.key(key, Some("C-t"));
        let mut detect = None;
        if action == Action::Toggle {
            detect = toggle(form);
            form::render(form);
            action = Action::Rescan { reveal: false };
        }
        (mode, action, detect)
    };
    if let Some(folder) = detect {
        ctx.spawn(detect_repo(mode, folder));
    }
    match action {
        Action::None | Action::Toggle => {}
        Action::Rescan { reveal } => form::start_scan(&st, mode, reveal),
        Action::Probe => form::kick_probe(&st, mode),
        Action::Submit => {
            ctx.spawn(submit());
        }
        Action::Close => {
            let _ = mode_close(mode);
        }
    }
}

pub fn on_resize(event: &Event) {
    let st = state();
    let mut b = st.borrow_mut();
    let Some(form) = b.as_mut() else { return };
    form.resized(event.get_i64("width"), event.get_i64("height"));
    form::render(form);
}

pub fn on_closed() {
    *state().borrow_mut() = None;
}

/// Validate and run: the folder or worktree first, then the window or
/// session running the command. On failure the form stays up with the
/// error; on success the form and the picker close and the client is
/// switched to the new agent.
async fn submit() {
    let st = state();
    let (mode, kind, values, labels, model_bits) = {
        let mut b = st.borrow_mut();
        let Some(form) = b.as_mut() else { return };
        let v = |l: &str| form.value(l);
        let has = |l: &str| idx(&form.fields, l).is_some();
        let mut err: Option<String> = None;
        let first = if form.model.kind == Kind::Worktree { "repo" } else { "folder" };
        if v(first).is_empty() {
            err = Some(format!("{first} is required"));
        } else if v("name").is_empty() {
            err = Some("name is required".into());
        } else if v("command").is_empty() {
            err = Some("command is required".into());
        } else if form.model.kind == Kind::Worktree
            && (v("dest").is_empty() || (!form.model.reuse && v("branch").is_empty()))
        {
            err = Some("dest and branch are required".into());
        } else if form.model.kind == Kind::Worktree && form.model.remote.is_some() {
            err = Some("a worktree on a linked server is not supported yet".into());
        } else if has("session") && !v("session").is_empty() && !session_exists(&v("session")) {
            err = Some(format!("no session '{}'", v("session")));
        } else if form.model.kind == Kind::Window && v("session").is_empty() {
            err = Some("session is required (C-t for a new one)".into());
        } else if (!has("session") || v("session").is_empty())
            && session_exists(&session_name(&v("name")))
        {
            err = Some(format!("session '{}' already exists", session_name(&v("name"))));
        }
        if let Some(err) = err {
            form.error = Some(err);
            form::render(form);
            return;
        }
        let values: Vec<String> =
            form.fields.iter().map(|f| f.effective().trim().to_string()).collect();
        let labels: Vec<&'static str> = form.fields.iter().map(|f| f.label).collect();
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        (
            form.mode,
            form.model.kind,
            values,
            labels,
            (
                form.model.confirm_create,
                form.model.reuse,
                form.model.remote.clone(),
                form.model.client_name.clone(),
                form.model.picker_mode,
                form.model.harness.clone(),
                form.model.env.clone(),
            ),
        )
    };
    let (confirmed, reuse, remote_host, client_name, picker_mode, harness, env) = model_bits;
    let remote = remote_host.is_some();
    let get = |l: &str| {
        labels
            .iter()
            .position(|x| *x == l)
            .map(|i| values[i].clone())
            .unwrap_or_default()
    };

    // The directory the agent runs in.
    let dir = match kind {
        Kind::Window | Kind::Session => {
            let folder = get("folder");
            // A remote folder is the remote's business; this disk knows
            // nothing about it.
            if !remote {
                match git::ensure_dir(&folder, confirmed).await {
                    Ok(Ensured::Ready) => {}
                    Ok(Ensured::Missing) => {
                        let mut b = st.borrow_mut();
                        if let Some(form) = b.as_mut().filter(|f| f.mode.0 == mode.0) {
                            form.busy = false;
                            form.model.confirm_create = true;
                            form::render(form);
                        }
                        return;
                    }
                    Err(e) => {
                        form::fail(&st, mode, e);
                        return;
                    }
                }
            }
            folder
        }
        Kind::Worktree => {
            let dest = get("dest");
            if !reuse {
                if let Err(e) = git::add_worktree(&get("repo"), &dest, &get("branch")).await {
                    form::fail(&st, mode, e);
                    return;
                }
            }
            dest
        }
    };

    // The window in the session, or a session of its own.
    let name = get("name");
    let launchers = st.borrow().as_ref().map(|f| f.model.launchers.clone()).unwrap_or_default();
    let command = expand_command(&launchers, &get("command"));
    // A per-window login: the assignments go before the expanded line,
    // so the launcher's own flags stay.
    let command = if env.is_empty() { command } else { format!("{env} {command}") };
    let landed = launch(Launch {
        dir,
        name,
        command,
        harness,
        resume: get("resume"),
        fork: fork_on(&get("fork")),
        session: get("session"),
        project: get("project").trim().to_string(),
        remote_host,
        focus: true,
        split: None,
        prompt: None,
        prompt_file: None,
    })
    .await;
    let landed = match landed {
        Ok(l) => l,
        Err(e) => {
            form::fail(&st, mode, e);
            return;
        }
    };
    let _ = mode_close(mode);
    *st.borrow_mut() = None;
    // The picker too: the point of starting an agent is to talk to it.
    if let Some(m) = picker_mode {
        let _ = mode_close(m);
    }
    if let Some(client) = client_name {
        let _ = run_command(&format!(
            "switch-client -c {} -t {}",
            quote(&client),
            quote(&format!("{}:", landed.target))
        ))
        .await;
    }
    let _ = display_message(&format!("agent started: {} in {}", landed.command, landed.target));
}

// ---------------------------------------------------------------------------
// starting the agent: the form's Enter and the `new` verb's flags meet here
// ---------------------------------------------------------------------------

/// What starts an agent, however it was asked for: the form's Enter or
/// the flags of `plugin-command agents "new ..."`. `command` is the line
/// as it runs, already expanded through the launchers with any
/// per-window environment ahead of it; `resume` and `fork` add the
/// harness's own flags to it.
pub struct Launch {
    pub dir: String,
    pub name: String,
    pub command: String,
    pub harness: String,
    pub resume: String,
    pub fork: bool,
    /// The session the window opens in; empty makes a session of its
    /// own, named after `name`.
    pub session: String,
    /// The `@project` the new window (the pane of a split, the new
    /// session) is tagged with; empty leaves it to the session's option
    /// and the directory.
    pub project: String,
    /// The linked server the session is mirrored from, when it is.
    pub remote_host: Option<String>,
    /// Make the new window the session's current one (a split's pane
    /// the active one). The form does: whoever pressed Enter wants to
    /// talk to the agent. The verb only on `--focus`: the one who ran it
    /// is usually an agent, and the user is reading something else.
    pub focus: bool,
    /// Split this pane instead of opening a window.
    pub split: Option<u32>,
    /// The agent's first prompt, typed for it: a positional argument
    /// for claude and codex, `--prompt` for opencode.
    pub prompt: Option<String>,
    /// The first prompt read from this file instead, as `"$(cat
    /// '<path>')"` on the command line: the shell the window runs reads
    /// it, so a prompt with quotes or newlines rides in a file untouched.
    /// Wins over `prompt` when both are given.
    pub prompt_file: Option<String>,
}

/// Where the agent landed.
pub struct Landed {
    /// The session, for a `switch-client -t`.
    pub target: String,
    /// The line that runs there.
    pub command: String,
    pub window: Option<u32>,
    pub pane: Option<u32>,
}

pub async fn launch(l: Launch) -> Result<Landed, String> {
    let fork = l.fork && !l.resume.is_empty();
    // opencode's fork is the service's: the copy exists before the
    // window opens, and the window resumes the copy.
    let resume = if fork && l.harness == "opencode" {
        crate::resolve::opencode_fork(&l.resume)
            .await
            .map_err(|e| format!("fork failed: {e}"))?
    } else {
        l.resume.clone()
    };
    let command = match resume.as_str() {
        "" => l.command.clone(),
        id => {
            let c = resume_command(&l.harness, &l.command, id);
            // Claude's fork: a copy of the conversation in a new session.
            if fork && l.harness == "claude" {
                format!("{c} --fork-session")
            } else {
                c
            }
        }
    };
    let command = prompt_command(&l.harness, &command, l.prompt.as_deref(), l.prompt_file.as_deref());
    let detach = if l.focus { "" } else { "-d " };
    // The ids there before, so the new window (or pane) can be told
    // from the old ones after: made with `-d` it is nobody's current
    // window, so nothing else names it.
    let windows_before: Vec<u32> =
        list_windows().unwrap_or_default().into_iter().map(|w| w.id).collect();
    let panes_before: Vec<u32> =
        list_panes().unwrap_or_default().into_iter().map(|p| p.id).collect();
    let (cmd, target) = match l.split {
        Some(p) => (
            format!(
                "split-window {detach}-t {} -c {} {}",
                quote(&format!("%{p}")),
                quote(&l.dir),
                quote(&command)
            ),
            format_expand(OptionTarget::Pane(PaneId(p)), "#{session_name}").unwrap_or_default(),
        ),
        None if !l.session.is_empty() => (
            format!(
                "new-window {detach}-t {} -c {} -n {} {}",
                quote(&format!("{}:", l.session)),
                quote(&l.dir),
                quote(&l.name),
                quote(&command)
            ),
            l.session.clone(),
        ),
        None => {
            let sess = session_name(&l.name);
            (
                format!("new-session -d -s {} -c {} {}", quote(&sess), quote(&l.dir), quote(&command)),
                sess,
            )
        }
    };
    if fork {
        // The copy is named before it appears: the roster applies this
        // to whatever agent shows up in that window.
        crate::provider::name_window_when_seen(&target, &l.name, &l.name);
    }
    run_command(&cmd).await.map_err(|e| e.message.clone())?;
    let (window, pane) = match l.split {
        Some(_) => {
            let pane = list_panes()
                .unwrap_or_default()
                .into_iter()
                .find(|p| !panes_before.contains(&p.id));
            (pane.as_ref().map(|p| p.window), pane.map(|p| p.id))
        }
        None => {
            let window = list_windows()
                .unwrap_or_default()
                .into_iter()
                .find(|w| !windows_before.contains(&w.id));
            (window.as_ref().map(|w| w.id), window.and_then(|w| w.active_pane))
        }
    };
    // The project rides on what was made - a new session, the pane of a
    // split, else the window - as its `@project` option. A remote
    // session's shadow window would only get a local option, so its
    // provider is asked to set it on the window there instead.
    if !l.project.is_empty() {
        let set = match (&l.remote_host, l.split, window) {
            (Some(host), _, w) => {
                let wid = w
                    .and_then(|w| {
                        format_expand(OptionTarget::Window(WindowId(w)), "#{window_remote_id}").ok()
                    })
                    .filter(|id| !id.is_empty());
                let r = match wid {
                    Some(id) => {
                        crate::view::act_remote(host, &id, "project_window", Some(&l.project)).await
                    }
                    None => Err("the new window's remote id is not known yet".to_string()),
                };
                if let Err(why) = r {
                    let _ = display_message(&format!("project not set on {host}: {why}"));
                }
                None
            }
            (None, None, _) if l.session.is_empty() => Some(format!("-t {}", quote(&target))),
            (None, Some(_), _) => pane.map(|p| format!("-p -t {}", quote(&format!("%{p}")))),
            (None, None, Some(w)) => Some(format!("-w -t {}", quote(&format!("@{w}")))),
            _ => None,
        };
        if let Some(set) = set {
            let _ = run_command(&format!("set-option {set} @project {}", quote(&l.project))).await;
        }
    }
    Ok(Landed { target, command, window, pane })
}

// ---------------------------------------------------------------------------
// the `new` verb with flags: an agent starting another agent, no form
// ---------------------------------------------------------------------------

/// The flags that make `new` start the agent itself: anything that says
/// what to start, or where. `@host`, `--client` and `--profile` alone
/// still open the form (the accounts panel's `n` asks that way).
const DIRECT_FLAGS: &[&str] = &[
    "--resume",
    "--fork",
    "--project",
    "-p",
    "--session",
    "-s",
    "--new-session",
    "--name",
    "-n",
    "--folder",
    "-c",
    "--command",
    "--split",
    "--window",
    "-w",
    "--focus",
    "--prompt",
    "--prompt-file",
];

pub fn is_direct(words: &[String]) -> bool {
    words
        .iter()
        .any(|w| DIRECT_FLAGS.contains(&w.split('=').next().unwrap_or("")))
}

/// The verb's text as words, split the way a shell splits a line:
/// blanks separate, single or double quotes hold a word together
/// (`--project 'Ooga booga'`), a backslash keeps the next character
/// outside single quotes. The verb itself is the first word.
pub fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (Some(_), '\\') => cur.push(chars.next().unwrap_or('\\')),
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// The pane the verb was run against (`-t $TMUX_PANE`): its folder,
/// session and `@project` are the defaults, and it is the pane a
/// `--split` divides.
pub struct Caller {
    pub pane: Option<u32>,
}

fn value<'a>(
    flag: &str,
    inline: Option<String>,
    it: &mut impl Iterator<Item = &'a String>,
) -> Result<String, String> {
    inline
        .or_else(|| it.next().cloned())
        .ok_or_else(|| format!("new: {flag} needs a value"))
}

/// Start an agent from the verb's words (after `new`), no form:
///
///   --resume <id>      resume that harness session (`claude:<uuid>`
///                      or the bare id; the roster row behind it gives
///                      the harness, folder, name and tag defaults)
///   --fork             as a copy of it
///   --command <line>   what to run: a launcher's name or a line
///                      (default: the harness, through its launcher)
///   --folder <dir>     where (default: the row's, else the caller's)
///   --session <name>   which session (default: the caller's)
///   --new-session      a session of its own, named after --name
///   --name <name>      the window's name (default: the row's name,
///                      else the folder's)
///   --project <tag>    its @project (default: the caller's)
///   --split            a pane beside the caller instead of a window
///   --window <w>       with --split: in that window (index, name, @id)
///   --focus            switch to it (default: made in the background)
///   --prompt <text>    its first prompt
///   --prompt-file <p>  its first prompt, read from that file
///   --profile <dir>    CLAUDE_CONFIG_DIR for it
///   @host              on that linked server
pub async fn direct(
    words: &[String],
    caller: Caller,
    launchers: Vec<(String, String)>,
) -> Result<Landed, String> {
    let mut resume: Option<String> = None;
    let mut fork = false;
    let mut project: Option<String> = None;
    let mut session: Option<String> = None;
    let mut own_session = false;
    let mut name: Option<String> = None;
    let mut folder: Option<String> = None;
    let mut command: Option<String> = None;
    let mut split = false;
    let mut window: Option<String> = None;
    let mut focus = false;
    let mut profile: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut prompt_file: Option<String> = None;
    let mut remote: Option<String> = None;
    let mut it = words.iter();
    while let Some(w) = it.next() {
        let (flag, inline) = match w.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v.to_string())),
            _ => (w.as_str(), None),
        };
        match flag {
            "--resume" => resume = Some(value(flag, inline, &mut it)?),
            "--fork" => fork = true,
            "--project" | "-p" => project = Some(value(flag, inline, &mut it)?),
            "--session" | "-s" => session = Some(value(flag, inline, &mut it)?),
            "--new-session" => own_session = true,
            "--name" | "-n" => name = Some(value(flag, inline, &mut it)?),
            "--folder" | "-c" => folder = Some(value(flag, inline, &mut it)?),
            "--command" => command = Some(value(flag, inline, &mut it)?),
            "--split" => split = true,
            "--window" | "-w" => {
                window = Some(value(flag, inline, &mut it)?);
                split = true;
            }
            "--focus" => focus = true,
            "--profile" => profile = Some(value(flag, inline, &mut it)?),
            "--prompt" => prompt = Some(value(flag, inline, &mut it)?),
            "--prompt-file" => prompt_file = Some(value(flag, inline, &mut it)?),
            // The form's: a client to open on. Nothing opens here.
            "--client" => {
                value(flag, inline, &mut it)?;
            }
            f if f.starts_with('@') && f.len() > 1 => remote = Some(f[1..].to_string()),
            f => return Err(format!("new: unknown flag {f}")),
        }
    }
    // The row behind the id, when the roster has it: its harness, its
    // folder, its name and its tag are the defaults.
    let (harness_hint, id) = match resume.as_deref() {
        Some(r) => match (r.split_once(':'), resumable_id(r)) {
            (Some((k, _)), Some(id)) => (Some(k.to_string()), Some(id)),
            _ => (None, Some(r.to_string())),
        },
        None => (None, None),
    };
    let row = match &id {
        Some(id) => {
            let ids: Vec<String> =
                ["claude", "codex", "opencode"].iter().map(|k| format!("{k}:{id}")).collect();
            crate::store::by_ids(&ids).await.unwrap_or_default().into_iter().next()
        }
        None => None,
    };
    // The caller's pane: folder, session, project.
    let here = caller
        .pane
        .and_then(|p| {
            format_expand(
                OptionTarget::Pane(PaneId(p)),
                "#{pane_current_path}\u{1}#{session_name}\u{1}#{@project}",
            )
            .ok()
        })
        .unwrap_or_default();
    let mut here = here.split('\u{1}').map(|s| Some(s.trim().to_string()).filter(|s| !s.is_empty()));
    let (here_dir, here_session, here_project) = (here.next().flatten(), here.next().flatten(), here.next().flatten());
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());

    let harness = harness_hint
        .or_else(|| row.as_ref().map(|a| a.kind.clone()))
        .or_else(|| {
            command
                .as_deref()
                .and_then(|c| c.split_whitespace().next())
                .map(basename)
                .filter(|w| matches!(w.as_str(), "claude" | "codex" | "opencode"))
        })
        .unwrap_or_else(|| "claude".to_string());
    let command = expand_command(&launchers, command.as_deref().unwrap_or(&harness));
    let command = match profile {
        Some(dir) => format!("CLAUDE_CONFIG_DIR={} {command}", quote(&dir)),
        None => command,
    };
    let dir = nonempty(folder)
        .or_else(|| nonempty(row.as_ref().and_then(|a| a.cwd.clone())))
        .or(here_dir)
        .ok_or("new: no folder - give --folder, or run it with -t <pane>")?;
    if remote.is_none() {
        match git::ensure_dir(&dir, false).await? {
            Ensured::Ready => {}
            Ensured::Missing => return Err(format!("new: no folder {dir}")),
        }
    }
    let session = if own_session {
        String::new()
    } else {
        nonempty(session)
            .or_else(|| {
                let host = remote.as_ref()?;
                list_sessions()
                    .ok()?
                    .into_iter()
                    .map(|s| s.name)
                    .find(|n| n.starts_with(&format!("{host}/")))
            })
            .or(here_session)
            .ok_or("new: no session - give --session, or run it with -t <pane>")?
    };
    if !session.is_empty() && !session_exists(&session) {
        return Err(format!("new: no session '{session}'"));
    }
    let name = nonempty(name)
        .or_else(|| nonempty(row.as_ref().map(crate::view::display_name)))
        .unwrap_or_else(|| basename(&dir));
    if session.is_empty() && session_exists(&session_name(&name)) {
        return Err(format!("new: session '{}' already exists", session_name(&name)));
    }
    let project = nonempty(project)
        .or(here_project)
        .or_else(|| nonempty(row.as_ref().and_then(|a| a.project.clone())))
        .unwrap_or_default();
    let split = if split {
        Some(match window {
            Some(w) => window_pane(&session, &w)?,
            None => caller.pane.ok_or("new: --split needs -t <pane>")?,
        })
    } else {
        None
    };
    launch(Launch {
        dir,
        name,
        command,
        harness,
        resume: id.unwrap_or_default(),
        fork,
        session,
        project,
        remote_host: remote,
        focus,
        split,
        prompt,
        prompt_file,
    })
    .await
}

/// The active pane of a window named by index, name or `@id` in a
/// session, for `--window`.
fn window_pane(session: &str, target: &str) -> Result<u32, String> {
    let sessions = list_sessions().map_err(|e| e.message.clone())?;
    let windows = list_windows().map_err(|e| e.message.clone())?;
    let s = sessions
        .iter()
        .find(|s| s.name == session)
        .ok_or_else(|| format!("new: no session '{session}'"))?;
    let id = if let Some(n) = target.strip_prefix('@') {
        n.parse::<u32>().ok().filter(|id| s.windows.iter().any(|(_, w)| w == id))
    } else if let Ok(i) = target.parse::<u32>() {
        s.windows.iter().find(|(idx, _)| *idx == i).map(|(_, id)| *id)
    } else {
        s.windows
            .iter()
            .map(|(_, id)| *id)
            .find(|id| windows.iter().any(|w| w.id == *id && w.name == target))
    };
    let id = id.ok_or_else(|| format!("new: no window '{target}' in {session}"))?;
    windows
        .iter()
        .find(|w| w.id == id)
        .and_then(|w| w.active_pane)
        .ok_or_else(|| format!("new: window '{target}' has no pane"))
}
