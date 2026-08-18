//! SDK example plugin: create git worktrees from the choose-tree chooser.
//!
//! Wire keys to it in ~/.tmux.conf:
//!
//! ```tmux
//! bind -T choose-tree W plugin-command worktree new   # repo from highlight
//! bind W plugin-command worktree new                  # repo from current pane
//! ```
//!
//! From the chooser (`prefix w` / `prefix s`) the key closes it and
//! delivers a `plugin-command` event whose target is the *highlighted*
//! item (a session row resolves to its active pane); from a plain
//! binding the target is the current pane. The plugin detects the git
//! repo from that pane's cwd and opens a small form (a plugin UI mode)
//! on the window the pressing client is actually looking at. Every
//! field is editable — the detected repo is just a prefill (✓ while
//! unchanged, C-u clears, Up from name selects it) — and on Enter it
//! runs `git worktree add`, then creates a session rooted in the new
//! worktree and switches the client to it. ("pick" is accepted as an
//! alias of "new" for older configs.)
//!
//! Load server-scoped with caps `mode`, `run-process`, `run-command`.
//!
//! Build: cargo build -p worktree --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::rc::Rc;

use tmux_plugin_sdk::prelude::*;

/// Form geometry (cells). Height fits the fields + error + hint lines.
const FORM_WIDTH: u32 = 64;
const FORM_HEIGHT: u32 = 12;

struct Field {
    label: &'static str,
    value: String,
    /// Once the user edits a field it stops mirroring `name`.
    touched: bool,
}

struct Form {
    mode: ModeId,
    width: u32,
    height: u32,
    /// Detected repo root, kept to mark the repo field with ✓ while its
    /// value still matches the detection. The field itself is always
    /// editable; `git worktree add` uses whatever it holds on submit.
    detected: Option<String>,
    /// Pressing client (name, current session) for the final
    /// switch-client; resolved once when the form opens.
    client_name: Option<String>,
    /// Field order: repo, name, dest, branch.
    fields: Vec<Field>,
    focused: usize,
    error: Option<String>,
    /// A `git worktree add` is in flight; input is ignored until it
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

    fn repo_path(&self) -> String {
        self.value("repo")
    }

    /// dest/branch mirror `name` until individually edited.
    fn sync_mirrors(&mut self) {
        let name = self.value("name");
        let base = dest_base(&self.repo_path());
        let di = self.idx("dest");
        if !self.fields[di].touched {
            self.fields[di].value = format!("{base}{name}");
        }
        let bi = self.idx("branch");
        if !self.fields[bi].touched {
            self.fields[bi].value = name;
        }
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

struct Shared {
    form: Option<Form>,
}

type State = Rc<RefCell<Shared>>;

struct Worktree {
    state: State,
}

/// Repo detection + form-open context, gathered asynchronously after the
/// plugin-command event. The detected repo only prefills the (always
/// editable) repo field.
async fn open_form(state: State, target_pane: Option<u64>, client: Option<u64>) {
    // Repo from the highlighted item's pane cwd.
    let cwd = target_pane
        .and_then(|p| resolve_pane(PaneId(p as u32)).ok())
        .and_then(|v| v.get("cwd").and_then(|c| c.as_str()).map(String::from));
    // Two lines: the worktree toplevel and the absolute common .git dir.
    // When the pane sits inside a linked worktree, prefer the main
    // working tree (common dir minus "/.git") so the dest prefill doesn't
    // nest worktrees inside worktrees.
    let repo = match &cwd {
        Some(dir) => match run_job(
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
        },
        None => None,
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
        log("worktree: cannot resolve the client's current window");
        let _ = display_message("worktree: no client to open the form for");
        return;
    };

    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window as u32)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("new worktree".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("worktree: mode_open failed: {}", e.message));
            return;
        }
    };

    let mut fields = Vec::new();
    fields.push(Field {
        label: "repo",
        value: repo.clone().unwrap_or_default(),
        touched: false,
    });
    fields.push(Field { label: "name", value: String::new(), touched: false });
    fields.push(Field { label: "dest", value: String::new(), touched: false });
    fields.push(Field { label: "branch", value: String::new(), touched: false });

    let mut form = Form {
        mode,
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        detected: repo,
        client_name: client_info.and_then(|(name, _)| name),
        // A prefilled repo is usually accepted as-is: start on name (Up
        // selects the repo field to change it). An empty repo field must
        // be filled first: start there.
        focused: if fields[0].value.is_empty() { 0 } else { 1 },
        fields,
        error: None,
        busy: false,
    };
    form.sync_mirrors();
    render(&form);
    state.borrow_mut().form = Some(form);
}

fn render(form: &Form) {
    let w = form.width as usize;
    let mut out = String::from("\x1b[2J\x1b[H\r\n");
    if form.fields[0].value.trim().is_empty() {
        out.push_str("  \x1b[33mno git repo detected — enter one:\x1b[0m\r\n\r\n");
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
        out.push_str("  \x1b[33mcreating worktree...\x1b[0m\r\n");
    } else {
        out.push_str("\r\n");
    }
    out.push_str("\r\n  \x1b[2mTab next · Enter create · Esc cancel\x1b[0m");
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

/// Validate and run the creation pipeline. On failure the form stays up
/// with the error rendered; on success the mode closes and the client is
/// switched to a fresh session rooted in the worktree.
async fn submit(state: State) {
    let (mode, repo, name, dest, branch, client_name) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.form.as_mut() else { return };
        let repo = form.repo_path();
        let name = form.value("name");
        let dest = form.value("dest");
        let branch = form.value("branch");
        let err = if repo.is_empty() {
            Some("repo is required")
        } else if name.is_empty() {
            Some("name is required")
        } else if dest.is_empty() || branch.is_empty() {
            Some("dest and branch are required")
        } else {
            None
        };
        if let Some(err) = err {
            form.error = Some(err.to_string());
            render(form);
            return;
        }
        form.busy = true;
        form.error = None;
        render(form);
        (form.mode, repo, name, dest, branch, form.client_name.clone())
    };

    // Existing branch -> check it out; otherwise create it with -b.
    let exists = run_job(
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
    let add = if exists {
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

    let _ = mode_close(mode);
    state.borrow_mut().form = None;

    // A session rooted in the new worktree; tolerate an existing one
    // with the same name (the switch below still lands somewhere sane).
    let sess = session_name(&name);
    if let Err(e) = run_command(&format!(
        "new-session -d -s {} -c {}",
        quote(&sess),
        quote(&dest)
    ))
    .await
    {
        log(&format!("worktree: new-session failed: {}", e.message));
    }
    if let Some(client) = client_name {
        if let Err(e) = run_command(&format!(
            "switch-client -c {} -t {}",
            quote(&client),
            quote(&sess)
        ))
        .await
        {
            log(&format!("worktree: switch-client failed: {}", e.message));
        }
    }
    let _ = display_message(&format!("worktree ready: {dest}"));
}

/// Re-enable the form with an error, unless it was closed meanwhile.
fn fail(state: &State, mode: ModeId, error: String) {
    let mut st = state.borrow_mut();
    if let Some(form) = st.form.as_mut().filter(|f| f.mode.0 == mode.0) {
        form.busy = false;
        form.error = Some(error);
        render(form);
    }
}

impl Plugin for Worktree {
    const NAME: &'static str = "worktree";
    type Config = serde_json::Value;

    fn init(ctx: &Ctx, _config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        Ok(Worktree { state: Rc::new(RefCell::new(Shared { form: None })) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.event.as_str() {
            "plugin-command" => {
                // "pick" is a historical alias: the repo field is always
                // editable now, so both commands open the same form.
                if !matches!(
                    event.data.get("text").and_then(|v| v.as_str()),
                    Some("new") | Some("pick")
                ) {
                    return;
                }
                if self.state.borrow().form.is_some() {
                    let _ = display_message("worktree: form already open");
                    return;
                }
                let target_pane = event.scope.pane.map(u64::from);
                let client = event.scope.client.map(u64::from);
                let state = Rc::clone(&self.state);
                ctx.spawn(open_form(state, target_pane, client));
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
                    "Tab" | "Down" => {
                        form.focused = (form.focused + 1) % form.fields.len();
                        render(form);
                    }
                    "BTab" | "Up" => {
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
                        if form.fields[i].label == "name"
                            || form.fields[i].label == "repo"
                        {
                            form.sync_mirrors();
                        }
                        render(form);
                    }
                    "C-u" => {
                        let i = form.focused;
                        form.fields[i].value.clear();
                        form.fields[i].touched = false;
                        form.error = None;
                        form.sync_mirrors();
                        render(form);
                    }
                    "Space" => {
                        let i = form.focused;
                        form.fields[i].value.push(' ');
                        if form.fields[i].label != "name" {
                            form.fields[i].touched = true;
                        }
                        form.error = None;
                        form.sync_mirrors();
                        render(form);
                    }
                    k if k.chars().count() == 1
                        && !k.chars().next().unwrap().is_control() =>
                    {
                        let i = form.focused;
                        form.fields[i].value.push_str(k);
                        if form.fields[i].label != "name"
                            && form.fields[i].label != "repo"
                        {
                            form.fields[i].touched = true;
                        }
                        form.error = None;
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

tmux_plugin!(Worktree);
