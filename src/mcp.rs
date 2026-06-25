//! `funes mcp`: expose recall over the Model Context Protocol (stdio transport),
//! so any MCP client (Claude Code, Cursor, …) can call funes as a first-class tool.
//! stdout is the JSON-RPC channel — logs must go to stderr.

use crate::hub::Store;
use crate::recall;
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
    #[schemars(description = "Restrict to a project (the directory segment under `projects`)")]
    pub project: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetRequest {
    #[schemars(description = "Session id from a recall hit's `→ get` line")]
    pub session_id: String,
    #[schemars(description = "Turn uuid from a recall hit's `→ get` line")]
    pub turn_uuid: String,
    #[schemars(description = "Turns within this seq window of the target are included (default 3)")]
    pub window: Option<i64>,
}

#[derive(Clone)]
pub(crate) struct Funes {
    #[allow(dead_code)]
    tool_router: ToolRouter<Funes>,
}

#[tool_router]
impl Funes {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Recall decisions, rationale, and context from the user's past AI agent sessions. Returns ranked passages with provenance (timestamp, session, block type) plus surrounding neighbor chunks. Each hit carries a `→ get <session_id> <turn_uuid>` line — call `get` with those to read the full surrounding turns. Use when the user references earlier work, or when you lack context that may exist in a prior session. Also recall before asserting the history of anything — that it was never built, was dropped, is out of scope, or was never discussed; a confident claim about a past decision is the cue you're missing context this holds."
    )]
    async fn recall(
        &self,
        Parameters(RecallRequest {
            query,
            k,
            block_type,
            project,
        }): Parameters<RecallRequest>,
    ) -> String {
        match recall::recall(
            Store::resolve(None),
            query,
            k.unwrap_or(8),
            30,
            30.0,
            1,
            block_type,
            project,
        )
        .await
        {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("recall error: {e}"),
        }
    }

    #[tool(
        description = "Drill down on a recall hit: fetch the named turn plus the turns within `window` of it, each reassembled into readable text. Pass the `session_id` and `turn_uuid` from a recall hit's `→ get` line."
    )]
    async fn get(
        &self,
        Parameters(GetRequest {
            session_id,
            turn_uuid,
            window,
        }): Parameters<GetRequest>,
    ) -> String {
        match recall::get(Store::resolve(None), session_id, turn_uuid, window.unwrap_or(3)).await {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("get error: {e}"),
        }
    }

    #[tool(description = "Show funes index statistics (chunk count and store).")]
    async fn status(&self) -> String {
        recall::status(Store::resolve(None))
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
                 about a past decision is the cue to recall first. Drill into a hit with `get`."
                    .to_string(),
            )
    }
}

pub async fn run() -> Result<()> {
    let service = Funes::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
