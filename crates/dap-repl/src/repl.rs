//! The REPL engine. It tracks session and frame state, evaluates expressions,
//! and drives execution. Every function returns data instead of printing, so
//! the front-end owns all input and output.

use anyhow::{Result, bail};
use serde_json::json;

use dap_client::dap::DapClient;
use dap_client::dap::types::{EvaluateBody, StackFrame, StackTraceBody, ThreadsBody};
use dap_client::model::SessionState;

/// Session state, the stopped thread, and the frames an expression can target.
pub struct Session {
    pub state: SessionState,
    thread_id: Option<i64>,
    stack: Vec<StackFrame>,
    selected: usize,
}

impl Session {
    pub fn new() -> Self {
        Self {
            state: SessionState::Connecting,
            thread_id: None,
            stack: Vec::new(),
            selected: 0,
        }
    }

    /// The frame an expression evaluates against, or nothing when there is none.
    /// The adapter only keeps frame references valid while the program is
    /// stopped, so the stack empties when it resumes.
    pub fn frame_id(&self) -> Option<i64> {
        self.stack.get(self.selected).map(|f| f.id)
    }

    /// The frame currently selected for navigation and evaluation.
    pub fn current_frame(&self) -> Option<&StackFrame> {
        self.stack.get(self.selected)
    }

    pub fn stack(&self) -> &[StackFrame] {
        &self.stack
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn thread_id(&self) -> Option<i64> {
        self.thread_id
    }

    /// Resolve the stopped thread and its call stack, selecting the top frame.
    /// The thread comes from the event when it carries one, otherwise from the
    /// first thread the adapter reports.
    pub async fn on_stopped(&mut self, client: &DapClient, thread_hint: Option<i64>) -> Result<()> {
        self.state = SessionState::Stopped;
        self.stack.clear();
        self.selected = 0;

        self.thread_id = match thread_hint {
            Some(id) => Some(id),
            None => {
                let resp = client.request("threads", Some(json!({}))).await?;
                resp.parse_body::<ThreadsBody>()?
                    .threads
                    .first()
                    .map(|t| t.id)
            }
        };

        if let Some(thread_id) = self.thread_id {
            self.stack = fetch_stack(client, thread_id).await?;
        }
        Ok(())
    }

    pub fn on_continued(&mut self) {
        self.state = SessionState::Running;
        // A thread keeps its identity across a resume, so pausing still has a
        // target. Frame references do not, so the stack is dropped.
        self.stack.clear();
        self.selected = 0;
    }

    pub fn on_ended(&mut self) {
        self.state = SessionState::Ended;
        self.thread_id = None;
        self.stack.clear();
        self.selected = 0;
    }

    /// Select the caller of the current frame. False at the outermost frame.
    pub fn select_up(&mut self) -> bool {
        if self.selected + 1 < self.stack.len() {
            self.selected += 1;
            true
        } else {
            false
        }
    }

    /// Select the callee of the current frame. False at the innermost frame.
    pub fn select_down(&mut self) -> bool {
        if self.selected > 0 {
            self.selected -= 1;
            true
        } else {
            false
        }
    }

    /// Select a frame by index. False when the index is past the stack.
    pub fn select_index(&mut self, index: usize) -> bool {
        if index < self.stack.len() {
            self.selected = index;
            true
        } else {
            false
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// The outcome of an evaluation. It holds the value the adapter rendered and a
/// type when the adapter provides one.
pub struct Evaluated {
    pub result: String,
    pub ty: Option<String>,
}

/// Evaluate a user-typed expression in the given frame. It uses the [repl
/// evaluate context](https://microsoft.github.io/debug-adapter-protocol/specification#Requests_Evaluate)
/// of the Debug Adapter Protocol.
///
/// That context is not sandboxed. A typed expression can assign to variables or
/// call functions, so evaluating one can change the running program.
pub async fn evaluate(client: &DapClient, expression: &str, frame_id: i64) -> Result<Evaluated> {
    let resp = client
        .request(
            "evaluate",
            Some(json!({
                "expression": expression,
                "frameId": frame_id,
                "context": "repl"
            })),
        )
        .await?;
    if !resp.success {
        bail!(
            "{}",
            resp.message.unwrap_or_else(|| "evaluate failed".into())
        );
    }
    let body = resp.parse_body::<EvaluateBody>()?;
    Ok(Evaluated {
        result: body.result,
        ty: body.ty,
    })
}

/// Drive execution of the stopped thread. The command is a DAP run-control
/// request such as continue, next, stepIn, stepOut, or pause. The resulting stop
/// or resume arrives later as a broadcast event.
pub async fn drive(client: &DapClient, command: &str, thread_id: i64) -> Result<()> {
    let resp = client
        .request(command, Some(json!({ "threadId": thread_id })))
        .await?;
    if !resp.success {
        bail!(
            "{}",
            resp.message.unwrap_or_else(|| format!("{command} failed"))
        );
    }
    Ok(())
}

/// Fetch a thread's full call stack. A stale request after the program resumed
/// underneath us yields an empty stack rather than an error.
async fn fetch_stack(client: &DapClient, thread_id: i64) -> Result<Vec<StackFrame>> {
    let resp = client
        .request("stackTrace", Some(json!({ "threadId": thread_id })))
        .await?;
    if !resp.success {
        return Ok(Vec::new());
    }
    Ok(resp.parse_body::<StackTraceBody>()?.stack_frames)
}
