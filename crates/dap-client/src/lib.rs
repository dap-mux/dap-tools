//! Client-side DAP engine for the dap-mux tool suite.
//!
//! A late-joining DAP client. It performs the read requests, evaluates
//! expressions, drives execution, correlates responses, tracks session and frame
//! state, and builds the variable tree. It is the opposite end of the wire from a
//! debug adapter and carries no terminal UI, so the front end builds on top of it.

pub mod dap;
pub mod model;
