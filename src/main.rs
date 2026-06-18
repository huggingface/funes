//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it
//! from `~/.claude/projects/**/*.jsonl`. Index location is `$FUNES_DB` or `~/.funes`.

use funes::{index, mcp, recall};

use anyhow::Result;
use clap::{Parser, Subcommand};
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
    },
    /// List indexed sessions, newest activity first.
    List {
        /// Restrict to a project.
        #[arg(long)]
        project: Option<String>,
        /// Max sessions to show.
        #[arg(long, default_value_t = 50)]
        limit: usize,
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
    Status,
    /// Run as an MCP server over stdio (for Claude Code, Cursor, …).
    Mcp,
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
        } => {
            print!(
                "{}",
                recall::recall(
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
        Cmd::List { project, limit } => {
            print!("{}", recall::list(project, limit).await?);
            Ok(())
        }
        Cmd::Get {
            session_id,
            turn_uuid,
            window,
        } => {
            print!("{}", recall::get(session_id, turn_uuid, window).await?);
            Ok(())
        }
        Cmd::Index { source, no_thinking } => {
            let src = source.unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_default();
                PathBuf::from(home).join(".claude").join("projects")
            });
            index::run_index(&src, no_thinking).await
        }
        Cmd::Status => {
            print!("{}", recall::status().await?);
            Ok(())
        }
        Cmd::Mcp => mcp::run().await,
    }
}
