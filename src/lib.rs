//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lance), then read via
//! `recall` (hybrid → rerank → recency → neighbors), `list`, `get`, `status`.
//! The binary ([`main`]) is a thin CLI over these modules; integration tests drive
//! them directly.

pub mod capture_store;
pub mod chunk;
pub mod claude_traces;
pub mod config;
pub mod dataset;
pub mod hello;
pub mod hf_dataset;
pub mod hf_traces;
pub mod hub;
pub mod index;
pub mod mcp;
pub mod push;
pub mod recall;
pub mod scan;
pub mod scrub;
pub mod source;
pub mod trace;
