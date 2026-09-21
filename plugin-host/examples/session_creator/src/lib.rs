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
//! The form, its completion lists and the git steps come from the
//! `formkit` crate; this file is what the fields mean (the two kinds,
//! their mirror rules) and what Enter does (a session).
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
//! Load server-scoped with caps `mode`, `run-process`, `run-command`,
//! `fs-list`, `fs-read-any` (the last three are `formkit::complete::CAPS`).
//!
//! Build: cargo build -p session_creator --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::rc::Rc;

use formkit::complete::{RowKind, Source};
use formkit::form::{self, field, Action, Field, Form, Model, Shared};
use formkit::git::{self, Ensured};
use formkit::text::{basename, dest_base, quote, scan_base};
use tmux_plugin_sdk::prelude::*;

/// Form geometry (cells). The height is the closed form; an open list
/// adds a rule and up to `LIST_MAX` rows through `mode_resize`.
const FORM_WIDTH: u32 = 76;
const FORM_HEIGHT: u32 = 12;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Worktree,
}

/// What the session creator knows about its fields: which kind is up,
/// what was detected, and the two one-shot states an Enter can leave
/// behind (a folder to confirm, a worktree to reopen).
struct Creator {
    kind: Kind,
    /// Detected repo root, kept to mark the repo field with ✓ while its
    /// value still matches the detection. The field itself is always
    /// editable; `git worktree add` uses whatever it holds on submit.
    detected: Option<String>,
    /// Pressing client for the final switch-client; resolved once when
    /// the form opens.
    client_name: Option<String>,
    /// Plain kind: the folder was missing on the last Enter; the next
    /// Enter creates it with `mkdir -p`. Any edit disarms this.
    confirm_create: bool,
    /// `dest` holds a worktree that already exists, so submit must not
    /// run `git worktree add`. Any edit of repo or dest clears it.
    reuse: bool,
}

fn value_of(fields: &[Field], label: &str) -> String {
    fields
        .iter()
        .find(|f| f.label == label)
        .map(|f| f.value.trim().to_string())
        .unwrap_or_default()
}

impl Model for Creator {
    fn source(&self, fields: &[Field], i: usize) -> Option<Source> {
        let f = &fields[i];
        match f.label {
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
            _ => None,
        }
    }

    /// Mirror rules, per kind: worktree — dest/branch follow name (dest
    /// base from repo); plain — name follows basename(folder). A touched
    /// field stops following.
    fn mirror(&mut self, fields: &mut [Field]) {
        fn idx(fields: &[Field], label: &str) -> usize {
            fields.iter().position(|f| f.label == label).unwrap()
        }
        match self.kind {
            Kind::Worktree => {
                let name = value_of(fields, "name");
                let base = dest_base(&value_of(fields, "repo"));
                let di = idx(fields, "dest");
                if !fields[di].touched {
                    fields[di].value = format!("{base}{name}");
                }
                let bi = idx(fields, "branch");
                if !fields[bi].touched {
                    fields[bi].value = name;
                }
            }
            Kind::Plain => {
                let folder = value_of(fields, "folder");
                let ni = idx(fields, "name");
                if !fields[ni].touched {
                    fields[ni].value = basename(&folder);
                }
            }
        }
    }

    fn accepted(&mut self, _fields: &[Field], _i: usize, kind: RowKind) {
        self.confirm_create = false;
        // Picking an existing worktree turns Enter into "open it".
        self.reuse = kind == RowKind::Worktree;
    }

    fn edited(&mut self, _fields: &[Field], _i: usize) {
        self.confirm_create = false;
        self.reuse = false;
    }

    fn title(&self) -> String {
        "New Session".into()
    }

    fn kinds(&self) -> Option<(Vec<&'static str>, usize)> {
        let active = match self.kind {
            Kind::Plain => 0,
            Kind::Worktree => 1,
        };
        Some((vec!["plain", "worktree"], active))
    }

    fn toggle_hint(&self) -> Option<String> {
        Some(
            match self.kind {
                Kind::Plain => "worktree",
                Kind::Worktree => "plain",
            }
            .into(),
        )
    }

    fn banner(&self, fields: &[Field]) -> Option<String> {
        if !fields[0].value.trim().is_empty() {
            return None;
        }
        Some(
            match self.kind {
                Kind::Plain => "no folder detected — enter one:",
                Kind::Worktree => "no git repo detected — enter one:",
            }
            .into(),
        )
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

    fn status(&self, _fields: &[Field], busy: bool) -> Option<String> {
        if busy {
            return Some(
                match self.kind {
                    Kind::Plain => "creating session...",
                    Kind::Worktree if self.reuse => "opening worktree...",
                    Kind::Worktree => "creating worktree...",
                }
                .into(),
            );
        }
        if self.confirm_create {
            return Some("folder does not exist — Enter again to create it".into());
        }
        None
    }
}

type State = Shared<Creator>;

struct SessionCreator {
    state: State,
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

/// Switch plain ↔ worktree in place, carrying name across and mapping
/// folder ↔ repo. Returns the folder to run repo detection on when
/// entering the worktree kind.
fn toggle(form: &mut Form<Creator>) -> Option<String> {
    let name = form.fields[form.idx("name")].clone();
    let detect = match form.model.kind {
        Kind::Plain => {
            let folder = form.value("folder");
            form.model.kind = Kind::Worktree;
            form.replace_fields(
                vec![
                    field("repo", folder.clone()),
                    name,
                    field("dest", String::new()),
                    field("branch", String::new()),
                ],
                "name",
            );
            (!folder.is_empty()).then_some(folder)
        }
        Kind::Worktree => {
            let repo = form.value("repo");
            form.model.kind = Kind::Plain;
            form.replace_fields(vec![field("folder", repo), name], "name");
            None
        }
    };
    form.model.confirm_create = false;
    form.model.reuse = false;
    detect
}

/// Context gathering + form open, run asynchronously after the
/// plugin-command event. Detected values only prefill (always editable)
/// fields.
async fn open_form(state: State, kind: Kind, target_pane: Option<u64>, client: Option<u64>) {
    // Folder/repo from the target pane's cwd (highlighted item in the
    // chooser, current pane otherwise).
    let cwd = target_pane
        .and_then(|p| resolve_pane(PaneId(p as u32)).ok())
        .and_then(|p| (!p.cwd.is_empty()).then_some(p.cwd));
    let repo = match kind {
        Kind::Worktree => match &cwd {
            Some(dir) => git::root(dir).await,
            None => None,
        },
        Kind::Plain => None,
    };

    // The form must open where the user is looking: the pressing
    // client's current window (the target may be in another session).
    let client_info = client.and_then(|cid| {
        list_clients().ok()?.iter().find_map(|c| {
            (u64::from(c.id) == cid).then(|| (Some(c.name.clone()), c.session))
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
    // A prefilled first field is usually accepted as-is: start on name
    // (Up selects it to change it). An empty one must be filled first:
    // start there.
    let focused = if fields[0].value.is_empty() { 0 } else { 1 };

    let model = Creator {
        kind,
        detected: repo,
        client_name: client_info.and_then(|(name, _)| name),
        confirm_create: false,
        reuse: false,
    };
    let mut form = Form::new(mode, FORM_WIDTH, FORM_HEIGHT, fields, model);
    form.focused = focused;
    form::render(&mut form);
    *state.borrow_mut() = Some(form);

    form::start_scan(&state, mode, false);
}

/// Repo detection after a plain → worktree toggle. Applies only if the
/// form still shows the same untouched folder value in the same mode.
async fn detect_repo(state: State, mode: ModeId, folder: String) {
    let Some(root) = git::root(&folder).await else { return };
    let mut st = state.borrow_mut();
    let Some(form) = st.as_mut() else { return };
    if form.mode.0 != mode.0 || form.model.kind != Kind::Worktree {
        return;
    }
    let ri = form.idx("repo");
    if form.fields[ri].value != folder {
        return;
    }
    form.fields[ri].value = root.clone();
    form.model.detected = Some(root);
    form.model.mirror(&mut form.fields);
    form::render(form);
}

/// Validate and run the creation pipeline for the current kind. On
/// failure the form stays up with the error rendered; on success the
/// mode closes and the client is switched to the new session.
async fn submit(state: State) {
    let (mode, kind, values, client_name, confirmed, reuse) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut() else { return };
        let mut err = match form.model.kind {
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
                    || (!form.model.reuse && form.value("branch").is_empty())
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
            form::render(form);
            return;
        }
        let values: Vec<String> =
            form.fields.iter().map(|f| f.value.trim().to_string()).collect();
        let confirmed = form.model.confirm_create;
        let reuse = form.model.reuse;
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        (
            form.mode,
            form.model.kind,
            values,
            form.model.client_name.clone(),
            confirmed,
            reuse,
        )
    };

    let (dir, name) = match kind {
        Kind::Plain => {
            let folder = values[0].clone();
            let name = values[1].clone();
            match git::ensure_dir(&folder, confirmed).await {
                Ok(Ensured::Ready) => {}
                Ok(Ensured::Missing) => {
                    // Ask for one more Enter before creating the folder.
                    let mut st = state.borrow_mut();
                    if let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) {
                        form.busy = false;
                        form.model.confirm_create = true;
                        form::render(form);
                    }
                    return;
                }
                Err(e) => {
                    form::fail(&state, mode, e);
                    return;
                }
            }
            (folder, name)
        }
        Kind::Worktree => {
            let (repo, name, dest, branch) =
                (values[0].clone(), values[1].clone(), values[2].clone(), values[3].clone());
            // A worktree picked from the list already exists: open it.
            if !reuse {
                if let Err(e) = git::add_worktree(&repo, &dest, &branch).await {
                    form::fail(&state, mode, e);
                    return;
                }
            }
            (dest, name)
        }
    };

    // The session first, the close second: a duplicate name (or any
    // new-session error) renders in the still-open form.
    let sess = session_name(&name);
    if let Err(e) =
        run_command(&format!("new-session -d -s {} -c {}", quote(&sess), quote(&dir))).await
    {
        form::fail(&state, mode, e.message.clone());
        return;
    }
    let _ = mode_close(mode);
    *state.borrow_mut() = None;

    if let Some(client) = client_name {
        if let Err(e) =
            run_command(&format!("switch-client -c {} -t {}", quote(&client), quote(&sess)))
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

impl Plugin for SessionCreator {
    const NAME: &'static str = "session_creator";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        Ok(SessionCreator { state: Rc::new(RefCell::new(None)) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => {
                let kind = match event.get_str("text") {
                    Some("new") => Kind::Plain,
                    Some("worktree") => Kind::Worktree,
                    _ => return,
                };
                if self.state.borrow().is_some() {
                    let _ = display_message("session_creator: form already open");
                    return;
                }
                let target_pane = event.scope.pane.map(u64::from);
                let client = event.scope.client.map(u64::from);
                let state = Rc::clone(&self.state);
                ctx.spawn(open_form(state, kind, target_pane, client));
            }
            "mode-key" => {
                let Some(key) = event.get_str("key") else { return };
                // What the key did, decided inside the borrow and acted
                // on after it: the form's own keys through formkit, the
                // kind swap here.
                let (mode, action, detect) = {
                    let mut st = self.state.borrow_mut();
                    let Some(form) = st.as_mut() else { return };
                    if event.get_i64("mode") != Some(form.mode.0 as i64) {
                        return;
                    }
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
                    ctx.spawn(detect_repo(Rc::clone(&self.state), mode, folder));
                }
                match action {
                    Action::None | Action::Toggle => {}
                    Action::Rescan { reveal } => form::start_scan(&self.state, mode, reveal),
                    Action::Probe => form::kick_probe(&self.state, mode),
                    Action::Submit => {
                        ctx.spawn(submit(Rc::clone(&self.state)));
                    }
                    Action::Close => {
                        let _ = mode_close(mode);
                    }
                }
            }
            "mode-resize" => {
                let mut st = self.state.borrow_mut();
                let Some(form) = st.as_mut() else { return };
                if event.get_i64("mode") != Some(form.mode.0 as i64) {
                    return;
                }
                form.resized(event.get_i64("width"), event.get_i64("height"));
                form::render(form);
            }
            "mode-closed" => {
                let mut st = self.state.borrow_mut();
                if st.as_ref().is_some_and(|f| event.get_i64("mode") == Some(f.mode.0 as i64)) {
                    *st = None;
                }
            }
            _ => {}
        }
    }
}

tmux_plugin!(SessionCreator);
