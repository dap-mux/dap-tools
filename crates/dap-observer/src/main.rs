//! dap-observer — a read-only, late-joining DAP client that rides a
//! dap-mux session and renders the current frame's variables.

mod headless;
mod ui;

use anyhow::Result;
use clap::Parser;
use dap_client::dap;

/// Read-only DAP variable watcher for a dap-mux session.
#[derive(Parser)]
#[command(name = "dap-observer", version, about, long_about = None)]
struct Args {
    /// Mux address as `host:port`, or a bare `port` (assumes 127.0.0.1).
    #[arg(value_name = "host:port | port")]
    target: Option<String>,

    /// Print DAP data to stdout instead of the UI.
    #[arg(long)]
    headless: bool,
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

    // Connection failure, usually no mux listening.
    let (client, events) = match dap::connect(&addr).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };

    let code = if args.headless {
        headless::run(client, events, &addr).await?
    } else {
        ui::run(client, events).await?;
        0
    };
    std::process::exit(code);
}
