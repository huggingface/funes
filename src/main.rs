//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it
//! from `~/.claude/projects/**/*.jsonl`. funes's home is `$FUNES_HOME` or `~/.funes`.

use funes::{config, hub, index, mcp, push, recall};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "funes", version, about = "Recall over your past AI agent sessions.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Recall passages from past sessions (hybrid → rerank → recency → neighbors).
    Recall {
        /// What to recall (free text).
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// How many results to show.
        #[arg(short, long, default_value_t = 8)]
        k: usize,
        /// How many fused candidates to rerank.
        #[arg(long, default_value_t = 30)]
        candidates: usize,
        /// Recency half-life in days (a hit this old keeps half its weight). 0 disables.
        #[arg(long, default_value_t = 30.0)]
        half_life: f64,
        /// Adjacent chunks (within this seq window) to attach to each hit. 0 disables.
        #[arg(long, default_value_t = 1)]
        neighbors: i64,
        /// Restrict to a block type: text | thinking | tool_use | tool_result.
        #[arg(long = "type", value_name = "BLOCK_TYPE")]
        block_type: Option<String>,
        /// Restrict to a project (the path segment under `projects`).
        #[arg(long)]
        project: Option<String>,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// List indexed sessions, newest activity first.
    List {
        /// Restrict to a project.
        #[arg(long)]
        project: Option<String>,
        /// Max sessions to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Drill down on a recall hit: a turn plus the turns around it, reassembled.
    Get {
        /// Session id (from a recall hit's `→ get` line).
        session_id: String,
        /// Turn uuid (from a recall hit's `→ get` line).
        turn_uuid: String,
        /// Turns within this seq window of the target are included.
        #[arg(long, default_value_t = 3)]
        window: i64,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Build or update the index from session transcripts.
    Index {
        /// Source directory of transcripts (default: ~/.claude/projects).
        source: Option<PathBuf>,
        /// Exclude thinking blocks.
        #[arg(long)]
        no_thinking: bool,
    },
    /// Show index statistics.
    Status {
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Set the active store: the remote this host reads from and publishes to (persisted in
    /// funes.json; honored by recall/push and the MCP server).
    Use {
        /// Remote to attach: `<org>/<repo>` (→ `hf://datasets/<org>/<repo>`) or a full `hf://…`
        /// URI. Pass `local` to detach and go back to the local index.
        store: String,
    },
    /// Re-publish the local index's new chunks to the active remote (manual; `index` already
    /// publishes automatically when a remote is attached).
    Push {
        /// Refresh the remote index after pushing (retrying on conflict) even if the unindexed
        /// backlog is below the auto-reindex threshold. With nothing new to push, reindex only.
        #[arg(long)]
        force_reindex: bool,
    },
    /// Run as an MCP server over stdio (for Claude Code, Cursor, …).
    Mcp,
}

/// Which store the read commands act on. Shared by `recall`/`list`/`get`/`status`.
#[derive(Args)]
struct StoreOpts {
    /// A remote to read for this call — an `<org>/<repo>` shorthand or `hf://…` URI — overriding
    /// the active store. Defaults to the active store, else the local index.
    #[arg(long)]
    remote: Option<String>,
}

impl StoreOpts {
    fn resolve(self) -> hub::Store {
        hub::Store::resolve(self.remote)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Recall {
            query,
            k,
            candidates,
            half_life,
            neighbors,
            block_type,
            project,
            store,
        } => {
            print!(
                "{}",
                recall::recall(
                    store.resolve(),
                    query.join(" "),
                    k,
                    candidates,
                    half_life,
                    neighbors,
                    block_type,
                    project
                )
                .await?
            );
            Ok(())
        }
        Cmd::List { project, limit, store } => {
            print!("{}", recall::list(store.resolve(), project, limit).await?);
            Ok(())
        }
        Cmd::Get {
            session_id,
            turn_uuid,
            window,
            store,
        } => {
            print!("{}", recall::get(store.resolve(), session_id, turn_uuid, window).await?);
            Ok(())
        }
        Cmd::Index { source, no_thinking } => {
            let dir = source.unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".claude").join("projects")
            });
            index::run_index(&dir, no_thinking).await
        }
        Cmd::Status { store } => {
            print!("{}", recall::status(store.resolve()).await?);
            Ok(())
        }
        Cmd::Use { store } => use_store(store),
        Cmd::Push { force_reindex } => {
            let remote = config::load()
                .remote
                .ok_or_else(|| anyhow!("no active remote — attach one with `funes use <org>/<repo>`"))?;
            print!("{}", push::run_push(hub::Store::parse(&remote), force_reindex).await?);
            Ok(())
        }
        Cmd::Mcp => mcp::run().await,
    }
}

/// `funes use <store>`: attach a remote (or `local` to detach), persisted in funes.json.
fn use_store(spec: String) -> Result<()> {
    let mut cfg = config::load();
    if spec == "local" {
        cfg.remote = None;
        config::save(&cfg)?;
        println!("active store: local index");
        return Ok(());
    }
    match hub::Store::parse(&spec) {
        hub::Store::Remote { uri } => {
            println!("active store: {uri}");
            cfg.remote = Some(uri);
            config::save(&cfg)
        }
        hub::Store::Local { .. } => Err(anyhow!(
            "`funes use` takes a remote (e.g. `acme/kb` or `hf://datasets/<org>/<repo>`); for the \
             local index use `funes use local`, or relocate funes's home with $FUNES_HOME"
        )),
    }
}
