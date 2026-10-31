//! The hand-off form: `n` in the channel view starts an agent with the
//! selected messages as its first prompt.
//!
//! The messages are written to a file first (`prompts/<channel>-<first
//! ts>.md` under the plugin's data directory, the transcript `y` would
//! have copied under a `From Slack #channel on <date>:` header), and the
//! form - the agents plugin's new-agent fields, through `formkit` - asks
//! what to say above them (`ask`), where the agent runs and what runs.
//! `C-e` opens the file in `$EDITOR` in a popup; the form waits for it
//! through `wait-for` and shows the new line count. The `prompt` field
//! picks the file or the tmux paste buffer instead (`Tab`/`Space`
//! toggle). Enter rewrites the file as the ask, a blank line and the
//! body, then asks the agents plugin to start the agent:
//! `new --folder … --prompt-file <path>` through its `new` service, or,
//! on a server whose agents plugin has no such service, the same verb
//! through `plugin-command` with the answer read back from the caller
//! pane's `@agents_new` option. The result shows in the form, which then
//! closes on its own or on any key; an error keeps it open.
//!
//! Borrow discipline as in chat.rs: decide under the form's borrow, drop
//! it, then run the async work.

use std::cell::RefCell;
use std::rc::Rc;

use formkit::complete::Source;
use formkit::form::{self, field, Action, Field, Form, Model, Shared};
use formkit::text::{quote, scan_base};
use tmux_plugin_sdk::abi::ErrorCode;
use tmux_plugin_sdk::prelude::*;

use crate::{chat, timefmt};

const FORM_WIDTH: u32 = 76;
/// Tag, rule, banner, 7 fields, blank, status, blank, hint.
const FORM_HEIGHT: u32 = 15;
/// How long a successful form stays up before it closes itself.
const LINGER_MS: u64 = 1500;
/// `fs_read` chunk when the prompt file is read back.
const CHUNK: usize = 64 * 1024;

/// Where the prompt comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptSource {
    /// The file under the data directory (the default).
    File,
    /// The tmux paste buffer, saved into the same file at submit.
    Clipboard,
}

impl PromptSource {
    fn toggle(self) -> Self {
        match self {
            PromptSource::File => PromptSource::Clipboard,
            PromptSource::Clipboard => PromptSource::File,
        }
    }
}

/// What the channel view hands over when `n` is pressed.
pub struct Request {
    pub st: crate::Shared,
    /// "#general", "@tejas" - the view's title.
    pub label: String,
    /// The channel's bare name, for the file and the window name.
    pub channel: String,
    pub thread: bool,
    /// The oldest selected message's ts, for the file name.
    pub first_ts: String,
    /// The messages as `chat::transcript` renders them.
    pub transcript: String,
    pub count: usize,
    pub client: Option<u64>,
    pub pane: Option<u32>,
    pub tz: i64,
}

pub struct Handoff {
    st: crate::Shared,
    label: String,
    count: usize,
    /// The prompt file: its absolute path, and its path under the data
    /// directory (what `fs_write`/`fs_read` take, and what the field
    /// shows).
    path: String,
    rel: String,
    /// What the file holds below the ask: the header and the
    /// transcript, or whatever the editor left there.
    body: String,
    source: PromptSource,
    /// The client the form was opened from, for the popup.
    client_name: Option<String>,
    /// The pane the view was opened against: the `new` verb's `-t`.
    pane: Option<u32>,
    /// `<tmux_binary> -S <socket_path>`, for the popup's `wait-for`.
    tmux: Option<(String, String)>,
    /// The editor popup is up; keys wait for it.
    editing: bool,
    /// The agent started: what the status says; any key closes.
    done: Option<String>,
}

impl Handoff {
    fn prompt_value(&self) -> String {
        prompt_label(self.source, &self.rel, line_count(&self.body))
    }
}

impl Model for Handoff {
    fn source(&self, fields: &[Field], i: usize) -> Option<Source> {
        match fields[i].label {
            // Needs `fs-list` and `fs-read-any`: without them the list
            // says so instead of completing, and the field still takes
            // a typed path.
            "folder" => Some(Source::Dirs { base: scan_base(&fields[i].value) }),
            _ => None,
        }
    }

    fn mirror(&mut self, _fields: &mut [Field]) {}

    fn title(&self) -> String {
        "Agent from Slack".into()
    }

    fn banner(&self, _fields: &[Field]) -> Option<String> {
        Some(format!(
            "{} message{} from {} → {}",
            self.count,
            if self.count == 1 { "" } else { "s" },
            self.label,
            self.rel
        ))
    }

    fn status(&self, _fields: &[Field], busy: bool) -> Option<String> {
        if let Some(d) = &self.done {
            return Some(format!("{d} · any key closes"));
        }
        if self.editing {
            return Some("editing the prompt in $EDITOR - close it to come back".into());
        }
        if busy {
            return Some("starting the agent…".into());
        }
        None
    }

    fn submit_label(&self) -> &'static str {
        "start"
    }

    fn extra_hint(&self) -> Option<String> {
        Some("C-e edit the prompt in $EDITOR".into())
    }
}

thread_local! {
    static STATE: Shared<Handoff> = Rc::new(RefCell::new(None));
}

fn state() -> Shared<Handoff> {
    STATE.with(Rc::clone)
}

/// Whether the mode id is the form's.
pub fn owns(mode: Option<i64>) -> bool {
    state().borrow().as_ref().is_some_and(|f| Some(f.mode.0 as i64) == mode)
}

pub fn close() {
    let mode = state().borrow().as_ref().map(|f| f.mode);
    if let Some(m) = mode {
        let _ = mode_close(m);
    }
    *state().borrow_mut() = None;
}

// ---------------------------------------------------------------------------
// pure pieces (tested)
// ---------------------------------------------------------------------------

/// `From Slack #general on 2026-10-30:` (` · thread` after a thread's
/// label). `date` is `YYYY-MM-DD`.
pub fn header(label: &str, thread: bool, date: &str) -> String {
    format!("From Slack {label}{} on {date}:", if thread { " · thread" } else { "" })
}

/// The prompt file: the ask above a blank line, then the body (the
/// header, a blank line, the transcript - or what the editor left).
/// With no ask, the body alone.
pub fn prompt_text(ask: &str, body: &str) -> String {
    let ask = ask.trim();
    if ask.is_empty() {
        body.to_string()
    } else {
        format!("{ask}\n\n{body}")
    }
}

/// The file's first body: the header, a blank line, the transcript.
pub fn initial_body(header: &str, transcript: &str) -> String {
    format!("{header}\n\n{transcript}")
}

/// `prompts/general-1700000000.000100.md`: the channel's name with
/// anything a shell or a file system minds replaced, and the first
/// message's ts.
pub fn prompt_rel(channel: &str, first_ts: &str) -> String {
    let safe: String = channel
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' })
        .collect();
    let safe = if safe.is_empty() { "slack".to_string() } else { safe };
    let ts: String = first_ts.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    format!("prompts/{safe}-{ts}.md")
}

/// What the `prompt` field shows.
pub fn prompt_label(source: PromptSource, rel: &str, lines: usize) -> String {
    match source {
        PromptSource::File => format!("file: {rel} ({lines} line{})", if lines == 1 { "" } else { "s" }),
        PromptSource::Clipboard => "clipboard (the tmux paste buffer)".into(),
    }
}

pub fn line_count(text: &str) -> usize {
    text.lines().count()
}

/// The `new` verb's words for the form's values: every field the user
/// could change is spelled out, so the agents plugin's own defaults
/// (its caller pane) never override what the form showed.
pub fn new_words(folder: &str, session: &str, name: &str, command: &str, project: &str, path: &str) -> Vec<String> {
    let mut w: Vec<String> = vec!["--folder".into(), folder.into()];
    if session.trim().is_empty() || session.trim() == "new" {
        w.push("--new-session".into());
    } else {
        w.push("--session".into());
        w.push(session.into());
    }
    w.push("--name".into());
    w.push(name.into());
    w.push("--command".into());
    w.push(command.into());
    if !project.trim().is_empty() {
        w.push("--project".into());
        w.push(project.into());
    }
    w.push("--prompt-file".into());
    w.push(path.into());
    w
}

/// The popup's shell line: the editor on the file, then the signal the
/// form waits for. `$EDITOR` is the popup shell's to expand (tmux
/// leaves a single-quoted argument alone), `vim` when it is unset.
pub fn editor_line(path: &str, tmux_binary: &str, socket: &str, token: &str) -> String {
    format!(
        "${{EDITOR:-vim}} \"{}\"; \"{}\" -S \"{}\" wait-for -S {token}",
        sh_dq(path),
        sh_dq(tmux_binary),
        sh_dq(socket)
    )
}

/// Escape for the inside of a shell double-quoted string.
fn sh_dq(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '$' | '`' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Where the agent landed, from the `new` service's reply or the
/// `@agents_new` line.
#[derive(Debug, Default, PartialEq, serde::Deserialize)]
pub struct Landed {
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub command: String,
}

impl Landed {
    /// `%pane @window session` as `@agents_new` spells it.
    pub fn parse(line: &str) -> Result<Landed, String> {
        let line = line.trim();
        if let Some(e) = line.strip_prefix("error:") {
            return Err(e.trim().to_string());
        }
        let mut words = line.split_whitespace().peekable();
        let pane = words.next_if(|w| w.starts_with('%')).map(str::to_string);
        let window = words.next_if(|w| w.starts_with('@')).map(str::to_string);
        let session = words.collect::<Vec<_>>().join(" ");
        if session.is_empty() {
            return Err(format!("unexpected answer: {line}"));
        }
        Ok(Landed { pane, window, session, command: String::new() })
    }

    /// `started in main @3`: short enough for the form's status line,
    /// which clips from the left (a long command would push the words
    /// that matter off the edge).
    pub fn brief(&self) -> String {
        let mut s = format!("started in {}", self.session);
        if let Some(w) = &self.window {
            s.push(' ');
            s.push_str(w);
        }
        s
    }

    /// The brief with the command line, for the view's status.
    pub fn status(&self) -> String {
        let mut s = self.brief();
        if !self.command.is_empty() {
            s.push_str(": ");
            s.push_str(&self.command);
        }
        s
    }
}

// ---------------------------------------------------------------------------
// open
// ---------------------------------------------------------------------------

/// The pane the defaults come from: the view's, else the first pane
/// with a directory in the client's current window (its active pane may
/// be a float with none - the view itself).
fn default_pane(client: Option<u64>, pane: Option<u32>) -> Option<u32> {
    if let Some(p) = pane.filter(|p| resolve_pane(PaneId(*p)).ok().is_some_and(|i| !i.cwd.is_empty())) {
        return Some(p);
    }
    let window = listkit::window_for(client, pane)?;
    let w = resolve_window(WindowId(window)).ok()?;
    let has_cwd = |p: &u32| resolve_pane(PaneId(*p)).ok().is_some_and(|i| !i.cwd.is_empty());
    w.active_pane.filter(has_cwd).or_else(|| w.panes.iter().copied().find(has_cwd))
}

/// The client to put the popup on: the one the view was opened from,
/// else the first attached terminal client.
fn client_name(client: Option<u64>) -> Option<String> {
    let clients = list_clients().ok()?;
    client
        .and_then(|cid| clients.iter().find(|c| u64::from(c.id) == cid))
        .or_else(|| clients.iter().find(|c| c.attached && !c.control))
        .or_else(|| clients.first())
        .map(|c| c.name.clone())
}

pub async fn open(req: Request) {
    if state().borrow().is_some() {
        return;
    }
    let root = match fs_root() {
        Ok(r) => r,
        Err(e) => {
            chat::note(&req.st, format!("agent: no data dir: {}", e.message));
            return;
        }
    };
    let today = {
        let (days, _, _) = timefmt::local((now_ms() / 1000) as i64, req.tz);
        let (y, m, d) = timefmt::civil(days);
        format!("{y:04}-{m:02}-{d:02}")
    };
    let head = header(&req.label, req.thread, &today);
    let body = initial_body(&head, &req.transcript);
    let rel = prompt_rel(&req.channel, &req.first_ts);
    let path = format!("{root}/{rel}");
    if let Err(e) = fs_write(&rel, body.as_bytes().to_vec(), false).await {
        chat::note(&req.st, format!("agent: cannot write {rel}: {}", e.message));
        return;
    }
    // The defaults: the pane's folder, session and project.
    let pane = default_pane(req.client, req.pane);
    let here = pane
        .and_then(|p| {
            format_expand(
                OptionTarget::Pane(PaneId(p)),
                "#{pane_current_path}\u{1}#{session_name}\u{1}#{@project}\u{1}#{tmux_binary}\u{1}#{socket_path}",
            )
            .ok()
        })
        .or_else(|| format_expand(OptionTarget::Server, "\u{1}\u{1}\u{1}#{tmux_binary}\u{1}#{socket_path}").ok())
        .unwrap_or_default();
    let mut here = here.split('\u{1}').map(|s| s.trim().to_string());
    let folder = here.next().unwrap_or_default();
    let session = here.next().unwrap_or_default();
    let project = here.next().unwrap_or_default();
    let tmux = match (here.next(), here.next()) {
        (Some(b), Some(s)) if !b.is_empty() && !s.is_empty() => Some((b, s)),
        _ => None,
    };
    let Some(window) = listkit::window_for(req.client, req.pane) else {
        chat::note(&req.st, "agent: no window to open the form in".into());
        return;
    };
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("agent from slack".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            chat::note(&req.st, format!("agent: cannot open the form: {}", e.message));
            return;
        }
    };
    let model = Handoff {
        st: Rc::clone(&req.st),
        label: req.label.clone(),
        count: req.count,
        path,
        rel,
        body,
        source: PromptSource::File,
        client_name: client_name(req.client),
        pane,
        tmux,
        editing: false,
        done: None,
    };
    let prompt = model.prompt_value();
    let fields = vec![
        field("ask", String::new()),
        field("folder", folder),
        field("session", session),
        field("name", req.channel.clone()),
        field("command", "claude".into()),
        field("project", project),
        field("prompt", prompt),
    ];
    let mut form = Form::new(mode, FORM_WIDTH, FORM_HEIGHT, fields, model);
    form.focused = 0;
    form::render(&mut form);
    *state().borrow_mut() = Some(form);
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

pub fn on_mode_event(name: &str, event: &Event) {
    match name {
        "mode-key" => {
            let Some(key) = event.get_str("key") else { return };
            on_key(key);
        }
        "mode-resize" => {
            let st = state();
            let mut b = st.borrow_mut();
            let Some(form) = b.as_mut() else { return };
            form.resized(event.get_i64("width"), event.get_i64("height"));
            form::render(form);
        }
        "mode-closed" => {
            *state().borrow_mut() = None;
        }
        _ => {}
    }
}

fn on_key(key: &str) {
    let st = state();
    let (mode, action) = {
        let mut b = st.borrow_mut();
        let Some(form) = b.as_mut() else { return };
        let mode = form.mode;
        if form.model.done.is_some() {
            drop(b);
            let _ = mode_close(mode);
            return;
        }
        if form.model.editing || form.busy {
            return;
        }
        if key == "C-e" {
            drop(b);
            spawn(edit(mode));
            return;
        }
        // The prompt field is a toggle, not text.
        if form.focused == form.idx("prompt") {
            match key {
                "Tab" | "BTab" | "Space" | "C-n" | "C-p" => {
                    form.model.source = form.model.source.toggle();
                    let v = form.model.prompt_value();
                    let i = form.idx("prompt");
                    form.fields[i].value = v;
                    form.error = None;
                    form::render(form);
                    return;
                }
                "Enter" | "Escape" | "Up" | "Down" | "C-j" | "C-k" => {}
                _ => return,
            }
        }
        (mode, form.key(key, None))
    };
    match action {
        Action::None | Action::Toggle => {}
        Action::Rescan { reveal } => form::start_scan(&st, mode, reveal),
        Action::Probe => form::kick_probe(&st, mode),
        Action::Submit => {
            spawn(submit(mode));
        }
        Action::Close => {
            let _ = mode_close(mode);
        }
    }
}

/// Read the whole prompt file back.
async fn read_all(rel: &str) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        let (chunk, eof) = fs_read(rel, out.len() as u64, CHUNK).await.map_err(|e| e.message.clone())?;
        let n = chunk.len();
        out.extend_from_slice(&chunk);
        if eof || n == 0 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// `C-e`: the file in `$EDITOR`, in a popup on the form's client; the
/// form waits (`wait-for`) and shows the new line count.
async fn edit(mode: ModeId) {
    let st = state();
    let (path, rel, client, tmux) = {
        let mut b = st.borrow_mut();
        let Some(form) = b.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        let Some(tmux) = form.model.tmux.clone() else {
            form.error = Some("cannot find the tmux binary for the popup".into());
            form::render(form);
            return;
        };
        let Some(client) = form.model.client_name.clone() else {
            form.error = Some("no client to open the editor on".into());
            form::render(form);
            return;
        };
        // The paste buffer is not a file to edit: the editor edits the
        // file, which is what a submit uses when the field says so.
        form.model.editing = true;
        form.busy = true;
        form.error = None;
        form::render(form);
        (form.model.path.clone(), form.model.rel.clone(), client, tmux)
    };
    let token = format!("slack-prompt-{}", now_ms());
    let line = editor_line(&path, &tmux.0, &tmux.1, &token);
    let cmd = format!("display-popup -E -c {} -w 80% -h 80% {}", quote(&client), quote(&line));
    let opened = run_command(&cmd).await;
    let waited = match opened {
        Ok(()) => run_job(&format!("{} -S {} wait-for {token}", quote(&tmux.0), quote(&tmux.1)), None)
            .await
            .map(|_| ())
            .map_err(|e| e.message.clone()),
        Err(e) => Err(e.message.clone()),
    };
    let body = match waited {
        Ok(()) => read_all(&rel).await,
        Err(e) => Err(format!("editor: {e}")),
    };
    let mut b = st.borrow_mut();
    let Some(form) = b.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
    form.model.editing = false;
    form.busy = false;
    match body {
        Ok(text) => {
            form.model.body = text;
            let v = form.model.prompt_value();
            let i = form.idx("prompt");
            form.fields[i].value = v;
        }
        Err(e) => form.error = Some(e),
    }
    form::render(form);
}

#[derive(serde::Serialize)]
struct NewReq {
    words: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pane: Option<u32>,
}

/// Ask the agents plugin to start the agent: its `new` service, or the
/// verb through `plugin-command` and the caller pane's `@agents_new`
/// when no such service is registered.
async fn start(words: Vec<String>, pane: Option<u32>) -> Result<Landed, String> {
    let req = NewReq { words: words.clone(), pane };
    match service::call_json::<_, Landed>("agents", "new", &req).await {
        Ok(l) => return Ok(l),
        Err(e) if e.code == ErrorCode::NoSuchObject || e.code == ErrorCode::Unsupported || e.code == ErrorCode::CapDenied => {
            log(&format!("slack: agents new service: {} - using plugin-command", e.message));
        }
        Err(e) => return Err(e.message.clone()),
    }
    let Some(p) = pane else {
        return Err("the agents plugin here has no `new` service and the view has no pane to answer through".into());
    };
    let target = quote(&format!("%{p}"));
    let line = std::iter::once("new".to_string())
        .chain(words.iter().map(|w| quote(w)))
        .collect::<Vec<_>>()
        .join(" ");
    run_command(&format!("set-option -p -t {target} @agents_new pending")).await.map_err(|e| e.message.clone())?;
    run_command(&format!("plugin-command -t {target} agents {}", quote(&line))).await.map_err(|e| e.message.clone())?;
    for _ in 0..60 {
        if sleep_ms(250).await.is_err() {
            break;
        }
        let v = get_option_in(OptionTarget::Pane(PaneId(p)), "@agents_new").unwrap_or_default();
        let v = v.trim();
        if v.is_empty() || v == "pending" {
            continue;
        }
        return Landed::parse(v);
    }
    Err("the agents plugin did not answer (is it loaded?)".into())
}

/// Enter: the file, then the agent.
async fn submit(mode: ModeId) {
    let st = state();
    let (ask, folder, session, name, command, project, path, rel, body, source, pane) = {
        let mut b = st.borrow_mut();
        let Some(form) = b.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        let v = |l: &str| form.value(l);
        let err = if v("folder").is_empty() {
            Some("folder is required")
        } else if v("name").is_empty() {
            Some("name is required")
        } else if v("command").is_empty() {
            Some("command is required")
        } else if v("session").is_empty() {
            Some("session is required (`new` for one of its own)")
        } else {
            None
        };
        if let Some(e) = err {
            form.error = Some(e.into());
            form::render(form);
            return;
        }
        let values = (
            v("ask"),
            v("folder"),
            v("session"),
            v("name"),
            v("command"),
            v("project"),
            form.model.path.clone(),
            form.model.rel.clone(),
            form.model.body.clone(),
            form.model.source,
            form.model.pane,
        );
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        values
    };
    // The file the agent reads.
    let written: Result<(), String> = match source {
        PromptSource::File => fs_write(&rel, prompt_text(&ask, &body).into_bytes(), false)
            .await
            .map(|_| ())
            .map_err(|e| e.message.clone()),
        PromptSource::Clipboard => {
            // The ask first, the paste buffer appended under it; an
            // empty buffer leaves the file as short as the ask alone.
            let head = prompt_text(&ask, "");
            let r = fs_write(&rel, head.clone().into_bytes(), false).await.map_err(|e| e.message.clone());
            match r {
                Ok(_) => {
                    let _ = run_command(&format!("save-buffer -a {}", quote(&path))).await;
                    match read_all(&rel).await {
                        Ok(text) if text.trim().len() > head.trim().len() => Ok(()),
                        Ok(_) => Err("the paste buffer is empty".to_string()),
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
    };
    if let Err(e) = written {
        form::fail(&st, mode, format!("prompt file: {e}"));
        return;
    }
    let words = new_words(&folder, &session, &name, &command, &project, &path);
    match start(words, pane).await {
        Ok(landed) => {
            let chat_st = {
                let mut b = st.borrow_mut();
                let Some(form) = b.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
                form.busy = false;
                form.model.done = Some(landed.brief());
                form::render(form);
                Rc::clone(&form.model.st)
            };
            chat::note(&chat_st, format!("agent {}", landed.status()));
            let _ = sleep_ms(LINGER_MS).await;
            let still = state().borrow().as_ref().is_some_and(|f| f.mode.0 == mode.0 && f.model.done.is_some());
            if still {
                let _ = mode_close(mode);
            }
        }
        Err(e) => form::fail(&st, mode, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_prompt_text() {
        assert_eq!(header("#general", false, "2026-10-30"), "From Slack #general on 2026-10-30:");
        assert_eq!(header("@tejas", true, "2026-10-30"), "From Slack @tejas · thread on 2026-10-30:");
        let body = initial_body("From Slack #general on 2026-10-30:", "Tejas (2026-10-30 14:02): shipped\nzack (2026-10-30 14:05): nice\n");
        assert_eq!(body, "From Slack #general on 2026-10-30:\n\nTejas (2026-10-30 14:02): shipped\nzack (2026-10-30 14:05): nice\n");
        assert_eq!(prompt_text("  fix it  ", &body), format!("fix it\n\n{body}"));
        assert_eq!(prompt_text("", &body), body, "no ask: the body alone");
        assert_eq!(line_count(&body), 4);
    }

    #[test]
    fn file_name_and_field_label() {
        assert_eq!(prompt_rel("general", "1700000000.000100"), "prompts/general-1700000000.000100.md");
        assert_eq!(prompt_rel("a b/c", "1.2"), "prompts/a-b-c-1.2.md");
        assert_eq!(prompt_rel("", "1.2"), "prompts/slack-1.2.md");
        assert_eq!(prompt_label(PromptSource::File, "prompts/g-1.md", 1), "file: prompts/g-1.md (1 line)");
        assert_eq!(prompt_label(PromptSource::File, "prompts/g-1.md", 4), "file: prompts/g-1.md (4 lines)");
        assert_eq!(prompt_label(PromptSource::Clipboard, "x", 4), "clipboard (the tmux paste buffer)");
        assert_eq!(PromptSource::File.toggle(), PromptSource::Clipboard);
    }

    #[test]
    fn the_verbs_words() {
        let w = new_words("/w/proj", "main", "general", "claude", "glm", "/d/prompts/general-1.md");
        assert_eq!(
            w,
            vec![
                "--folder", "/w/proj", "--session", "main", "--name", "general", "--command", "claude",
                "--project", "glm", "--prompt-file", "/d/prompts/general-1.md"
            ]
        );
        let w = new_words("/w", "new", "g", "sh -c 'exec cat'", "", "/p.md");
        assert!(w.contains(&"--new-session".to_string()));
        assert!(!w.contains(&"--session".to_string()));
        assert!(!w.contains(&"--project".to_string()));
        assert_eq!(w[w.len() - 3], "sh -c 'exec cat'", "a command line is one word");
    }

    #[test]
    fn editor_line_quotes_for_the_popup_shell() {
        let l = editor_line("/d/p q.md", "/usr/bin/tmux", "/tmp/tmux-501/x", "slack-prompt-1");
        assert_eq!(l, "${EDITOR:-vim} \"/d/p q.md\"; \"/usr/bin/tmux\" -S \"/tmp/tmux-501/x\" wait-for -S slack-prompt-1");
        assert_eq!(sh_dq("a\"b$c`d\\e"), "a\\\"b\\$c\\`d\\\\e");
    }

    #[test]
    fn landed_from_the_option_line() {
        let l = Landed::parse("%12 @4 alpha").unwrap();
        assert_eq!((l.pane.as_deref(), l.window.as_deref(), l.session.as_str()), (Some("%12"), Some("@4"), "alpha"));
        assert_eq!(l.status(), "started in alpha @4");
        let l = Landed::parse("  solo").unwrap();
        assert_eq!((l.pane, l.window, l.session.as_str()), (None, None, "solo"));
        assert_eq!(Landed::parse("error: new: no session 'nope'"), Err("new: no session 'nope'".to_string()));
        assert!(Landed::parse("").is_err());
        let l: Landed = serde_json::from_str(r#"{"pane":"%3","window":"@2","session":"main","command":"claude 'x'"}"#).unwrap();
        assert_eq!(l.status(), "started in main @2: claude 'x'");
        assert_eq!(l.brief(), "started in main @2", "the form's line leaves the command out");
    }
}
