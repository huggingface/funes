//! funes — recall over your past AI Agent sessions.
//!
//! The CLI: its definitions, and the dispatch that parses a command line, calls one command and
//! prints what comes back. `recall` reads the index (hybrid search, reranked for a deep pool);
//! `index` builds/updates it from the integrations' spools or a turns file. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::agents;
use funes::commands::target::Target;
use funes::commands::{add, ask, bind, compact, index, mcp, push, recall, scrub, sketch, update};
use funes::hub;
use funes::memory;
use funes::ui::banner::Banner;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::io::Write;

#[derive(Parser)]
#[command(name = "funes", version, about = "Recall over your past AI agent sessions.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Recall passages from past sessions (hybrid → neighbors, reranked when --candidates is several times -k).
    Recall {
        /// What to recall (free text).
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// How many results to show. Each is a passage to read.
        #[arg(short, long, default_value_t = recall::DEFAULT_K)]
        k: usize,
        /// Size of the fused pool the hits are cut from. At four times -k or more, the pool is
        /// reranked by the cross-encoder before the cut (seconds per call). Under that, the hits
        /// are the first -k of the fused order.
        #[arg(long, default_value_t = recall::DEFAULT_CANDIDATES)]
        candidates: usize,
        /// Adjacent chunks (within this seq window) to attach to each hit. 0 disables.
        #[arg(long, default_value_t = recall::DEFAULT_NEIGHBORS)]
        neighbors: i64,
        /// Restrict to a block type: text | thinking | tool_use | tool_result.
        #[arg(long = "type", value_name = "BLOCK_TYPE")]
        block_type: Option<String>,
        /// Restrict to a role, as the turns carry it (an agent's turns say user | assistant | tool |
        /// system).
        #[arg(long)]
        role: Option<String>,
        /// Restrict to a harness facet, as the turns carry it (`claude` also matches the older
        /// `claude_code`).
        #[arg(long)]
        harness: Option<String>,
        /// Restrict to turns whose checkout resolved to this repo (`owner/name`).
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,
        /// Restrict to turns on or after this date (`YYYY-MM-DD`).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Restrict to turns on or before this date (`YYYY-MM-DD`).
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// Read a range of a session's turns, addressed by the session's own seq.
    Get {
        /// Session id (from a recall hit's `→ get` line).
        session_id: String,
        /// First turn to read, as the session's own seq. Defaults to the session's start.
        #[arg(long, value_name = "SEQ")]
        from: Option<i64>,
        #[arg(long, value_name = "SEQ", help = format!("Last turn to read, as the session's own seq. Defaults to {} turns from --from.", recall::DEFAULT_SPAN))]
        to: Option<i64>,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// Ask a coding agent one question, grounded in a memory — nothing installed.
    ///
    /// Borrows the agent for a single answer: funes recalls from the memory and hands the agent
    /// the passages in its prompt, so the answer comes back in one turn. Name a memory with
    /// --memory to ask against any published one; omit it for the memory this host is bound to.
    #[command(
        subcommand_value_name = "AGENT",
        subcommand_help_heading = "Agents",
        override_usage = "funes ask <AGENT> <QUESTION>... [--memory MEMORY]"
    )]
    Ask {
        #[command(subcommand)]
        agent: AskAgent,
    },
    /// Build or update your local memory from session transcripts, then compact it.
    Index {
        /// A `.funes.jsonl` turns file, or a directory of them. Omit — in a terminal — to index
        /// every integration's spool (~/.funes/spool/<id>); `--harness <id>` alone targets one. An
        /// automated (non-terminal) run must name a target.
        path: Option<String>,
        /// Index only this integration's spool, ~/.funes/spool/<id>. Refused with a PATH, whose
        /// turns name their own harness.
        #[arg(long, value_name = "ID")]
        harness: Option<String>,
        /// Validate PATH without indexing it: parse, count turns and chunks, report rejected files
        /// and duplicate ids; write nothing. Exits non-zero if a unit was rejected.
        #[arg(long, requires = "path")]
        check: bool,
        /// Exclude thinking blocks. A spool file indexed this way is kept, its thinking still owed.
        #[arg(long)]
        no_thinking: bool,
        /// Index only the most recent N sessions per source. Omit to index all.
        #[arg(long)]
        limit: Option<usize>,
        /// Don't ask: a budgeted (no-path) run finishes all remaining work instead of offering it;
        /// an explicit path skips the first-index size confirmation.
        #[arg(long)]
        yes: bool,
        /// Create the local memory with the multilingual embedding model, for sessions not in English.
        /// A memory keeps the model it was created with, so an existing English memory refuses it.
        #[arg(long, conflicts_with = "check")]
        multilingual: bool,
    },
    /// Find a literal string everywhere in one session — exhaustive, unranked.
    Scan {
        /// The literal to find. Not a regex.
        #[arg(value_name = "NEEDLE")]
        needle: String,
        /// The session to scan (from a `sessions` row or a recall hit's `→ get` line).
        #[arg(value_name = "SESSION_ID")]
        session_id: String,
        /// First turn to scan, as the session's own seq. Defaults to the session's start.
        #[arg(long, value_name = "SEQ")]
        from: Option<i64>,
        /// Last turn to scan, as the session's own seq. Defaults to the session's end.
        #[arg(long, value_name = "SEQ")]
        to: Option<i64>,
        /// Match regardless of case.
        #[arg(short, long)]
        ignore_case: bool,
        /// Characters of surrounding text to show on each side of a match.
        #[arg(long, default_value_t = recall::DEFAULT_CONTEXT)]
        context: usize,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// List a memory's sessions, oldest first.
    Sessions {
        /// Keep only sessions whose checkout resolved to this repo (`owner/name`).
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,
        /// Keep only sessions that started on or after this date (`YYYY-MM-DD`).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
        /// Keep only sessions that started on or before this date (`YYYY-MM-DD`).
        #[arg(long, value_name = "DATE")]
        until: Option<String>,
        #[arg(long, value_name = "N", help = format!("Rows to list, keeping the most recent. Defaults to {}, capped at {} — walk with --offset for more. Zero is an error.", recall::SESSIONS_LIMIT, recall::SESSIONS_LIMIT_MAX))]
        limit: Option<usize>,
        /// Skip this many of the most recent matches before taking --limit, to walk back in time.
        #[arg(long, value_name = "N", default_value_t = 0)]
        offset: usize,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// Digest one session: the passages most distinctive within it, chosen without a query.
    Sketch {
        /// Session to digest (from a `sessions` row or a hit's `→ get` line).
        session_id: String,
        /// How many distinct places to show. Clamped to 40, and to what --max-chars can render.
        #[arg(long, value_name = "N")]
        units: Option<usize>,
        /// Total characters to render. Clamped to 40000.
        #[arg(long, value_name = "N")]
        max_chars: Option<usize>,
        /// First turn to digest, as the session's own seq. Defaults to the session's start.
        #[arg(long, value_name = "SEQ")]
        from: Option<i64>,
        /// Last turn to digest, as the session's own seq. Defaults to the session's end.
        #[arg(long, value_name = "SEQ")]
        to: Option<i64>,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// Show the local index, and the memory this host is bound to with what this host has yet to
    /// push there.
    Status,
    /// Publish your local memory's new embedded chunks to a remote memory on the HF Hub.
    Push {
        /// Memory to publish to: `<org>/<repo>` or a full `hf://…` URI.
        #[arg(value_name = "MEMORY")]
        memory: String,
        /// Skip the confirmation when the target holds none of the chunks to publish.
        #[arg(short, long)]
        yes: bool,
        /// Compact the remote after pushing (retrying on conflict) even if the unindexed backlog is
        /// below the auto-compaction threshold. With nothing new to push, compact only.
        #[arg(long, alias = "force-reindex")]
        compact: bool,
        /// Publish embedded chunks from these sessions. Omit to publish every embedded chunk the
        /// remote does not already hold.
        #[arg(long, value_name = "SESSION")]
        sessions: Vec<String>,
    },
    /// Compact a memory: merge its small fragments, add its unindexed rows to the search indexes, and
    /// delete its old versions.
    ///
    /// `funes index` and `funes push` also compact on their own.
    Compact {
        /// Memory to compact: a local path, `<org>/<repo>`, or a full `hf://…` URI. Omit for your
        /// local memory.
        #[arg(value_name = "MEMORY")]
        memory: Option<String>,
    },
    /// Redact secrets from your local memory in place — for rows indexed before redaction existed (or
    /// flagged by an updated ruleset); needs no source transcript. Cleans the local memory only: it
    /// does NOT scrub an already-published remote, which the push gate can only stop adding to.
    Scrub,
    /// Update funes in place: download the latest release binary for this platform and replace the
    /// running executable, then bring the installed agent integrations to their newest release.
    Update {
        /// Reinstall the latest binary even if this build is already up to date.
        #[arg(short, long)]
        force: bool,
    },
    /// Run as an MCP server over stdio (for Claude Code, Cursor, …).
    Mcp {
        /// Memory to serve — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
        /// Defaults to the memory this host is bound to, else your local memory.
        #[arg(value_name = "MEMORY")]
        memory: Option<String>,
    },
    /// Bind this host to a memory: every installed agent recalls from it and publishes to it.
    ///
    /// Re-runs each installed agent's setup on the memory, and does the first push when this host
    /// has a local memory it has never published there. `local` unbinds.
    Bind {
        /// The memory — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
        #[arg(value_name = "MEMORY")]
        memory: String,
    },
    /// Add funes to a coding agent.
    ///
    /// Installs funes's read tools and automatic per-turn indexing, on the memory this host is
    /// bound to. Naming a memory binds the host to it first, as `funes bind` does.
    Add {
        /// Agent to add: `claude`, `codex`, `pi`, `hermes`, or any other registered agent.
        #[arg(value_name = "AGENT")]
        agent: String,
        #[command(flatten)]
        memory: AddMemory,
        /// Install the integration from here rather than from the catalog: a directory
        /// holding it, or an `hf://buckets/<owner>/<bucket>/<path>/<id>.tar.gz` archive with a
        /// `SHA256SUMS` beside it.
        #[arg(long, value_name = "DIR|URL")]
        from: Option<String>,
        /// Answer yes to the add's own question — indexing the agent's existing sessions on a
        /// first add — so it runs unattended. It does not confirm an integration funes can't vouch
        /// for; that takes `--trust`.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Confirm an integration funes can't vouch for — one from `--from`, or an installed copy
        /// changed since funes installed it — without a terminal, as answering yes to "Trust it?"
        /// does. Its setup runs as you; pass this only for files you have checked.
        #[arg(long)]
        trust: bool,
    },
    /// Remove funes from a coding agent.
    ///
    /// Unregisters funes's read tools and removes its automation and integration files. Your local
    /// memory, source transcripts, and remote memories are left untouched.
    Remove {
        /// Agent to remove: `claude`, `codex`, `pi`, `hermes`, or any other registered agent.
        #[arg(value_name = "AGENT")]
        agent: String,
    },
}

// The `[MEMORY]` positional of `add`; the user-facing help comes from the field doc below.
#[derive(Args)]
struct AddMemory {
    /// Memory to bind this host to first, as `funes bind` does — `<org>/<repo>`, an `hf://…` URI, or
    /// `local`. Omitted, the agent takes the memory the host is bound to.
    #[arg(value_name = "MEMORY")]
    memory: Option<String>,
}

// Flattened into every ask agent so they share the question positional and the read `--memory`
// flag; the user-facing help comes from the field docs.
#[derive(Args)]
struct AskArgs {
    /// The question to answer (free text).
    #[arg(required = true, num_args = 1..)]
    question: Vec<String>,
    #[command(flatten)]
    memory: MemoryOpts,
}

#[derive(Subcommand)]
enum AskAgent {
    Claude {
        #[command(flatten)]
        args: AskArgs,
    },
    Codex {
        #[command(flatten)]
        args: AskArgs,
    },
}

/// Which memory a read verb reads. Shared by every verb that takes `--memory`.
#[derive(Args)]
struct MemoryOpts {
    /// The memory to read, as it stands — an `<org>/<repo>` shorthand, an `hf://…` URI, a local
    /// path, or `local`. Omitted, the memory this host is bound to, with this host's turns not yet
    /// pushed to it; your local memory when the host is local.
    #[arg(long = "memory", value_name = "MEMORY")]
    memory: Option<String>,
}

impl MemoryOpts {
    fn resolve(self) -> Result<Target> {
        Target::resolve(self.memory.as_deref())
    }
}

fn main() -> Result<()> {
    hub::configure_user_agent_origin();
    tokio::runtime::Runtime::new()?.block_on(run())
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    // A read is where a user of an install that stopped capturing lands, so it carries the line
    // that says so — on stderr, so stdout stays the agent-format text the MCP tools return.
    if matches!(
        cli.cmd,
        Cmd::Recall { .. }
            | Cmd::Get { .. }
            | Cmd::Sessions { .. }
            | Cmd::Scan { .. }
            | Cmd::Sketch { .. }
            | Cmd::Status
            | Cmd::Ask { .. }
    ) {
        if let Some(note) = agents::stale_install_notice() {
            eprint!("{note}");
        }
    }
    match cli.cmd {
        Cmd::Recall {
            query,
            k,
            candidates,
            neighbors,
            block_type,
            role,
            harness,
            repo,
            since,
            until,
            memory,
        } => {
            let memory = memory.resolve()?;
            let query = query.join(" ");
            let spinner = Banner::spinner("recalling…");
            let (note, hits) = recall::recall_hits(
                memory,
                query,
                k,
                candidates,
                neighbors,
                recall::RecallFilter {
                    block_type,
                    role,
                    harness,
                    repo,
                    since,
                    until,
                },
                &|label| spinner.set(label),
            )
            .await?;
            drop(spinner);
            print!("{}", recall::rendered(&note, &hits));
            Ok(())
        }
        Cmd::Scan {
            needle,
            session_id,
            from,
            to,
            ignore_case,
            context,
            memory,
        } => {
            print!(
                "{}",
                recall::scan(memory.resolve()?, needle, session_id, from, to, ignore_case, context).await?
            );
            Ok(())
        }
        Cmd::Sessions {
            repo,
            since,
            until,
            limit,
            offset,
            memory,
        } => {
            let filter = recall::SessionFilter {
                repo,
                since,
                until,
                limit,
                offset,
            };
            print!("{}", recall::sessions(memory.resolve()?, filter).await?);
            Ok(())
        }
        Cmd::Sketch {
            session_id,
            units,
            max_chars,
            from,
            to,
            memory,
        } => {
            print!(
                "{}",
                sketch::run(memory.resolve()?, session_id, from, to, units, max_chars).await?
            );
            Ok(())
        }
        Cmd::Get {
            session_id,
            from,
            to,
            memory,
        } => {
            let range = recall::TurnRange { from, to };
            print!("{}", recall::get(memory.resolve()?, session_id, range).await?);
            Ok(())
        }
        Cmd::Ask { agent } => match agent {
            AskAgent::Claude { args } => ask::claude(args.question.join(" "), args.memory.resolve()?).await,
            AskAgent::Codex { args } => ask::codex(args.question.join(" "), args.memory.resolve()?).await,
        },
        Cmd::Index {
            path,
            harness,
            check,
            no_thinking,
            limit,
            yes,
            multilingual,
        } => {
            index::run(index::IndexArgs {
                path,
                harness,
                check,
                no_thinking,
                limit,
                yes,
                multilingual,
            })
            .await
        }
        Cmd::Status => {
            let bound = memory::binding()?;
            let note = bind::legacy_note(bound.as_deref())?;
            print!("{}", recall::host_status(bound.as_deref(), note.as_deref()).await?);
            // Show the status body before the (bounded, best-effort) update check, so a slow or
            // offline Hub can't delay the useful output.
            std::io::stdout().flush().ok();
            if let Some(notice) = update::upgrade_notice().await {
                print!("{notice}");
            }
            Ok(())
        }
        Cmd::Push {
            memory,
            yes,
            compact,
            sessions,
        } => {
            let pushed = push::run(&memory, compact, yes, &sessions).await?;
            print!("{}", pushed.report);
            // Secrets held back everything — a non-zero exit, so automation can react.
            if pushed.blocked {
                std::process::exit(2);
            }
            Ok(())
        }
        Cmd::Compact { memory } => {
            print!("{}", compact::run(memory::Memory::resolve(memory)).await?);
            Ok(())
        }
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update::run(force).await,
        Cmd::Mcp { memory } => mcp::run(memory).await,
        Cmd::Bind { memory } => bind::run(&memory).await,
        Cmd::Add {
            agent,
            memory,
            from,
            yes,
            trust,
        } => {
            add::run(
                &agent,
                memory.memory.as_deref(),
                from.as_deref(),
                add::Unattended { yes, trust },
            )
            .await
        }
        Cmd::Remove { agent } => add::remove(&agent).await,
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Cmd};
    use clap::Parser;

    #[test]
    fn push_still_takes_force_reindex() {
        for flag in ["--compact", "--force-reindex"] {
            let cli = Cli::try_parse_from(["funes", "push", "org/memory", flag]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Push { compact: true, .. }), "{flag}");
        }
    }
}
