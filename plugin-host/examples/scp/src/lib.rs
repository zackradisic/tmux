//! SDK example plugin: copy files between this machine and the hosts
//! behind the remote session links, with `scp`.
//!
//! One form, four fields: where the source is (`from`: `local` or a
//! host), its path, where it goes (`to`), and the path there. The host
//! fields are dropdowns: Tab lists `local` and every host with a
//! `remote-attach` link (plus any in the `hosts` config), and cycles
//! through them; any other ssh host can be typed in. The path fields
//! complete: on `local` from a listing of the directory being typed in,
//! on a host from one `ls` over ssh. A directory row ends in `/`, so
//! taking it and pressing Tab again steps into it. `C-t` swaps the two
//! sides.
//!
//! Enter runs `scp -r` (`-3` when both sides are hosts) in a popup on
//! the pressing client, so scp's own progress shows; a copy that fails
//! leaves the popup open with the error, Escape closes it. With the
//! config `run = "job"` the copy runs in the background instead and the
//! form waits with "copying…", showing a failure in place.
//!
//! The form opens prefilled from the pane it was called on: a pane in a
//! mirrored session starts with `from` set to that host and the pane's
//! remote cwd, a local pane with its cwd and `to` set to the first
//! connected host.
//!
//! Wire a key to it in ~/.tmux.conf:
//!
//! ```tmux
//! bind T plugin-command scp copy
//! ```
//!
//! Load server-scoped with caps `mode`, `run-process`, `run-command`,
//! `fs-list`, `fs-read-any` (the last three are `formkit::complete::CAPS`).
//! Role `view`: nothing here needs to run on the remote, so the manifest
//! entry says `role = "view"` and the link does not push it.
//!
//! Config (all strings): `run` = `popup` (default) or `job`; `args` =
//! extra scp flags (default `-r`); `hosts` = comma-separated ssh hosts
//! to offer besides the linked ones.
//!
//! Build: cargo build -p scp --target wasm32-unknown-unknown --release

use std::cell::RefCell;
use std::rc::Rc;

use formkit::complete::Source;
use formkit::form::{self, field, Action, Field, Form, Model, Shared};
use formkit::text::{expand_home, last_line, quote, scan_base};
use tmux_plugin_sdk::prelude::*;

/// Form geometry (cells). The height is the closed form; an open list
/// adds a rule and up to `LIST_MAX` rows through `mode_resize`.
const FORM_WIDTH: u32 = 76;
const FORM_HEIGHT: u32 = 12;

/// The word that stands for this machine in a host field. An empty host
/// field means the same.
const LOCAL: &str = "local";

/// Field indices. Two fields share the label `path`, so the form is
/// addressed by position, never by label.
const FROM: usize = 0;
const FROM_PATH: usize = 1;
const TO: usize = 2;
const TO_PATH: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    /// `display-popup -EE` on the pressing client: progress shows, a
    /// failure stays on screen.
    Popup,
    /// `run_job` behind the form: the form waits, a failure renders in it.
    Job,
}

#[derive(serde::Deserialize, Default)]
struct Config {
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    args: Option<String>,
    #[serde(default)]
    hosts: Option<String>,
}

/// A host a host field can offer, and whether its link is up right now
/// (a typed-in host is not in this list at all).
#[derive(Clone, Debug)]
struct Host {
    name: String,
    /// `None` for a host from the config: no link to ask.
    connected: Option<bool>,
}

fn is_local(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v == LOCAL
}

/// What the copy form knows: the hosts to offer, the client to open the
/// popup on, and how to run.
struct Copier {
    hosts: Vec<Host>,
    client_name: Option<String>,
    run: Run,
    args: String,
}

impl Model for Copier {
    fn source(&self, fields: &[Field], i: usize) -> Option<Source> {
        match i {
            FROM | TO => {
                let mut words = vec![(LOCAL.to_string(), "this machine".to_string())];
                for h in &self.hosts {
                    let meta = match h.connected {
                        Some(true) => "linked, connected",
                        Some(false) => "linked, disconnected",
                        None => "ssh",
                    };
                    words.push((h.name.clone(), meta.to_string()));
                }
                Some(Source::Choice { title: "hosts".into(), words })
            }
            FROM_PATH | TO_PATH => {
                let host = fields[i - 1].value.trim().to_string();
                let base = scan_base(&fields[i].value);
                if is_local(&host) {
                    Some(Source::Files { base })
                } else {
                    Some(Source::Remote { host, base })
                }
            }
            _ => None,
        }
    }

    /// Nothing follows anything: a destination is the user's to say.
    fn mirror(&mut self, _fields: &mut [Field]) {}

    fn title(&self) -> String {
        "Copy Files".into()
    }

    fn toggle_hint(&self) -> Option<String> {
        Some("swap".into())
    }

    fn mark(&self, fields: &[Field], i: usize) -> Option<String> {
        if i != FROM && i != TO {
            return None;
        }
        let v = fields[i].value.trim();
        let state = if is_local(v) {
            String::new()
        } else {
            match self.hosts.iter().find(|h| h.name == v) {
                Some(Host { connected: Some(true), .. }) => " \x1b[32m●\x1b[0m".to_string(),
                Some(Host { connected: Some(false), .. }) => {
                    " \x1b[31m○ link down\x1b[0m".to_string()
                }
                _ => " \x1b[2mssh\x1b[0m".to_string(),
            }
        };
        Some(format!("\x1b[2m▾\x1b[0m{state}"))
    }

    fn status(&self, _fields: &[Field], busy: bool) -> Option<String> {
        busy.then(|| "copying…".to_string())
    }

    fn submit_label(&self) -> &'static str {
        "copy"
    }
}

type State = Shared<Copier>;

struct Scp {
    state: State,
    run: Run,
    args: String,
    extra_hosts: Vec<String>,
}

/// Every host with a link, connected or not, in session order and
/// without repeats (two links to one host are one row).
fn linked_hosts() -> Vec<Host> {
    let mut hosts: Vec<Host> = Vec::new();
    for s in list_sessions().unwrap_or_default() {
        let Ok(line) = format_expand(
            OptionTarget::Session(SessionId(s.id)),
            "#{remote_host}\t#{remote_connected}",
        ) else {
            continue;
        };
        let mut it = line.split('\t');
        let name = it.next().unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let up = it.next().unwrap_or("").trim() == "1";
        match hosts.iter_mut().find(|h| h.name == name) {
            // Any live link makes the host reachable.
            Some(h) => h.connected = Some(h.connected.unwrap_or(false) || up),
            None => hosts.push(Host { name: name.to_string(), connected: Some(up) }),
        }
    }
    hosts
}

/// A path field's prefill: the directory with a trailing `/`, so the
/// first Tab lists what is in it.
fn dir_prefill(cwd: &str) -> String {
    if cwd.is_empty() {
        String::new()
    } else {
        format!("{}/", cwd.trim_end_matches('/'))
    }
}

/// Context gathering + form open, run asynchronously after the
/// plugin-command event.
async fn open_form(
    state: State,
    run: Run,
    args: String,
    extra_hosts: Vec<String>,
    target_pane: Option<u64>,
    client: Option<u64>,
) {
    let pane = target_pane.and_then(|p| resolve_pane(PaneId(p as u32)).ok());
    let mut hosts = linked_hosts();
    for name in extra_hosts {
        if !hosts.iter().any(|h| h.name == name) {
            hosts.push(Host { name, connected: None });
        }
    }

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
        log("scp: cannot resolve the client's current window");
        let _ = display_message("scp: no client to open the form for");
        return;
    };

    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width: FORM_WIDTH,
        height: FORM_HEIGHT,
        title: Some("copy files".into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("scp: mode_open failed: {}", e.message));
            return;
        }
    };

    // A pane in a mirrored session copies FROM its host; a local pane
    // copies TO the first connected host.
    let (from, from_path, to) = match &pane {
        Some(p) if p.remote && !p.host.is_empty() => {
            (p.host.clone(), dir_prefill(&p.cwd), LOCAL.to_string())
        }
        Some(p) => {
            let to = hosts
                .iter()
                .find(|h| h.connected == Some(true))
                .map(|h| h.name.clone())
                .unwrap_or_else(|| LOCAL.to_string());
            (LOCAL.to_string(), dir_prefill(&p.cwd), to)
        }
        None => (LOCAL.to_string(), String::new(), LOCAL.to_string()),
    };
    let fields = vec![
        field(LABELS[FROM], from),
        field(LABELS[FROM_PATH], from_path),
        field(LABELS[TO], to),
        field(LABELS[TO_PATH], String::new()),
    ];

    let model = Copier {
        hosts,
        client_name: client_info.and_then(|(name, _)| name),
        run,
        args,
    };
    let mut form = Form::new(mode, FORM_WIDTH, FORM_HEIGHT, fields, model);
    // The side is prefilled; the thing to copy is what is missing.
    form.focused = FROM_PATH;
    form::render(&mut form);
    *state.borrow_mut() = Some(form);

    form::start_scan(&state, mode, false);
}

/// The labels, by position. Two fields say `path`: the form is
/// addressed by index, and a swap keeps the labels where they are.
const LABELS: [&str; 4] = ["from", "path", "to", "path"];

/// C-t: the two sides change places, edits and all.
fn swap(form: &mut Form<Copier>) {
    form.fields.swap(FROM, TO);
    form.fields.swap(FROM_PATH, TO_PATH);
    for (f, l) in form.fields.iter_mut().zip(LABELS) {
        f.label = l;
    }
    form.error = None;
    form.list = None;
}

/// One side of the copy as scp wants it: `path` here, `host:path` there.
/// A local `~` is expanded here (scp does not); a remote one is left to
/// the remote. An empty remote path is that host's home.
fn endpoint(host: &str, path: &str, home: Option<&str>) -> String {
    if is_local(host) {
        quote(&expand_home(path, home))
    } else {
        quote(&format!("{}:{}", host.trim(), path))
    }
}

/// tmux expands formats in a popup's command; a `#` in a path must not
/// start one.
fn no_formats(s: &str) -> String {
    s.replace('#', "##")
}

/// Validate and run the copy. A popup takes it from here; a job keeps
/// the form up until it is done.
async fn submit(state: State) {
    let (mode, run, client_name, cmd, what) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut() else { return };
        let from = form.fields[FROM].value.trim().to_string();
        let from_path = form.fields[FROM_PATH].value.trim().to_string();
        let to = form.fields[TO].value.trim().to_string();
        let to_path = form.fields[TO_PATH].value.trim().to_string();
        let err = if from_path.is_empty() {
            Some("a source path is required".to_string())
        } else if is_local(&to) && to_path.is_empty() {
            Some("a destination path is required (a remote one may be empty: its home)".to_string())
        } else if is_local(&from) && is_local(&to) && from_path == to_path {
            Some("source and destination are the same".to_string())
        } else {
            None
        };
        if let Some(err) = err {
            form.error = Some(err);
            form::render(form);
            return;
        }
        let home = form.home.clone();
        let src = endpoint(&from, &from_path, home.as_deref());
        let dst = endpoint(&to, &to_path, home.as_deref());
        // Host to host goes through here (-3): the two need not see
        // each other, and the keys are here.
        let three = if !is_local(&from) && !is_local(&to) { " -3" } else { "" };
        let args = form.model.args.trim();
        let args = if args.is_empty() { String::new() } else { format!(" {args}") };
        let cmd = format!("scp{args}{three} {src} {dst}");
        let what = format!("{} → {}", src.trim_matches('\''), dst.trim_matches('\''));
        form.busy = true;
        form.error = None;
        form.list = None;
        form::render(form);
        (form.mode, form.model.run, form.model.client_name.clone(), cmd, what)
    };

    match (run, client_name) {
        (Run::Popup, Some(client)) => {
            // The popup is the progress display; the form is done.
            let _ = mode_close(mode);
            *state.borrow_mut() = None;
            let shell = format!(
                "{cmd} || {{ printf '\\n[scp failed - Escape closes]\\n'; exit 1; }}"
            );
            let popup = format!(
                "display-popup -c {} -EE -w 80% -h 30% -T {} {}",
                quote(&client),
                quote(" scp "),
                quote(&no_formats(&shell))
            );
            if let Err(e) = run_command(&popup).await {
                log(&format!("scp: display-popup failed: {}", e.message));
                let _ = display_message(&format!("scp: could not open the popup: {}", e.message));
            }
        }
        _ => match run_job(&cmd, None).await {
            Ok(out) if out.status == 0 => {
                let _ = mode_close(mode);
                *state.borrow_mut() = None;
                let _ = display_message(&format!("copied: {what}"));
            }
            Ok(out) => form::fail(&state, mode, last_line(&out.output, "scp failed")),
            Err(e) => form::fail(&state, mode, format!("scp could not run: {}", e.message)),
        },
    }
}

impl Plugin for Scp {
    const NAME: &'static str = "scp";
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command"]).map_err(|e| e.message.clone())?;
        let run = match config.run.as_deref().map(str::trim) {
            Some("job") => Run::Job,
            _ => Run::Popup,
        };
        let args = config.args.unwrap_or_else(|| "-r".to_string());
        let extra_hosts = config
            .hosts
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .collect();
        Ok(Scp { state: Rc::new(RefCell::new(None)), run, args, extra_hosts })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => {
                match event.get_str("text").map(str::trim) {
                    Some("copy") | Some("") | None => {}
                    _ => return,
                }
                if self.state.borrow().is_some() {
                    let _ = display_message("scp: form already open");
                    return;
                }
                let target_pane = event.scope.pane.map(u64::from);
                let client = event.scope.client.map(u64::from);
                ctx.spawn(open_form(
                    Rc::clone(&self.state),
                    self.run,
                    self.args.clone(),
                    self.extra_hosts.clone(),
                    target_pane,
                    client,
                ));
            }
            "mode-key" => {
                let Some(key) = event.get_str("key") else { return };
                // What the key did, decided inside the borrow and acted
                // on after it: the form's own keys through formkit, the
                // swap here.
                let (mode, action) = {
                    let mut st = self.state.borrow_mut();
                    let Some(form) = st.as_mut() else { return };
                    if event.get_i64("mode") != Some(form.mode.0 as i64) {
                        return;
                    }
                    let mode = form.mode;
                    let mut action = form.key(key, Some("C-t"));
                    if action == Action::Toggle {
                        swap(form);
                        form::render(form);
                        action = Action::Rescan { reveal: false };
                    }
                    (mode, action)
                };
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

tmux_plugin!(Scp);
