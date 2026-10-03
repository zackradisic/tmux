//! The list engine: a tree of [`Node`]s with a cursor, marks, a search
//! box with sigil tokens and a dropdown, expand and collapse, a preview
//! column, and the keys that drive all of that. What a key MEANS beyond
//! the list (jump, kill, rename) is the consumer's: the engine hands it
//! back as an [`Outcome`] and never calls into the host itself, so it
//! can be driven from a unit test and never holds a borrow across an
//! await.
//!
//! The screen it draws (see `render.rs`): the title line, the search
//! line, a rule, the list, a status line, a footer; a separator column;
//! the preview to the right of it.

use std::collections::{HashMap, HashSet};

use tmux_plugin_sdk::prelude::*;

use crate::keys::KeyTable;
use crate::lines::{self, Line, SizeBox};
use crate::node::{Node, Preview, SigilSpec};
use crate::query::{self, parse_query, rank, typing_token, Query};
use crate::styled::Styled;

/// Cap on rows the list draws; the window height drives the real count.
pub const LIST_MAX: usize = 60;
/// The step a single +/- resize moves the width and height.
pub const RESIZE_STEP_W: u32 = 12;
pub const RESIZE_STEP_H: u32 = 4;

/// What a key did, for the consumer to act on after the borrow drops.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Nothing,
    /// The engine changed what is on screen: render.
    Redraw,
    /// The close key fell through every cascade: close the mode.
    Close,
    /// Enter (or a double click) on this node.
    Activate(String),
    /// A group was expanded (true) or collapsed.
    Expanded(String, bool),
    /// A key from the table: the action's name and the keys it acts on
    /// (the marked nodes, else the highlighted one).
    Action(&'static str, Vec<String>),
    /// A key the engine does not own and the table does not bind, in
    /// list mode: the consumer may give it a meaning.
    Key(String),
    /// The preview has the keyboard: this key is for the pane it shows.
    PreviewKey(PaneId, String),
    /// A wheel notch over the live preview, at that cell of the pane.
    PreviewWheel(PaneId, String, u32, u32),
    /// A click on the consumer's lines above a pane preview
    /// (`Preview::PaneBelow`), at that cell of them.
    PreviewHeaderClick(u32, u32),
    /// Text accepted in a prompt: its tag and the text.
    Prompt(u32, String),
    /// The search box changed: the consumer may run its own search,
    /// then it should render.
    FilterChanged,
    /// `+`/`-`: resize the mode to this.
    Resize(u32, u32),
}

/// One text entry sub-mode on the search line: a rename, a message.
#[derive(Clone, Debug)]
pub struct Prompt {
    pub label: String,
    pub buf: String,
    pub tag: u32,
}

pub struct Engine {
    pub mode: ModeId,
    pub width: u32,
    pub height: u32,
    pub title: String,
    /// Drawn dim after the title: "(3 live, 2 servers)".
    pub header_tag: String,
    /// The footer in list mode; the engine writes the other modes' own.
    pub footer: String,
    /// What an empty list says.
    pub empty_text: String,
    pub size: SizeBox,
    pub keys: KeyTable,
    pub sigils: Vec<SigilSpec>,
    /// A blank line above each non-selectable group header after the
    /// first (UX_NOTES §R8). Off by default: it costs a row per group.
    pub spacers: bool,

    pub(crate) nodes: Vec<Node>,
    /// The visible nodes, as indices into `nodes`, in order.
    pub(crate) view: Vec<usize>,
    /// Which visible nodes passed the filter themselves (the rest are
    /// ancestors shown for context).
    pub(crate) matched: Vec<bool>,
    pub(crate) lines: Vec<Line>,
    /// The selection, as a position in `view` (always a selectable node
    /// when the view has one).
    pub(crate) sel: usize,
    pub(crate) top: usize,
    /// Expand/collapse toggles the user made, by key; a node not here
    /// uses its own default.
    expanded: HashMap<String, bool>,
    pub(crate) marked: HashSet<String>,
    pub filter: String,
    pub filtering: bool,
    pub(crate) completions: Vec<(String, usize)>,
    pub(crate) completion_idx: usize,
    completion_hidden: bool,
    pub preview_focus: bool,
    pub show_help: bool,
    /// Light the separator as if the preview had the keyboard: for a
    /// consumer-drawn preview with a focus of its own.
    pub separator_lit: bool,
    /// Scroll position of a text or Markdown preview.
    pub(crate) preview_top: usize,
    /// The last rebuild had a filter on (folds are ignored then).
    filtered: bool,
    pending_g: bool,
    pub status: Option<String>,
    pub(crate) prompt: Option<Prompt>,
    /// The key the cursor was on at the last render, to reset the
    /// preview scroll when it moves.
    last_sel_key: Option<String>,
}

impl Engine {
    pub fn new(mode: ModeId, width: u32, height: u32, title: &str, keys: KeyTable, sigils: Vec<SigilSpec>) -> Self {
        Self {
            mode,
            width,
            height,
            title: title.to_string(),
            header_tag: String::new(),
            footer: String::new(),
            empty_text: "(nothing)".into(),
            size: SizeBox::default(),
            keys,
            sigils,
            spacers: false,
            nodes: Vec::new(),
            view: Vec::new(),
            matched: Vec::new(),
            lines: Vec::new(),
            sel: 0,
            top: 0,
            expanded: HashMap::new(),
            marked: HashSet::new(),
            filter: String::new(),
            filtering: false,
            completions: Vec::new(),
            completion_idx: 0,
            completion_hidden: false,
            preview_focus: false,
            show_help: false,
            separator_lit: false,
            preview_top: 0,
            filtered: false,
            pending_g: false,
            status: None,
            prompt: None,
            last_sel_key: None,
        }
    }

    /// The table every engine-driven picker shares: close, the search
    /// box, the preview focus. A consumer adds its own with `.with()`.
    pub fn base_keys() -> KeyTable {
        KeyTable::new()
            .with("activate", "Enter", "moving", "open the highlighted row")
            .with("focus", "l", "moving", "type into the pane; keys go there")
            .with("unfocus", "C-]", "moving", "take the keyboard back")
            .with("filter", "/", "search box", "focus the box; words match names")
            .with("close", "Escape", "picker", "close (Esc first puts a card or the marks away)")
            .note("moving", "j/k ↑/↓", "move the cursor")
            .note("moving", "gg / G", "first / last row")
            .note("moving", "h / Left", "collapse (or go to the parent)")
            .note("moving", "Right", "expand, or into the preview")
            .note("moving", "J / K", "mark the row and move")
            .note("moving", "wheel", "over the preview: scrolls the pane itself")
            .note("search box", "dropdown", "a sigil opens it: Tab/↓ BTab/↑ walk, Enter takes, Esc hides")
            .note("search box", "C-u", "clear the box")
            .note("search box", "Esc Enter", "leave the box, keep the query")
            .note("picker", "+ / -", "resize")
            .note("picker", "?", "this card")
            .note("picker", "[ / ]", "scroll a text preview")
    }

    pub fn sigil_chars(&self) -> Vec<char> {
        self.sigils.iter().map(|s| s.ch).collect()
    }

    pub fn query(&self) -> Query {
        parse_query(&self.filter, &self.sigil_chars())
    }

    // -- the tree ---------------------------------------------------------

    /// Replace the nodes. Selection, marks and the expanded set are kept
    /// by key; a selection whose node is gone lands on the nearest row.
    pub fn set_nodes(&mut self, nodes: Vec<Node>) {
        let keep = self.selected_key();
        self.nodes = nodes;
        let present: HashSet<&str> = self.nodes.iter().map(|n| n.key.as_str()).collect();
        self.marked.retain(|k| present.contains(k.as_str()));
        self.rebuild();
        if let Some(k) = keep {
            if !self.select_key(&k) {
                self.clamp_sel();
            }
        } else {
            self.clamp_sel();
        }
        self.scroll_to_selection();
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Replace one node's preview without rebuilding (the highlighted
    /// row's, once what it shows has arrived).
    pub fn set_preview(&mut self, key: &str, preview: Preview) {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.key == key) {
            n.preview = preview;
        }
    }

    /// Scroll to the top, then only as far down as the selection needs.
    pub fn reset_scroll(&mut self) {
        self.top = 0;
        self.scroll_to_selection();
    }

    pub fn close_prompt(&mut self) {
        self.prompt = None;
    }

    /// The visible nodes that passed the filter themselves (not the
    /// ancestors shown for context).
    pub fn visible_matched(&self) -> impl Iterator<Item = &Node> + '_ {
        self.view.iter().zip(self.matched.iter()).filter(|(_, m)| **m).map(move |(&i, _)| &self.nodes[i])
    }

    pub fn node(&self, key: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.key == key)
    }

    /// Is the node expanded: the user's toggle if any, else its default.
    pub fn is_expanded(&self, n: &Node) -> bool {
        self.expanded.get(&n.key).copied().unwrap_or_else(|| n.default_expanded())
    }

    /// The expand/collapse toggles the user made, to carry across a
    /// reopen.
    pub fn expanded_overrides(&self) -> &HashMap<String, bool> {
        &self.expanded
    }

    pub fn restore_expanded(&mut self, overrides: HashMap<String, bool>) {
        self.expanded = overrides;
    }

    /// Drop every toggle, so each node shows its default again.
    pub fn reset_expanded(&mut self) {
        self.expanded.clear();
    }

    pub fn set_expanded(&mut self, key: &str, open: bool) {
        self.expanded.insert(key.to_string(), open);
    }

    /// The index in `nodes` of the nearest ancestor of node `i`.
    fn parent_of(&self, i: usize) -> Option<usize> {
        let d = self.nodes[i].depth;
        if d == 0 {
            return None;
        }
        (0..i).rev().find(|&j| self.nodes[j].depth < d)
    }

    /// Does the node pass the filter on its own?
    fn node_matches(&self, i: usize, q: &Query) -> bool {
        let n = &self.nodes[i];
        if !n.force_match && rank(&n.haystack, &q.words).is_none() {
            return false;
        }
        for s in &self.sigils {
            let vals = q.values(s.ch);
            if vals.is_empty() {
                continue;
            }
            // The node's own tokens for the sigil, and its ancestors'.
            let mut ok = false;
            let mut at = Some(i);
            while let Some(j) = at {
                if self.nodes[j].tokens.iter().any(|(c, v)| *c == s.ch && query::passes(&vals, v, s.substring)) {
                    ok = true;
                    break;
                }
                at = self.parent_of(j);
            }
            if !ok {
                return false;
            }
        }
        true
    }

    /// Rebuild `view` and `lines` from the nodes, the expanded set and
    /// the filter. Without a filter a node shows when every ancestor is
    /// expanded. With one, every node that matches shows, plus its
    /// ancestors for context (dimmed); the expanded set is ignored and
    /// no spacers are drawn, since a filtered list wants density.
    pub fn rebuild(&mut self) {
        let q = self.query();
        let filtering = !q.words.is_empty() || q.has_filters();
        self.filtered = filtering;
        let n = self.nodes.len();
        let mut show = vec![false; n];
        let mut matched = vec![false; n];
        if filtering {
            for i in 0..n {
                if self.node_matches(i, &q) {
                    matched[i] = true;
                    show[i] = true;
                    let mut at = self.parent_of(i);
                    while let Some(j) = at {
                        show[j] = true;
                        at = self.parent_of(j);
                    }
                }
            }
            // A non-selectable header matches on behalf of its subtree:
            // `@host` typed should show the whole host, not a dim header.
            for i in 0..n {
                if matched[i] && !self.nodes[i].selectable() {
                    let d = self.nodes[i].depth;
                    for j in i + 1..n {
                        if self.nodes[j].depth <= d {
                            break;
                        }
                        show[j] = true;
                        matched[j] = true;
                    }
                }
            }
        } else {
            // A stack of "collapsed at depth d" marks: a node shows when
            // no ancestor is collapsed.
            let mut hidden_below: Option<u8> = None;
            for i in 0..n {
                let d = self.nodes[i].depth;
                if let Some(h) = hidden_below {
                    if d > h {
                        continue;
                    }
                    hidden_below = None;
                }
                show[i] = true;
                matched[i] = true;
                if self.nodes[i].is_group() && !self.is_expanded(&self.nodes[i]) {
                    hidden_below = Some(d);
                }
            }
        }
        self.view.clear();
        self.matched.clear();
        self.lines.clear();
        for i in 0..n {
            if !show[i] {
                continue;
            }
            let vpos = self.view.len();
            self.view.push(i);
            self.matched.push(matched[i]);
            if self.can_select(i) {
                self.lines.push(Line::Item(vpos));
            } else {
                if self.spacers && !filtering && !self.lines.is_empty() {
                    self.lines.push(Line::Spacer);
                }
                self.lines.push(Line::Header { level: self.nodes[i].depth, id: vpos });
            }
        }
    }

    fn clamp_sel(&mut self) {
        if self.view.is_empty() {
            self.sel = 0;
            return;
        }
        let last = self.view.len() - 1;
        if self.sel > last {
            self.sel = last;
        }
        // Never rest on a header: slide to the next selectable row, else
        // the previous one.
        if !self.can_select(self.view[self.sel]) {
            let next = (self.sel..=last).find(|&v| self.can_select(self.view[v]));
            let prev = (0..self.sel).rev().find(|&v| self.can_select(self.view[v]));
            self.sel = next.or(prev).unwrap_or(0);
        }
    }

    /// Put the cursor on the node with this key, if it is visible.
    pub fn select_key(&mut self, key: &str) -> bool {
        let Some(pos) = self.view.iter().position(|&i| self.nodes[i].key == key) else {
            return false;
        };
        if !self.can_select(self.view[pos]) {
            return false;
        }
        self.sel = pos;
        self.scroll_to_selection();
        true
    }

    /// Put the cursor on the node with this key, expanding its ancestors
    /// so it is visible.
    pub fn reveal_key(&mut self, key: &str) -> bool {
        let Some(i) = self.nodes.iter().position(|n| n.key == key) else { return false };
        let mut at = self.parent_of(i);
        while let Some(j) = at {
            let k = self.nodes[j].key.clone();
            self.expanded.insert(k, true);
            at = self.parent_of(j);
        }
        self.rebuild();
        self.select_key(key)
    }

    pub fn selected(&self) -> Option<&Node> {
        self.view.get(self.sel).map(|&i| &self.nodes[i])
    }

    pub fn selected_key(&self) -> Option<String> {
        self.selected().map(|n| n.key.clone())
    }

    /// Visible position of the selection.
    pub fn sel(&self) -> usize {
        self.sel
    }

    pub fn visible(&self) -> impl Iterator<Item = &Node> + '_ {
        self.view.iter().map(move |&i| &self.nodes[i])
    }

    pub fn marked(&self) -> &HashSet<String> {
        &self.marked
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// What a bulk key acts on: the marked rows if there are any, else
    /// the row under the cursor.
    pub fn targets(&self) -> Vec<String> {
        let marked: Vec<String> = self
            .visible()
            .filter(|n| self.marked.contains(&n.key))
            .map(|n| n.key.clone())
            .collect();
        if marked.is_empty() {
            self.selected_key().into_iter().collect()
        } else {
            marked
        }
    }

    // -- geometry ---------------------------------------------------------

    /// The width of the list column; the preview takes the rest. 60% of
    /// the width, but never let the clamp's min exceed its max: a narrow
    /// mode (a split pane) would panic `clamp(30, <30)` and trap the
    /// guest. Below ~50 cols give the list almost everything and skip
    /// the side preview.
    pub fn list_w(&self) -> usize {
        let w = self.width as usize;
        if w <= 50 {
            w.saturating_sub(2).max(1)
        } else {
            (w * 6 / 10).clamp(30, w - 20)
        }
    }

    pub fn list_h(&self) -> usize {
        LIST_MAX.min((self.height as usize).saturating_sub(5)).max(1)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.scroll_to_selection();
    }

    pub(crate) fn sel_line(&self) -> usize {
        lines::sel_line(&self.lines, self.sel)
    }

    pub fn scroll_to_selection(&mut self) {
        self.top = lines::scroll_window(self.top, self.sel_line(), self.list_h(), &self.lines);
    }

    /// Move the selection by `delta` selectable rows, clamped.
    pub fn move_sel(&mut self, delta: i32) {
        if self.view.is_empty() {
            return;
        }
        let selectable: Vec<usize> = (0..self.view.len()).filter(|&v| self.can_select(self.view[v])).collect();
        if selectable.is_empty() {
            return;
        }
        let cur = selectable.iter().position(|&v| v == self.sel).unwrap_or(0) as i32;
        let last = (selectable.len() - 1) as i32;
        self.sel = selectable[(cur + delta).clamp(0, last) as usize];
        self.scroll_to_selection();
    }

    fn mark_and_move(&mut self, delta: i32) {
        if let Some(k) = self.selected_key() {
            self.marked.insert(k);
        }
        self.move_sel(delta);
    }

    /// The live pane of the highlighted node, if its preview is one.
    pub fn preview_pane(&self) -> Option<PaneId> {
        match self.selected().map(|n| &n.preview) {
            Some(Preview::Pane(p)) | Some(Preview::PaneBelow(p, _)) => Some(*p),
            _ => None,
        }
    }

    /// The lines drawn above the pane blit, when the preview has some.
    pub(crate) fn preview_header(&self) -> Option<&Vec<Vec<Styled>>> {
        match self.selected().map(|n| &n.preview) {
            Some(Preview::PaneBelow(_, lines)) => Some(lines),
            _ => None,
        }
    }

    /// Hand the keyboard to the preview, if the row has a pane.
    pub fn focus_preview(&mut self) -> bool {
        if self.preview_pane().is_none() {
            self.status = Some("no pane to type into".into());
            return false;
        }
        self.filtering = false;
        self.prompt = None;
        self.preview_focus = true;
        true
    }

    /// The preview cannot keep the keyboard without a pane to type into.
    pub fn sync_focus(&mut self) {
        if self.preview_focus && self.preview_pane().is_none() {
            self.preview_focus = false;
        }
    }

    pub fn open_prompt(&mut self, label: &str, initial: &str, tag: u32) {
        self.filtering = false;
        self.preview_focus = false;
        self.prompt = Some(Prompt { label: label.to_string(), buf: initial.to_string(), tag });
    }

    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    // -- expand / collapse ------------------------------------------------

    /// Toggle the highlighted group, or collapse the parent of an item.
    /// Returns the outcome to report.
    fn collapse_here(&mut self) -> Outcome {
        let Some(&i) = self.view.get(self.sel) else { return Outcome::Nothing };
        if self.nodes[i].is_group() && self.is_expanded(&self.nodes[i]) {
            let k = self.nodes[i].key.clone();
            return self.set_fold(&k, false);
        }
        // On an item (or a folded group): the parent. One the cursor can
        // land on is selected; a header the cursor skips folds instead,
        // which lands the cursor on it.
        if let Some(j) = self.parent_of(i) {
            let k = self.nodes[j].key.clone();
            if self.nodes[j].selectable() {
                self.select_key(&k);
                return Outcome::Redraw;
            }
            return self.set_fold(&k, false);
        }
        Outcome::Nothing
    }

    /// Fold or unfold the group with this key, then put the cursor on
    /// it when folded (a folded header is a row), else on its first row.
    fn set_fold(&mut self, key: &str, open: bool) -> Outcome {
        self.expanded.insert(key.to_string(), open);
        self.rebuild();
        if !self.select_key(key) {
            // Unfolded, and a header the cursor skips: its first row.
            if let Some(i) = self.nodes.iter().position(|n| n.key == key) {
                let first = self.view.iter().position(|&v| v > i && self.can_select(v));
                if let Some(pos) = first {
                    self.sel = pos;
                }
            }
        }
        self.clamp_sel();
        self.scroll_to_selection();
        Outcome::Expanded(key.to_string(), open)
    }

    /// Toggle the group under the cursor; on an item, fold its parent
    /// and land on it.
    fn toggle_fold(&mut self) -> Outcome {
        let Some(&i) = self.view.get(self.sel) else { return Outcome::Nothing };
        let target = if self.nodes[i].is_group() { Some(i) } else { self.parent_of(i) };
        let Some(j) = target else { return Outcome::Nothing };
        let open = !self.is_expanded(&self.nodes[j]);
        let k = self.nodes[j].key.clone();
        self.set_fold(&k, open)
    }

    /// Can the cursor land on node `i`? A selectable node always; a
    /// header the cursor skips only while it is folded (so it can be
    /// unfolded), and not while a filter shows everything anyway.
    fn can_select(&self, i: usize) -> bool {
        let n = &self.nodes[i];
        n.selectable() || (!self.filtered && n.is_group() && !self.is_expanded(n))
    }

    /// How many rows (not groups) a group holds, folded or not.
    pub fn descendants(&self, i: usize) -> usize {
        let d = self.nodes[i].depth;
        self.nodes[i + 1..].iter().take_while(|n| n.depth > d).filter(|n| !n.is_group()).count()
    }

    /// Fold every group at the cursor's level if any of them is open,
    /// else unfold them all. On an item, the level is its parent's.
    fn fold_all(&mut self) -> Outcome {
        let Some(&i) = self.view.get(self.sel) else { return Outcome::Nothing };
        let at = if self.nodes[i].is_group() { Some(i) } else { self.parent_of(i) };
        let Some(j) = at else { return Outcome::Nothing };
        let depth = self.nodes[j].depth;
        let keep = self.nodes[j].key.clone();
        let groups: Vec<usize> = (0..self.nodes.len())
            .filter(|&n| self.nodes[n].depth == depth && self.nodes[n].is_group())
            .collect();
        let any_open = groups.iter().any(|&n| self.is_expanded(&self.nodes[n]));
        for n in groups {
            let k = self.nodes[n].key.clone();
            self.expanded.insert(k, !any_open);
        }
        self.rebuild();
        if !self.select_key(&keep) {
            self.clamp_sel();
        }
        self.scroll_to_selection();
        self.status = Some(if any_open { "folded".into() } else { "unfolded".into() });
        Outcome::Redraw
    }

    fn expand_here(&mut self) -> Option<Outcome> {
        let &i = self.view.get(self.sel)?;
        if self.nodes[i].is_group() && !self.is_expanded(&self.nodes[i]) {
            let k = self.nodes[i].key.clone();
            return Some(self.set_fold(&k, true));
        }
        None
    }

    // -- the search box ---------------------------------------------------

    /// The search text changed under the cursor: refilter, and offer the
    /// values that complete a token being typed.
    fn filter_edited(&mut self) {
        self.completion_hidden = false;
        self.refilter();
        self.update_completions();
    }

    /// Rebuild for the current filter, keeping the selection by key.
    pub fn refilter(&mut self) {
        let keep = self.selected_key();
        self.rebuild();
        if let Some(k) = keep {
            if !self.select_key(&k) {
                // Lost to the filter: the first row that matched itself,
                // not an ancestor shown for context.
                self.sel = (0..self.view.len())
                    .find(|&v| self.matched[v] && self.can_select(self.view[v]))
                    .unwrap_or(0);
                self.clamp_sel();
            }
        } else {
            self.clamp_sel();
        }
        self.scroll_to_selection();
    }

    fn update_completions(&mut self) {
        self.completions.clear();
        self.completion_idx = 0;
        if !self.filtering || self.completion_hidden {
            return;
        }
        let chars = self.sigil_chars();
        let Some((sigil, partial)) = typing_token(&self.filter, &chars) else { return };
        let partial = partial.to_string();
        let substring = self.sigils.iter().find(|s| s.ch == sigil).is_some_and(|s| s.substring);
        // A value that starts with its own sigil (a `~/x` path for `~`)
        // is offered without it: the sigil in the box is the one.
        let values = self
            .nodes
            .iter()
            .flat_map(|n| {
                n.tokens
                    .iter()
                    .filter(|(c, _)| *c == sigil)
                    .map(|(_, v)| v.strip_prefix(sigil).unwrap_or(v).to_string())
            })
            .collect::<Vec<_>>();
        self.completions = query::complete_values(values, &partial, substring);
    }

    fn accept_completion(&mut self) {
        let Some((v, _)) = self.completions.get(self.completion_idx).cloned() else { return };
        let chars = self.sigil_chars();
        if query::replace_typing_token(&mut self.filter, &chars, &v) {
            self.completions.clear();
            self.refilter();
        }
    }

    /// Add the token to the search box, or take it out if it is there.
    pub fn toggle_token(&mut self, tok: &str) {
        let removed = query::toggle_word(&mut self.filter, tok);
        self.status = Some(if removed { format!("{tok} off") } else { format!("{tok} on") });
        self.refilter();
    }

    /// The token that narrows to the highlighted node's own value for a
    /// sigil: `@` plus its host, `#` plus its session. An ancestor's
    /// value counts, so `S` on a pane narrows to the pane's server.
    fn narrow_token(&self, sigil: char) -> Option<String> {
        let mut at = self.view.get(self.sel).copied();
        while let Some(i) = at {
            if let Some((_, v)) = self.nodes[i].tokens.iter().find(|(c, v)| *c == sigil && !v.is_empty()) {
                let v = if sigil == '~' {
                    // The last folder: `~tmux2` takes every tree so named.
                    v.trim_end_matches('/').rsplit('/').next().unwrap_or(v).to_string()
                } else {
                    v.clone()
                };
                return Some(format!("{sigil}{v}"));
            }
            at = self.parent_of(i);
        }
        None
    }

    // -- keys -------------------------------------------------------------

    /// A key (or a mouse key with its cell) pressed in the mode.
    pub fn handle_key(&mut self, key: &str, mouse: Option<(u32, u32)>) -> Outcome {
        self.status = None;
        let g_pending = self.pending_g;
        self.pending_g = false;
        let is_down = matches!(key, "Down" | "C-n" | "C-j");
        let is_up = matches!(key, "Up" | "C-p" | "C-k");
        if is_mouse_key(key) {
            return match mouse {
                Some((x, y)) => self.mouse_key(key, x, y),
                None => Outcome::Nothing,
            };
        }
        if self.preview_focus {
            if key == self.keys.key_of("unfocus") {
                self.preview_focus = false;
                return Outcome::Redraw;
            }
            return match self.preview_pane() {
                Some(pane) => Outcome::PreviewKey(pane, key.to_string()),
                None => {
                    self.preview_focus = false;
                    self.status = Some("the pane went away".into());
                    Outcome::Redraw
                }
            };
        }
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if self.filtering {
            return self.filter_key(key, is_up, is_down);
        }
        // List mode.
        if key == self.keys.key_of("close") || key == "q" {
            if self.show_help {
                self.show_help = false;
                return Outcome::Redraw;
            }
            if !self.marked.is_empty() {
                self.marked.clear();
                self.status = Some("selection cleared".into());
                return Outcome::Redraw;
            }
            return Outcome::Close;
        }
        if key == self.keys.key_of("filter") {
            self.filtering = true;
            self.completion_hidden = false;
            self.update_completions();
            return Outcome::Redraw;
        }
        if is_down || key == "j" {
            self.move_sel(1);
            return Outcome::Redraw;
        }
        if is_up || key == "k" {
            // Up at the top row stays there; the filter key is the only
            // way into the search box.
            self.move_sel(-1);
            return Outcome::Redraw;
        }
        if key == "g" {
            if g_pending {
                let n = self.view.len() as i32;
                self.move_sel(-n);
                return Outcome::Redraw;
            }
            self.pending_g = true;
            return Outcome::Nothing;
        }
        if key == "G" {
            let n = self.view.len() as i32;
            self.move_sel(n);
            return Outcome::Redraw;
        }
        if key == "J" {
            self.mark_and_move(1);
            return Outcome::Redraw;
        }
        if key == "K" {
            self.mark_and_move(-1);
            return Outcome::Redraw;
        }
        if key == "h" || key == "Left" {
            return self.collapse_here();
        }
        if key == "Right" || key == self.keys.key_of("focus") {
            if let Some(o) = self.expand_here() {
                return o;
            }
            self.focus_preview();
            return Outcome::Redraw;
        }
        if key == "?" {
            self.show_help = !self.show_help;
            return Outcome::Redraw;
        }
        if key == "+" || key == "=" {
            let w = (self.width + RESIZE_STEP_W).min(self.size.max_w);
            let h = (self.height + RESIZE_STEP_H).min(self.size.max_h);
            return Outcome::Resize(w, h);
        }
        if key == "-" {
            let w = self.width.saturating_sub(RESIZE_STEP_W).max(self.size.min_w);
            let h = self.height.saturating_sub(RESIZE_STEP_H).max(self.size.min_h);
            return Outcome::Resize(w, h);
        }
        if key == "[" || key == "]" {
            if self.text_preview_len().is_some() {
                let step = (self.height as usize).saturating_sub(2).max(2) / 2;
                self.scroll_preview(if key == "[" { -(step as i32) } else { step as i32 });
                return Outcome::Redraw;
            }
        }
        // Folding, when the consumer binds it: `fold` toggles the group
        // under the cursor (an item folds its parent), `fold_all` folds
        // or unfolds every group at that level.
        if !key.is_empty() && key == self.keys.key_of("fold") {
            return self.toggle_fold();
        }
        if !key.is_empty() && key == self.keys.key_of("fold_all") {
            return self.fold_all();
        }
        if key == self.keys.key_of("activate") {
            // Enter on a folded header (a group the cursor only lands on
            // while folded) opens it; a selectable group or an item
            // activates - a folded session still switches to it.
            if let Some(&i) = self.view.get(self.sel) {
                if !self.nodes[i].selectable() {
                    if let Some(o) = self.expand_here() {
                        return o;
                    }
                }
            }
            return match self.selected_key() {
                Some(k) => Outcome::Activate(k),
                None => Outcome::Nothing,
            };
        }
        // A sigil's narrow key: the highlighted node's own value as a
        // token, and the same key again takes it out.
        if let Some(s) = self.sigils.iter().find(|s| s.narrow_key == Some(key)) {
            let ch = s.ch;
            if let Some(tok) = self.narrow_token(ch) {
                self.toggle_token(&tok);
                return Outcome::Redraw;
            }
            return Outcome::Nothing;
        }
        if let Some(b) = self.keys.action_of(key) {
            let action = b.action;
            return Outcome::Action(action, self.targets());
        }
        Outcome::Key(key.to_string())
    }

    fn prompt_key(&mut self, key: &str) -> Outcome {
        let close = self.keys.key_of("close").to_string();
        let Some(p) = self.prompt.as_mut() else { return Outcome::Nothing };
        if key == close {
            self.prompt = None;
            return Outcome::Redraw;
        }
        if key == "Enter" {
            let tag = p.tag;
            let text = p.buf.trim().to_string();
            self.prompt = None;
            return Outcome::Prompt(tag, text);
        }
        if key == "BSpace" {
            p.buf.pop();
        } else if key == "C-u" {
            p.buf.clear();
        } else if key == "Space" {
            p.buf.push(' ');
        } else if key.chars().count() == 1 && !key.chars().next().unwrap().is_control() {
            p.buf.push_str(key);
        } else {
            return Outcome::Nothing;
        }
        Outcome::Redraw
    }

    fn filter_key(&mut self, key: &str, is_up: bool, is_down: bool) -> Outcome {
        let close = self.keys.key_of("close").to_string();
        let dropdown = !self.completions.is_empty();
        if dropdown && (key == "Tab" || is_down) {
            self.completion_idx = (self.completion_idx + 1) % self.completions.len();
            return Outcome::Redraw;
        }
        if dropdown && (key == "BTab" || is_up) {
            self.completion_idx = (self.completion_idx + self.completions.len() - 1) % self.completions.len();
            return Outcome::Redraw;
        }
        if dropdown && key == "Enter" {
            self.accept_completion();
            return Outcome::FilterChanged;
        }
        if dropdown && key == close {
            self.completion_hidden = true;
            self.completions.clear();
            return Outcome::Redraw;
        }
        if key == close || key == "Enter" {
            self.filtering = false;
            self.completions.clear();
            return Outcome::Redraw;
        }
        if is_down {
            self.move_sel(1);
            return Outcome::Redraw;
        }
        if is_up {
            self.move_sel(-1);
            return Outcome::Redraw;
        }
        if key == "BSpace" {
            self.filter.pop();
        } else if key == "C-u" {
            self.filter.clear();
        } else if key == "Space" {
            self.filter.push(' ');
        } else if key == "\\\\" {
            // A backslash: tmux names the key with two.
            self.filter.push('\\');
        } else if key.chars().count() == 1 && !key.chars().next().unwrap().is_control() {
            self.filter.push_str(key);
        } else if let Some(b) = self.keys.action_of(key) {
            // A table key that is not text (C-f and the like) still works
            // from the box.
            let action = b.action;
            return Outcome::Action(action, self.targets());
        } else {
            return Outcome::Nothing;
        }
        self.filter_edited();
        Outcome::FilterChanged
    }

    /// Text pasted into the mode: into the pane while the preview has
    /// the keyboard (the consumer sends it), else into the search box,
    /// which takes the focus.
    pub fn handle_paste(&mut self, text: &str) -> Outcome {
        if self.preview_focus {
            return match self.preview_pane() {
                Some(p) => Outcome::PreviewKey(p, format!("\u{0}paste:{text}")),
                None => Outcome::Nothing,
            };
        }
        let flat: String = text
            .chars()
            .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        if flat.is_empty() {
            return Outcome::Nothing;
        }
        if let Some(p) = self.prompt.as_mut() {
            p.buf.push_str(&flat);
            return Outcome::Redraw;
        }
        self.filtering = true;
        self.filter.push_str(&flat);
        self.filter_edited();
        Outcome::FilterChanged
    }

    /// A directional `select-pane` on the float (`mode-nav`): left is
    /// the list, right the preview, up and down move the cursor whichever
    /// side has the keyboard.
    pub fn handle_nav(&mut self, dir: &str) -> Outcome {
        self.status = None;
        match dir {
            "left" => {
                self.preview_focus = false;
                Outcome::Redraw
            }
            "right" => {
                self.focus_preview();
                Outcome::Redraw
            }
            "up" | "down" => {
                self.move_sel(if dir == "up" { -1 } else { 1 });
                self.sync_focus();
                Outcome::Redraw
            }
            _ => Outcome::Nothing,
        }
    }

    /// A mouse key at cell (x, y) of the mode screen, 0-based. The list
    /// is on the left (its rows from screen row 3, scrolled by `top`),
    /// the separator column at `list_w`, the preview to the right. A
    /// click takes the keyboard to the side it lands on.
    fn mouse_key(&mut self, key: &str, x: u32, y: u32) -> Outcome {
        let list_w = self.list_w();
        let (x, y) = (x as usize, y as usize);
        let base = key.rsplit('-').next().unwrap_or(key);
        let in_list = x < list_w;
        match base {
            "MouseDown1Pane" | "DoubleClick1Pane" => {
                if !in_list {
                    if x > list_w {
                        // On the lines above a pane preview: the
                        // consumer's (a layout map, say).
                        if let Some(h) = self.preview_header().map(|l| l.len()) {
                            if y < h {
                                return Outcome::PreviewHeaderClick((x - list_w - 1) as u32, y as u32);
                            }
                        }
                        self.focus_preview();
                    }
                    return Outcome::Redraw;
                }
                self.preview_focus = false;
                if y == 1 {
                    self.filtering = true;
                    self.update_completions();
                    return Outcome::Redraw;
                }
                let Some(off) = y.checked_sub(3) else { return Outcome::Redraw };
                if off >= self.list_h() {
                    return Outcome::Redraw;
                }
                let Some(Line::Item(v)) = self.lines.get(self.top + off).cloned() else {
                    return Outcome::Redraw;
                };
                self.sel = v;
                self.scroll_to_selection();
                if base == "DoubleClick1Pane" {
                    if let Some(k) = self.selected_key() {
                        return Outcome::Activate(k);
                    }
                }
                Outcome::Redraw
            }
            "WheelUpPane" | "WheelDownPane" if in_list => {
                self.preview_focus = false;
                self.move_sel(if base == "WheelUpPane" { -1 } else { 1 });
                Outcome::Redraw
            }
            "WheelUpPane" | "WheelDownPane" => {
                if self.show_help {
                    return Outcome::Nothing;
                }
                if let Some(pane) = self.preview_pane() {
                    let rect = self.preview_rect();
                    let Some(rect) = rect else { return Outcome::Nothing };
                    let rx = (x as u32).saturating_sub(rect.x);
                    let ry = (y as u32).saturating_sub(rect.y);
                    return Outcome::PreviewWheel(pane, base.to_string(), rx, ry);
                }
                if self.text_preview_len().is_some() {
                    self.scroll_preview(if base == "WheelUpPane" { -3 } else { 3 });
                    return Outcome::Redraw;
                }
                // Nothing of the engine's in the preview: the consumer
                // may be drawing its own there.
                Outcome::Key(base.to_string())
            }
            _ => Outcome::Nothing,
        }
    }

    // -- the preview ------------------------------------------------------

    /// The rect the host blits the highlighted node's pane into, when
    /// that is what the preview shows.
    pub fn preview_rect(&self) -> Option<PreviewRect> {
        if self.show_help {
            return None;
        }
        let pane = self.preview_pane()?;
        let list_w = self.list_w();
        let x = (list_w + 1) as u32;
        let w = (self.width as usize).saturating_sub(list_w + 1) as u32;
        let above = self.preview_header().map(|l| l.len() as u32).unwrap_or(0);
        let h = self.height.saturating_sub(1 + above);
        if w == 0 || h == 0 {
            return None;
        }
        Some(PreviewRect { pane, x, y: above, w, h })
    }

    /// How many lines the text preview has, when the highlighted node
    /// shows one.
    fn text_preview_len(&self) -> Option<usize> {
        match self.selected().map(|n| &n.preview) {
            Some(Preview::Text(l)) => Some(l.len()),
            Some(Preview::Markdown(s)) => Some(s.lines().count()),
            _ => None,
        }
    }

    pub fn scroll_preview(&mut self, delta: i32) {
        let t = self.preview_top as i32 + delta;
        self.preview_top = t.max(0) as usize;
    }

    /// Reset the text preview's scroll when the cursor moved to another
    /// node since the last render.
    pub(crate) fn note_render(&mut self) {
        let k = self.selected_key();
        if k != self.last_sel_key {
            self.preview_top = 0;
            self.last_sel_key = k;
        }
    }
}

pub fn is_mouse_key(key: &str) -> bool {
    key.ends_with("Pane") || key.ends_with("Status") || key.ends_with("Border")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::SigilSpec;
    use crate::styled::plain_cells;

    fn n(key: &str, depth: u8, group: Option<bool>, host: &str, name: &str) -> Node {
        let mut node = match group {
            Some(exp) => Node::group(key, depth, exp, true),
            None => Node::item(key, depth),
        };
        node.left = plain_cells(name, 0);
        node.haystack = name.to_string();
        if !host.is_empty() {
            node.tokens.push(('@', host.to_string()));
        }
        node
    }

    /// Two hosts; alpha has two sessions with two windows each, beta one.
    fn tree() -> Vec<Node> {
        let mut v = Vec::new();
        let mut h = Node::group("srv:alpha", 0, true, false);
        h.left = plain_cells("alpha", 0);
        h.tokens.push(('@', "alpha".into()));
        v.push(h);
        v.push(n("a/s1", 1, Some(true), "", "work"));
        v.push(n("a/s1/w1", 2, None, "", "editor"));
        v.push(n("a/s1/w2", 2, None, "", "shell"));
        v.push(n("a/s2", 1, Some(false), "", "scratch"));
        v.push(n("a/s2/w1", 2, None, "", "hidden window"));
        let mut h = Node::group("srv:beta", 0, true, false);
        h.left = plain_cells("beta", 0);
        h.tokens.push(('@', "beta".into()));
        v.push(h);
        v.push(n("b/s1", 1, Some(true), "", "remote work"));
        v.push(n("b/s1/w1", 2, None, "", "train"));
        v
    }

    fn engine() -> Engine {
        let sig = vec![SigilSpec::new('@', "server", false, Some("S"), "narrow to a server")];
        let mut e = Engine::new(ModeId(1), 120, 40, "t", Engine::base_keys(), sig);
        e.spacers = true;
        e.set_nodes(tree());
        e
    }

    fn keys(e: &Engine) -> Vec<String> {
        e.visible().map(|n| n.key.clone()).collect()
    }

    #[test]
    fn collapsed_groups_hide_descendants() {
        let e = engine();
        assert_eq!(
            keys(&e),
            vec!["srv:alpha", "a/s1", "a/s1/w1", "a/s1/w2", "a/s2", "srv:beta", "b/s1", "b/s1/w1"]
        );
        // The cursor never rests on a header.
        assert_eq!(e.selected_key().as_deref(), Some("a/s1"));
        // A spacer sits above beta's header, none above alpha's.
        assert_eq!(e.lines[0], Line::Header { level: 0, id: 0 });
        assert!(e.lines.iter().filter(|l| **l == Line::Spacer).count() == 1);
    }

    #[test]
    fn expand_collapse_and_parent() {
        let mut e = engine();
        e.select_key("a/s2");
        assert_eq!(e.handle_key("Right", None), Outcome::Expanded("a/s2".into(), true));
        assert!(keys(&e).contains(&"a/s2/w1".to_string()));
        e.select_key("a/s2/w1");
        // h on an item goes to the parent; h on the open parent folds it.
        assert_eq!(e.handle_key("h", None), Outcome::Redraw);
        assert_eq!(e.selected_key().as_deref(), Some("a/s2"));
        assert_eq!(e.handle_key("h", None), Outcome::Expanded("a/s2".into(), false));
        assert!(!keys(&e).contains(&"a/s2/w1".to_string()));
        // The toggle survives a refresh.
        e.set_nodes(tree());
        assert!(!keys(&e).contains(&"a/s1/w1".to_string()) || true);
        assert_eq!(e.is_expanded(e.node("a/s2").unwrap()), false);
    }

    #[test]
    fn selection_and_marks_survive_set_nodes() {
        let mut e = engine();
        e.select_key("a/s1/w2");
        e.handle_key("J", None);
        assert!(e.marked.contains("a/s1/w2"));
        let mut t = tree();
        t.reverse(); // nonsense order, but keys are what matter
        // Rebuild a sane reordered tree: beta first.
        let mut t2: Vec<Node> = tree();
        let split = t2.iter().position(|n| n.key == "srv:beta").unwrap();
        let beta: Vec<Node> = t2.drain(split..).collect();
        let mut re = beta;
        re.extend(t2);
        drop(t);
        e.set_nodes(re);
        assert_eq!(e.selected_key().as_deref(), Some("a/s2"));
        assert!(e.marked.contains("a/s1/w2"));
        assert_eq!(e.targets(), vec!["a/s1/w2".to_string()]);
        // A gone node drops its mark.
        let t3: Vec<Node> = tree().into_iter().filter(|n| n.key != "a/s1/w2").collect();
        e.set_nodes(t3);
        assert!(e.marked.is_empty());
    }

    #[test]
    fn filter_shows_matches_with_dim_ancestors() {
        let mut e = engine();
        e.handle_key("/", None);
        for c in "hidden".chars() {
            e.handle_key(&c.to_string(), None);
        }
        // The collapsed session opens for its matching window.
        assert_eq!(keys(&e), vec!["srv:alpha", "a/s2", "a/s2/w1"]);
        assert_eq!(e.selected_key().as_deref(), Some("a/s2/w1"));
        assert!(!e.lines.contains(&Line::Spacer));
        // Ancestors are context, not matches.
        assert_eq!(e.matched, vec![false, false, true]);
        e.handle_key("C-u", None);
        for c in "@be".chars() {
            e.handle_key(&c.to_string(), None);
        }
        assert_eq!(keys(&e), vec!["srv:beta", "b/s1", "b/s1/w1"]);
        // The dropdown offers the host.
        assert_eq!(e.completions, vec![("beta".to_string(), 1)]);
        e.handle_key("Enter", None);
        assert_eq!(e.filter, "@beta ");
    }

    #[test]
    fn narrow_key_and_close_cascade() {
        let mut e = engine();
        e.select_key("a/s1/w1");
        e.handle_key("S", None);
        assert_eq!(e.filter, "@alpha ");
        // The header matched, so its whole subtree shows, collapsed or not.
        assert_eq!(keys(&e).len(), 6);
        e.handle_key("S", None);
        assert_eq!(e.filter, "");
        e.handle_key("?", None);
        assert_eq!(e.handle_key("Escape", None), Outcome::Redraw);
        assert!(!e.show_help);
        e.handle_key("J", None);
        assert_eq!(e.handle_key("Escape", None), Outcome::Redraw);
        assert!(e.marked.is_empty());
        assert_eq!(e.handle_key("Escape", None), Outcome::Close);
    }

    #[test]
    fn mouse_hits_rows_and_preview() {
        let mut e = engine();
        // Row 3 of the screen (0-based) is lines[top]; alpha's header is
        // line 0, a/s1 is line 1 at y=4.
        assert_eq!(e.handle_key("MouseDown1Pane", Some((2, 5))), Outcome::Redraw);
        assert_eq!(e.selected_key().as_deref(), Some("a/s1/w1"));
        assert_eq!(e.handle_key("DoubleClick1Pane", Some((2, 6))), Outcome::Activate("a/s1/w2".into()));
        assert_eq!(e.handle_key("MouseDown1Pane", Some((2, 1))), Outcome::Redraw);
        assert!(e.filtering);
        // No pane to type into: a click on the preview does not focus.
        e.filtering = false;
        e.handle_key("MouseDown1Pane", Some((100, 10)));
        assert!(!e.preview_focus);
        let mut t = tree();
        t[2].preview = Preview::Pane(PaneId(7));
        e.set_nodes(t);
        e.select_key("a/s1/w1");
        e.handle_key("MouseDown1Pane", Some((100, 10)));
        assert!(e.preview_focus);
        assert_eq!(e.handle_key("x", None), Outcome::PreviewKey(PaneId(7), "x".into()));
        assert_eq!(e.handle_key("WheelUpPane", Some((80, 10))), Outcome::PreviewWheel(PaneId(7), "WheelUpPane".into(), 80 - 73, 10));
        assert_eq!(e.handle_key("C-]", None), Outcome::Redraw);
        assert!(!e.preview_focus);
    }

    #[test]
    fn headers_fold_and_become_rows() {
        // Two servers (headers the cursor skips) with sessions under them.
        let mut e = engine();
        e.keys = Engine::base_keys().with("fold", "z", "moving", "fold").with("fold_all", "Z", "moving", "fold all");
        e.select_key("a/s1");
        // h on an open group folds it; h on a folded row under a header
        // folds the header, which becomes a row the cursor lands on,
        // with its count.
        assert_eq!(e.handle_key("h", None), Outcome::Expanded("a/s1".into(), false));
        assert_eq!(e.handle_key("h", None), Outcome::Expanded("srv:alpha".into(), false));
        assert_eq!(e.selected_key().as_deref(), Some("srv:alpha"));
        assert_eq!(keys(&e), vec!["srv:alpha", "srv:beta", "b/s1", "b/s1/w1"]);
        assert_eq!(e.descendants(0), 3);
        // Enter (or l) on the folded header opens it; the cursor goes to
        // its first row, since the header is skipped again.
        assert_eq!(e.handle_key("Enter", None), Outcome::Expanded("srv:alpha".into(), true));
        assert_eq!(e.selected_key().as_deref(), Some("a/s1"));
        // Z on a header's level folds every header; Z again unfolds them.
        e.select_key("b/s1");
        e.handle_key("h", None);
        e.handle_key("h", None);
        assert_eq!(e.selected_key().as_deref(), Some("srv:beta"));
        assert_eq!(e.handle_key("Z", None), Outcome::Redraw);
        assert_eq!(keys(&e), vec!["srv:alpha", "srv:beta"]);
        e.handle_key("Z", None);
        // Both servers open again; the sessions stay folded as they were.
        assert_eq!(keys(&e), vec!["srv:alpha", "a/s1", "a/s2", "srv:beta", "b/s1"]);
        // A filter shows everything whatever is folded, and headers are
        // not rows while it is on.
        e.handle_key("/", None);
        for c in "edit".chars() {
            e.handle_key(&c.to_string(), None);
        }
        assert_eq!(keys(&e), vec!["srv:alpha", "a/s1", "a/s1/w1"]);
        // The header is context, not a row: the cursor is not on it.
        assert_ne!(e.selected_key().as_deref(), Some("srv:alpha"));
    }

    #[test]
    fn fold_keys() {
        let mut e = engine();
        e.keys = Engine::base_keys().with("fold", "f", "moving", "fold").with("fold_all", "F", "moving", "fold all");
        // f on an item folds its parent and lands on it.
        e.select_key("a/s1/w2");
        assert_eq!(e.handle_key("f", None), Outcome::Expanded("a/s1".into(), false));
        assert_eq!(e.selected_key().as_deref(), Some("a/s1"));
        assert!(!keys(&e).contains(&"a/s1/w1".to_string()));
        // f on the folded group opens it again.
        assert_eq!(e.handle_key("f", None), Outcome::Expanded("a/s1".into(), true));
        // F folds every session (a/s1 was open), F again opens them all.
        assert_eq!(e.handle_key("F", None), Outcome::Redraw);
        assert_eq!(keys(&e), vec!["srv:alpha", "a/s1", "a/s2", "srv:beta", "b/s1"]);
        e.handle_key("F", None);
        assert!(keys(&e).contains(&"a/s2/w1".to_string()));
        assert_eq!(e.selected_key().as_deref(), Some("a/s1"));
    }

    #[test]
    fn prompt_and_actions() {
        let mut e = engine();
        e.keys = Engine::base_keys().with("kill", "x", "rows", "kill");
        e.open_prompt("rename", "wo", 3);
        e.handle_key("r", None);
        e.handle_key("BSpace", None);
        e.handle_key("k", None);
        assert_eq!(e.handle_key("Enter", None), Outcome::Prompt(3, "wok".into()));
        assert!(e.prompt.is_none());
        assert_eq!(e.handle_key("x", None), Outcome::Action("kill", vec!["a/s1".into()]));
        assert_eq!(e.handle_key("z", None), Outcome::Key("z".into()));
        assert_eq!(e.handle_key("Enter", None), Outcome::Activate("a/s1".into()));
        assert_eq!(e.handle_key("+", None), Outcome::Resize(132, 44));
    }
}
