//! What a consumer hands the engine on every refresh: a flat, pre-order
//! list of nodes with a depth each. The engine never knows what a node
//! IS (a session, an agent, a server); it knows its key, its place in
//! the tree, what to draw, what to search, and what to show beside it.

use tmux_plugin_sdk::prelude::PaneId;

use crate::styled::Styled;

/// A group folds its descendants (the nodes that follow it at a greater
/// depth); an item is a leaf. A group the cursor can land on (a session,
/// a window) is `selectable`; one that only labels what is under it (a
/// server header) is not, and gets a spacer above it.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Group { expanded: bool, selectable: bool },
    Item,
}

/// What the preview column shows while the node is highlighted.
#[derive(Clone, Debug)]
pub enum Preview {
    None,
    /// A live mirror of a local pane.
    Pane(PaneId),
    /// A live mirror of a local pane, under a few lines of the
    /// consumer's own (a layout map, a title).
    PaneBelow(PaneId, Vec<Vec<Styled>>),
    /// Pre-styled lines (a capture, an info card).
    Text(Vec<Vec<Styled>>),
    /// Markdown, rendered at the preview's width.
    Markdown(String),
}

#[derive(Clone, Debug)]
pub struct Node {
    /// Stable identity, unique across the whole list: selection, marks
    /// and the expanded set key on it, so they survive a refresh that
    /// reorders the nodes.
    pub key: String,
    /// 0 is top level; children follow their parent at `depth + 1`.
    pub depth: u8,
    pub kind: NodeKind,
    /// The row's text, styled by the consumer (a badge, a bold name).
    pub left: Vec<Styled>,
    /// The right-aligned meta column; clipped before `left` is.
    pub right: Vec<Styled>,
    /// What the free words of the search box rank against.
    pub haystack: String,
    /// The values this node carries for each filter sigil: `('@', host)`,
    /// `('#', session)`. A descendant inherits its ancestors' tokens for
    /// filtering, so a pane need not repeat its session's name.
    pub tokens: Vec<(char, String)>,
    pub preview: Preview,
    /// Drawn dimmed: stale, disconnected, or an ancestor shown only for
    /// context while a filter is on.
    pub dim: bool,
    /// "You are here": a bright left border.
    pub here: bool,
    /// Shown whatever the search words say (a hit the consumer found in
    /// the node's content); the sigil tokens still apply.
    pub force_match: bool,
    /// How a non-selectable header draws: `Some(glyph)` puts the glyph
    /// and a space before bold text (a server); `None` draws the text as
    /// given, indented (a band under a server).
    pub header_glyph: Option<char>,
    /// Cells of indent before the row's text; `None` is two per depth.
    pub indent: Option<u8>,
}

impl Node {
    pub fn item(key: impl Into<String>, depth: u8) -> Self {
        Self {
            key: key.into(),
            depth,
            kind: NodeKind::Item,
            left: Vec::new(),
            right: Vec::new(),
            haystack: String::new(),
            tokens: Vec::new(),
            preview: Preview::None,
            dim: false,
            here: false,
            force_match: false,
            header_glyph: Some('▪'),
            indent: None,
        }
    }

    /// The row's indent in cells.
    pub fn indent_cells(&self) -> usize {
        self.indent.map(usize::from).unwrap_or(self.depth as usize * 2)
    }

    pub fn group(key: impl Into<String>, depth: u8, expanded: bool, selectable: bool) -> Self {
        let mut n = Self::item(key, depth);
        n.kind = NodeKind::Group { expanded, selectable };
        n
    }

    pub fn is_group(&self) -> bool {
        matches!(self.kind, NodeKind::Group { .. })
    }

    pub fn selectable(&self) -> bool {
        match self.kind {
            NodeKind::Group { selectable, .. } => selectable,
            NodeKind::Item => true,
        }
    }

    pub fn default_expanded(&self) -> bool {
        match self.kind {
            NodeKind::Group { expanded, .. } => expanded,
            NodeKind::Item => true,
        }
    }
}

/// A filter sigil the consumer supports: the character that starts a
/// token in the search box, whether a typed value matches by substring
/// (a directory's tail) rather than prefix, and the key that toggles a
/// token for the highlighted node's own value (`S` for its server).
#[derive(Clone, Debug)]
pub struct SigilSpec {
    pub ch: char,
    /// What the value is, for the `?` card: `@server`, `#session`.
    pub noun: &'static str,
    pub substring: bool,
    pub narrow_key: Option<&'static str>,
    /// What the `?` card says it does: "narrow to a server (prefix)".
    pub help: &'static str,
}

impl SigilSpec {
    pub const fn new(
        ch: char,
        noun: &'static str,
        substring: bool,
        narrow_key: Option<&'static str>,
        help: &'static str,
    ) -> Self {
        Self { ch, noun, substring, narrow_key, help }
    }
}
