//! dap-repl is an interactive console for a paused dap-mux session. It evaluates
//! expressions you type and drives execution with commands like :next and
//! :continue.
//!
//! What you type runs inside the live program, and the commands move the shared
//! session, so either can change what every client on the mux sees.

mod repl;

use std::io::Write;

use anyhow::Result;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

use dap_client::dap::types::{EventMessage, StoppedBody};
use dap_client::dap::{self, ConnEvent, DapClient};
use dap_client::model::SessionState;

use repl::Session;

/// Interactive DAP console for a dap-mux session.
#[derive(Parser)]
#[command(name = "dap-repl", version, about, long_about = None)]
struct Args {
    /// Mux address as host:port, or a bare port that assumes 127.0.0.1.
    #[arg(value_name = "host:port | port")]
    target: Option<String>,
}

impl Args {
    fn addr(&self) -> String {
        dap::resolve_addr(self.target.as_deref())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let addr = args.addr();

    // A failed connection usually means nothing is listening on the mux.
    let (client, events) = match dap::connect(&addr).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };

    let code = run(client, events, &addr).await?;
    std::process::exit(code);
}

/// What a DAP event did, so the loop knows whether to redraw or exit.
enum EventResult {
    Ignored,
    Printed,
    Ended,
}

fn prompt() {
    print!("(dap) ");
    let _ = std::io::stdout().flush();
}

async fn run(
    client: DapClient,
    mut events: UnboundedReceiver<ConnEvent>,
    addr: &str,
) -> Result<i32> {
    println!("connected to mux at {addr}");
    // Do the minimal late-join handshake. The mux then replays the current stopped state.
    dap::initialize(&client).await?;
    println!("initialized. type an expression, or :help for commands. Ctrl-D quits.");

    let mut session = Session::new();
    let mut last_input: Option<String> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut exit_code = 0;
    prompt();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                break;
            }
            line = lines.next_line() => match line {
                Ok(Some(raw)) => {
                    if handle_input(&client, &mut session, &mut last_input, raw.trim()).await {
                        break;
                    }
                    prompt();
                }
                // Stdin reached its end, for example from Ctrl-D.
                Ok(None) => {
                    println!();
                    break;
                }
                Err(e) => {
                    eprintln!("stdin error: {e}");
                    exit_code = 1;
                    break;
                }
            },
            ev = events.recv() => match ev {
                None => break,
                Some(ConnEvent::Disconnected(Some(err))) => {
                    eprintln!("\n!! {err}");
                    exit_code = 1;
                    break;
                }
                Some(ConnEvent::Disconnected(None)) => {
                    println!("\n■ session ended.");
                    break;
                }
                Some(ConnEvent::Dap(msg)) => match handle_event(&client, &mut session, msg).await {
                    EventResult::Ignored => {}
                    // Re-emit the prompt only when an event printed, so the
                    // stream of ignored events stays silent.
                    EventResult::Printed => prompt(),
                    EventResult::Ended => break,
                },
            },
        }
    }

    client.disconnect().await;
    Ok(exit_code)
}

/// Route one line of input. Returns true when the user asked to quit.
async fn handle_input(
    client: &DapClient,
    session: &mut Session,
    last_input: &mut Option<String>,
    input: &str,
) -> bool {
    // An empty line repeats the last input, command or expression, so pressing
    // Enter keeps stepping or re-runs the last expression. Otherwise the line
    // becomes the new repeat target.
    let line = if input.is_empty() {
        match last_input {
            Some(prev) => prev.clone(),
            None => return false,
        }
    } else {
        *last_input = Some(input.to_string());
        input.to_string()
    };

    match line.strip_prefix(':') {
        Some(command) => run_command(client, session, command.trim()).await,
        None => {
            eval_line(client, session, &line).await;
            false
        }
    }
}

/// Run a colon command. Returns true when the user asked to quit.
async fn run_command(client: &DapClient, session: &mut Session, cmd: &str) -> bool {
    let mut parts = cmd.split_whitespace();
    let Some(verb) = parts.next() else {
        return false;
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
            None => println!("-- usage: :frame <index>"),
        },
        "bt" | "where" => print_stack(session),
        "help" => print_help(),
        "quit" | "q" => return true,
        other => println!("-- :{other} not recognized (try :help)"),
    }
    false
}

/// Issue a step or continue against the stopped thread. The resulting stop or
/// resume is announced by the event loop.
async fn drive(client: &DapClient, session: &Session, command: &str) {
    if session.state != SessionState::Stopped {
        println!("-- can't {command} unless stopped");
        return;
    }
    let Some(thread_id) = session.thread_id() else {
        println!("-- can't {command}: no thread");
        return;
    };
    if let Err(e) = repl::drive(client, command, thread_id).await {
        println!("!! {e}");
    }
}

async fn pause(client: &DapClient, session: &Session) {
    if session.state != SessionState::Running {
        println!("-- can't pause unless running");
        return;
    }
    let Some(thread_id) = session.thread_id() else {
        println!("-- can't pause: no thread");
        return;
    };
    if let Err(e) = repl::drive(client, "pause", thread_id).await {
        println!("!! {e}");
    }
}

fn navigate(session: &mut Session, up: bool) {
    if session.current_frame().is_none() {
        println!("-- not stopped at a frame");
        return;
    }
    let moved = if up {
        session.select_up()
    } else {
        session.select_down()
    };
    if moved {
        print_selected_frame(session);
    } else if up {
        println!("-- already at the outermost frame");
    } else {
        println!("-- already at the innermost frame");
    }
}

fn select_frame(session: &mut Session, index: usize) {
    if session.current_frame().is_none() {
        println!("-- not stopped at a frame");
    } else if session.select_index(index) {
        print_selected_frame(session);
    } else {
        println!(
            "-- no frame {index}, the stack has {}",
            session.stack().len()
        );
    }
}

fn print_selected_frame(session: &Session) {
    if let Some(frame) = session.current_frame() {
        println!(
            "#{} {} @ line {}",
            session.selected(),
            frame.name,
            frame.line
        );
    }
}

fn print_stack(session: &Session) {
    if session.stack().is_empty() {
        println!("-- no stack, not stopped at a frame");
        return;
    }
    for (index, frame) in session.stack().iter().enumerate() {
        let marker = if index == session.selected() {
            "*"
        } else {
            " "
        };
        println!("{marker} #{index} {} @ line {}", frame.name, frame.line);
    }
}

fn print_help() {
    println!("commands:");
    println!("  :c :continue   resume execution");
    println!("  :n :next       step over");
    println!("  :s :step       step into");
    println!("  :o :finish     step out");
    println!("  :pause         pause a running program");
    println!("  :up :down      move to the calling or called frame");
    println!("  :frame <n>     select frame n");
    println!("  :bt :where     print the call stack");
    println!("  :help          show this list");
    println!("  :q :quit       exit");
    println!("anything else is evaluated in the selected frame.");
    println!("an empty line repeats the last input.");
}

/// Evaluate one entered line against the selected frame, or explain why it can't.
async fn eval_line(client: &DapClient, session: &Session, expr: &str) {
    let Some(frame_id) = session.frame_id() else {
        let why = match session.state {
            SessionState::Running => "the program is running",
            SessionState::Ended => "the session has ended",
            SessionState::Stopped => "no frame is resolved at this stop",
            SessionState::Connecting => "not stopped yet",
        };
        println!("-- nothing to evaluate against ({why})");
        return;
    };
    match repl::evaluate(client, expr, frame_id).await {
        Ok(ev) => match ev.ty {
            Some(ty) if !ty.is_empty() => println!("=> {} : {ty}", ev.result),
            _ => println!("=> {}", ev.result),
        },
        Err(e) => println!("!! {e}"),
    }
}

/// Update session state from a DAP event. Each printed line starts with a
/// newline so it breaks away from a dangling prompt.
async fn handle_event(client: &DapClient, session: &mut Session, msg: EventMessage) -> EventResult {
    match msg.event.as_str() {
        "stopped" => {
            let body: StoppedBody = msg
                .body
                .and_then(|b| serde_json::from_value(b).ok())
                .unwrap_or_default();
            match session.on_stopped(client, body.thread_id).await {
                Ok(()) => match session.current_frame() {
                    Some(frame) => println!(
                        "\n⏸ stop ({}) → {} @ line {}",
                        body.reason, frame.name, frame.line
                    ),
                    None => println!("\n⏸ stopped ({}) — no frame resolved", body.reason),
                },
                Err(e) => println!("\n!! {e}"),
            }
            EventResult::Printed
        }
        "continued" => {
            session.on_continued();
            println!("\n▶ running…");
            EventResult::Printed
        }
        "terminated" | "exited" => {
            session.on_ended();
            println!("\n■ session ended.");
            EventResult::Ended
        }
        _ => EventResult::Ignored,
    }
}
