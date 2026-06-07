//! Interpretation: input actions and DAP events mutate the model and produce no
//! output. Each interpretation returns the side-effecting DAP work it wants done
//! as a list of effects. The event loop runs those effects and feeds their
//! results back as updates, which are interpreted the same way. Keeping the
//! decision pure and the I/O in effects is what lets the update step be exercised
//! without a terminal or a live connection.

use dap_client::dap::types::{EventMessage, StackFrame, StoppedBody};
use dap_client::model::{SessionState, VarNode};

use crate::model::{DebuggerModel, InputMode, Row, TranscriptEntry, Tree, Watch, WatchState};

/// A semantic action, derived from a keypress by the view but carrying no
/// terminal-UI types of its own.
pub enum Action {
    Quit,
    ToggleHelp,
    EnterInsert,
    InputChar(char),
    InputBackspace,
    InputSubmit,
    InputCancel,
    RowDown,
    RowUp,
    RowFirst,
    RowLast,
    Expand,
    Collapse,
    ToggleExpand,
    ToggleWatch,
    SelectCallerFrame,
    SelectCalleeFrame,
    Continue,
    StepOver,
    StepInto,
    StepOut,
    Pause,
}

/// DAP work the event loop should perform off the model. Each effect carries the
/// epoch it was issued under, so a reply for a superseded frame view is dropped.
pub enum Effect {
    ResolveStop {
        thread_hint: Option<i64>,
        epoch: u64,
    },
    FetchScopes {
        frame_id: i64,
        epoch: u64,
    },
    FetchChildren {
        target: FetchTarget,
        var_ref: i64,
        epoch: u64,
    },
    Evaluate {
        expression: String,
        frame_id: i64,
        epoch: u64,
    },
    EvalWatch {
        expression: String,
        frame_id: i64,
        epoch: u64,
    },
    Drive {
        command: &'static str,
        thread_id: i64,
        epoch: u64,
    },
}

/// Which forest a fetched child belongs to, located stably so the reply lands on
/// the right node even if the watch list changed while the fetch was in flight.
pub enum FetchTarget {
    Scope {
        path: Vec<usize>,
    },
    Watch {
        expression: String,
        subpath: Vec<usize>,
    },
    Result {
        path: Vec<usize>,
    },
}

/// The result of an effect, tagged with the epoch it was issued under.
pub struct Update {
    pub epoch: u64,
    pub kind: UpdateKind,
}

pub enum UpdateKind {
    Stopped {
        thread_id: Option<i64>,
        stack: Vec<StackFrame>,
        frame_id: Option<i64>,
        roots: Vec<VarNode>,
    },
    Scopes {
        roots: Vec<VarNode>,
    },
    Children {
        target: FetchTarget,
        children: Vec<VarNode>,
    },
    Evaluated {
        result: Result<VarNode, String>,
    },
    Watch {
        expression: String,
        node: Option<VarNode>,
    },
    /// A run-control request the adapter rejected.
    DriveFailed {
        message: String,
    },
    /// A resolve or scope fetch failed at the transport level.
    Failed {
        message: String,
    },
}

enum ExpandWant {
    Expand,
    Collapse,
    Toggle,
}

/// Interpret one input action against the model. The cached rows are refreshed
/// once at the end, so the render that follows reuses this build.
pub fn apply_action(model: &mut DebuggerModel, action: Action) -> Vec<Effect> {
    let effects = match action {
        Action::Quit => {
            model.should_quit = true;
            vec![]
        }
        Action::ToggleHelp => {
            model.show_help = !model.show_help;
            vec![]
        }
        Action::EnterInsert => {
            model.mode = InputMode::Insert;
            vec![]
        }
        Action::InputChar(c) => {
            model.input.push(c);
            vec![]
        }
        Action::InputBackspace => {
            model.input.pop();
            vec![]
        }
        Action::InputCancel => {
            model.input.clear();
            model.mode = InputMode::Normal;
            vec![]
        }
        Action::InputSubmit => submit_input(model),
        Action::RowDown => {
            step_row(model, true);
            vec![]
        }
        Action::RowUp => {
            step_row(model, false);
            vec![]
        }
        Action::RowFirst => {
            if let Some(i) = model.rows.iter().position(Row::selectable) {
                model.selected_row = i;
            }
            vec![]
        }
        Action::RowLast => {
            if let Some(i) = model.rows.iter().rposition(Row::selectable) {
                model.selected_row = i;
            }
            vec![]
        }
        Action::Expand => set_expanded(model, ExpandWant::Expand),
        Action::Collapse => set_expanded(model, ExpandWant::Collapse),
        Action::ToggleExpand => set_expanded(model, ExpandWant::Toggle),
        Action::ToggleWatch => toggle_watch(model),
        Action::SelectCallerFrame => select_frame(model, true),
        Action::SelectCalleeFrame => select_frame(model, false),
        Action::Continue => drive_action(model, "continue"),
        Action::StepOver => drive_action(model, "next"),
        Action::StepInto => drive_action(model, "stepIn"),
        Action::StepOut => drive_action(model, "stepOut"),
        Action::Pause => pause_action(model),
    };
    model.rebuild_rows();
    effects
}

/// Interpret one DAP event, updating session and frame state. The cached rows are
/// refreshed once at the end, so the render that follows reuses this build.
pub fn apply_event(model: &mut DebuggerModel, msg: EventMessage) -> Vec<Effect> {
    let effects = match msg.event.as_str() {
        "stopped" => {
            let body: StoppedBody = msg
                .body
                .and_then(|b| serde_json::from_value(b).ok())
                .unwrap_or_default();
            // Re-root: new generation, drop the old frame, rebuild from this stop.
            model.epoch += 1;
            model.stop_count += 1;
            model.state = SessionState::Stopped;
            model.stack.clear();
            model.roots.clear();
            model.frame_id = None;
            model.selected_frame = 0;
            model.selected_row = 0;
            model.stop_reason = body.reason.clone();
            // Watches survive re-rooting, but their last values are stale until
            // the new frame resolves and they are re-evaluated against it.
            for watch in &mut model.watches {
                watch.state = WatchState::Pending;
            }
            model.status = "stopped — resolving frame…".to_string();
            vec![Effect::ResolveStop {
                thread_hint: body.thread_id,
                epoch: model.epoch,
            }]
        }
        "continued" => {
            // The last stop's frame references are now invalid, so any resolve or
            // fetch still in flight from that stop must be discarded. Bumping the
            // generation does that. The old tree stays on screen, drawn stale,
            // until the next stop re-roots it.
            model.epoch += 1;
            model.state = SessionState::Running;
            // A thread keeps its identity across a resume, so pausing still has a
            // target. Frame references do not, so the stack is dropped and
            // evaluation is refused until the next stop.
            model.stack.clear();
            model.selected_frame = 0;
            model.frame_id = None;
            model.status = "running — variables stale".to_string();
            vec![]
        }
        "terminated" | "exited" => {
            model.state = SessionState::Ended;
            model.thread_id = None;
            model.stack.clear();
            model.frame_id = None;
            model.status = "session ended".to_string();
            vec![]
        }
        _ => vec![],
    };
    model.rebuild_rows();
    effects
}

/// Apply the result of an effect, discarding anything tagged with a superseded
/// generation of the frame view. A discarded reply changes nothing, so only the
/// paths that mutate fall through to rebuild the cached rows.
pub fn apply_update(model: &mut DebuggerModel, update: Update) -> Vec<Effect> {
    let effects = match update.kind {
        UpdateKind::Stopped {
            thread_id,
            stack,
            frame_id,
            roots,
        } => {
            if update.epoch != model.epoch {
                return vec![];
            }
            model.thread_id = thread_id;
            model.stack = stack;
            model.selected_frame = 0;
            model.frame_id = frame_id;
            model.roots = roots;
            match frame_id {
                Some(frame_id) => {
                    model.status = "stopped".to_string();
                    for watch in &mut model.watches {
                        watch.state = WatchState::Pending;
                    }
                    watch_effects(model, frame_id)
                }
                None => {
                    model.status = "stopped — no frames (idle)".to_string();
                    // No frame to evaluate against, so watches can never resolve
                    // this stop. Flip them to unavailable rather than spinning.
                    for watch in &mut model.watches {
                        watch.state = WatchState::Unavailable;
                    }
                    vec![]
                }
            }
        }
        UpdateKind::Scopes { roots } => {
            if update.epoch != model.epoch {
                return vec![];
            }
            model.roots = roots;
            model.status = "stopped".to_string();
            vec![]
        }
        UpdateKind::Children { target, children } => {
            if update.epoch != model.epoch {
                return vec![];
            }
            let node = match &target {
                FetchTarget::Scope { path } => model.scope_node_mut(path),
                FetchTarget::Watch {
                    expression,
                    subpath,
                } => model.watch_node_mut_by_expression(expression, subpath),
                FetchTarget::Result { path } => model.result_node_mut(path),
            };
            if let Some(node) = node {
                node.children = Some(children);
            }
            vec![]
        }
        UpdateKind::Watch { expression, node } => {
            if update.epoch != model.epoch {
                return vec![];
            }
            if let Some(watch) = model
                .watches
                .iter_mut()
                .find(|w| w.expression == expression)
            {
                watch.state = match node {
                    Some(node) => WatchState::Resolved(node),
                    None => WatchState::Unavailable,
                };
            }
            vec![]
        }
        // An evaluation result is history, not part of the live frame view, so it
        // is recorded regardless of generation.
        UpdateKind::Evaluated { result } => {
            match result {
                Ok(node) => model.push_transcript(TranscriptEntry::Evaluated { node }),
                Err(message) => model.push_transcript(TranscriptEntry::Error { message }),
            }
            vec![]
        }
        UpdateKind::DriveFailed { message } => {
            model.push_transcript(TranscriptEntry::Error { message });
            vec![]
        }
        UpdateKind::Failed { message } => {
            model.status = format!("error: {message}");
            vec![]
        }
    };
    model.rebuild_rows();
    effects
}

fn submit_input(model: &mut DebuggerModel) -> Vec<Effect> {
    let raw = model.input.trim().to_string();
    model.input.clear();

    // An empty line repeats the last expression, so pressing enter re-runs it.
    let line = if raw.is_empty() {
        match &model.last_expression {
            Some(previous) => previous.clone(),
            None => return vec![],
        }
    } else {
        raw
    };

    let command = line.strip_prefix(':').unwrap_or(&line);
    if command == "help" || command == "?" {
        model.push_transcript(TranscriptEntry::Help);
        return vec![];
    }
    if line.starts_with(':') {
        model.push_transcript(TranscriptEntry::Unrecognized { input: line });
        return vec![];
    }

    model.last_expression = Some(line.clone());
    match model.frame_id {
        Some(frame_id) => vec![Effect::Evaluate {
            expression: line,
            frame_id,
            epoch: model.epoch,
        }],
        None => {
            model.push_transcript(TranscriptEntry::Unavailable {
                reason: evaluation_unavailable_reason(model.state),
            });
            vec![]
        }
    }
}

/// Move the shared selection through the stack. With no stack at all the program
/// is not stopped at a frame, which is a different thing from sitting at an edge
/// of the stack and reads as such.
fn select_frame(model: &mut DebuggerModel, toward_caller: bool) -> Vec<Effect> {
    if model.stack.is_empty() {
        model.status = "not stopped at a frame".to_string();
        return vec![];
    }
    let moved = if toward_caller {
        model.select_caller_frame()
    } else {
        model.select_callee_frame()
    };
    if moved {
        return on_frame_changed(model);
    }
    model.status = if toward_caller {
        "already at the outermost frame"
    } else {
        "already at the innermost frame"
    }
    .to_string();
    vec![]
}

/// Reset the frame view to the newly selected frame and fetch its scopes. Bumping
/// the generation discards any fetch still in flight for the previous frame.
fn on_frame_changed(model: &mut DebuggerModel) -> Vec<Effect> {
    model.epoch += 1;
    model.selected_row = 0;
    model.frame_id = model.selected_stack_frame().map(|f| f.id);
    model.roots.clear();
    for watch in &mut model.watches {
        watch.state = WatchState::Pending;
    }
    model.status = format!("frame #{}", model.selected_frame);

    let Some(frame_id) = model.frame_id else {
        return vec![];
    };
    let mut effects = vec![Effect::FetchScopes {
        frame_id,
        epoch: model.epoch,
    }];
    effects.extend(watch_effects(model, frame_id));
    effects
}

/// Re-evaluate every watch against the given frame.
fn watch_effects(model: &DebuggerModel, frame_id: i64) -> Vec<Effect> {
    model
        .watches
        .iter()
        .map(|watch| Effect::EvalWatch {
            expression: watch.expression.clone(),
            frame_id,
            epoch: model.epoch,
        })
        .collect()
}

fn drive_action(model: &mut DebuggerModel, command: &'static str) -> Vec<Effect> {
    if model.state != SessionState::Stopped {
        model.status = format!("can't {command} unless stopped");
        return vec![];
    }
    let Some(thread_id) = model.thread_id else {
        model.status = format!("can't {command}: no thread");
        return vec![];
    };
    vec![Effect::Drive {
        command,
        thread_id,
        epoch: model.epoch,
    }]
}

fn pause_action(model: &mut DebuggerModel) -> Vec<Effect> {
    if model.state != SessionState::Running {
        model.status = "can't pause unless running".to_string();
        return vec![];
    }
    let Some(thread_id) = model.thread_id else {
        model.status = "can't pause: no thread".to_string();
        return vec![];
    };
    vec![Effect::Drive {
        command: "pause",
        thread_id,
        epoch: model.epoch,
    }]
}

fn step_row(model: &mut DebuggerModel, forward: bool) {
    let count = model.rows.len() as isize;
    let mut i = model.selected_row as isize;
    loop {
        i += if forward { 1 } else { -1 };
        if i < 0 || i >= count {
            return;
        }
        if model.rows[i as usize].selectable() {
            model.selected_row = i as usize;
            return;
        }
    }
}

/// Expand or collapse the selected node, triggering a lazy child fetch on first
/// expand. Non-expandable rows issue no request.
fn set_expanded(model: &mut DebuggerModel, want: ExpandWant) -> Vec<Effect> {
    let (tree, path) = {
        let Some(row) = model.rows.get(model.selected_row) else {
            return vec![];
        };
        if !row.expandable {
            return vec![];
        }
        (row.tree, row.path.clone())
    };

    let node = match tree {
        Tree::Scope => model.scope_node_mut(&path),
        Tree::Watch => model.watch_node_mut(&path),
        Tree::Result => model.result_node_mut(&path),
    };
    let mut fetch_ref = None;
    if let Some(node) = node {
        let expanded = match want {
            ExpandWant::Expand => true,
            ExpandWant::Collapse => false,
            ExpandWant::Toggle => !node.expanded,
        };
        node.expanded = expanded;
        if expanded && node.children.is_none() {
            fetch_ref = Some(node.var_ref);
        }
    }

    let Some(var_ref) = fetch_ref else {
        return vec![];
    };
    let target = match tree {
        Tree::Scope => FetchTarget::Scope { path },
        Tree::Watch => FetchTarget::Watch {
            expression: model.watches[path[0]].expression.clone(),
            subpath: path[1..].to_vec(),
        },
        Tree::Result => FetchTarget::Result { path },
    };
    vec![Effect::FetchChildren {
        target,
        var_ref,
        epoch: model.epoch,
    }]
}

/// Pin or unpin the selected node as a watch. On a watch root, unpin it. On any
/// other node, a scope variable, an evaluation result, or a node inside a watch,
/// pin or unpin it by its own evaluate name, so pressing the key deep in a
/// subtree adds that descendant rather than removing the whole root.
fn toggle_watch(model: &mut DebuggerModel) -> Vec<Effect> {
    let (is_watch_root, root_index, expression) = {
        let Some(row) = model.rows.get(model.selected_row) else {
            return vec![];
        };
        (
            row.tree == Tree::Watch && row.path.len() == 1,
            row.path.first().copied(),
            row.eval_name.clone(),
        )
    };

    if is_watch_root {
        if let Some(index) = root_index
            && index < model.watches.len()
        {
            model.watches.remove(index);
        }
        return vec![];
    }
    if expression.is_empty() {
        return vec![];
    }
    if let Some(position) = model
        .watches
        .iter()
        .position(|w| w.expression == expression)
    {
        model.watches.remove(position);
        return vec![];
    }
    model.watches.push(Watch {
        expression: expression.clone(),
        state: WatchState::Pending,
    });
    // Pinned while stopped: evaluate immediately against the live frame.
    if model.state == SessionState::Stopped
        && let Some(frame_id) = model.frame_id
    {
        return vec![Effect::EvalWatch {
            expression,
            frame_id,
            epoch: model.epoch,
        }];
    }
    vec![]
}

fn evaluation_unavailable_reason(state: SessionState) -> String {
    let why = match state {
        SessionState::Running => "the program is running",
        SessionState::Ended => "the session has ended",
        SessionState::Stopped => "no frame is resolved at this stop",
        SessionState::Connecting => "not stopped yet",
    };
    format!("nothing to evaluate against ({why})")
}
