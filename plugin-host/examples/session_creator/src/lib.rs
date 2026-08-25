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
//! clears a field, C-j/C-k move between fields).
//!
//! Load server-scoped with caps `mode`, `run-process`, `run-command`.
//!
//! Build: cargo build -p session_creator --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::rc::Rc;

use tmux_plugin_sdk::prelude::*;

/// Form geometry (cells). Height fits the fields + error + hint lines.
const FORM_WIDTH: u32 = 64;
const FORM_HEIGHT: u32 = 12;

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

struct Form {
    mode: ModeId,
    width: u32,
    height: u32,
    kind: Kind,
    /// Detected repo root, kept to mark the repo field with ✓ while its
    /// value still matches the detection. The field itself is always
    /// editable; `git worktree add` uses whatever it holds on submit.
    detected: Option<String>,
    /// Pressing client (name, current session) for the final
    /// switch-client; resolved once when the form opens.
    client_name: Option<String>,
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
        self.sync_mirrors();
        detect
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
        .ok()
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .any(|s| s.get("name").and_then(|n| n.as_str()) == Some(name))
            })
        })
        .unwrap_or(false)
}

struct Shared {
    form: Option<Form>,
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
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from));
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
        list_clients().ok()?.as_array()?.iter().find_map(|c| {
            (c.get("id").and_then(|v| v.as_u64()) == Some(cid)).then(|| {
                (
                    c.get("name").and_then(|v| v.as_str()).map(String::from),
                    c.get("session").and_then(|v| v.as_u64()),
                )
            })
        })
    });
    let window = client_info
        .as_ref()
        .and_then(|(_, s)| *s)
        .and_then(|s| resolve_session(SessionId(s as u32)).ok())
        .and_then(|v| v.get("current_window").and_then(|w| w.as_u64()));
    let Some(window) = window else {
        log("session_creator: cannot resolve the client's current window");
        let _ = display_message("session_creator: no client to open the form for");
        return;
    };

    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window as u32)),
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
        kind,
        detected: repo,
        client_name: client_info.and_then(|(name, _)| name),
        // A prefilled first field is usually accepted as-is: start on
        // name (Up selects it to change it). An empty one must be
        // filled first: start there.
        focused: if fields[0].value.is_empty() { 0 } else { 1 },
        fields,
        error: None,
        confirm_create: false,
        busy: false,
    };
    form.sync_mirrors();
    render(&form);
    state.borrow_mut().form = Some(form);
}

fn render(form: &Form) {
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
        let mark = if f.label == "repo" && form.detected.as_ref() == Some(&f.value) {
            " \x1b[32m✓\x1b[0m"
        } else {
            ""
        };
        let val = clip(&f.value, w.saturating_sub(14));
        if focused {
            out.push_str(&format!(
                "  \x1b[1m{:<7}\x1b[0m \x1b[7m{val}\x1b[27m\x1b[7m \x1b[0m{mark}\r\n",
                f.label
            ));
        } else {
            out.push_str(&format!("  {:<7} {val}{mark}\r\n", f.label));
        }
    }
    out.push_str("\r\n");
    if let Some(err) = &form.error {
        out.push_str(&format!("  \x1b[31m{}\x1b[0m\r\n", clip(err, w - 4)));
    } else if form.busy {
        let doing = match form.kind {
            Kind::Plain => "creating session...",
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
    out.push_str(&format!(
        "\r\n  \x1b[2mC-t {other} · Tab next · Enter create · Esc cancel\x1b[0m"
    ));
    let _ = mode_write(form.mode, out.as_bytes());
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
    let (mode, kind, values, client_name, confirmed) = {
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
                    || form.value("branch").is_empty()
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
        form.busy = true;
        form.error = None;
        render(form);
        (form.mode, form.kind, values, form.client_name.clone(), confirmed)
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
        Ok(SessionCreator { state: Rc::new(RefCell::new(Shared { form: None })) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.event.as_str() {
            "plugin-command" => {
                let kind = match event.data.get("text").and_then(|v| v.as_str()) {
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
                if event.data.get("mode").and_then(|v| v.as_u64()) != Some(form.mode.0)
                {
                    return;
                }
                let Some(key) = event.data.get("key").and_then(|v| v.as_str()) else {
                    return;
                };
                if form.busy {
                    return;
                }
                match key {
                    "Escape" => {
                        let _ = mode_close(form.mode);
                    }
                    "Enter" => {
                        drop(st);
                        ctx.spawn(submit(Rc::clone(&self.state)));
                    }
                    "C-t" => {
                        let detect = form.toggle();
                        render(form);
                        if let Some(folder) = detect {
                            let mode = form.mode;
                            drop(st);
                            ctx.spawn(detect_repo(
                                Rc::clone(&self.state),
                                mode,
                                folder,
                            ));
                        }
                    }
                    "Tab" | "Down" | "C-j" => {
                        form.focused = (form.focused + 1) % form.fields.len();
                        render(form);
                    }
                    "BTab" | "Up" | "C-k" => {
                        form.focused =
                            (form.focused + form.fields.len() - 1) % form.fields.len();
                        render(form);
                    }
                    "BSpace" => {
                        let i = form.focused;
                        form.fields[i].value.pop();
                        form.fields[i].touched = !form.fields[i].value.is_empty()
                            && form.fields[i].touched;
                        form.error = None;
                        form.confirm_create = false;
                        form.sync_mirrors();
                        render(form);
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
                        form.sync_mirrors();
                        render(form);
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
                        render(form);
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
                        form.sync_mirrors();
                        render(form);
                    }
                    _ => {}
                }
            }
            "mode-resize" => {
                let mut st = self.state.borrow_mut();
                let Some(form) = st.form.as_mut() else { return };
                if event.data.get("mode").and_then(|v| v.as_u64()) != Some(form.mode.0)
                {
                    return;
                }
                if let Some(w) = event.data.get("width").and_then(|v| v.as_u64()) {
                    form.width = w as u32;
                }
                if let Some(h) = event.data.get("height").and_then(|v| v.as_u64()) {
                    form.height = h as u32;
                }
                render(form);
            }
            "mode-closed" => {
                let mut st = self.state.borrow_mut();
                if st
                    .form
                    .as_ref()
                    .is_some_and(|f| {
                        event.data.get("mode").and_then(|v| v.as_u64())
                            == Some(f.mode.0)
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
