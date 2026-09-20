//! A float with labelled text fields, one of which has the keyboard, and
//! a completion list under the focused field when it has a source.
//!
//! What the fields MEAN - which one completes from where, which follows
//! which until edited, what the tag in the corner says, what Enter does -
//! belongs to the caller, through [`Model`]. This module owns the rest:
//! text editing, focus, the list keys, the touched/mirror rule, the
//! render, the resize handshake with the host, and the async plumbing
//! that runs scans and probes without letting a stale result land.
//!
//! The form lives in a shared cell ([`Shared`]) because scans and probes
//! outlive the callback that started them; every one of them checks the
//! form is still the same one (by mode id and generation) before it
//! writes anything back.

use std::cell::RefCell;
use std::rc::Rc;

use tmux_plugin_sdk::executor::{cancel as cancel_task, spawn as spawn_task, TaskId};
use tmux_plugin_sdk::prelude::*;

use crate::complete::{detail_command, scan, Completion, RowKind, Source};
use crate::text::{clip, expand_home};

/// One labelled text field.
#[derive(Clone, Debug)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    /// Once the user edits a field it stops mirroring its source (see
    /// [`Model::mirror`]). Emptying it with BSpace resumes the mirror.
    pub touched: bool,
}

pub fn field(label: &'static str, value: String) -> Field {
    Field { label, value, touched: false }
}

/// What the caller knows about its fields.
pub trait Model: 'static {
    /// The completion source for field `i`, or `None` for a field that
    /// takes free text only.
    fn source(&self, fields: &[Field], i: usize) -> Option<Source>;

    /// Refill the fields that follow another one, skipping any the user
    /// has touched. Called after every edit and every accepted row.
    fn mirror(&mut self, fields: &mut [Field]);

    /// A row of `kind` was taken into field `i`.
    fn accepted(&mut self, _fields: &[Field], _i: usize, _kind: RowKind) {}

    /// Field `i` was edited by hand (typed, erased, cleared).
    fn edited(&mut self, _fields: &[Field], _i: usize) {}

    /// The tag in the top right corner, e.g. `[plain]`.
    fn tag(&self) -> String;

    /// The word for what the toggle key swaps to (`worktree`), or `None`
    /// when the form has one kind only. Shown in the hint line.
    fn toggle_hint(&self) -> Option<String> {
        None
    }

    /// A line under the tag, drawn in yellow: what is missing, usually.
    fn banner(&self, _fields: &[Field]) -> Option<String> {
        None
    }

    /// A decoration after field `i`'s value (`✓`, `(exists)`), with its
    /// own colour codes.
    fn mark(&self, _fields: &[Field], _i: usize) -> Option<String> {
        None
    }

    /// The status line when there is no error: what is being done while
    /// busy, or what the next Enter will do. Drawn in yellow.
    fn status(&self, _fields: &[Field], _busy: bool) -> Option<String> {
        None
    }

    /// The verb in the hint line: "Enter create".
    fn submit_label(&self) -> &'static str {
        "create"
    }
}

pub struct Form<M: Model> {
    pub mode: ModeId,
    pub width: u32,
    pub height: u32,
    /// The size the host last confirmed through `mode-resize`. A resize
    /// is requested only when the wanted size differs from this, so the
    /// request and the event cannot chase each other.
    pub sized: (u32, u32),
    /// The closed form is never shorter than this.
    pub min_height: u32,
    pub fields: Vec<Field>,
    pub focused: usize,
    pub error: Option<String>,
    /// Something is in flight; input is ignored until it resolves (the
    /// caller clears it on failure, or closes the form on success).
    pub busy: bool,
    /// The completion list for the focused field, when it has one.
    pub list: Option<Completion>,
    /// Wall clock from the last scan, used to render commit ages. The
    /// guest has no clock of its own.
    pub now: i64,
    /// Bumped whenever a scan is started or invalidated.
    pub generation: u64,
    /// The scan task, so a newer one can stop it. A scan does real work
    /// after each await - list, rank, build rows - so a superseded one
    /// is cancelled rather than left to finish and be thrown away.
    pub scan_task: Option<TaskId>,
    /// A probe worker is alive. The only thing stopping a burst of key
    /// presses from starting a job each.
    pub probing: bool,
    /// The home directory, read once when the form opens, so a `~` in a
    /// path field completes like any other directory.
    pub home: Option<String>,
    pub model: M,
}

/// The cell a form lives in while it is open.
pub type Shared<M> = Rc<RefCell<Option<Form<M>>>>;

/// What a key did, decided inside the borrow and acted on after it.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// The focused field or its value changed: start (or refilter) its
    /// completion list. See [`start_scan`].
    Rescan,
    /// The rows on screen changed: ask git about the new ones. See
    /// [`kick_probe`].
    Probe,
    /// Enter.
    Submit,
    /// The toggle key: the caller swaps its kind, then rescans.
    Toggle,
    /// Esc with no list to hide: close the mode.
    Close,
}

impl<M: Model> Form<M> {
    pub fn new(
        mode: ModeId,
        width: u32,
        min_height: u32,
        fields: Vec<Field>,
        model: M,
    ) -> Form<M> {
        let mut form = Form {
            mode,
            width,
            height: min_height,
            sized: (width, min_height),
            min_height,
            fields,
            focused: 0,
            error: None,
            busy: false,
            list: None,
            now: 0,
            generation: 0,
            scan_task: None,
            probing: false,
            // Synchronous, so `~` is expandable before the first scan.
            home: home_dir().ok().filter(|h| !h.is_empty()),
            model,
        };
        form.model.mirror(&mut form.fields);
        form
    }

    pub fn idx(&self, label: &str) -> usize {
        self.fields.iter().position(|f| f.label == label).unwrap()
    }

    /// A field's value, trimmed.
    pub fn value(&self, label: &str) -> String {
        self.fields[self.idx(label)].value.trim().to_string()
    }

    /// Replace the field set (a kind swap). The field with the focused
    /// label keeps the focus when it survives; else `fallback` does.
    pub fn replace_fields(&mut self, fields: Vec<Field>, fallback: &str) {
        let focus_label = self.fields[self.focused].label;
        self.fields = fields;
        self.focused = self
            .fields
            .iter()
            .position(|f| f.label == focus_label)
            .unwrap_or_else(|| self.idx(fallback));
        self.error = None;
        self.list = None;
        self.model.mirror(&mut self.fields);
    }

    /// The fragment the list filters on: the last path component for a
    /// path source, the whole value otherwise.
    pub fn fragment(&self, i: usize) -> String {
        let v = &self.fields[i].value;
        match self.model.source(&self.fields, i) {
            Some(Source::Branches { .. }) | Some(Source::Words { .. }) | None => {
                v.trim().to_string()
            }
            _ => match v.rfind('/') {
                Some(p) => v[p + 1..].to_string(),
                None => v.clone(),
            },
        }
    }

    /// The closed height: header, fields, status, hint, but never under
    /// `min_height`.
    fn closed_height(&self) -> u32 {
        (8 + self.fields.len() as u32).max(self.min_height)
    }

    /// Wanted outer size: the closed form, plus a rule and the visible
    /// rows when a list is up.
    pub fn wanted_size(&self) -> (u32, u32) {
        let extra = match &self.list {
            Some(p) if p.shown() => 1 + p.height().max(1) as u32,
            _ => 0,
        };
        (self.width, self.closed_height() + extra)
    }

    /// Mode-resize arrived: this is the size we really have, so a render
    /// must not ask for the old one again.
    pub fn resized(&mut self, w: Option<i64>, h: Option<i64>) {
        if let Some(w) = w {
            self.width = w as u32;
        }
        if let Some(h) = h {
            self.height = h as u32;
        }
        self.sized = (self.width, self.height);
    }

    /// An edit of the focused field by hand, with the touched rule and
    /// the mirrors that follow.
    fn edited(&mut self, touched: bool) {
        let i = self.focused;
        self.fields[i].touched = touched;
        self.error = None;
        self.model.edited(&self.fields, i);
        self.model.mirror(&mut self.fields);
    }

    /// Take the highlighted row into its field. False when nothing was
    /// taken (no row, or a disabled one - the error says why).
    pub fn accept(&mut self) -> bool {
        let Some(p) = self.list.as_ref() else { return false };
        let field = p.field;
        match p.accept() {
            Ok(Some((value, kind))) => {
                self.fields[field].value = value;
                self.fields[field].touched = true;
                self.error = None;
                self.model.accepted(&self.fields, field, kind);
                self.model.mirror(&mut self.fields);
                true
            }
            Ok(None) => false,
            Err(why) => {
                self.error = Some(why);
                false
            }
        }
    }

    /// Whether the list is up with a highlighted row.
    pub fn in_list(&self) -> bool {
        self.list.as_ref().is_some_and(|p| p.sel.is_some())
    }

    /// Whether the list is up at all.
    pub fn listed(&self) -> bool {
        self.list.as_ref().is_some_and(|p| p.shown())
    }

    /// One key. `toggle_key` is the caller's kind-swap key, if any
    /// ("C-t"); everything else is the form's own.
    pub fn key(&mut self, key: &str, toggle_key: Option<&str>) -> Action {
        if self.busy {
            return Action::None;
        }
        if Some(key) == toggle_key {
            return Action::Toggle;
        }
        let n = self.fields.len();
        match key {
            "Escape" => {
                // The list first, the form second. Hiding the list frees
                // C-j/C-k to move between fields.
                if self.listed() {
                    if let Some(p) = self.list.as_mut() {
                        p.sel = None;
                        p.hidden = true;
                    }
                    self.error = None;
                    render(self);
                    Action::None
                } else {
                    Action::Close
                }
            }
            "Enter" => Action::Submit,
            "Down" | "C-n" | "C-j" => {
                let listed = self.list.as_ref().is_some_and(|p| p.shown() && !p.view.is_empty());
                if listed {
                    self.list.as_mut().unwrap().step(1);
                    self.error = None;
                    render(self);
                    Action::Probe
                } else {
                    // No list to step into: the key keeps its old
                    // meaning rather than going dead.
                    self.focused = (self.focused + 1) % n;
                    Action::Rescan
                }
            }
            "Up" | "C-p" | "C-k" => {
                // Leaving the top row puts the cursor back in the text; a
                // second Up then moves to the field above.
                if self.in_list() {
                    self.list.as_mut().unwrap().step(-1);
                    self.error = None;
                    render(self);
                    Action::Probe
                } else {
                    self.focused = (self.focused + n - 1) % n;
                    Action::Rescan
                }
            }
            "Tab" => {
                // Complete like a shell: take the highlighted row, or the
                // first row when none is highlighted, and stay in the
                // field with the cursor at the end. The list re-filters
                // on the completed value.
                let listed = self.list.as_ref().is_some_and(|p| p.shown() && !p.view.is_empty());
                if !listed {
                    return Action::None;
                }
                let p = self.list.as_mut().unwrap();
                if p.sel.is_none() {
                    p.sel = Some(0);
                }
                if self.accept() {
                    self.list.as_mut().unwrap().sel = None;
                    Action::Rescan
                } else {
                    render(self);
                    Action::None
                }
            }
            "BTab" => {
                self.focused = (self.focused + n - 1) % n;
                Action::Rescan
            }
            "BSpace" => {
                let i = self.focused;
                self.fields[i].value.pop();
                // Erasing to empty resumes the mirror.
                let touched = !self.fields[i].value.is_empty() && self.fields[i].touched;
                self.edited(touched);
                Action::Rescan
            }
            "C-u" => {
                // Clear to type fresh: touched keeps the mirror from
                // instantly refilling the field (BSpace past empty is the
                // resume-the-mirror gesture).
                self.fields[self.focused].value.clear();
                self.edited(true);
                Action::Rescan
            }
            "Space" => {
                self.fields[self.focused].value.push(' ');
                self.edited(true);
                Action::Rescan
            }
            k if k.chars().count() == 1 && !k.chars().next().unwrap().is_control() => {
                self.fields[self.focused].value.push_str(k);
                self.edited(true);
                Action::Rescan
            }
            _ => Action::None,
        }
    }
}

/// Draw the form: tag, banner, fields, list, status, hint. Asks the host
/// for the size the content wants first; the mode-resize event that
/// follows carries the size the window actually allowed.
pub fn render<M: Model>(form: &mut Form<M>) {
    let want = form.wanted_size();
    if want != form.sized && mode_resize(form.mode, want.0, want.1).is_ok() {
        form.sized = want;
    }

    let w = form.width as usize;
    let mut out = String::from("\x1b[2J\x1b[H");
    out.push_str(&format!(
        "\x1b[2m{:>width$}\x1b[0m\r\n",
        form.model.tag(),
        width = w.saturating_sub(2)
    ));
    match form.model.banner(&form.fields) {
        Some(b) => out.push_str(&format!("  \x1b[33m{}\x1b[0m\r\n\r\n", clip(&b, w - 4))),
        None => out.push_str("\r\n\r\n"),
    }
    let labelw = form.fields.iter().map(|f| f.label.chars().count()).max().unwrap_or(0).max(7);
    for (i, f) in form.fields.iter().enumerate() {
        let focused = i == form.focused && !form.busy;
        let mark = form.model.mark(&form.fields, i).map(|m| format!(" {m}")).unwrap_or_default();
        let val = clip(&f.value, w.saturating_sub(labelw + 13));
        if focused {
            // The cursor block sits in the text only while the list has
            // no selection; once you are in the list the field is quiet.
            let in_list = form.list.as_ref().is_some_and(|p| p.field == i && p.sel.is_some());
            if in_list {
                out.push_str(&format!(
                    "  \x1b[1m{:<labelw$}\x1b[0m \x1b[4m{val}\x1b[0m{mark}\r\n",
                    f.label
                ));
            } else {
                out.push_str(&format!(
                    "  \x1b[1m{:<labelw$}\x1b[0m \x1b[7m{val}\x1b[27m\x1b[7m \x1b[0m{mark}\r\n",
                    f.label
                ));
            }
        } else {
            out.push_str(&format!("  {:<labelw$} {val}{mark}\r\n", f.label));
        }
    }

    if let Some(p) = &form.list {
        p.render(&mut out, w, form.now);
    }

    out.push_str("\r\n");
    if let Some(err) = &form.error {
        out.push_str(&format!("  \x1b[31m{}\x1b[0m\r\n", clip(err, w - 4)));
    } else if let Some(s) = form.model.status(&form.fields, form.busy) {
        out.push_str(&format!("  \x1b[33m{}\x1b[0m\r\n", clip(&s, w - 4)));
    } else {
        out.push_str("\r\n");
    }
    let verb = form.model.submit_label();
    let toggle = form.model.toggle_hint().map(|t| format!("C-t {t} · ")).unwrap_or_default();
    let hint = if form.in_list() {
        format!("Tab accept · C-j/C-k move · Enter {verb} · Esc hide list")
    } else if form.listed() {
        format!("{toggle}C-j list · Tab accept · Enter {verb} · Esc hide list")
    } else {
        format!("{toggle}C-j/C-k field · Enter {verb} · Esc cancel")
    };
    out.push_str(&format!("\r\n  \x1b[2m{hint}\x1b[0m"));
    let _ = mode_write(form.mode, out.as_bytes());
}

/// Start (or reuse) the completion list for the focused field. Same
/// source key, same rows: only the filter runs. A new key cancels the
/// scan in flight and starts another.
pub fn start_scan<M: Model>(state: &Shared<M>, mode: ModeId) {
    let (generation, field, source) = {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        let field = form.focused;
        let Some(source) = form.model.source(&form.fields, field) else {
            form.list = None;
            render(form);
            return;
        };
        let source = expand_source(source, form.home.as_deref());
        let key = source.key();
        let frag = form.fragment(field);
        if let Some(p) = form.list.as_mut() {
            if p.key == key {
                p.field = field;
                p.hidden = false;
                p.refilter(&frag);
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
        form.list = Some(Completion::pending(field, key, generation));
        render(form);
        (generation, field, source)
    };
    let task = spawn_task(scan_into(Rc::clone(state), mode, generation, field, source));
    if let Some(form) = state.borrow_mut().as_mut() {
        form.scan_task = Some(task);
    }
}

/// `~` in a source's paths, against the form's home.
fn expand_source(source: Source, home: Option<&str>) -> Source {
    match source {
        Source::Dirs { base } => Source::Dirs { base: expand_home(&base, home) },
        Source::Repos { base } => Source::Repos { base: expand_home(&base, home) },
        Source::Dests { base, repo } => Source::Dests {
            base: expand_home(&base, home),
            repo: expand_home(&repo, home),
        },
        Source::Branches { repo } => Source::Branches { repo: expand_home(&repo, home) },
        other => other,
    }
}

/// Run a scan and install the rows, unless the form moved on meanwhile.
async fn scan_into<M: Model>(
    state: Shared<M>,
    mode: ModeId,
    generation: u64,
    field: usize,
    source: Source,
) {
    let scanned = scan(source).await;
    let mut st = state.borrow_mut();
    let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
    if form.generation != generation || form.focused != field {
        return; // a newer scan owns the list
    }
    if scanned.now != 0 {
        form.now = scanned.now;
    }
    let frag = form.fragment(field);
    let Some(p) = form.list.as_mut() else { return };
    p.install(scanned);
    p.refilter(&frag);
    render(form);
    drop(st);

    // The rows are on screen now, so fill in what git knows about them.
    kick_probe(&state, mode);
}

/// Start a probe worker unless one is already running.
///
/// This is the whole rate limit. A key press does not start work; it only
/// makes sure a worker exists. A burst of presses therefore lands inside
/// the job the running worker is already awaiting, and costs nothing.
/// There is no timer, so a single press starts immediately, and the
/// pacing comes from how long git actually takes.
pub fn kick_probe<M: Model>(state: &Shared<M>, mode: ModeId) {
    {
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        if form.probing {
            return;
        }
        form.probing = true;
    }
    spawn_task(probe_worker(Rc::clone(state), mode));
}

/// The one worker. It loops until the rows on screen have nothing left to
/// ask about, so anything that scrolled into view while a job was running
/// is picked up by the next turn. "Is there work" is read off the rows
/// themselves rather than tracked in a flag.
async fn probe_worker<M: Model>(state: Shared<M>, mode: ModeId) {
    loop {
        let batch = {
            let mut st = state.borrow_mut();
            let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
            form.list.as_mut().and_then(|p| p.probe_batch())
        };
        let Some((batch, generation)) = batch else { break };
        let paths: Vec<String> = batch.iter().map(|(_, p)| p.clone()).collect();
        let Ok(out) = run_job(&detail_command(&paths), None).await else { break };
        let mut st = state.borrow_mut();
        let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) else { return };
        if let Some(p) = form.list.as_mut() {
            // A newer scan owns the list now. Its own rows are unprobed,
            // so the next turn picks them up; only this result is stale.
            if p.generation == generation {
                p.probe_apply(&batch, &out.output);
                render(form);
            }
        }
    }
    if let Some(form) = state.borrow_mut().as_mut() {
        form.probing = false;
    }
}

/// Re-enable the form with an error, unless it was closed meanwhile.
pub fn fail<M: Model>(state: &Shared<M>, mode: ModeId, error: String) {
    let mut st = state.borrow_mut();
    if let Some(form) = st.as_mut().filter(|f| f.mode.0 == mode.0) {
        form.busy = false;
        form.error = Some(error);
        render(form);
    }
}
