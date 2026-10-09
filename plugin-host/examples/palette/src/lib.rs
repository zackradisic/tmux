//! palette: the command palette. `prefix+Space` (bind it to
//! `plugin-command palette open`) opens a float in the shape of the
//! agents picker: a search box that has the keyboard from the start,
//! the rows under it, what the highlighted row will run beside them.
//! Type to narrow, `C-j`/`C-k` or the arrows to move, Enter runs the
//! row, Esc closes. Each row shows the key that would have run it
//! directly, so the list doubles as the cheat sheet.
//!
//! The rows come from three places, grouped under headers:
//!
//! - **plugins**: every plugin named in `providers` is asked for its
//!   rows (its `palette` service method, a JSON list of
//!   `{title, hint, key, text}`); a row runs as `plugin-command <plugin>
//!   <text>` with the pane the palette was opened from as the target.
//!   A plugin that does not answer is skipped.
//! - **keys**: the prefix-table bindings that carry a `-N` note
//!   (`list-keys -T prefix`), the note as the title. One of tmux's own
//!   (its note is in `stock.rs`) goes under a folded `tmux` group; a
//!   binding from the user's config shows up top. Add a note to a
//!   binding to put it in the palette.
//! - **items**: rows in the config, `"title|key|command"`, for one-off
//!   tmux commands.
//!
//! Config: `providers = ["scp", "agents", "sessions"]` (or one
//! comma-separated string); `items = ["Reload config|R|source-file
//! ~/.tmux.conf"]`; `stock = "fold"` (default), `"hide"` or `"show"` for
//! tmux's own bindings.
//!
//! Load server-scoped with caps `read-state`, `run-command`,
//! `run-process`, `mode`, `service-call`.

use std::cell::RefCell;
use std::rc::Rc;

use listkit::engine::Outcome;
use listkit::lines::{default_size, SizeBox};
use listkit::styled::{plain_cells, Styled, ST_BOLD, ST_CYAN, ST_DIM};
use listkit::{Engine, Node, Preview};
use serde::{Deserialize, Serialize};
use tmux_plugin_sdk::prelude::*;

mod stock;

pub const NAME: &str = "palette";

/// The float: most of a small window, never huge.
const SIZE: SizeBox = SizeBox { min_w: 60, min_h: 12, max_w: 120, max_h: 40, fill_tenths: 7 };

/// A row as a provider plugin returns it, and as the config spells it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Row {
    pub title: String,
    pub hint: String,
    /// The key that runs it directly (`T`, `C-r`), shown on the right;
    /// empty when there is none.
    pub key: String,
    /// A tmux command string (a binding, a config item).
    pub command: String,
    /// A provider's row instead: the text for `plugin-command`.
    pub text: String,
}

/// Where a row came from, which decides its group and how it runs.
#[derive(Clone, Debug, PartialEq)]
enum Source {
    Plugin(String),
    /// A noted binding from the user's config.
    Keys,
    /// A noted binding tmux ships with.
    Stock,
    /// A row from the config.
    Items,
}

#[derive(Clone, Debug)]
struct Entry {
    source: Source,
    row: Row,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Config {
    providers: Option<serde_json::Value>,
    items: Option<Vec<String>>,
    stock: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StockMode {
    Fold,
    Hide,
    Show,
}

struct Settings {
    providers: Vec<String>,
    items: Vec<Row>,
    stock: StockMode,
}

struct Picker {
    engine: Engine,
    /// The pane the palette was opened from: the target a provider's
    /// row is run with.
    pane: Option<u32>,
    entries: Vec<Entry>,
    /// Sources still being gathered; the header says so.
    pending: usize,
    /// The user moved or typed: the cursor is theirs, not reset as rows
    /// arrive.
    touched: bool,
}

type State = Rc<RefCell<Option<Picker>>>;

struct Palette {
    state: State,
    settings: Rc<Settings>,
}

/// `"title|key|command"`, with the key optional (`"title||command"`).
fn parse_item(s: &str) -> Option<Row> {
    let mut parts = s.splitn(3, '|');
    let title = parts.next()?.trim();
    let key = parts.next()?.trim();
    let command = parts.next()?.trim();
    if title.is_empty() || command.is_empty() {
        return None;
    }
    Some(Row {
        title: title.to_string(),
        key: key.to_string(),
        command: command.to_string(),
        ..Row::default()
    })
}

/// The providers list: an array of names, or one string with commas.
fn parse_providers(v: Option<&serde_json::Value>) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |s: &str| {
        for p in s.split(',') {
            let p = p.trim();
            if !p.is_empty() && !out.iter().any(|o| o == p) {
                out.push(p.to_string());
            }
        }
    };
    match v {
        Some(serde_json::Value::Array(a)) => {
            for x in a {
                if let Some(s) = x.as_str() {
                    push(s);
                }
            }
        }
        Some(serde_json::Value::String(s)) => push(s),
        _ => {}
    }
    out
}

/// One line of `list-keys -T prefix -F '#{key_string}\t#{key_note}\t#{key_command}'`.
fn parse_binding(line: &str) -> Option<Row> {
    let mut it = line.splitn(3, '\t');
    let key = it.next()?.trim();
    let note = it.next()?.trim();
    let command = it.next()?.trim();
    if key.is_empty() || note.is_empty() || command.is_empty() {
        return None;
    }
    Some(Row {
        title: note.to_string(),
        key: key.to_string(),
        command: command.to_string(),
        ..Row::default()
    })
}

/// The plugin and text of a `plugin-command [-t x] <plugin> <text>`
/// string as `list-keys` prints it, quotes stripped; `None` for any
/// other command.
fn plugin_command_parts(cmd: &str) -> Option<(String, String)> {
    let mut words = shell_words(cmd).into_iter();
    if words.next()? != "plugin-command" {
        return None;
    }
    let mut w = words.next()?;
    while w.starts_with('-') {
        // -t target (two words) or any other flag.
        if w == "-t" {
            words.next()?;
        }
        w = words.next()?;
    }
    let plugin = w;
    let text = words.collect::<Vec<_>>().join(" ");
    Some((plugin, text))
}

/// Words of a command string, with `"..."` and `'...'` taken as one word
/// and the quotes dropped.
fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut had = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) if c == '\\' && quote == Some('"') => escaped = true,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                had = true;
            }
            None if c == '\\' => escaped = true,
            None if c.is_whitespace() => {
                if !cur.is_empty() || had {
                    out.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() || had {
        out.push(cur);
    }
    out
}

/// A binding that runs a provider's row is the same action: the row
/// takes the binding's key and the binding goes.
fn merge_keys(entries: &mut Vec<Entry>) {
    let mut drop = Vec::new();
    for i in 0..entries.len() {
        if !matches!(entries[i].source, Source::Keys | Source::Stock) {
            continue;
        }
        let Some((plugin, text)) = plugin_command_parts(&entries[i].row.command) else { continue };
        let key = entries[i].row.key.clone();
        let Some(j) = entries.iter().position(|e| e.source == Source::Plugin(plugin.clone()) && e.row.text.trim() == text.trim()) else {
            continue;
        };
        if entries[j].row.key.is_empty() {
            entries[j].row.key = key;
        }
        drop.push(i);
    }
    for i in drop.into_iter().rev() {
        entries.remove(i);
    }
}

fn is_stock(row: &Row) -> bool {
    stock::STOCK_NOTES.contains(&row.title.as_str())
}

/// A string as one tmux command argument.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// What running the entry means to tmux.
fn command_for(e: &Entry, pane: Option<u32>) -> String {
    match &e.source {
        Source::Plugin(name) => {
            let target = pane.map(|p| format!(" -t %{p}")).unwrap_or_default();
            format!("plugin-command{target} {} {}", quote(name), quote(&e.row.text))
        }
        _ => e.row.command.clone(),
    }
}

fn group_label(s: &Source) -> String {
    match s {
        Source::Plugin(name) => name.clone(),
        Source::Keys => "keys".into(),
        Source::Stock => "tmux".into(),
        Source::Items => "items".into(),
    }
}

/// The nodes: a header per source in the order the sources were
/// declared (plugins, then the config's items, then the user's keys),
/// the stock bindings last and folded.
fn build_nodes(p: &Picker, providers: &[String], stock: StockMode) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut order: Vec<Source> = providers.iter().map(|n| Source::Plugin(n.clone())).collect();
    order.push(Source::Items);
    order.push(Source::Keys);
    if stock != StockMode::Hide {
        order.push(Source::Stock);
    }
    for src in order {
        let rows: Vec<(usize, &Entry)> = p.entries.iter().enumerate().filter(|(_, e)| e.source == src).collect();
        if rows.is_empty() {
            continue;
        }
        let label = group_label(&src);
        let folded = src == Source::Stock && stock == StockMode::Fold;
        let mut g = Node::group(format!("g/{label}"), 0, !folded, folded);
        g.left = plain_cells(&label, 0);
        g.header_glyph = Some('▪');
        g.haystack = String::new();
        nodes.push(g);
        for (i, e) in rows {
            let mut n = Node::item(format!("e/{i}"), 1);
            let mut left: Vec<Styled> = plain_cells(&e.row.title, 0);
            if !e.row.hint.is_empty() {
                left.extend(plain_cells("  ", 0));
                left.extend(plain_cells(&e.row.hint, ST_DIM));
            }
            n.left = left;
            if !e.row.key.is_empty() {
                n.right = plain_cells(&e.row.key, ST_CYAN);
            }
            n.haystack = format!("{} {} {}", e.row.title, e.row.hint, label);
            let cmd = command_for(e, p.pane);
            let mut lines: Vec<Vec<Styled>> = vec![plain_cells(&e.row.title, ST_BOLD)];
            if !e.row.hint.is_empty() {
                lines.push(plain_cells(&e.row.hint, 0));
            }
            lines.push(Vec::new());
            if !e.row.key.is_empty() {
                let mut l = plain_cells("key  ", ST_DIM);
                l.extend(plain_cells(&format!("prefix {}", e.row.key), 0));
                lines.push(l);
            }
            lines.push(plain_cells("runs", ST_DIM));
            for part in cmd.split(" \\; ") {
                lines.push(plain_cells(part, 0));
            }
            n.preview = Preview::Text(lines);
            nodes.push(n);
        }
    }
    nodes
}

fn render(p: &mut Picker, settings: &Settings) {
    p.engine.header_tag = if p.pending > 0 { "gathering…".into() } else { String::new() };
    p.engine.footer = "type to search · C-j/C-k ↑/↓ move · Enter run · Esc close".into();
    let nodes = build_nodes(p, &settings.providers, settings.stock);
    let first = nodes.iter().find(|n| n.key.starts_with("e/")).map(|n| n.key.clone());
    p.engine.set_nodes(nodes);
    if !p.touched {
        if let Some(k) = first {
            p.engine.select_key(&k);
        }
    }
    let (out, _rect) = p.engine.render();
    let _ = mode_write(p.engine.mode, out.as_bytes());
}

fn refresh(state: &State, settings: &Settings) {
    if let Some(p) = state.borrow_mut().as_mut() {
        render(p, settings);
    }
}

/// Open the float and gather the rows, drawing as each source lands.
async fn open(state: State, settings: Rc<Settings>, client: Option<u64>, pane: Option<u32>) {
    let Some(window) = listkit::window_for(client, pane) else {
        let _ = display_message("palette: no window to open on");
        return;
    };
    let (ww, wh) = resolve_window(WindowId(window)).map(|wi| (wi.width, wi.height)).unwrap_or((SIZE.max_w, SIZE.max_h));
    let (width, height) = default_size(ww, wh, &SIZE);
    let mode = match mode_open(&ModeOpts {
        window: Some(WindowId(window)),
        width,
        height,
        title: Some(NAME.into()),
        ..Default::default()
    }) {
        Ok(m) => m,
        Err(e) => {
            let _ = display_message(&format!("palette: open: {}", e.message));
            return;
        }
    };
    let mut engine = Engine::new(mode, width, height, NAME, Engine::base_keys(), Vec::new());
    engine.size = SIZE;
    engine.quick = true;
    engine.filtering = true;
    engine.empty_text = "(nothing matches)".into();
    let items: Vec<Entry> = settings.items.iter().cloned().map(|row| Entry { source: Source::Items, row }).collect();
    let p = Picker { engine, pane, entries: items, pending: 1 + settings.providers.len(), touched: false };
    *state.borrow_mut() = Some(p);
    refresh(&state, &settings);

    // The bindings, through the binary the server runs (the one on PATH
    // may be another tmux).
    let bin = format_expand(OptionTarget::Server, "#{tmux_binary}").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "tmux".into());
    let cmd = format!("{} list-keys -T prefix -F '#{{key_string}}\t#{{key_note}}\t#{{key_command}}'", quote(&bin));
    let keys = match run_job(&cmd, None).await {
        Ok(out) if out.status == 0 => out.output.lines().filter_map(parse_binding).collect::<Vec<_>>(),
        Ok(out) => {
            log(&format!("palette: list-keys failed: {}", out.output.lines().last().unwrap_or("")));
            Vec::new()
        }
        Err(e) => {
            log(&format!("palette: list-keys: {}", e.message));
            Vec::new()
        }
    };
    {
        let mut b = state.borrow_mut();
        let Some(p) = b.as_mut().filter(|p| p.engine.mode.0 == mode.0) else { return };
        for row in keys {
            let source = if is_stock(&row) { Source::Stock } else { Source::Keys };
            p.entries.push(Entry { source, row });
        }
        p.pending -= 1;
        merge_keys(&mut p.entries);
        render(p, &settings);
    }

    for name in settings.providers.clone() {
        let rows: Vec<Row> = match service::call_json(&name, "palette", &serde_json::json!({})).await {
            Ok(rows) => rows,
            Err(e) => {
                log(&format!("palette: {name}: {}", e.message));
                Vec::new()
            }
        };
        let mut b = state.borrow_mut();
        let Some(p) = b.as_mut().filter(|p| p.engine.mode.0 == mode.0) else { return };
        for row in rows {
            if row.title.trim().is_empty() {
                continue;
            }
            p.entries.push(Entry { source: Source::Plugin(name.clone()), row });
        }
        p.pending -= 1;
        merge_keys(&mut p.entries);
        render(p, &settings);
    }
}

/// A key, a resize or a paste went to the engine: act on what it said.
fn execute(state: &State, settings: &Settings, outcome: Outcome) {
    let mut run: Option<String> = None;
    let mut close = false;
    {
        let mut b = state.borrow_mut();
        let Some(p) = b.as_mut() else { return };
        match outcome {
            Outcome::Nothing => return,
            Outcome::Redraw | Outcome::FilterChanged | Outcome::Expanded(..) => {
                render(p, settings);
                return;
            }
            Outcome::Close => close = true,
            Outcome::Resize(w, h) => {
                let _ = mode_resize(p.engine.mode, w, h);
                return;
            }
            Outcome::Activate(key) => {
                if let Some(label) = key.strip_prefix("g/") {
                    // Enter on a group header folds or unfolds it.
                    let k = format!("g/{label}");
                    let open = p.engine.node(&k).map(|n| p.engine.is_expanded(n)).unwrap_or(true);
                    p.engine.set_expanded(&k, !open);
                    render(p, settings);
                    return;
                }
                let Some(e) = key.strip_prefix("e/").and_then(|i| i.parse::<usize>().ok()).and_then(|i| p.entries.get(i)) else {
                    return;
                };
                run = Some(command_for(e, p.pane));
                close = true;
            }
            _ => return,
        }
    }
    if close {
        if let Some(p) = state.borrow_mut().take() {
            let _ = mode_close(p.engine.mode);
        }
    }
    if let Some(cmd) = run {
        spawn(async move {
            if let Err(e) = run_command(&cmd).await {
                let _ = display_message(&format!("palette: {}", e.message));
            }
        });
    }
}

impl Plugin for Palette {
    const NAME: &'static str = NAME;
    type Config = Config;

    fn init(ctx: &Ctx, config: Self::Config) -> Result<Self, String> {
        ctx.subscribe(&["plugin-command", "mode-key", "mode-nav", "mode-paste", "mode-resize", "mode-closed"])
            .map_err(|e| e.message.clone())?;
        let stock = match config.stock.as_deref().map(str::trim) {
            Some("hide") => StockMode::Hide,
            Some("show") => StockMode::Show,
            _ => StockMode::Fold,
        };
        let settings = Settings {
            providers: parse_providers(config.providers.as_ref()),
            items: config.items.unwrap_or_default().iter().filter_map(|s| parse_item(s)).collect(),
            stock,
        };
        Ok(Palette { state: Rc::new(RefCell::new(None)), settings: Rc::new(settings) })
    }

    fn on_event(&mut self, ctx: &Ctx, event: Event) {
        match event.name().as_str() {
            "plugin-command" => {
                match event.get_str("text").map(str::trim) {
                    Some("open") | Some("") | None => {}
                    _ => return,
                }
                if let Some(p) = self.state.borrow_mut().take() {
                    // The key again closes it.
                    let _ = mode_close(p.engine.mode);
                    return;
                }
                let client = event.scope.client.map(u64::from);
                let pane = event.scope.pane.map(u32::from);
                ctx.spawn(open(Rc::clone(&self.state), Rc::clone(&self.settings), client, pane));
            }
            "mode-key" => {
                let outcome = {
                    let mut b = self.state.borrow_mut();
                    let Some(p) = b.as_mut() else { return };
                    if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
                        return;
                    }
                    let key = event.get_str("key").unwrap_or("").to_string();
                    let mouse = match (event.get_i64("mouse_x"), event.get_i64("mouse_y")) {
                        (Some(x), Some(y)) if x >= 0 && y >= 0 => Some((x as u32, y as u32)),
                        _ => None,
                    };
                    p.touched = true;
                    p.engine.handle_key(&key, mouse)
                };
                execute(&self.state, &self.settings, outcome);
            }
            "mode-nav" => {
                let outcome = {
                    let mut b = self.state.borrow_mut();
                    let Some(p) = b.as_mut() else { return };
                    if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
                        return;
                    }
                    let dir = event.get_str("dir").unwrap_or("").to_string();
                    p.engine.handle_nav(&dir)
                };
                execute(&self.state, &self.settings, outcome);
            }
            "mode-paste" => {
                let outcome = {
                    let mut b = self.state.borrow_mut();
                    let Some(p) = b.as_mut() else { return };
                    if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
                        return;
                    }
                    let Some(text) = event.get_str("text") else { return };
                    p.engine.handle_paste(text)
                };
                execute(&self.state, &self.settings, outcome);
            }
            "mode-resize" => {
                let mut b = self.state.borrow_mut();
                let Some(p) = b.as_mut() else { return };
                if event.get_i64("mode") != Some(p.engine.mode.0 as i64) {
                    return;
                }
                let w = event.get_i64("width").map(|v| v as u32).unwrap_or(p.engine.width);
                let h = event.get_i64("height").map(|v| v as u32).unwrap_or(p.engine.height);
                p.engine.resize(w, h);
                render(p, &self.settings);
            }
            "mode-closed" => {
                let mut b = self.state.borrow_mut();
                if b.as_ref().is_some_and(|p| event.get_i64("mode") == Some(p.engine.mode.0 as i64)) {
                    *b = None;
                }
            }
            _ => {}
        }
    }
}

tmux_plugin!(Palette);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_parse() {
        let r = parse_item("Reload config|R|source-file ~/.tmux.conf").unwrap();
        assert_eq!((r.title.as_str(), r.key.as_str(), r.command.as_str()), ("Reload config", "R", "source-file ~/.tmux.conf"));
        let r = parse_item("Jump||run-shell 'x | y'").unwrap();
        assert_eq!(r.key, "");
        assert_eq!(r.command, "run-shell 'x | y'");
        assert!(parse_item("no command|k|").is_none());
        assert!(parse_item("just a title").is_none());
    }

    #[test]
    fn providers_parse() {
        assert_eq!(parse_providers(Some(&serde_json::json!("scp, agents,scp"))), vec!["scp", "agents"]);
        assert_eq!(parse_providers(Some(&serde_json::json!(["a", "b"]))), vec!["a", "b"]);
        assert!(parse_providers(None).is_empty());
    }

    #[test]
    fn bindings_parse_and_sort() {
        let stock = parse_binding("$\tRename current session\tcommand-prompt -I \"#S\" { rename-session -- \"%%\" }").unwrap();
        assert!(is_stock(&stock));
        assert_eq!(stock.key, "$");
        let user = parse_binding("T\tClipboard to a host\tplugin-command scp clipboard").unwrap();
        assert!(!is_stock(&user));
        assert!(parse_binding("x\t\tsplit-window").is_none(), "a binding without a note is not a row");
        assert!(parse_binding("garbage").is_none());
    }

    #[test]
    fn provider_rows_run_as_plugin_commands() {
        let e = Entry {
            source: Source::Plugin("scp".into()),
            row: Row { title: "OCR".into(), text: "ocr".into(), ..Row::default() },
        };
        assert_eq!(command_for(&e, Some(7)), "plugin-command -t %7 'scp' 'ocr'");
        assert_eq!(command_for(&e, None), "plugin-command 'scp' 'ocr'");
        let k = Entry { source: Source::Keys, row: Row { command: "next-layout".into(), ..Row::default() } };
        assert_eq!(command_for(&k, Some(7)), "next-layout");
    }

    #[test]
    fn plugin_commands_are_recognised() {
        assert_eq!(plugin_command_parts("plugin-command scp clipboard"), Some(("scp".into(), "clipboard".into())));
        assert_eq!(plugin_command_parts("plugin-command agents \"pick here\""), Some(("agents".into(), "pick here".into())));
        assert_eq!(plugin_command_parts("plugin-command -t %3 'scp' 'ocr'"), Some(("scp".into(), "ocr".into())));
        assert_eq!(plugin_command_parts("split-window -h"), None);
        assert_eq!(plugin_command_parts("plugin-command"), None);
    }

    #[test]
    fn a_binding_lends_its_key_to_the_provider_row() {
        let mut entries = vec![
            Entry { source: Source::Plugin("scp".into()), row: Row { title: "Clipboard to a host".into(), text: "clipboard".into(), ..Row::default() } },
            Entry { source: Source::Keys, row: Row { title: "Paste the clipboard somewhere".into(), key: "T".into(), command: "plugin-command scp clipboard".into(), ..Row::default() } },
            Entry { source: Source::Keys, row: Row { title: "Split".into(), key: "%".into(), command: "split-window -h".into(), ..Row::default() } },
        ];
        merge_keys(&mut entries);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].row.key, "T");
        assert_eq!(entries[1].row.title, "Split");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
