//! DAP transport and message types.

pub mod transport;
pub mod types;

use serde_json::{Value, json};

pub use transport::{ConnEvent, DapClient, connect};

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
