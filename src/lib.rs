//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lance), then read via
//! `recall` (hybrid → rerank → recency → neighbors), `list`, `get`, `status`.
//! The binary ([`main`]) is a thin CLI over these modules; integration tests drive
//! them directly.

// funes is unix-only (Linux/macOS): the release targets, install.sh, and the in-place
// self-update all assume unix semantics. Fail with a clear message on other platforms rather
// than a confusing missing-symbol error deep in a module.
#[cfg(not(unix))]
compile_error!("funes is unix-only (Linux/macOS)");

#[cfg(feature = "blas")]
pub mod blas;
pub mod capture_store;
pub mod chunk;
pub mod claude;
pub mod claude_traces;
pub mod codex;
pub mod codex_traces;
pub mod dataset;
pub mod fetch_store;
pub mod harness;
pub mod hello;
pub mod hermes;
pub mod hf_dataset;
pub mod hf_traces;
pub mod hub;
pub mod index;
pub mod inference;
pub mod jsonl;
pub mod lock;
pub mod mcp;
pub mod opencode;
pub mod pi;
pub mod pi_traces;
pub mod push;
pub mod recall;
pub mod render;
pub mod scan;
pub mod scrub;
pub mod source;
pub mod trace;
pub mod update;
