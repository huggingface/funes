//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lance), then read via
//! `recall` (hybrid → rerank → recency → neighbors), `get`, `status`.
//! The binary ([`main`]) is a thin CLI over these modules; integration tests drive
//! them directly.

// funes is unix-only (Linux/macOS): the release targets, install.sh, and the in-place
// self-update all assume unix semantics. Fail with a clear message on other platforms rather
// than a confusing missing-symbol error deep in a module.
#[cfg(not(unix))]
compile_error!("funes is unix-only (Linux/macOS)");

pub mod agents;
pub mod chunk;
pub mod commands;
pub mod inference;
pub mod memory;
pub mod scan;
pub mod traces;
pub mod ui;
