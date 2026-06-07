//! The debugger model: the whole application state the interface draws from.
//!
//! This module carries no terminal-UI types. Input actions and DAP events mutate
//! the model, rendering reads it, and the model can be exercised without a
//! terminal. A future non-interactive face can reuse the model and supply its own
//! rendering.

use dap_client::dap::types::StackFrame;
use dap_client::model::{SessionState, VarNode};

/// The selected frame is the cursor of debugging. It is a single field. The stack
/// view, the variables view, and evaluation all read it, so they can never
/// disagree about which frame is current.
pub struct DebuggerModel {
    pub state: SessionState,
    pub thread_id: Option<i64>,
    pub stack: Vec<StackFrame>,
    /// Index into `stack` of the one selected frame shared across the interface.
    pub selected_frame: usize,
    /// The selected frame's id, the target for evaluation. Cleared when no frame
    /// is resolved, so evaluation while running is refused rather than aimed at a
    /// dead frame.
    pub frame_id: Option<i64>,
    pub stop_reason: String,
    pub stop_count: u64,
    /// Generation of the current frame view. It advances on every stop, resume,
    /// and frame selection change. Async replies tagged with a superseded
    /// generation are discarded, so a fetch in flight for an old frame never
    /// lands on the new one.
    pub epoch: u64,
    /// Scopes of the selected frame.
    pub roots: Vec<VarNode>,
    /// Durable watch list. The expressions survive re-rooting. Each state is
    /// ephemeral and re-evaluated against the selected frame.
    pub watches: Vec<Watch>,
    /// Evaluation results and messages, append-only so result indices stay stable
    /// for async children replies.
    pub transcript: Vec<TranscriptEntry>,
    /// The last evaluated expression, so an empty input line repeats it.
    pub last_expression: Option<String>,
    pub status: String,
    pub should_quit: bool,
    /// Cursor among the flattened variable, watch, and result rows.
    pub selected_row: usize,
    /// The flattened rows the panes draw, rebuilt from the state above whenever
    /// it changes. Holding them here lets a keypress and the render that follows
    /// share one flatten of the tree instead of each building its own.
    pub rows: Vec<Row>,
    pub input: String,
    pub mode: InputMode,
    pub show_help: bool,
}

/// Whether typed characters edit the expression line or act as commands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
}

/// A pinned watch: a durable expression plus its most recent evaluation against
/// the selected frame.
pub struct Watch {
    pub expression: String,
    pub state: WatchState,
}

/// The evaluation state of a watch at the current frame.
pub enum WatchState {
    /// Evaluation in flight, or not yet started for this frame.
    Pending,
    Resolved(VarNode),
    /// Did not resolve in the current frame, for example stepped out of scope.
    Unavailable,
}

/// One line of the transcript. An evaluated value keeps its node so it can be
/// expanded and watched with the same machinery as a frame variable. The rest are
/// messages.
pub enum TranscriptEntry {
    Evaluated { node: VarNode },
    Error { message: String },
    Unavailable { reason: String },
    Unrecognized { input: String },
    Help,
}

/// Which forest a flattened row belongs to, so a fetched child or a watch toggle
/// lands on the right backing node.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tree {
    Watch,
    Scope,
    Result,
}

/// The shape of a flattened row, which decides whether the cursor can land on it
/// and how it draws.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A non-selectable section divider.
    Header,
    /// A non-selectable transcript message.
    Message,
    /// A real variable, watch, or evaluation-result node.
    Node,
    /// A pending or unavailable watch line carrying status text.
    Placeholder,
}

/// How a message row reads, so the view can color it without parsing the text.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Error,
    Note,
}

/// A flattened, currently-visible row. `path` locates the backing node within its
/// `tree`: for scopes it indexes `roots`, for watches `path[0]` indexes `watches`
/// and the rest descends the resolved node, for results `path[0]` indexes
/// `transcript` and the rest descends the evaluated node. Rows are addressed by
/// position, not by `variablesReference`, so a watched variable and its in-tree
/// twin stay independently selectable.
pub struct Row {
    pub tree: Tree,
    pub kind: RowKind,
    pub tone: Tone,
    pub path: Vec<usize>,
    pub depth: usize,
    pub name: String,
    pub value: String,
    pub ty: String,
    pub eval_name: String,
    pub expandable: bool,
    pub expanded: bool,
    pub fetched: bool,
    /// A watch root row, drawn with a pin marker.
    pub watched: bool,
}

impl Row {
    pub fn selectable(&self) -> bool {
        matches!(self.kind, RowKind::Node | RowKind::Placeholder)
    }
}

impl DebuggerModel {
    pub fn new() -> Self {
        DebuggerModel {
            state: SessionState::Connecting,
            thread_id: None,
            stack: Vec::new(),
            selected_frame: 0,
            frame_id: None,
            stop_reason: String::new(),
            stop_count: 0,
            epoch: 0,
            roots: Vec::new(),
            watches: Vec::new(),
            transcript: Vec::new(),
            last_expression: None,
            status: "waiting for the program to stop (breakpoint or step)…".to_string(),
            should_quit: false,
            selected_row: 0,
            rows: Vec::new(),
            input: String::new(),
            mode: InputMode::Normal,
            show_help: false,
        }
    }

    /// The selected frame, or nothing when the stack is empty.
    pub fn selected_stack_frame(&self) -> Option<&StackFrame> {
        self.stack.get(self.selected_frame)
    }

    /// Select the caller of the current frame. False at the outermost frame.
    pub fn select_caller_frame(&mut self) -> bool {
        if self.selected_frame + 1 < self.stack.len() {
            self.selected_frame += 1;
            true
        } else {
            false
        }
    }

    /// Select the callee of the current frame. False at the innermost frame.
    pub fn select_callee_frame(&mut self) -> bool {
        if self.selected_frame > 0 {
            self.selected_frame -= 1;
            true
        } else {
            false
        }
    }

    pub fn push_transcript(&mut self, entry: TranscriptEntry) {
        self.transcript.push(entry);
    }

    /// Locate a scope node by positional path.
    pub fn scope_node_mut(&mut self, path: &[usize]) -> Option<&mut VarNode> {
        let (&first, rest) = path.split_first()?;
        descend(self.roots.get_mut(first)?, rest)
    }

    /// Locate a watch node by positional path.
    pub fn watch_node_mut(&mut self, path: &[usize]) -> Option<&mut VarNode> {
        let (&watch_index, rest) = path.split_first()?;
        let WatchState::Resolved(root) = &mut self.watches.get_mut(watch_index)?.state else {
            return None;
        };
        descend(root, rest)
    }

    /// Locate a node within a watch's subtree by the watch's stable expression,
    /// used when an async children reply lands so a concurrent watch-list edit
    /// can't misdirect it.
    pub fn watch_node_mut_by_expression(
        &mut self,
        expression: &str,
        subpath: &[usize],
    ) -> Option<&mut VarNode> {
        let watch = self
            .watches
            .iter_mut()
            .find(|w| w.expression == expression)?;
        let WatchState::Resolved(root) = &mut watch.state else {
            return None;
        };
        descend(root, subpath)
    }

    /// Locate an evaluation-result node by positional path. The transcript is
    /// append-only, so the leading index is stable across the fetch round-trip.
    pub fn result_node_mut(&mut self, path: &[usize]) -> Option<&mut VarNode> {
        let (&index, rest) = path.split_first()?;
        let TranscriptEntry::Evaluated { node } = self.transcript.get_mut(index)? else {
            return None;
        };
        descend(node, rest)
    }

    /// Flatten the watch section, the selected frame's scopes, and the transcript
    /// into the rows currently visible. Watches come first under a header, then
    /// the scopes, then the transcript. Every node row carries its `(tree, path)`
    /// address. This is the pure source the cached `rows` are built from.
    pub fn build_rows(&self) -> Vec<Row> {
        let mut out = Vec::new();

        if !self.watches.is_empty() {
            out.push(Row::header("watched"));
            for (watch_index, watch) in self.watches.iter().enumerate() {
                match &watch.state {
                    WatchState::Resolved(node) => {
                        out.push(Row::node(Tree::Watch, vec![watch_index], 0, node));
                        if node.expanded
                            && let Some(children) = &node.children
                        {
                            walk_nodes(Tree::Watch, &[watch_index], children, 1, &mut out);
                        }
                    }
                    WatchState::Pending => {
                        out.push(Row::watch_placeholder(watch_index, &watch.expression, "…"))
                    }
                    WatchState::Unavailable => out.push(Row::watch_placeholder(
                        watch_index,
                        &watch.expression,
                        "(unavailable)",
                    )),
                }
            }
        }

        walk_nodes(Tree::Scope, &[], &self.roots, 0, &mut out);

        if !self.transcript.is_empty() {
            out.push(Row::header("transcript"));
            for (index, entry) in self.transcript.iter().enumerate() {
                match entry {
                    TranscriptEntry::Evaluated { node } => {
                        out.push(Row::node(Tree::Result, vec![index], 0, node));
                        if node.expanded
                            && let Some(children) = &node.children
                        {
                            walk_nodes(Tree::Result, &[index], children, 1, &mut out);
                        }
                    }
                    TranscriptEntry::Error { message } => {
                        out.push(Row::message(format!("!! {message}"), Tone::Error));
                    }
                    TranscriptEntry::Unavailable { reason } => {
                        out.push(Row::message(format!("-- {reason}"), Tone::Note));
                    }
                    TranscriptEntry::Unrecognized { input } => {
                        out.push(Row::message(
                            format!("-- {input} is not a command; type an expression to evaluate"),
                            Tone::Note,
                        ));
                    }
                    TranscriptEntry::Help => {
                        for line in HELP_LINES {
                            out.push(Row::message((*line).to_string(), Tone::Note));
                        }
                    }
                }
            }
        }

        out
    }

    /// Rebuild the cached rows from the current state and re-clamp the cursor.
    /// Called after every state change, so the next render and the next action
    /// both read a current row list without flattening the tree again.
    pub fn rebuild_rows(&mut self) {
        self.rows = self.build_rows();
        self.clamp_row();
    }

    /// Move the cursor off a now-out-of-range or non-selectable row to the nearest
    /// selectable one.
    fn clamp_row(&mut self) {
        if self.rows.is_empty() {
            self.selected_row = 0;
            return;
        }
        let last = self.rows.len() - 1;
        let mut selected = self.selected_row.min(last);
        if !self.rows[selected].selectable() {
            selected = match self.rows[selected..].iter().position(Row::selectable) {
                Some(offset) => selected + offset,
                None => self.rows[..selected]
                    .iter()
                    .rposition(Row::selectable)
                    .unwrap_or(0),
            };
        }
        self.selected_row = selected;
    }
}

impl Default for DebuggerModel {
    fn default() -> Self {
        Self::new()
    }
}

/// The help text, shown both as a transcript entry and in the overlay.
pub const HELP_LINES: &[&str] = &[
    "i            type an expression to evaluate (Esc to leave)",
    "⏎ on input   evaluate (empty repeats the last expression)",
    "j / k        move the variable cursor",
    "h / l        collapse / expand the selected node",
    "g / G        first / last row",
    "K / J        select the calling / called frame",
    "w            pin or unpin the selected node as a watch",
    "c            continue          n  step over",
    "s            step into         o  step out",
    "p            pause a running program",
    "?            toggle this help      q  quit",
];

/// Descend a node by a path of child indices.
fn descend<'a>(mut node: &'a mut VarNode, path: &[usize]) -> Option<&'a mut VarNode> {
    for &i in path {
        node = node.children.as_mut()?.get_mut(i)?;
    }
    Some(node)
}

/// Flatten a node forest into visible rows, tagging each with its `(tree, path)`.
fn walk_nodes(tree: Tree, prefix: &[usize], nodes: &[VarNode], depth: usize, out: &mut Vec<Row>) {
    for (i, node) in nodes.iter().enumerate() {
        let mut path = prefix.to_vec();
        path.push(i);
        out.push(Row::node(tree, path.clone(), depth, node));
        if node.expanded
            && let Some(children) = &node.children
        {
            walk_nodes(tree, &path, children, depth + 1, out);
        }
    }
}

impl Row {
    fn node(tree: Tree, path: Vec<usize>, depth: usize, node: &VarNode) -> Row {
        Row {
            tree,
            kind: RowKind::Node,
            tone: Tone::Normal,
            path,
            depth,
            name: node.name.clone(),
            value: node.value.clone(),
            ty: node.ty.clone(),
            eval_name: node.eval_name.clone(),
            expandable: node.expandable(),
            expanded: node.expanded,
            fetched: node.children.is_some(),
            watched: tree == Tree::Watch && depth == 0,
        }
    }

    fn header(label: &str) -> Row {
        Row {
            tree: Tree::Scope,
            kind: RowKind::Header,
            tone: Tone::Note,
            path: Vec::new(),
            depth: 0,
            name: label.to_string(),
            value: String::new(),
            ty: String::new(),
            eval_name: String::new(),
            expandable: false,
            expanded: false,
            fetched: false,
            watched: false,
        }
    }

    fn message(text: String, tone: Tone) -> Row {
        Row {
            tree: Tree::Result,
            kind: RowKind::Message,
            tone,
            path: Vec::new(),
            depth: 0,
            name: text,
            value: String::new(),
            ty: String::new(),
            eval_name: String::new(),
            expandable: false,
            expanded: false,
            fetched: false,
            watched: false,
        }
    }

    fn watch_placeholder(watch_index: usize, expression: &str, status: &str) -> Row {
        Row {
            tree: Tree::Watch,
            kind: RowKind::Placeholder,
            tone: Tone::Normal,
            path: vec![watch_index],
            depth: 0,
            name: expression.to_string(),
            value: status.to_string(),
            ty: String::new(),
            eval_name: expression.to_string(),
            expandable: false,
            expanded: false,
            fetched: false,
            watched: true,
        }
    }
}
