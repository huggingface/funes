//! The parsed-trace model. A transcript becomes a sequence of [`Turn`]s, each carrying typed
//! [`Block`]s. Every source parser produces this shape, and everything downstream —
//! chunk → embed → store → recall — operates on it, so the model is source-agnostic and lives on
//! its own here.

pub struct Block {
    pub block_type: String, // "text" | "thinking" | "tool_use" | "tool_result"
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
}

pub struct Turn {
    pub session_id: String,
    pub project: String,
    pub turn_uuid: String,
    pub parent_uuid: Option<String>,
    pub seq: i64,
    pub ts: String,
    pub role: String,
    pub blocks: Vec<Block>,
    pub source_path: String,
    /// Which coding agent produced this session: `claude_code` | `codex` | `pi`.
    pub harness: String,
}
