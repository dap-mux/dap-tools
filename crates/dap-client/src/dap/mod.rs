//! DAP transport and message types.

pub mod transport;
pub mod types;

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

pub use transport::{ConnEvent, DapClient, connect};

/// The mux address used when the user gives no target.
pub const DEFAULT_ADDR: &str = "127.0.0.1:5679";

/// Resolve an optional target into a concrete address. A bare port assumes the
/// loopback host. Nothing falls back to the default mux address.
pub fn resolve_addr(target: Option<&str>) -> String {
    match target {
        None => DEFAULT_ADDR.to_string(),
        Some(t) if t.contains(':') => t.to_string(),
        Some(port) => format!("127.0.0.1:{port}"),
    }
}

/// Upper bound on the late-join `initialize` round-trip. A mux that accepts the
/// connection but never answers must not wedge us here: this handshake runs
/// before the event loop, so without a bound there would be no live Ctrl-C or
/// key handling to abort it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Perform the late-join `initialize` handshake, bounded by `HANDSHAKE_TIMEOUT`.
pub async fn initialize(client: &DapClient) -> Result<()> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        client.request("initialize", Some(initialize_args())),
    )
    .await
    .context("initialize handshake timed out — is the mux responding?")?
    .context("initialize handshake failed")?;
    Ok(())
}

/// Arguments for the minimal late-join `initialize`.
///
/// Crucially we do NOT advertise `supportsRunInTerminalRequest`, so the mux
/// routes any reverse `runInTerminal` request to the client driving the
/// session and never to us.
pub fn initialize_args() -> Value {
    json!({
        "adapterID": "dap-observer",
        "clientID": "dap-observer",
        "clientName": "dap-observer",
        "pathFormat": "path",
        "linesStartAt1": true,
        "columnsStartAt1": true,
        "supportsVariableType": true,
        "supportsRunInTerminalRequest": false
    })
}
