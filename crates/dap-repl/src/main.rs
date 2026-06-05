//! dap-repl is an interactive console for a paused dap-mux session. It evaluates
//! expressions you type and drives execution with commands like :next and
//! :continue.
//!
//! What you type runs inside the live program, and the commands move the shared
//! session, so either can change what every client on the mux sees.

mod outcome;
mod repl;

use std::fmt::Write as _;
use std::io::Write;

use anyhow::Result;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::UnboundedReceiver;

use dap_client::dap::{self, ConnEvent, DapClient};

use outcome::{Outcome, decide_event, decide_input};
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

/// Where an outcome came from. Output for an event starts on a fresh line so it
/// breaks away from a possibly dangling prompt, the way input output, which
/// always follows a freshly typed line, does not need to.
enum Source {
    Input,
    Event,
}

/// Write text to the terminal and flush it. A failure here means the terminal is
/// gone, for example an SSH session that dropped, so callers end the session
/// rather than write into a void.
fn emit(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn prompt() -> Result<()> {
    emit("(dap) ")
}

async fn run(
    client: DapClient,
    mut events: UnboundedReceiver<ConnEvent>,
    addr: &str,
) -> Result<i32> {
    emit(&format!("connected to mux at {addr}\n"))?;
    // Do the minimal late-join handshake. The mux then replays the current stopped state.
    dap::initialize(&client).await?;
    emit("initialized. type an expression, or :help for commands. Ctrl-D quits.\n")?;

    let mut session = Session::new();
    let mut last_input: Option<String> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut exit_code = 0;
    prompt()?;

    // A write error inside the loop means the terminal is gone, the usual cause
    // being a dropped SSH session. We are still attached to the shared mux, so we
    // tear that down on the way out the same as any other exit, just with no exit
    // code to report into a terminal that can no longer show it.
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = emit("\n");
                break;
            }
            line = lines.next_line() => match line {
                Ok(Some(raw)) => {
                    let outcome =
                        decide_input(&client, &mut session, &mut last_input, raw.trim()).await;
                    if render(&outcome, Source::Input).is_err() {
                        break;
                    }
                    if matches!(outcome, Outcome::Quit) {
                        break;
                    }
                    if prompt().is_err() {
                        break;
                    }
                }
                // Stdin reached its end, for example from Ctrl-D.
                Ok(None) => {
                    let _ = emit("\n");
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
                    let _ = emit("\n■ session ended.\n");
                    break;
                }
                Some(ConnEvent::Dap(msg)) => {
                    let outcome = decide_event(&client, &mut session, msg).await;
                    let shown = match render(&outcome, Source::Event) {
                        Ok(shown) => shown,
                        Err(_) => break,
                    };
                    if matches!(outcome, Outcome::Ended) {
                        break;
                    }
                    // Re-emit the prompt only when the event showed something, so
                    // the stream of ignored events stays silent.
                    if shown && prompt().is_err() {
                        break;
                    }
                }
            },
        }
    }

    client.disconnect().await;
    Ok(exit_code)
}

/// Turn an outcome into the terminal output it stands for. This is the only place
/// the print front-end writes. It reports whether anything was shown, so the loop
/// can skip the reprompt when an outcome is silent, and surfaces a write error so
/// the loop can end when the terminal goes away.
fn render(outcome: &Outcome, source: Source) -> Result<bool> {
    let mut out = String::new();
    match outcome {
        Outcome::Evaluated { value, ty } => match ty {
            Some(ty) if !ty.is_empty() => {
                let _ = writeln!(out, "=> {value} : {ty}");
            }
            _ => {
                let _ = writeln!(out, "=> {value}");
            }
        },
        Outcome::EvaluationUnavailable { reason }
        | Outcome::DriveUnavailable { reason }
        | Outcome::NavigationBlocked { reason } => {
            let _ = writeln!(out, "-- {reason}");
        }
        Outcome::FrameSelected { index, frame } => {
            let _ = writeln!(out, "#{index} {} @ line {}", frame.name, frame.line);
        }
        Outcome::Stack { frames, selected } => {
            for (index, frame) in frames.iter().enumerate() {
                let marker = if index == *selected { "*" } else { " " };
                let _ = writeln!(
                    out,
                    "{marker} #{index} {} @ line {}",
                    frame.name, frame.line
                );
            }
        }
        Outcome::Help => write_help(&mut out),
        Outcome::Unrecognized { command } => {
            let _ = writeln!(out, "-- :{command} not recognized (try :help)");
        }
        Outcome::Failed { error } => {
            let _ = writeln!(out, "!! {error}");
        }
        Outcome::Stopped { reason, frame } => match frame {
            Some(frame) => {
                let _ = writeln!(
                    out,
                    "⏸ stop ({reason}) → {} @ line {}",
                    frame.name, frame.line
                );
            }
            None => {
                let _ = writeln!(out, "⏸ stopped ({reason}) — no frame resolved");
            }
        },
        Outcome::Resumed => {
            let _ = writeln!(out, "▶ running…");
        }
        Outcome::Ended => {
            let _ = writeln!(out, "■ session ended.");
        }
        Outcome::DriveIssued | Outcome::Quit | Outcome::Noop => {}
    }

    if out.is_empty() {
        return Ok(false);
    }
    let mut stdout = std::io::stdout().lock();
    if matches!(source, Source::Event) {
        stdout.write_all(b"\n")?;
    }
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(true)
}

fn write_help(out: &mut String) {
    out.push_str(
        "commands:
  :c :continue   resume execution
  :n :next       step over
  :s :step       step into
  :o :finish     step out
  :pause         pause a running program
  :up :down      move to the calling or called frame
  :frame <n>     select frame n
  :bt :where     print the call stack
  :help          show this list
  :q :quit       exit
anything else is evaluated in the selected frame.
an empty line repeats the last input.
",
    );
}
