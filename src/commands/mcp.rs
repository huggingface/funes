//! `funes mcp`: expose recall over the Model Context Protocol (stdio transport),
//! so any MCP client (Claude Code, Cursor, …) can call funes as a first-class tool.
//! stdout is the JSON-RPC channel — logs must go to stderr.
//!
//! With `--socket` the server listens on a unix socket instead, and serves every connection as its
//! own MCP session: the agents on a host share the one process, and with it the embedder and
//! reranker it loads — most of what a `funes mcp` process costs. `--proxy` relays stdio to that
//! socket, for an integration that can only spawn a stdio server.

use super::recall;
use super::target::Target;
use crate::agents;
use crate::home;
use crate::memory::lock::{try_lock_file, FileLock};
use anyhow::{bail, Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::stdio;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinSet;

// A request refuses a parameter its schema does not list: an agent reading the schema never sends
// one, so an unknown field is the caller's bug, failed with the field named rather than ignored.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallRequest {
    #[schemars(
        description = "Natural-language description of what to recall from past sessions. A query matches best in the language they are written in, which `status` reports."
    )]
    pub query: String,
    #[schemars(
        description = "Hits returned (default 8). Each is a verbatim passage of a few hundred tokens, so `k` is what a call costs to read."
    )]
    pub k: Option<usize>,
    #[schemars(
        description = "One row per hit, no passage: the provenance, the `→ get` target and the hit's opening words. For choosing what to read before a `get`, at a fraction of the tokens."
    )]
    pub oneline: Option<bool>,
    #[schemars(
        description = "Size of the fused pool the hits are cut from (default 30): how far the search looks. Larger than `k`, the pool is reranked by the cross-encoder before the cut, which costs seconds and grows with the pool, so `k` can stay small while the search looks deep. A pool of `k` rows or fewer is read whole, in the fused order."
    )]
    pub candidates: Option<usize>,
    #[schemars(description = "Restrict to a block type: text | thinking | tool_use | tool_result")]
    pub block_type: Option<String>,
    #[schemars(
        description = "Restrict to a role, as the turns carry it (an agent's turns say user | assistant | tool | system)"
    )]
    pub role: Option<String>,
    #[schemars(
        description = "Restrict to a harness facet, as the turns carry it (`claude` also matches the older `claude_code`)"
    )]
    pub harness: Option<String>,
    #[schemars(description = "Restrict to turns whose checkout resolved to this repo, as `owner/name`")]
    pub repo: Option<String>,
    #[schemars(description = "Restrict to turns on or after this date, `YYYY-MM-DD`")]
    pub since: Option<String>,
    #[schemars(description = "Restrict to turns on or before this date, `YYYY-MM-DD`")]
    pub until: Option<String>,
    #[schemars(
        description = "Memory to read for this call, as it stands — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Omitted, the server's memory, with this host's turns not yet pushed to it when the host is bound to it."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Memory to read for this call, as it stands — the one the hit's `→ get` line carries, and none when it carries none, so the drill-down reads what the search read. Omitted, the server's memory, where a session this host has not finished pushing is read from the local memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Memory to list, as it stands — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Omitted, the server's memory, with the sessions this host has not finished pushing listed from the local memory when the host is bound to it."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Memory to scan, as it stands — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Omitted, the server's memory, where a session this host has not finished pushing is read from the local memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        description = "Memory to read for this call, as it stands — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Omitted, the server's memory, where a session this host has not finished pushing is read from the local memory."
    )]
    pub memory: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {
    #[schemars(
        description = "Memory to inspect — `<org>/<repo>`, an `hf://…` URI, a local path, or `local`. Omitted, the server's memory."
    )]
    pub memory: Option<String>,
}

/// A tool's text, led by the stale-install note when there is one. A tool result is the one channel
/// every client hands to the model, so this is where an install that stopped capturing gets said;
/// the CLI prints the same line on stderr and keeps stdout byte-identical to this text.
fn noted(text: String) -> String {
    match agents::stale_install_notice() {
        Some(note) => format!("{note}\n{text}"),
        None => text,
    }
}

#[derive(Clone)]
pub(crate) struct Funes {
    /// The memory the server was started on (`funes mcp <memory>`), read by every call naming
    /// none; `None` leaves those calls to the host's binding, as on the CLI.
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

    /// What a call reads: the memory it names, as it stands; naming none, the server's.
    fn target(&self, spec: Option<String>) -> Result<Target> {
        match spec.filter(|s| !s.trim().is_empty()) {
            Some(spec) => Target::resolve(Some(&spec)),
            None => Target::served(self.memory.as_deref()),
        }
    }

    #[tool(
        description = "Semantic search over the user's past AI agent sessions: describe what you are after, get back the verbatim passages — what was decided, tried, measured or investigated. Call it when they refer to earlier work, or when you are about to re-derive something a session may already have settled. Call it too before claiming that something was never built, was dropped, or was never discussed: the code cannot show that, only the sessions can. Ranked top-k, so it gives you a foothold on a topic, not every session touching it. Ranking weighs relevance, not age: for the latest word on a topic, or one stretch of time, bound it with `since`/`until`. Each hit is a passage framed by its exchange, the prompt that asked, the assistant's last word on it and the user's reply, so the answer is usually in the reply itself: `k` is the token cost, `candidates` the reach, and a pool larger than `k` is reranked, at seconds of latency that grow with the pool, so the reach can grow without the reading. `oneline` lists the hits without their passages, to choose what to read."
    )]
    async fn recall(
        &self,
        Parameters(RecallRequest {
            query,
            k,
            oneline,
            candidates,
            block_type,
            role,
            harness,
            repo,
            since,
            until,
            memory,
        }): Parameters<RecallRequest>,
    ) -> String {
        noted(
            match async {
                recall::recall(
                    self.target(memory)?,
                    query,
                    k.unwrap_or(recall::DEFAULT_K),
                    candidates.unwrap_or(recall::DEFAULT_CANDIDATES),
                    recall::RecallFilter {
                        block_type,
                        role,
                        harness,
                        repo,
                        since,
                        until,
                    },
                    oneline.unwrap_or(false),
                )
                .await
            }
            .await
            {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => "no results".to_string(),
                Err(e) => format!("recall error: {e}"),
            },
        )
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
        noted(
            match async { recall::get(self.target(memory)?, session_id, range).await }.await {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => "no results".to_string(),
                Err(e) => format!("get error: {e}"),
            },
        )
    }

    #[tool(
        description = "Enumerate a memory's sessions, oldest first. Metadata only — when it started and when its last indexed turn was, harness, repo, turn count, id, and the prompt each opened with, which is the ask a session started from, not what it became. Filter by repo or date to get the session ids the other verbs take; a whole memory can run to thousands of rows."
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
        noted(
            match async { recall::sessions(self.target(memory)?, filter).await }.await {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => "no results".to_string(),
                Err(e) => format!("sessions error: {e}"),
            },
        )
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
        noted(
            match async {
                recall::scan(
                    self.target(memory)?,
                    needle,
                    session_id,
                    from,
                    to,
                    ignore_case.unwrap_or(false),
                    context.unwrap_or(recall::DEFAULT_CONTEXT),
                )
                .await
            }
            .await
            {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => "no results".to_string(),
                Err(e) => format!("scan error: {e}"),
            },
        )
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
        noted(
            match async { super::sketch::run(self.target(memory)?, session_id, from, to, units, max_chars).await }.await
            {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => "no results".to_string(),
                Err(e) => format!("sketch error: {e}"),
            },
        )
    }

    #[tool(
        description = "Health and size of a memory: indexed chunks, the languages its sessions are written in, local chunks awaiting embedding, source sessions awaiting indexing, and for a remote this host pushes to what it has yet to push. Call it when a read comes back empty or thinner than expected — it says whether the memory is the problem rather than the call."
    )]
    async fn status(&self, Parameters(StatusRequest { memory }): Parameters<StatusRequest>) -> String {
        // No update check here: it needs the network, and the "update available" notice belongs
        // on the human-facing CLI `funes status`, not on this hot, otherwise-local tool path.
        noted(
            async { recall::status(self.target(memory)?).await }
                .await
                .unwrap_or_else(|e| format!("status error: {e}")),
        )
    }
}

#[tool_handler]
impl ServerHandler for Funes {
    fn get_info(&self) -> ServerConfig {
        let mut server_info = Implementation::default();
        server_info.name = "funes".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        let mut instructions = "Persistent memory over the user's past AI coding sessions: their transcripts, \
                                indexed automatically as they work and read-only here — nothing has to be saved. \
                                When earlier work matters, this is the memory to consult."
            .to_string();
        if let Some(note) = agents::stale_install_notice() {
            instructions.push_str("\n\n");
            instructions.push_str(&note);
        }
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(instructions)
    }
}

pub async fn run(
    memory: Option<String>,
    socket: Option<Option<PathBuf>>,
    proxy: Option<Option<PathBuf>>,
) -> Result<()> {
    if let Some(path) = socket {
        return serve_socket(memory, path.unwrap_or_else(default_socket)).await;
    }
    if let Some(path) = proxy {
        return proxy_stdio(path.unwrap_or_else(default_socket)).await;
    }

    let service = Funes::new(memory).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// The socket a `--socket` or `--proxy` naming no path uses: beside the local memory, in the home a
/// host's agents and its index already share.
fn default_socket() -> PathBuf {
    home::dir().join("mcp.sock")
}

/// Serve on `path`: one MCP session per connection, all of them on the one server this process
/// made, and therefore on the one embedder and reranker it loads.
///
/// A path a server of this version already serves is left to it, and this returns: a launcher may
/// start-or-join with the same command, without a check of its own — or of the memory, which stays
/// the running server's. A server of another version is replaced: it stops taking agents once its
/// socket is gone, and exits when its last one leaves.
async fn serve_socket(memory: Option<String>, path: PathBuf) -> Result<()> {
    let listener = {
        // Probe, clear and bind as one step: two servers starting together would each find nothing
        // listening, and the second would unlink the first's fresh socket.
        let mut lock = path.clone().into_os_string();
        lock.push(".lock");
        let lock = PathBuf::from(lock);
        // Bounded: the lock file is another local user's to hold, and a launch that waited on it
        // forever would wedge with nothing on stderr.
        let _held = match tokio::time::timeout(Duration::from_secs(30), socket_lock(&lock)).await {
            Ok(held) => held?,
            Err(_) => bail!("another funes is starting on {} — retry", path.display()),
        };
        match server_version(&path).await {
            Some(v) if v == env!("CARGO_PKG_VERSION") => return Ok(()),
            Some(v) => eprintln!("funes mcp: replacing the funes {v} server on {}", path.display()),
            None => {}
        }
        match std::fs::symlink_metadata(&path) {
            // A socket nobody of this version answers on: a killed server's, or an older one's.
            Ok(m) if m.file_type().is_socket() => {
                std::fs::remove_file(&path).with_context(|| format!("removing the stale socket {}", path.display()))?
            }
            Ok(_) => bail!("{} is not a socket — name another path", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        // Agents run as the user whose memory this is; nobody else may talk to it, from the moment
        // the socket exists.
        // SAFETY: `umask` swaps a process attribute and cannot fail.
        let umask = unsafe { libc::umask(0o177) };
        let bound = UnixListener::bind(&path);
        unsafe { libc::umask(umask) };
        bound.with_context(|| format!("binding {}", path.display()))?
    };
    let ino = std::fs::symlink_metadata(&path)?.ino();
    eprintln!("funes mcp: listening on {}", path.display());

    let funes = Funes::new(memory);
    let mut sessions = JoinSet::new();
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let funes = funes.clone();
                    sessions.spawn(async move {
                        let (read, write) = stream.into_split();
                        match funes.serve(AsyncRwTransport::new_server(read, write)).await {
                            // Waiting on the session is what keeps it open: dropping the service ends it.
                            Ok(service) => {
                                let _ = service.waiting().await;
                            }
                            Err(e) => eprintln!("funes mcp: an agent could not start a session: {e}"),
                        }
                    });
                }
                // Out of descriptors, or a client gone before it was accepted: the agents already
                // connected keep their sessions, and the next accept may succeed.
                Err(e) => {
                    eprintln!("funes mcp: accepting an agent: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = sessions.join_next() => {}
            _ = check.tick() => {
                let ours = std::fs::symlink_metadata(&path).map(|m| m.ino()).ok() == Some(ino);
                if !ours {
                    break;
                }
            }
        }
    }
    // Replaced or removed: no agent can reach this server any more, so it serves out the ones it has.
    drop(listener);
    while sessions.join_next().await.is_some() {}
    Ok(())
}

/// `path`'s start lock, waiting for whoever holds it — another starter — to let go; the caller
/// bounds the wait.
async fn socket_lock(path: &Path) -> Result<FileLock> {
    loop {
        if let Some(held) = try_lock_file(path)? {
            return Ok(held);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The version of the funes server answering on `path`, from its reply to an `initialize`; `None`
/// if nothing answers.
async fn server_version(path: &Path) -> Option<String> {
    let probe = async {
        let mut stream = UnixStream::connect(path).await.ok()?;
        let hello = serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "funes-probe", "version": env!("CARGO_PKG_VERSION")}}});
        stream.write_all(format!("{hello}\n").as_bytes()).await.ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.ok()?;
        let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
        // A server too old to say which it is, is not this one.
        Some(
            reply["result"]["serverInfo"]["version"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
        )
    };
    tokio::time::timeout(Duration::from_secs(5), probe).await.ok().flatten()
}

/// Bridge this process's stdio to the server on `path`, a byte-for-byte relay: an agent whose
/// integration can only spawn a stdio server still reaches the shared one. The end of stdin ends the
/// requests, not the relay; the server closing ends it, once its last reply is out.
async fn proxy_stdio(path: PathBuf) -> Result<()> {
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {} — is a server on it?", path.display()))?;
    let (mut read, mut write) = stream.into_split();
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut write).await;
        // Dropping the write half shuts it down: the server sees the requests end and answers them.
        drop(write);
    });
    let mut stdout = tokio::io::stdout();
    let relayed = tokio::io::copy(&mut read, &mut stdout).await;
    let _ = stdout.flush().await;
    if let Err(e) = relayed {
        eprintln!("funes mcp: relaying from {}: {e}", path.display());
        std::process::exit(1);
    }
    // stdin is read on a blocking thread nothing can interrupt: dropping the runtime would wait for
    // the agent's next byte, so the relay exits here instead.
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_refuses_a_parameter_its_schema_does_not_list() {
        let err = serde_json::from_value::<RecallRequest>(serde_json::json!({"query": "x", "neighbors": 0}))
            .expect_err("a retired parameter must fail the call");
        assert!(err.to_string().contains("unknown field `neighbors`"), "got: {err}");
        let err = serde_json::from_value::<GetRequest>(serde_json::json!({"session_id": "s", "form": 3}))
            .expect_err("a misspelled parameter must fail the call");
        assert!(err.to_string().contains("unknown field `form`"), "got: {err}");
        serde_json::from_value::<RecallRequest>(serde_json::json!({"query": "x", "oneline": true}))
            .expect("a listed parameter parses");
    }
}
