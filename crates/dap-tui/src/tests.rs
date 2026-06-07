//! State-transition tests over the update step and in-memory snapshots of the
//! pane contents. None of these touch a terminal: interpretation is exercised by
//! applying actions, events, and effect results to a model and asserting on the
//! resulting state, and the panes are checked by snapshotting the rows the model
//! flattens to.

use serde_json::json;

use dap_client::dap::types::{EventMessage, StackFrame};
use dap_client::model::{SessionState, VarNode};

use crate::model::{DebuggerModel, Row, RowKind, TranscriptEntry, Watch, WatchState};
use crate::update::{Action, Effect, Update, UpdateKind, apply_action, apply_event, apply_update};

fn frame(id: i64, name: &str, line: i64) -> StackFrame {
    StackFrame {
        id,
        name: name.to_string(),
        line,
        source: None,
    }
}

fn var(name: &str, value: &str, var_ref: i64) -> VarNode {
    VarNode {
        name: name.to_string(),
        value: value.to_string(),
        ty: String::new(),
        eval_name: name.to_string(),
        var_ref,
        children: None,
        expanded: false,
    }
}

fn stopped_event(reason: &str, thread_id: i64) -> EventMessage {
    EventMessage {
        event: "stopped".to_string(),
        body: Some(json!({ "reason": reason, "threadId": thread_id })),
    }
}

fn event(name: &str) -> EventMessage {
    EventMessage {
        event: name.to_string(),
        body: None,
    }
}

/// A model stopped at a two-frame stack with the inner frame selected, as if the
/// stop resolved cleanly. Frame 10 is the inner frame, 11 its caller.
fn stopped_model() -> DebuggerModel {
    let mut model = DebuggerModel::new();
    apply_event(&mut model, stopped_event("step", 1));
    let epoch = model.epoch;
    apply_update(
        &mut model,
        Update {
            epoch,
            kind: UpdateKind::Stopped {
                thread_id: Some(1),
                stack: vec![frame(10, "inner", 5), frame(11, "outer", 9)],
                frame_id: Some(10),
                roots: vec![var("Locals", "", 100)],
            },
        },
    );
    model
}

#[test]
fn stopped_event_marks_stopped_and_requests_resolution() {
    let mut model = DebuggerModel::new();
    let effects = apply_event(&mut model, stopped_event("breakpoint", 1));

    assert_eq!(model.state, SessionState::Stopped);
    assert_eq!(model.stop_count, 1);
    assert_eq!(model.epoch, 1);
    assert_eq!(model.stop_reason, "breakpoint");
    match effects.as_slice() {
        [Effect::ResolveStop { thread_hint, epoch }] => {
            assert_eq!(*thread_hint, Some(1));
            assert_eq!(*epoch, 1);
        }
        _ => panic!("expected a single ResolveStop effect"),
    }
}

#[test]
fn resolution_selects_the_top_frame() {
    let model = stopped_model();
    assert_eq!(model.selected_frame, 0);
    assert_eq!(model.frame_id, Some(10));
    assert_eq!(model.selected_stack_frame().unwrap().id, 10);
    assert_eq!(model.roots.len(), 1);
}

#[test]
fn a_stop_resets_a_deep_selection_to_the_top_frame() {
    let mut model = stopped_model();
    model.select_caller_frame();
    assert_eq!(model.selected_frame, 1);

    apply_event(&mut model, stopped_event("step", 1));
    let epoch = model.epoch;
    apply_update(
        &mut model,
        Update {
            epoch,
            kind: UpdateKind::Stopped {
                thread_id: Some(1),
                stack: vec![frame(20, "a", 1), frame(21, "b", 2), frame(22, "c", 3)],
                frame_id: Some(20),
                roots: vec![],
            },
        },
    );
    assert_eq!(model.selected_frame, 0);
    assert_eq!(model.frame_id, Some(20));
}

#[test]
fn a_superseded_resolution_is_discarded() {
    let mut model = DebuggerModel::new();
    apply_event(&mut model, stopped_event("step", 1));
    // A second stop supersedes the first before its resolution lands.
    apply_event(&mut model, stopped_event("step", 1));

    let effects = apply_update(
        &mut model,
        Update {
            epoch: 1,
            kind: UpdateKind::Stopped {
                thread_id: Some(1),
                stack: vec![frame(10, "stale", 1)],
                frame_id: Some(10),
                roots: vec![],
            },
        },
    );
    assert!(effects.is_empty());
    assert_eq!(model.frame_id, None);
    assert!(model.stack.is_empty());
}

#[test]
fn evaluate_targets_the_selected_frame() {
    let mut model = stopped_model();
    model.input = "x + 1".to_string();

    let effects = apply_action(&mut model, Action::InputSubmit);
    match effects.as_slice() {
        [
            Effect::Evaluate {
                expression,
                frame_id,
                ..
            },
        ] => {
            assert_eq!(expression, "x + 1");
            assert_eq!(*frame_id, 10);
        }
        _ => panic!("expected an Evaluate effect targeting the selected frame"),
    }
    assert_eq!(model.last_expression.as_deref(), Some("x + 1"));
    assert!(model.input.is_empty());
}

#[test]
fn evaluating_against_the_caller_frame_after_navigating() {
    let mut model = stopped_model();
    apply_action(&mut model, Action::SelectCallerFrame);
    model.input = "y".to_string();

    let effects = apply_action(&mut model, Action::InputSubmit);
    match effects.as_slice() {
        [Effect::Evaluate { frame_id, .. }] => assert_eq!(*frame_id, 11),
        _ => panic!("expected evaluation against the caller frame"),
    }
}

#[test]
fn evaluation_is_unavailable_with_no_frame() {
    let mut model = DebuggerModel::new();
    apply_event(&mut model, event("continued"));
    model.input = "x".to_string();

    let effects = apply_action(&mut model, Action::InputSubmit);
    assert!(effects.is_empty());
    assert!(matches!(
        model.transcript.last(),
        Some(TranscriptEntry::Unavailable { .. })
    ));
}

#[test]
fn an_evaluation_result_is_recorded_as_an_inspectable_node() {
    let mut model = stopped_model();
    let epoch = model.epoch;
    apply_update(
        &mut model,
        Update {
            epoch,
            kind: UpdateKind::Evaluated {
                result: Ok(var("x", "42", 7)),
            },
        },
    );
    match model.transcript.last() {
        Some(TranscriptEntry::Evaluated { node }) => {
            assert_eq!(node.value, "42");
            assert!(node.expandable());
        }
        _ => panic!("expected an evaluated transcript node"),
    }
}

#[test]
fn a_failed_evaluation_is_recorded_as_an_error() {
    let mut model = stopped_model();
    let epoch = model.epoch;
    apply_update(
        &mut model,
        Update {
            epoch,
            kind: UpdateKind::Evaluated {
                result: Err("name 'x' is not defined".to_string()),
            },
        },
    );
    assert!(matches!(
        model.transcript.last(),
        Some(TranscriptEntry::Error { .. })
    ));
}

#[test]
fn an_evaluation_result_survives_a_later_stop() {
    let mut model = stopped_model();
    let issued = model.epoch;
    // A stop arrives before the slow evaluation returns. The transcript is
    // history, so the result still lands rather than being dropped as stale.
    apply_event(&mut model, stopped_event("step", 1));
    apply_update(
        &mut model,
        Update {
            epoch: issued,
            kind: UpdateKind::Evaluated {
                result: Ok(var("x", "42", 0)),
            },
        },
    );
    assert!(matches!(
        model.transcript.last(),
        Some(TranscriptEntry::Evaluated { .. })
    ));
}

#[test]
fn continue_is_issued_while_stopped() {
    let mut model = stopped_model();
    let effects = apply_action(&mut model, Action::Continue);
    match effects.as_slice() {
        [
            Effect::Drive {
                command, thread_id, ..
            },
        ] => {
            assert_eq!(*command, "continue");
            assert_eq!(*thread_id, 1);
        }
        _ => panic!("expected a Drive effect"),
    }
}

#[test]
fn a_step_while_running_is_refused() {
    let mut model = DebuggerModel::new();
    apply_event(&mut model, event("continued"));
    let effects = apply_action(&mut model, Action::StepOver);
    assert!(effects.is_empty());
    assert!(model.status.contains("unless stopped"));
}

#[test]
fn pause_requires_a_running_program() {
    let mut model = stopped_model();
    let effects = apply_action(&mut model, Action::Pause);
    assert!(effects.is_empty());
    assert!(model.status.contains("unless running"));

    apply_event(&mut model, event("continued"));
    let effects = apply_action(&mut model, Action::Pause);
    match effects.as_slice() {
        [Effect::Drive { command, .. }] => assert_eq!(*command, "pause"),
        _ => panic!("expected a pause Drive effect"),
    }
}

#[test]
fn navigating_toward_the_caller_moves_the_shared_selection() {
    let mut model = stopped_model();
    let before = model.epoch;

    let effects = apply_action(&mut model, Action::SelectCallerFrame);
    assert_eq!(model.selected_frame, 1);
    assert_eq!(model.frame_id, Some(11));
    assert!(model.epoch > before);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchScopes { frame_id: 11, .. }))
    );
}

#[test]
fn navigating_past_the_outermost_frame_does_not_move() {
    let mut model = stopped_model();
    apply_action(&mut model, Action::SelectCallerFrame);
    let effects = apply_action(&mut model, Action::SelectCallerFrame);
    assert_eq!(model.selected_frame, 1);
    assert!(effects.is_empty());
    assert!(model.status.contains("outermost"));
}

#[test]
fn navigating_with_no_frame_says_not_stopped() {
    let mut model = DebuggerModel::new();
    apply_event(&mut model, event("continued"));
    let effects = apply_action(&mut model, Action::SelectCallerFrame);
    assert!(effects.is_empty());
    assert!(model.status.contains("not stopped"));
}

#[test]
fn continued_marks_running_and_drops_the_frame() {
    let mut model = stopped_model();
    apply_event(&mut model, event("continued"));
    assert_eq!(model.state, SessionState::Running);
    assert_eq!(model.frame_id, None);
    assert!(model.stack.is_empty());
}

#[test]
fn ending_the_session_clears_run_control_targets() {
    let mut model = stopped_model();
    apply_event(&mut model, event("terminated"));
    assert_eq!(model.state, SessionState::Ended);
    assert_eq!(model.thread_id, None);
}

#[test]
fn variables_pane_snapshot() {
    let mut model = stopped_model();
    let mut locals = var("Locals", "", 100);
    locals.expanded = true;
    locals.children = Some(vec![var("count", "3", 0), var("items", "[..]", 200)]);
    model.roots = vec![locals];

    assert_eq!(
        snapshot_rows(&model),
        vec!["[0]-v Locals", "[1]-. count=3", "[1]-> items=[..]",]
    );
}

#[test]
fn watches_scopes_and_transcript_share_one_pane() {
    let mut model = stopped_model();
    let mut locals = var("Locals", "", 100);
    locals.expanded = true;
    locals.children = Some(vec![var("n", "7", 0)]);
    model.roots = vec![locals];
    model.watches = vec![
        Watch {
            expression: "n".to_string(),
            state: WatchState::Resolved(var("n", "7", 0)),
        },
        Watch {
            expression: "missing".to_string(),
            state: WatchState::Unavailable,
        },
    ];
    model.transcript = vec![
        TranscriptEntry::Evaluated {
            node: var("n+1", "8", 0),
        },
        TranscriptEntry::Error {
            message: "nope".to_string(),
        },
    ];

    assert_eq!(
        snapshot_rows(&model),
        vec![
            "# watched",
            "[0]*. n=7",
            "[0] missing (unavailable)",
            "[0]-v Locals",
            "[1]-. n=7",
            "# transcript",
            "[0]-. n+1=8",
            "!! nope",
        ]
    );
}

#[test]
fn stack_pane_marks_the_selected_frame() {
    let mut model = stopped_model();
    assert_eq!(
        snapshot_stack(&model),
        vec![">#0 inner @ 5", " #1 outer @ 9"]
    );

    apply_action(&mut model, Action::SelectCallerFrame);
    assert_eq!(
        snapshot_stack(&model),
        vec![" #0 inner @ 5", ">#1 outer @ 9"]
    );
}

/// Render the variables-pane rows to a compact, terminal-free form: depth in
/// brackets, a watch marker, an expansion marker, then the node text.
fn snapshot_rows(model: &DebuggerModel) -> Vec<String> {
    model.build_rows().iter().map(snapshot_row).collect()
}

fn snapshot_row(row: &Row) -> String {
    match row.kind {
        RowKind::Header => format!("# {}", row.name),
        RowKind::Message => row.name.clone(),
        RowKind::Placeholder => format!("[{}] {} ({})", row.depth, row.name, strip(&row.value)),
        RowKind::Node => {
            let watch = if row.watched { '*' } else { '-' };
            let expand = if !row.expandable {
                '.'
            } else if row.expanded {
                'v'
            } else {
                '>'
            };
            let mut text = format!("[{}]{}{} {}", row.depth, watch, expand, row.name);
            if !row.ty.is_empty() {
                text.push_str(&format!(":{}", row.ty));
            }
            if !row.value.is_empty() {
                text.push_str(&format!("={}", row.value));
            }
            text
        }
    }
}

fn strip(value: &str) -> String {
    value.trim_matches(['(', ')']).to_string()
}

fn snapshot_stack(model: &DebuggerModel) -> Vec<String> {
    model
        .stack
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            let marker = if index == model.selected_frame {
                '>'
            } else {
                ' '
            };
            format!("{marker}#{index} {} @ {}", frame.name, frame.line)
        })
        .collect()
}
