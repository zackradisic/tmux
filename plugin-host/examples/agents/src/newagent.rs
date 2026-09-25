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
    picker_mode: ModeId,
    /// The row's harness, for the shape of the resume flag.
    harness: String,
    /// The resume id, kept across the worktree kind, which has no field.
    resume: String,
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
) -> Vec<Field> {
    match kind {
        Kind::Window => vec![
            field("session", session.to_string()),
            field("folder", folder.to_string()),
            name,
            command,
            resume,
        ],
        Kind::Session => vec![field("folder", folder.to_string()), name, command, resume],
        Kind::Worktree => vec![
            field("repo", folder.to_string()),
            name,
            field("dest", String::new()),
            field("branch", String::new()),
            field("session", session.to_string()),
            command,
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
        "claude" | "codex" => Some(rest.to_string()),
        _ => None,
    }
}

/// The command line with the harness's resume flag: `claude --resume
/// <id>` (Claude, and any harness without a known form), `codex resume
/// <id>` (a subcommand; global flags in the launcher line stay ahead of
/// it).
pub fn resume_command(harness: &str, command: &str, resume: &str) -> String {
    match harness {
        "codex" => format!("{command} resume {resume}"),
        _ => format!("{command} --resume {resume}"),
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
    form.replace_fields(fields_for(next, &session, &folder, name, command, resume), "name");
    form.model.confirm_create = false;
    form.model.reuse = false;
    (next == Kind::Worktree && !folder.is_empty() && form.model.remote.is_none())
        .then_some(folder)
}

/// Open the form over the picker, prefilled from its highlighted row.
/// `picker` is read for the row and the picker's mode; the form is its
/// own float.
pub async fn open(picker: Rc<RefCell<Option<Picker>>>, client: Option<u64>) {
    if state().borrow().is_some() {
        return;
    }
    // What the row gives us, read under one borrow.
    let (picker_mode, launchers, session, remote, local_pane, harness, revive) = {
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
        // session is gone and a new one has to carry it.
        let revive = row.filter(|a| !a.live()).map(|a| {
            (
                resumable_id(&a.id).unwrap_or_default(),
                a.cwd.clone().unwrap_or_default(),
                old_session.clone().unwrap_or_default(),
            )
        });
        (p.mode, p.launchers.clone(), session, remote, local_pane, harness, revive)
    };
    // The folder: the row's pane cwd (a shadow's is the remote's cached
    // path), else the pressing client's pane, like `prefix S`.
    let client_info = client.and_then(|cid| {
        list_clients().ok()?.iter().find_map(|c| {
            (u64::from(c.id) == cid).then(|| (c.name.clone(), c.session))
        })
    });
    let client_pane = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window)
        .and_then(|w| resolve_window(WindowId(w)).ok())
        .and_then(|w| w.active_pane);
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
    let window = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s)).ok())
        .and_then(|v| v.current_window);
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
    // The old session's name for the new session, held as if typed so
    // the name's mirror (the folder's basename) leaves it be.
    let mut name = field("name", String::new());
    if kind == Kind::Session && !old_session.is_empty() {
        name.value = session_name(&old_session);
        name.touched = true;
    }
    let fields = fields_for(
        kind,
        &session,
        &folder,
        name,
        field("command", command),
        field("resume", resume.clone()),
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
        harness,
        resume,
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
                form.model.remote.is_some(),
                form.model.client_name.clone(),
                form.model.picker_mode,
                form.model.harness.clone(),
            ),
        )
    };
    let (confirmed, reuse, remote, client_name, picker_mode, harness) = model_bits;
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
    let command = match get("resume").as_str() {
        "" => command,
        id => resume_command(&harness, &command, id),
    };
    let session = get("session");
    let (cmd, target) = if !session.is_empty() {
        (
            format!(
                "new-window -t {} -c {} -n {} {}",
                quote(&format!("{session}:")),
                quote(&dir),
                quote(&name),
                quote(&command)
            ),
            session.clone(),
        )
    } else {
        let sess = session_name(&name);
        (
            format!(
                "new-session -d -s {} -c {} {}",
                quote(&sess),
                quote(&dir),
                quote(&command)
            ),
            sess,
        )
    };
    if let Err(e) = run_command(&cmd).await {
        form::fail(&st, mode, e.message.clone());
        return;
    }
    let _ = mode_close(mode);
    *st.borrow_mut() = None;
    // The picker too: the point of starting an agent is to talk to it.
    let _ = mode_close(picker_mode);
    if let Some(client) = client_name {
        let _ = run_command(&format!(
            "switch-client -c {} -t {}",
            quote(&client),
            quote(&format!("{target}:"))
        ))
        .await;
    }
    let _ = display_message(&format!("agent started: {command} in {target}"));
}
