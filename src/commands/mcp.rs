//! `funes mcp`: expose recall over the Model Context Protocol (stdio transport),
//! so any MCP client (Claude Code, Cursor, …) can call funes as a first-class tool.
//! stdout is the JSON-RPC channel — logs must go to stderr.

use super::recall;
use crate::memory::Memory;
use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RecallRequest {
    #[schemars(description = "Natural-language description of what to recall from past sessions")]
    pub query: String,
    #[schemars(description = "Number of results to return")]
    pub k: Option<usize>,
    #[schemars(
        description = "Recency half-life in days: a hit that old keeps half its score. Pass 0 to weigh every age alike, for a memory spanning months or an answer that may be old."
    )]
    pub half_life: Option<f64>,
    #[schemars(description = "Adjacent chunks attached to each hit for context; 0 returns the hits alone.")]
    pub neighbors: Option<i64>,
    #[schemars(
        description = "How many fused candidates to rerank. Raise it when a topic is rare and the first pass may not surface it."
    )]
    pub candidates: Option<usize>,
    #[schemars(description = "Restrict to a block type: text | thinking | tool_use | tool_result")]
    pub block_type: Option<String>,
    #[schemars(description = "Restrict to a harness: claude | codex | pi | hermes")]
    pub harness: Option<String>,
    #[schemars(
        description = "Memory to read for this call — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetRequest {
    #[schemars(description = "Session id from a recall hit's `→ get` line")]
    pub session_id: String,
    #[schemars(
        description = "First turn to read, as the session's own seq, which a hit's `→ get` line gives you. Defaults to the session's start."
    )]
    pub from: Option<i64>,
    #[schemars(
        description = "Last turn to read, as the session's own seq. Omit it to read a fixed span from `from`; every reply names the range it covered."
    )]
    pub to: Option<i64>,
    #[schemars(
        description = "Memory to read for this call — the one the recall hit's `→ get` line names. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionsRequest {
    #[schemars(description = "Keep only sessions whose checkout resolved to this repo, as `owner/name`")]
    pub repo: Option<String>,
    #[schemars(description = "Keep only sessions that started on or after this date, `YYYY-MM-DD`")]
    pub since: Option<String>,
    #[schemars(description = "Keep only sessions that started on or before this date, `YYYY-MM-DD`")]
    pub until: Option<String>,
    #[schemars(
        description = "Rows to list, keeping the most recent. More than the maximum cannot fit in one reply — walk with `offset` instead. Zero is an error, not every match."
    )]
    pub limit: Option<usize>,
    #[schemars(
        description = "Skip this many of the most recent matches before taking `limit`, to walk a listing back through time. The closing line names the offset that continues, and a given offset always names the same session."
    )]
    pub offset: Option<usize>,
    #[schemars(
        description = "Memory to list — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ScanRequest {
    #[schemars(
        description = "The literal string to find. Not regex — a pattern that silently matched nothing would read as a clean result."
    )]
    pub needle: String,
    #[schemars(description = "Session to scan — the id from a `sessions` row or a recall hit's `→ get` line")]
    pub session_id: String,
    #[schemars(
        description = "First turn to scan, as the session's own seq. Omit to start at the session's beginning; pass the seq a capped scan told you to continue from."
    )]
    pub from: Option<i64>,
    #[schemars(description = "Last turn to scan, as the session's own seq. Omit to scan to the end of the session.")]
    pub to: Option<i64>,
    #[schemars(description = "Match regardless of case")]
    pub ignore_case: Option<bool>,
    #[schemars(description = "Characters of surrounding text shown on each side of a match")]
    pub context: Option<usize>,
    #[schemars(
        description = "Memory to scan — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SketchRequest {
    #[schemars(description = "Session to digest — the id from a `sessions` row or a hit's `→ get` line")]
    pub session_id: String,
    #[schemars(
        description = "How many distinct places to show — breadth (default 8, maximum 40). Also held to what `max_chars` can render at 240 characters apiece, and any narrowing is reported in the reply."
    )]
    pub units: Option<usize>,
    #[schemars(
        description = "Total characters to render — cost (default 8000, maximum 40000). More than that is a reply no caller receives."
    )]
    pub max_chars: Option<usize>,
    #[schemars(
        description = "First turn to digest, as the session's own seq. Digest a long session in windows to get coverage proportional to its length rather than a fixed sample of it."
    )]
    pub from: Option<i64>,
    #[schemars(description = "Last turn to digest, as the session's own seq.")]
    pub to: Option<i64>,
    #[schemars(
        description = "Memory to read for this call — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusRequest {
    #[schemars(
        description = "Memory to inspect — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
    )]
    pub memory: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Funes {
    /// Explicit memory spec (`funes mcp <memory>`), pinned for the server's lifetime. `None` reads
    /// the local memory unless a call passes its own `memory`.
    memory: Option<String>,
    #[allow(dead_code)]
    tool_router: ToolRouter<Funes>,
}

#[tool_router]
impl Funes {
    fn new(memory: Option<String>) -> Self {
        Self {
            memory,
            tool_router: Self::tool_router(),
        }
    }

    /// The memory a call reads: its explicit `memory` argument wins over the server's `<memory>`,
    /// else the local memory.
    fn memory(&self, spec: Option<String>) -> Memory {
        Memory::resolve(spec.filter(|s| !s.trim().is_empty()).or_else(|| self.memory.clone()))
    }

    #[tool(
        description = "Semantic search over the user's past AI agent sessions: describe what you are after, get back the verbatim passages — what was decided, tried, measured or investigated. Call it when they refer to earlier work, or when you are about to re-derive something a session may already have settled. Call it too before claiming that something was never built, was dropped, or was never discussed: the code cannot show that, only the sessions can. Ranked top-k, so it gives you a foothold on a topic, not every session touching it."
    )]
    async fn recall(
        &self,
        Parameters(RecallRequest {
            query,
            k,
            half_life,
            neighbors,
            candidates,
            block_type,
            harness,
            memory,
        }): Parameters<RecallRequest>,
    ) -> String {
        match recall::recall(
            self.memory(memory),
            query,
            k.unwrap_or(recall::DEFAULT_K),
            candidates.unwrap_or(recall::DEFAULT_CANDIDATES),
            half_life.unwrap_or(recall::DEFAULT_HALF_LIFE),
            neighbors.unwrap_or(recall::DEFAULT_NEIGHBORS),
            block_type,
            harness,
        )
        .await
        {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("recall error: {e}"),
        }
    }

    #[tool(
        description = "Read one session's turns as they were written — whole blocks, splits reassembled, nothing ranked or summarized. Typically the follow-up to a search verb: expand a result into the turns around it. Sessions run to thousands of turns: read the stretch you need, not the session."
    )]
    async fn get(
        &self,
        Parameters(GetRequest {
            session_id,
            from,
            to,
            memory,
        }): Parameters<GetRequest>,
    ) -> String {
        let range = recall::TurnRange { from, to };
        match recall::get(self.memory(memory), session_id, range).await {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("get error: {e}"),
        }
    }

    #[tool(
        description = "Enumerate a memory's sessions, oldest first. Metadata only — date, harness, repo, turn count, id, and the prompt each opened with, which is the ask a session started from, not what it became. Filter by repo or date to get the session ids the other verbs take; a whole memory can run to thousands of rows."
    )]
    async fn sessions(
        &self,
        Parameters(SessionsRequest {
            repo,
            since,
            until,
            limit,
            offset,
            memory,
        }): Parameters<SessionsRequest>,
    ) -> String {
        let filter = recall::SessionFilter {
            repo,
            since,
            until,
            limit,
            offset: offset.unwrap_or(0),
        };
        match recall::sessions(self.memory(memory), filter).await {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("sessions error: {e}"),
        }
    }

    #[tool(
        description = "Find every occurrence of a literal string in one session — exhaustive, unranked, in reading order. For when you already know the wording and want where it occurs, rather than what a topic is about. Scope is one session and one spelling: a zero result clears only that."
    )]
    async fn scan(
        &self,
        Parameters(ScanRequest {
            needle,
            session_id,
            from,
            to,
            ignore_case,
            context,
            memory,
        }): Parameters<ScanRequest>,
    ) -> String {
        match recall::scan(
            self.memory(memory),
            needle,
            session_id,
            from,
            to,
            ignore_case.unwrap_or(false),
            context.unwrap_or(recall::DEFAULT_CONTEXT),
        )
        .await
        {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("scan error: {e}"),
        }
    }

    #[tool(
        description = "What one session was about, worked on, and ended up at: the passages most distinctive within it, verbatim, always including its opening ask and its last word. Takes a session id; there is no query. Enough to tell the user what a session was, in one call; not a way to find a particular thing in it."
    )]
    async fn sketch(
        &self,
        Parameters(SketchRequest {
            session_id,
            units,
            max_chars,
            from,
            to,
            memory,
        }): Parameters<SketchRequest>,
    ) -> String {
        match super::sketch::run(self.memory(memory), session_id, from, to, units, max_chars).await {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("sketch error: {e}"),
        }
    }

    #[tool(
        description = "Health and size of a memory: how much is indexed, what is still pending, and for a remote what this host has yet to push. Call it when a read comes back empty or thinner than expected — it says whether the memory is the problem rather than the call."
    )]
    async fn status(&self, Parameters(StatusRequest { memory }): Parameters<StatusRequest>) -> String {
        // No update check here: it needs the network, and the "update available" notice belongs
        // on the human-facing CLI `funes status`, not on this hot, otherwise-local tool path.
        recall::status(self.memory(memory))
            .await
            .unwrap_or_else(|e| format!("status error: {e}"))
    }
}

#[tool_handler]
impl ServerHandler for Funes {
    fn get_info(&self) -> ServerInfo {
        let mut server_info = Implementation::default();
        server_info.name = "funes".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Persistent memory over the user's past AI coding sessions: their transcripts, \
                 indexed automatically as they work and read-only here — nothing has to be saved. \
                 When earlier work matters, this is the memory to consult."
                    .to_string(),
            )
    }
}

pub async fn run(memory: Option<String>) -> Result<()> {
    let service = Funes::new(memory).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
