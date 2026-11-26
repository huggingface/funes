//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lance), then read via
//! `recall` (hybrid → reranked when the pool exceeds the hits), `get`, `status`.
//! The binary (`main.rs`) is the CLI's definitions and dispatch, nothing else; integration
//! tests drive the commands directly.
//!
//! One directory per layer, each with one job:
//!
//! - [`traces`] — where turns come from: turns files, the spools integrations write them into,
//!   and the `Turn`/`Block` model they produce.
//! - [`chunk`], [`scan`], [`home`] — what the layers share: chunk text and its ids, secret
//!   findings, and where funes's home keeps each thing.
//! - [`inference`] — embedding and reranking behind traits, so a backend swaps at build time.
//! - [`hub`] — *transport*: the Hugging Face Hub's client, credentials, and dataset-repo identity
//!   and lifecycle. Knows nothing about memories; four layers call it.
//! - [`memory`] — the memory itself: Lance and object-store *mechanics* under a *domain* that says
//!   what a memory is and what state it's in.
//! - [`commands`] — what funes does when you run it: orchestration and decisions.
//! - [`ui`] — how a result reaches the terminal.
//! - [`agents`] — registering funes with a coding agent (MCP + automation hooks).
//!
//! Where a new function goes: names an HF concept → transport; names Lance → mechanics; answers
//! *what is this memory, what state is it in* → domain; decides *what to do about it* → command.
//! Commands ask the layers below for state; they never infer it from error shapes.

// funes builds for Unix (Linux/macOS) and for Windows. Fail with a clear message on any other
// platform rather than a confusing missing-symbol error deep in a module.
#[cfg(not(any(unix, windows)))]
compile_error!("funes builds for Unix (Linux/macOS) and Windows only");

pub mod agents;
pub mod chunk;
pub mod commands;
pub mod home;
pub mod hub;
pub mod inference;
pub mod memory;
pub mod scan;
pub mod traces;
pub mod ui;
