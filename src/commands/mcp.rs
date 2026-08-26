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
    #[schemars(
        description = "Recency half-life in days: a hit this old keeps half its score (default 30). Pass 0 to weigh every age alike — do that when the memory spans months and the answer may be old, such as a founding decision."
    )]
    pub half_life: Option<f64>,
    #[schemars(
        description = "Adjacent chunks to attach to each hit for context (default 1). Raise it to read more around a hit without a follow-up `get`; 0 returns the hits alone."
    )]
    pub neighbors: Option<i64>,
    #[schemars(
        description = "How many fused candidates to rerank (default 30). Raise it when a topic is rare and the first pass may not surface it."
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
        description = "First turn to read, as the session's own seq — a dense counter over its turns, which a hit's `→ get` line gives you. Defaults to the session's start."
    )]
    pub from: Option<i64>,
    #[schemars(description = "Last turn to read, as the session's own seq. Defaults to 20 turns from `from`.")]
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
        description = "Rows to list, keeping the most recent (default 50, maximum 200). More than that cannot fit in one reply — walk with `offset` instead. Zero is an error, not every match."
    )]
    pub limit: Option<usize>,
    #[schemars(
        description = "Skip this many of the most recent matches before taking `limit` — how you walk a listing back through time. The closing line names the offset that continues, and a given offset always names the same session."
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
    #[schemars(description = "Match regardless of case (default false)")]
    pub ignore_case: Option<bool>,
    #[schemars(description = "Characters of surrounding text shown on each side of a match (default 100)")]
    pub context: Option<usize>,
    #[schemars(
        description = "Memory to scan — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Defaults to the server's memory."
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
            k.unwrap_or(8),
            candidates.unwrap_or(30),
            half_life.unwrap_or(30.0),
            neighbors.unwrap_or(1),
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
        description = "Read a range of one session's turns, each reassembled into readable text. Turns are addressed by `seq`, the session's own dense counter — a recall or scan hit's `→ get` line hands you the session, the range around the hit, and the memory to pass, so drilling into a hit is running what it printed. A session id on its own reads from the start, which is how you read a session you picked from `sessions` rather than found by searching. Widen or move by editing `from`/`to`; every reply closes with the range it covered and the session's size, so the next range is obvious."
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
        description = "List a memory's sessions, oldest first: date, harness, source repo (or workdir when the checkout did not resolve), turn count, full session id, and the prompt each session opened with. That prompt is the cheapest triage there is — it says what a session was for without reading any of it. `recall` is ranked retrieval: it surfaces what a query reaches and says nothing about the rest, so it can never tell you how much a memory holds or how much of it you have looked at. Call this whenever the population is the question: sizing a memory before a sweep, picking the sessions a criterion is about, reporting how many you actually examined, or checking whether a session you heard about by id is in there at all. Narrow with `repo`, `since` and `until` rather than listing everything; the closing line always states the full match count, so an elided row is visible and never silently dropped."
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
        description = "Find a literal string in every block of one session — exhaustive and unranked, where `recall` is ranked and partial. This is what answers questions of absence about a session: `recall` can show that something is present, never that it is nowhere, so a claim that a session does not mention some term has to be settled here. Use it to screen a session against a criterion — does this one name an internal project, a customer, a credential — and to locate where a term you already expect actually occurs. Splits are stitched back together first, so a needle straddling a chunk boundary is still found, and each hit carries a `→ get` line to read the turn around it. Scanning is per session by design: name the session with `session_id`, and use `sessions` to enumerate the ones to screen. A needle that returns nothing is absent from that session and says nothing about any other; absence clears only the exact spelling you passed, so pick the ones that matter and use `ignore_case` for case variation. Listing stops at 200 hits and names the `from` to continue at, so a common term in a long session is still walkable; `from`/`to` also scan just a stretch of a session — and then a zero clears only that stretch."
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
            context.unwrap_or(100),
        )
        .await
        {
            Ok(s) if !s.is_empty() => s,
            Ok(_) => "no results".to_string(),
            Err(e) => format!("scan error: {e}"),
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
