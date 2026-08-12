//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lance), then read via
//! `recall` (hybrid → rerank → recency → neighbors), `get`, `status`.
//! The binary ([`main`]) is a thin CLI over these modules; integration tests drive
//! them directly.
//!
//! One directory per layer, each with one job:
//!
//! - [`traces`] — where sessions come from and how each harness's transcript is parsed, plus the
//!   `Turn`/`Block` model every parser produces.
//! - [`chunk`], [`scan`] — the two models the layers share: chunk text and its ids, and secret
//!   findings.
//! - [`inference`] — embedding and reranking behind traits, so a backend swaps at build time.
//! - [`memory`] — the memory itself, in three sublayers: Lance and object-store *mechanics*, the
//!   Hub *transport*, and the *domain* — what a memory is and what state it's in.
//! - [`commands`] — what funes does when you run it: orchestration and decisions.
//! - [`ui`] — how a result reaches the terminal.
//! - [`agents`] — registering funes with a coding agent (MCP + automation hooks).
//!
//! Where a new function goes: names an HF concept → transport; names Lance → mechanics; answers
//! *what is this memory, what state is it in* → domain; decides *what to do about it* → command.
//! Commands ask the layers below for state; they never infer it from error shapes.

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
