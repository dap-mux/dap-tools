//! Async DAP transport.
//!
//! A single connection task owns the `TcpStream`, does `Content-Length`
//! framing, and is the only thing that touches the socket. The rest of the app
//! talks to it over channels: requests go in via a command channel (each with a
//! `oneshot` reply), decoded events come out via an event channel.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use super::types::{Inbound, Response};

/// A command sent from the app to the connection task.
enum Command {
    Request {
        command: String,
        args: Option<Value>,
        reply: oneshot::Sender<Response>,
    },
    Disconnect,
}

/// What the connection task pushes toward the app.
pub enum ConnEvent {
    /// A decoded DAP event (`stopped`, `continued`, `terminated`, …).
    Dap(super::types::EventMessage),
    /// The connection ended. `Some(err)` for an error, `None` for a clean EOF.
    Disconnected(Option<String>),
}

/// Handle to the connection task.
#[derive(Clone)]
pub struct DapClient {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl DapClient {
    /// Send a request and await its correlated response.
    pub async fn request(&self, command: &str, args: Option<Value>) -> Result<Response> {
        let (reply, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Request {
                command: command.to_string(),
                args,
                reply,
            })
            .map_err(|_| anyhow!("DAP connection task is gone"))?;
        rx.await
            .map_err(|_| anyhow!("DAP connection closed before responding to `{command}`"))
    }

    /// Send a non-terminating `disconnect`. The mux synthetic acks and the shared session keeps
    /// running for other clients.
    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(Command::Disconnect);
    }
}

/// Connect to `address` and spawn the connection task.
///
/// Returns the client handle and the event receiver. A connection failure
/// (no mux listening) surfaces here as an `Err` so `main` can exit non-zero.
pub async fn connect(address: &str) -> Result<(DapClient, mpsc::UnboundedReceiver<ConnEvent>)> {
    let stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("could not connect to {address} — is the mux running?"))?;
    stream.set_nodelay(true).ok();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    tokio::spawn(connection_task(stream, cmd_rx, event_tx));
    Ok((DapClient { cmd_tx }, event_rx))
}

async fn connection_task(
    stream: TcpStream,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    event_tx: mpsc::UnboundedSender<ConnEvent>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut pending: HashMap<i64, oneshot::Sender<Response>> = HashMap::new();
    let mut seq: i64 = 0;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Command::Request { command, args, reply }) => {
                    seq += 1;
                    pending.insert(seq, reply);
                    if let Err(e) = write_message(&mut write_half, seq, &command, args).await {
                        let _ = event_tx.send(ConnEvent::Disconnected(Some(e.to_string())));
                        break;
                    }
                }
                Some(Command::Disconnect) => {
                    seq += 1;
                    let _ = write_message(&mut write_half, seq, "disconnect", Some(json!({}))).await;
                }
                // All client handles dropped: nothing more to send. Keep
                // reading so in-flight replies/events can still arrive until EOF.
                None => {}
            },
            msg = read_message(&mut reader) => match msg {
                Ok(Some(value)) => match serde_json::from_value::<Inbound>(value) {
                    Ok(Inbound::Response(resp)) => {
                        if let Some(tx) = pending.remove(&resp.request_seq) {
                            let _ = tx.send(resp);
                        }
                    }
                    Ok(Inbound::Event(ev)) => {
                        let _ = event_tx.send(ConnEvent::Dap(ev));
                    }
                    // Reverse requests (e.g. runInTerminal) are routed to the
                    // client driving the session, not us, and anything we can't
                    // classify is ignored.
                    Ok(Inbound::Request(_)) | Err(_) => {}
                },
                Ok(None) => {
                    let _ = event_tx.send(ConnEvent::Disconnected(None));
                    break;
                }
                Err(e) => {
                    let _ = event_tx.send(ConnEvent::Disconnected(Some(e.to_string())));
                    break;
                }
            },
        }
    }
}

/// Encode and write one `Content-Length`-framed request.
async fn write_message<W: AsyncWriteExt + Unpin>(
    write_half: &mut W,
    seq: i64,
    command: &str,
    args: Option<Value>,
) -> Result<()> {
    let mut msg = json!({ "seq": seq, "type": "request", "command": command });
    if let Some(args) = args {
        msg["arguments"] = args;
    }
    let data = serde_json::to_vec(&msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", data.len());
    write_half.write_all(header.as_bytes()).await?;
    write_half.write_all(&data).await?;
    write_half.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed message. `Ok(None)` signals a clean EOF.
async fn read_message<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(rest.trim().parse().context("invalid Content-Length")?);
        }
    }
    let len = content_length.ok_or_else(|| anyhow!("message missing Content-Length header"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}
