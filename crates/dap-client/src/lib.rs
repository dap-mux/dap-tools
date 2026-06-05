//! Client-side DAP engine for the dap-mux tool suite.
//!
//! A read-only, late-joining DAP *client*: it sends the read requests and the
//! observer-safe `evaluate`, correlates responses, tracks session/frame state,
//! and builds the variable tree. It is the opposite end of the wire from a debug
//! adapter, and carries no terminal UI — front ends (the observer, a future
//! repl) build on top of it.

pub mod dap;
pub mod model;
