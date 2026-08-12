//! Reading agent sessions: where they come from ([`source`]), the per-harness parsers, and the
//! parsed-trace model they all produce.
//!
//! A transcript becomes a sequence of [`Turn`]s, each carrying typed [`Block`]s. Every parser
//! produces this shape, and everything downstream — chunk → embed → store → recall — operates on
//! it, so the model is source-agnostic and lives here, at the root of the parsers that fill it.

pub mod claude;
pub mod codex;
pub mod harness;
pub mod hermes;
pub mod jsonl;
pub mod parquet;
pub mod pi;
pub mod repo;
pub mod source;

pub struct Block {
    pub block_type: String, // "text" | "thinking" | "tool_use" | "tool_result"
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
}

pub struct Turn {
    pub session_id: String,
    pub workdir: String,
    pub turn_uuid: String,
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub blocks: Vec<Block>,
    pub source_path: String,
    /// Which coding agent produced this session: `claude_code` | `codex` | `pi` | `hermes`.
    pub harness: String,
}
