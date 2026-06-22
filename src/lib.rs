//! funes — recall over your past AI agent sessions.
//!
//! Pipeline: parse transcripts → chunk → embed → store (lancedb), then read via
//! `recall` (hybrid → rerank → recency → neighbors), `list`, `get`, `status`.
//! The binary ([`main`]) is a thin CLI over these modules; integration tests drive
//! them directly.

pub mod capture;
pub mod chunk;
pub mod db;
pub mod hub;
pub mod index;
pub mod mcp;
pub mod parse;
pub mod recall;
pub mod scan;
pub mod sync;
