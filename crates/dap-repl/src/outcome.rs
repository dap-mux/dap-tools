//! Turns each input line and each DAP event into an Outcome. This layer runs the
//! engine calls and updates session state but writes nothing. Output is a
//! separate rendering step.

use dap_client::dap::DapClient;
use dap_client::dap::types::{EventMessage, StackFrame, StoppedBody};
use dap_client::model::SessionState;

use crate::repl::{self, Session};

/// A stack frame reduced to what the front-end shows about it.
pub struct FrameView {
    pub name: String,
    pub line: i64,
}

impl FrameView {
    fn of(frame: &StackFrame) -> Self {
        Self {
            name: frame.name.clone(),
            line: frame.line,
        }
    }
}

/// What deciding one input line or one DAP event produced. Variants carry the
/// values an outcome needs, not formatted text.
pub enum Outcome {
    /// A value the adapter returned, with its type when it gave one.
    Evaluated {
        value: String,
        ty: Option<String>,
    },
    /// No frame to evaluate against.
    EvaluationUnavailable {
        reason: String,
    },
    /// A run-control request was accepted. Its stop or resume arrives later as an
    /// event, so there is nothing to show for the request itself.
    DriveIssued,
    /// A run-control request could not be made in the current state.
    DriveUnavailable {
        reason: String,
    },
    /// A frame became the selected one for navigation and evaluation.
    FrameSelected {
        index: usize,
        frame: FrameView,
    },
    /// A frame move or selection could not be made in the current state.
    NavigationBlocked {
        reason: String,
    },
    /// The call stack, with the index of the selected frame.
    Stack {
        frames: Vec<FrameView>,
        selected: usize,
    },
    Help,
    Unrecognized {
        command: String,
    },
    /// An adapter request failed.
    Failed {
        error: String,
    },
    /// The program stopped. The frame is absent when none resolved at the stop.
    Stopped {
        reason: String,
        frame: Option<FrameView>,
    },
    Resumed,
    Ended,
    Quit,
    /// Nothing to show.
    Noop,
}

/// Decide what one line of input means, a colon command or an expression.
///
/// An empty line repeats the last input, so pressing Enter keeps stepping or
/// re-runs the last expression. A non-empty line becomes the new repeat target.
pub async fn decide_input(
    client: &DapClient,
    session: &mut Session,
    last_input: &mut Option<String>,
    input: &str,
) -> Outcome {
    let line = if input.is_empty() {
        match last_input {
            Some(prev) => prev.clone(),
            None => return Outcome::Noop,
        }
    } else {
        *last_input = Some(input.to_string());
        input.to_string()
    };

    match line.strip_prefix(':') {
        Some(command) => run_command(client, session, command.trim()).await,
        None => evaluate_line(client, session, &line).await,
    }
}

async fn run_command(client: &DapClient, session: &mut Session, cmd: &str) -> Outcome {
    let mut parts = cmd.split_whitespace();
    let Some(verb) = parts.next() else {
        return Outcome::Noop;
    };
    match verb {
        "continue" | "c" => drive(client, session, "continue").await,
        "next" | "n" => drive(client, session, "next").await,
        "step" | "s" => drive(client, session, "stepIn").await,
        "finish" | "o" => drive(client, session, "stepOut").await,
        "pause" => pause(client, session).await,
        "up" => navigate(session, true),
        "down" => navigate(session, false),
        "frame" => match parts.next().and_then(|n| n.parse::<usize>().ok()) {
            Some(index) => select_frame(session, index),
            None => Outcome::NavigationBlocked {
                reason: "usage: :frame <index>".to_string(),
            },
        },
        "bt" | "where" => stack(session),
        "help" => Outcome::Help,
        "quit" | "q" => Outcome::Quit,
        other => Outcome::Unrecognized {
            command: other.to_string(),
        },
    }
}

/// Issue a step or continue against the stopped thread. A successful request
/// shows nothing now. Its stop or resume arrives later as an event.
async fn drive(client: &DapClient, session: &Session, command: &str) -> Outcome {
    if session.state != SessionState::Stopped {
        return Outcome::DriveUnavailable {
            reason: format!("can't {command} unless stopped"),
        };
    }
    let Some(thread_id) = session.thread_id() else {
        return Outcome::DriveUnavailable {
            reason: format!("can't {command}: no thread"),
        };
    };
    match repl::drive(client, command, thread_id).await {
        Ok(()) => Outcome::DriveIssued,
        Err(e) => Outcome::Failed {
            error: e.to_string(),
        },
    }
}

async fn pause(client: &DapClient, session: &Session) -> Outcome {
    if session.state != SessionState::Running {
        return Outcome::DriveUnavailable {
            reason: "can't pause unless running".to_string(),
        };
    }
    let Some(thread_id) = session.thread_id() else {
        return Outcome::DriveUnavailable {
            reason: "can't pause: no thread".to_string(),
        };
    };
    match repl::drive(client, "pause", thread_id).await {
        Ok(()) => Outcome::DriveIssued,
        Err(e) => Outcome::Failed {
            error: e.to_string(),
        },
    }
}

fn navigate(session: &mut Session, up: bool) -> Outcome {
    if session.current_frame().is_none() {
        return Outcome::NavigationBlocked {
            reason: "not stopped at a frame".to_string(),
        };
    }
    let moved = if up {
        session.select_up()
    } else {
        session.select_down()
    };
    if moved {
        selected_frame(session)
    } else if up {
        Outcome::NavigationBlocked {
            reason: "already at the outermost frame".to_string(),
        }
    } else {
        Outcome::NavigationBlocked {
            reason: "already at the innermost frame".to_string(),
        }
    }
}

fn select_frame(session: &mut Session, index: usize) -> Outcome {
    if session.current_frame().is_none() {
        Outcome::NavigationBlocked {
            reason: "not stopped at a frame".to_string(),
        }
    } else if session.select_index(index) {
        selected_frame(session)
    } else {
        Outcome::NavigationBlocked {
            reason: format!("no frame {index}, the stack has {}", session.stack().len()),
        }
    }
}

fn selected_frame(session: &Session) -> Outcome {
    match session.current_frame() {
        Some(frame) => Outcome::FrameSelected {
            index: session.selected(),
            frame: FrameView::of(frame),
        },
        None => Outcome::Noop,
    }
}

fn stack(session: &Session) -> Outcome {
    if session.stack().is_empty() {
        Outcome::NavigationBlocked {
            reason: "no stack, not stopped at a frame".to_string(),
        }
    } else {
        Outcome::Stack {
            frames: session.stack().iter().map(FrameView::of).collect(),
            selected: session.selected(),
        }
    }
}

/// Evaluate one line against the selected frame, or say why it can't.
async fn evaluate_line(client: &DapClient, session: &Session, expr: &str) -> Outcome {
    let Some(frame_id) = session.frame_id() else {
        let why = match session.state {
            SessionState::Running => "the program is running",
            SessionState::Ended => "the session has ended",
            SessionState::Stopped => "no frame is resolved at this stop",
            SessionState::Connecting => "not stopped yet",
        };
        return Outcome::EvaluationUnavailable {
            reason: format!("nothing to evaluate against ({why})"),
        };
    };
    match repl::evaluate(client, expr, frame_id).await {
        Ok(ev) => Outcome::Evaluated {
            value: ev.result,
            ty: ev.ty,
        },
        Err(e) => Outcome::Failed {
            error: e.to_string(),
        },
    }
}

/// Decide what one DAP event means, updating session state as it requires.
pub async fn decide_event(client: &DapClient, session: &mut Session, msg: EventMessage) -> Outcome {
    match msg.event.as_str() {
        "stopped" => {
            let body: StoppedBody = msg
                .body
                .and_then(|b| serde_json::from_value(b).ok())
                .unwrap_or_default();
            match session.on_stopped(client, body.thread_id).await {
                Ok(()) => Outcome::Stopped {
                    reason: body.reason,
                    frame: session.current_frame().map(FrameView::of),
                },
                Err(e) => Outcome::Failed {
                    error: e.to_string(),
                },
            }
        }
        "continued" => {
            session.on_continued();
            Outcome::Resumed
        }
        "terminated" | "exited" => {
            session.on_ended();
            Outcome::Ended
        }
        _ => Outcome::Noop,
    }
}
