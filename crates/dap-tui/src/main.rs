//! dap-tui is an interactive terminal debugger that joins a dap-mux session. It
//! unifies stack and frame navigation, variable inspection, expression
//! evaluation, and run control behind one shared selected frame.
//!
//! What you evaluate runs inside the live program, and run control moves the
//! shared session, so either can change what every client on the mux sees.

mod model;
mod update;
mod view;

#[cfg(test)]
mod tests;

use anyhow::Result;
use clap::Parser;
use dap_client::dap;

/// Interactive terminal debugger for a dap-mux session.
#[derive(Parser)]
#[command(name = "dap-tui", version, about, long_about = None)]
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

    view::run(client, events).await?;
    Ok(())
}
