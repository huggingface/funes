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
    #[schemars(description = "Number of results to return (default 8)")]
    pub k: Option<usize>,
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
        description = "Recall decisions, rationale, and context from the user's past AI agent sessions. Returns ranked passages with provenance (timestamp, session, block type) plus surrounding neighbor chunks. Each hit carries a `→ get <session_id> --from <seq> --to <seq>` line — call `get` with exactly those to read the full surrounding turns. Use when the user references earlier work, or when you lack context that may exist in a prior session. Recall subject-matter, not only decisions: before re-deriving how an API, library, or system behaves — or anything a past session already investigated — query the topic itself; research subagents accumulate exactly these findings and recall surfaces them, often as the top hit, so check before re-investigating from scratch. Also recall before asserting the history of anything — that it was never built, was dropped, is out of scope, or was never discussed; a confident claim about a past decision is the cue you're missing context this holds. To recall from a different memory than the server's default (e.g. a shared `<org>/<repo>` dataset on the HF Hub), pass `memory` — no CLI needed."
    )]
    async fn recall(
        &self,
        Parameters(RecallRequest {
            query,
            k,
            block_type,
            harness,
            memory,
        }): Parameters<RecallRequest>,
    ) -> String {
        match recall::recall(
            self.memory(memory),
            query,
            k.unwrap_or(8),
            30,
            30.0,
            1,
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
        description = "Read a range of one session's turns, each reassembled into readable text. Turns are addressed by `seq`, the session's own dense counter over its turns; a recall hit's `→ get` line carries the session, the range around the hit, and the memory to pass. A session id on its own reads from the start. Every reply closes with the range it covered and the session's size."
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
        description = "Show funes memory status: chunk and session counts, pending local indexing, and — for a remote memory — last push plus this host's pending push coverage."
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
                "Recall over the user's past AI agent sessions (hybrid search + cross-encoder \
                 rerank + recency). Call `recall` with a natural-language query when you need prior \
                 decisions, rationale, or context — and before asserting the history of anything \
                 (that it was never built, was dropped, or is out of scope): a confident claim \
                 about a past decision is the cue to recall first. Recall subject-matter too, not \
                 only decisions: before re-deriving how an API, library, or system behaves — or \
                 anything a prior session (often a research subagent) investigated — query the \
                 topic itself; recall surfaces those findings. Drill into a hit with `get`. Both \
                 take an optional `memory` to read a different memory for one call."
                    .to_string(),
            )
    }
}

pub async fn run(memory: Option<String>) -> Result<()> {
    let service = Funes::new(memory).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
