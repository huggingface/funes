//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid search, reranked for a deep pool); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::agents::{self, registry};
use funes::commands::target::Target;
use funes::commands::{ask, compact, index, mcp, push, recall, scrub, sketch, update};
use funes::hub;
use funes::inference::EmbeddingModel;
use funes::memory;
use funes::traces::spool;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
    /// --memory to ask against any published one; omit it for your local memory.
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
        /// A `.funes.jsonl` turns file (or a directory of them), a `.parquet` file, or a Hub
        /// trace repo `<org>/<repo>`. Omit — in a terminal — to index every integration's spool
        /// (~/.funes/spool/<id>); `--harness <id>` alone targets one. An automated (non-terminal)
        /// run must name a target.
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
    /// Show the local index, and each memory your agents are bound to with what this host has yet
    /// to push there.
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
    /// Idempotent — `--force` reinstalls the binary even when already up to date.
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

// Flattened into every agent so they share one optional `[MEMORY]` positional; the user-facing help
// comes from the field doc below.
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

/// Which memory the read commands act on. Shared by `recall`/`get`/`status`/`ask` and `mcp`.
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
            since,
            until,
            memory,
        } => {
            let memory = memory.resolve()?;
            let query = query.join(" ");
            let spinner = Spinner::start("recalling…");
            let progress = |label: &str| {
                if let Some(s) = &spinner {
                    s.set(label);
                }
            };
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
                    since,
                    until,
                },
                &progress,
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
            // `--harness` selects a spool; a path's turns name their own harness.
            if let (Some(p), Some(_)) = (&path, &harness) {
                return Err(anyhow!(
                    "`--harness` selects an integration's spool and does not apply to {p}: a turns file names its own harness"
                ));
            }
            if check {
                let path = path.expect("clap requires PATH with --check");
                let report = index::check(&PathBuf::from(&path), no_thinking, limit)?;
                print!("{}", report.text);
                if !report.all_accepted() {
                    return Err(anyhow!("{} unit(s) rejected", report.rejected));
                }
                return Ok(());
            }
            // A spool refresh (no explicit path — the per-turn hook and the terminal "keep me
            // fresh" case) is budgeted and rows-first; an explicit path or Hub repo is indexed in
            // full.
            let budgeted = path.is_none();
            let roots: Vec<PathBuf> = match (path, harness) {
                // An existing local path wins over reading the same string as a repo ref.
                (Some(p), _) if PathBuf::from(&p).exists() => vec![PathBuf::from(p)],
                (Some(p), _) if p.starts_with("hf://") || hub::is_remote_shorthand(&p) => {
                    // A Hub trace dataset: resolve to `hf://datasets/<owner>/<name>` and index its
                    // auto-converted parquet.
                    let memory::Memory::Remote { uri } = memory::Memory::parse(&p) else {
                        return Err(anyhow!("expected a Hub repo, got {p:?}"));
                    };
                    if multilingual {
                        index::ensure_local_memory(memory::dataset::model::MULTILINGUAL).await?;
                    }
                    return index::run_index_remote(&uri, no_thinking).await;
                }
                (Some(p), _) => return Err(anyhow!("no such path: {p}")),
                (None, Some(id)) => match spool::select(&id) {
                    Ok(dir) => vec![dir],
                    Err(e) => {
                        // Off a terminal this is a hook, and a hook asking for a spool nothing
                        // writes is an install older than the spool: leave the stamp the read
                        // verbs report. At a terminal the error itself is read, and a typo must
                        // not leave one.
                        if !std::io::stdin().is_terminal() && spool::is_id(&id) {
                            spool::note_missing(&id)?;
                        }
                        return Err(e);
                    }
                },
                // No target at all: index every spool — but only in a terminal. An automated run
                // (no TTY) must name a target, so a session-end hook indexes just its own spool — a
                // Claude session-end shouldn't pull in Codex or pi sessions.
                (None, None) => {
                    if !std::io::stdin().is_terminal() {
                        return Err(anyhow!(
                            "automated `funes index` needs a target — pass a path or `--harness <id>`; \
                             refusing to index every spool unattended"
                        ));
                    }
                    spool::spools()
                }
            };
            if multilingual {
                index::ensure_local_memory(memory::dataset::model::MULTILINGUAL).await?;
            }
            if roots.is_empty() {
                println!(
                    "no agent converts its sessions here yet — `funes add <agent>` sets that up, \
                     and funes indexes what lands in {}.",
                    spool::spool_root().display()
                );
                return Ok(());
            }
            if budgeted {
                index::run_index_budgeted(&roots, no_thinking, limit, yes).await
            } else {
                index::run_index_roots(&roots, no_thinking, limit, yes).await
            }
        }
        Cmd::Status => {
            let bound = memory::binding()?;
            let note = match bound {
                Some(_) => None,
                None => match legacy_bindings(&registry::default_root()?)?.as_slice() {
                    [] => None,
                    [(spec, ids)] => Some(format!(
                        "{} bound to {spec} by an older funes; `funes update` binds this host to it",
                        are(ids)
                    )),
                    several => Some(disagreement(several)),
                },
            };
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
            memory: remote,
            yes,
            compact,
            sessions,
        } => {
            let confirm = if yes {
                push::Confirm::Yes
            } else {
                push::Confirm::Ask(prompt_new_memory)
            };
            match push::run_push(memory::Memory::parse(&remote), compact, confirm, &sessions).await {
                Ok(pushed) => {
                    print!("{}", pushed.report);
                    // Secrets held back everything — surface a non-zero exit so automation can react.
                    if pushed.blocked {
                        std::process::exit(2);
                    }
                    Ok(())
                }
                Err(e) if push::is_read_only(&e) => Err(anyhow!(
                    "{remote} is read-only for your token — recall can read it, but publishing needs write access (check your HF token)"
                )),
                Err(e) => Err(e),
            }
        }
        Cmd::Compact { memory } => {
            let target = memory::Memory::resolve(memory);
            let label = target.label();
            match compact::run(target).await {
                Ok(report) => {
                    print!("{report}");
                    Ok(())
                }
                Err(e) if push::is_read_only(&e) => Err(anyhow!(
                    "{label} is read-only for your token — compacting it needs write access (check your HF token)"
                )),
                Err(e) => Err(e),
            }
        }
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update_funes(force).await,
        Cmd::Mcp { memory } => mcp::run(memory).await,
        Cmd::Bind { memory } => bind_memory(&memory).await,
        // `add` bootstraps the local pipeline: build the first index and do the first push — the
        // two one-time steps the automation can't do unattended — so nothing is left to run by hand.
        Cmd::Add { agent, memory, from } => add_agent(&agent, memory, from.as_deref()).await,
        Cmd::Remove { agent } => remove_agent(&agent).await,
    }
}

/// Resolve `id`'s integration and the memory, and run its `setup add`. The registry's manifest
/// says what `setup` last installed, and a refresh writes the new one first, so a run that stops
/// short of `setup add` — a memory that does not resolve, a first index declined — puts the
/// previous manifest back: what the agent runs is still the old install, and every read must keep
/// saying so. A first install that stops there takes its files away instead: nothing ran them,
/// so they are not an installed copy for the next run to ask about.
async fn add_agent(id: &str, memory: AddMemory, from: Option<&str>) -> Result<()> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let root = registry::default_root()?;
    let previous = registry::installed_manifest(&root, id);
    let fresh = !root.join(id).is_dir();
    let installed = std::cell::Cell::new(false);
    let ran = &installed;
    let result = async {
        let refresh = fresh || from.is_some() || registry::refreshes_on_every_run(&root, id);
        let (integration, provisioned) = prepare_agent(id, refresh, from).await?;
        // Naming a memory binds the host to it, as `funes bind` does. Naming none, the agent takes
        // the host's.
        let (resolved, binds) = match memory.memory.as_deref() {
            Some(spec) => (resolve_named(spec).await?, true),
            None => adopt_host_memory(&root, id).await?,
        };
        let record = provisioned.map(|(origin, files)| registry::Installed::new(&integration.manifest, origin, files));
        let registry_root = &root;
        let install = |memory: Option<String>| async move {
            // Counted as run before it runs: a setup that fails part-way may have wired some of
            // the agent to these files, and `remove` must find them recorded to run unasked.
            ran.set(true);
            setup_add(&integration, memory.as_deref())?;
            // The note an older funes kept beside the agent is obsolete once its setup ran on the
            // host's memory; left, a later bare add would bind the host to it again.
            registry::bind(registry_root, id, None)
        };
        let outcome = bootstrap_add(id, resolved, binds, install).await;
        // Recorded once setup has run, whatever came after: a first push that failed leaves the
        // new files installed, and the record must say so.
        if ran.get() {
            if let Some(record) = record {
                registry::record(&root, &record)?;
            }
        }
        outcome
    }
    .await;
    if !installed.get() {
        if fresh {
            registry::discard(&root, id)?;
        } else if let Some(previous) = previous {
            registry::restore_manifest(&root, id, &previous)?;
        }
    }
    result
}

/// Run `integration`'s `setup add`, bound to `memory`.
fn setup_add(integration: &registry::Integration, memory: Option<&str>) -> Result<()> {
    integration.add(memory)?;
    // Whatever an older hook asked for, this install's hooks are the ones that ask now.
    spool::forget_missing(&integration.manifest.id)
}

/// Resolve `id`'s integration for a run and confirm it when funes can't vouch for it — all before
/// `add` touches a memory or `remove` runs anything. With `refresh`, its files are fetched into
/// the registry first, so the script funes executes is the one it just wrote — unless the source
/// holds the release already installed; a source that can't be reached leaves the installed copy
/// to run. Without, the installed copy runs as installed. Says what it refreshed the files with,
/// when it did.
async fn prepare_agent(
    id: &str,
    refresh: bool,
    from: Option<&str>,
) -> Result<(registry::Integration, Option<(registry::Origin, registry::Files)>)> {
    let root = registry::default_root()?;
    // Decided before the refresh: a first install that fails part-way is not an installed copy.
    let installed = root.join(id).is_dir();
    let mut provisioned = None;
    let provenance = if refresh {
        match registry::provision(&root, id, from).await {
            Ok(None) => None,
            Ok(Some(registry::Provisioned {
                provenance,
                origin,
                files,
            })) => {
                // Files confirmed once, from the same source, unchanged since: confirmed still.
                let provenance = match provenance {
                    registry::Provenance::Unvouched(_) if registry::is_recorded(&root, id, &origin, &files) => {
                        registry::Provenance::Vouched
                    }
                    provenance => provenance,
                };
                provisioned = Some((origin, files));
                Some(provenance)
            }
            // Another publisher's files are not a refresh the installed copy stands in for.
            Err(e) if e.downcast_ref::<registry::Takeover>().is_some() => return Err(e),
            Err(e) if installed => {
                // What failed and why, without the layers between: a request error names its URL
                // at every one.
                let why = match e.chain().count() {
                    1 => e.to_string(),
                    _ => format!("{e}: {}", e.root_cause()),
                };
                eprintln!("note: the {id} integration could not be refreshed ({why}) — running the installed copy.");
                None
            }
            Err(e) => {
                let _ = registry::discard(&root, id);
                return Err(if e.downcast_ref::<registry::Absent>().is_some() {
                    unknown_agent(&root, id, e)
                } else {
                    e
                });
            }
        }
    } else {
        None
    };
    let provenance = match provenance {
        Some(provenance) => provenance,
        None => installed_provenance(&root, id)?,
    };
    let integration = registry::open(&root, id)?;
    confirm_trust(&integration, provenance)?;
    Ok((integration, provisioned))
}

/// Whether funes vouches for the copy installed at `root/<id>`: it does for files it recorded
/// installing and finds as it left them. A changed file, or no record, is confirmed like any
/// other files it did not write.
fn installed_provenance(root: &Path, id: &str) -> Result<registry::Provenance> {
    Ok(match registry::verify_installed(root, id)? {
        registry::Verification::Intact => registry::Provenance::Vouched,
        registry::Verification::Changed(files) => registry::Provenance::Unvouched(format!(
            "the installed copy, whose {} changed since funes installed it",
            files.join(", ")
        )),
        registry::Verification::Unrecorded => {
            registry::Provenance::Unvouched("the installed copy, recorded by nothing".to_string())
        }
    })
}

/// A failed provision for an agent with no files on this machine is usually a typo, so the error
/// names what is installed and where another integration comes from.
fn unknown_agent(root: &Path, id: &str, e: anyhow::Error) -> anyhow::Error {
    let installed = registry::registered_ids(root);
    let listing = if installed.is_empty() {
        "none are installed yet".to_string()
    } else {
        format!("installed: {}", installed.join(", "))
    };
    e.context(format!(
        "no {id} integration on this machine ({listing}) — see docs/add.md for the maintained agents and how to add your own"
    ))
}

/// Confirm before funes executes an integration it does not vouch for, saying what it is — the
/// package by publisher and version — and where it came from. The default is no.
fn confirm_trust(integration: &registry::Integration, provenance: registry::Provenance) -> Result<()> {
    let registry::Provenance::Unvouched(origin) = provenance else {
        return Ok(());
    };
    let manifest = &integration.manifest;
    let id = &manifest.id;
    let package = format!(
        "{id}{} by {} ({})",
        manifest.version.as_ref().map(|v| format!(" {v}")).unwrap_or_default(),
        registry::publisher(&manifest.repo),
        manifest.repo
    );
    if !std::io::stdin().is_terminal() {
        bail!(
            "the {id} integration at {} — {package} — comes from {origin}, which funes can't vouch \
             for — run this in a terminal to confirm it",
            integration.dir.display()
        );
    }
    if !confirm(
        &format!(
            "funes is about to run {}/setup — {package}, from {origin}. Trust it? [y/N] ",
            integration.dir.display()
        ),
        false,
    ) {
        bail!("{id} not confirmed — its setup was not run");
    }
    Ok(())
}

/// Run `id`'s `setup remove`, then delete its files. Nothing installed is the state `remove`
/// produces, so meeting it is a success, not an unknown agent. The installed copy is what runs —
/// fetched anew only when it speaks a contract this funes cannot run.
async fn remove_agent(id: &str) -> Result<()> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let root = registry::default_root()?;
    if !root.join(id).is_dir() {
        eprintln!("nothing to remove — no {id} integration is installed.");
        registry::discard(&root, id)?;
        return spool::forget_missing(id);
    }
    let (integration, _) = prepare_agent(id, registry::speaks_another_contract(&root, id), None).await?;
    integration.remove()?;
    registry::discard(&root, id)?;
    // The hooks that asked for the spool are gone with the integration, so the refusals they left
    // must not outlive it: `remove` takes the spool too, which would show them again.
    spool::forget_missing(id)
}

/// `funes update`: the binary, then every installed integration. A replaced binary is run to update
/// them, since its contract decides which releases they take; one this host cannot replace does not
/// hold them back.
async fn update_funes(force: bool) -> Result<()> {
    // First: an older release that just replaced its binary hands over to this `funes update`.
    update::wait_for_older_writers().await?;
    let binary = update::run(force).await;
    if let Ok(Some(exe)) = &binary {
        let e = Command::new(exe).arg("update").exec();
        return Err(anyhow::Error::new(e).context(format!("running {} to update the integrations", exe.display())));
    }
    let agents = update_agents().await;
    binary.and(agents)
}

/// Bring every installed integration forward.
async fn update_agents() -> Result<()> {
    let root = registry::default_root()?;
    // A host from before the binding is bound as its agents were; one that cannot be is still
    // brought forward, each agent bound as it was.
    match migrate_bindings(&root, None).await {
        Ok(Some(disagreement)) => eprintln!("note: {disagreement}"),
        Ok(None) => {}
        Err(e) => eprintln!("note: this host could not be bound as an older funes bound its agents: {e:#}"),
    }
    let mut failed = Vec::new();
    for id in registry::registered_ids(&root)
        .into_iter()
        .filter(|id| spool::is_id(id))
    {
        match update_agent(&root, &id).await {
            Ok(done) => println!("{done}"),
            Err(e) => {
                eprintln!("the {id} integration could not be updated: {e:#}");
                failed.push(id);
            }
        }
    }
    if !failed.is_empty() {
        bail!("not updated: {}", failed.join(", "));
    }
    Ok(())
}

/// Bring `id`'s install to what its source holds now and re-run its setup, bound as it was — only
/// when that changed its files. What it did, as a line to print.
async fn update_agent(root: &Path, id: &str) -> Result<String> {
    // Named with `--from`, an install stays as named: the one copy not refreshed on every run.
    let record = registry::installed(root, id);
    if let Some(record) = record.as_ref().filter(|_| !registry::refreshes_on_every_run(root, id)) {
        return Ok(format!(
            "The {id} integration stays as installed from {} until `funes add {id} --from` names its source again.",
            record.origin
        ));
    }
    let previous = registry::installed_manifest(root, id);
    let ran = std::cell::Cell::new(false);
    let result: Result<String> = async {
        let fetched = match registry::provision(root, id, None).await {
            // A copy nothing recorded, that its source does not list: nothing to bring it forward to.
            Err(e) if record.is_none() && e.downcast_ref::<registry::Absent>().is_some() => {
                return Ok(format!("The {id} integration stays as placed: {e}."));
            }
            fetched => fetched?,
        };
        let Some(provisioned) = fetched.filter(|p| !registry::is_recorded(root, id, &p.origin, &p.files)) else {
            return Ok(format!("The {id} integration is up to date."));
        };
        let integration = registry::open(root, id)?;
        confirm_trust(&integration, provisioned.provenance)?;
        let record = registry::Installed::new(&integration.manifest, provisioned.origin, provisioned.files);
        // Bound as the host is; on a host without the binding, as an older funes bound the agent.
        let bound = memory::binding()?.or(registry::binding(root, id)?);
        ran.set(true);
        let outcome = setup_add(&integration, bound.as_deref());
        // Recorded however setup ended: `remove` must run what a failed setup may have wired in.
        registry::record(root, &record)?;
        outcome?;
        let to = integration
            .manifest
            .version
            .as_ref()
            .map(|v| format!(" to {v}"))
            .unwrap_or_default();
        Ok(format!("Updated the {id} integration{to}."))
    }
    .await;
    if !ran.get() {
        if let Some(previous) = previous {
            registry::restore_manifest(root, id, &previous)?;
        }
    }
    result
}

/// A resolved memory binding: the memory spec, and whether funes just created the repo this run — the
/// signal the first push uses to skip the wrong-memory guard (an empty repo funes made for the user
/// is plainly not "the wrong memory").
#[derive(Clone)]
struct Resolved {
    memory: String,
    created: bool,
}

/// The memory `spec` names, checked on the Hub (a typo guard, offering to create it); `None` for
/// `local`.
async fn resolve_named(spec: &str) -> Result<Option<Resolved>> {
    match spec.trim() {
        "" | "local" => Ok(None),
        spec => Ok(Some(Resolved {
            memory: spec.to_string(),
            created: ensure_remote_exists(spec).await?,
        })),
    }
}

/// `funes bind`: bind this host to `spec`, `local` to unbind, every installed agent following, then
/// the first push when this host has a local memory and the memory is on the Hub.
async fn bind_memory(spec: &str) -> Result<()> {
    let resolved = resolve_named(spec).await?;
    bind_host(resolved.clone(), None).await?;
    let Some(Resolved { memory, created }) = resolved else {
        return Ok(());
    };
    if matches!(memory::Memory::parse(&memory), memory::Memory::Remote { .. })
        && index::local_memory_model().await?.is_some()
    {
        first_push(&memory, created).await.with_context(|| {
            format!(
                "this host is bound to {memory}, but the first push there did not go through — run \
                 `funes push {memory}` at a terminal to publish there"
            )
        })?;
    }
    Ok(())
}

/// Bind this host to `resolved`, `None` for its local memory: check its embedding model against
/// the local memory's, record it, and run every installed agent's setup on it, `except` the one
/// an add is about to install. The first push is the caller's.
async fn bind_host(resolved: Option<Resolved>, except: Option<&str>) -> Result<()> {
    let memory = resolved.as_ref().map(|r| r.memory.as_str());
    if let (Some(memory), Some(here)) = (memory, index::local_memory_model().await?) {
        if let Some(there) = bound_model(memory, false).await? {
            if there != here {
                bail!(
                    "{memory} is embedded with {}, and the local memory with {}: a memory takes one embedding model",
                    there.id(),
                    here.id()
                );
            }
        }
    }
    memory::bind(memory)?;
    let root = registry::default_root()?;
    let mut followed = Vec::new();
    let mut failed = Vec::new();
    for id in registry::registered_ids(&root)
        .into_iter()
        .filter(|id| spool::is_id(id) && except != Some(id.as_str()))
    {
        match follow(&root, &id, memory).await {
            Ok(()) => followed.push(id),
            Err(e) => {
                eprintln!("the {id} integration could not follow: {e:#}");
                failed.push(id);
            }
        }
    }
    let bound = match memory {
        Some(memory) => format!("bound this host to {memory}"),
        None => "this host is local".to_string(),
    };
    if followed.is_empty() {
        println!("{bound}.");
    } else {
        println!("{bound}: {} follow.", followed.join(", "));
    }
    if !failed.is_empty() {
        bail!("not bound: {}", failed.join(", "));
    }
    Ok(())
}

/// Run `id`'s installed setup on `memory`, and forget the binding an older funes noted beside it.
/// A copy refreshed to run here is recorded however setup ended, as `add` and `update` record
/// theirs.
async fn follow(root: &Path, id: &str, memory: Option<&str>) -> Result<()> {
    let (integration, provisioned) = prepare_agent(id, registry::speaks_another_contract(root, id), None).await?;
    let outcome = setup_add(&integration, memory);
    if let Some((origin, files)) = provisioned {
        registry::record(root, &registry::Installed::new(&integration.manifest, origin, files))?;
    }
    outcome?;
    registry::bind(root, id, None)
}

/// The memory a bare add installs an agent on: the host's. A host from before the binding is
/// bound as an older funes bound its agents first; one whose agents disagree is `funes bind`'s to
/// settle. A local host is offered a memory on the Hub, which the add then binds the host to, as
/// the flag says.
async fn adopt_host_memory(root: &Path, id: &str) -> Result<(Option<Resolved>, bool)> {
    if let Some(disagreement) = migrate_bindings(root, Some(id)).await? {
        bail!("{disagreement}");
    }
    if let Some(memory) = memory::binding()? {
        return Ok((Some(Resolved { memory, created: false }), false));
    }
    let offered = offer_hub_memory().await?;
    let binds = offered.is_some();
    Ok((offered, binds))
}

/// Bind a host from before the binding as an older funes bound its agents: the one memory they
/// noted becomes the host's, every installed agent following, the ones that older funes left
/// local included, `except` the one an add is about to install. Agents that noted different
/// memories come back as the sentence for the caller to print or refuse on, and nothing is bound.
async fn migrate_bindings(root: &Path, except: Option<&str>) -> Result<Option<String>> {
    if memory::binding()?.is_some() {
        return Ok(None);
    }
    let groups = legacy_bindings(root)?;
    match groups.as_slice() {
        [] => Ok(None),
        [(spec, ids)] => {
            println!(
                "an older funes bound {} to {spec}; binding this host to it.",
                ids.join(", ")
            );
            let memory = Resolved {
                memory: spec.clone(),
                created: false,
            };
            bind_host(Some(memory), except).await?;
            Ok(None)
        }
        several => Ok(Some(disagreement(several))),
    }
}

/// The memories an older funes bound this host's agents to, each as first typed with the agents
/// bound to it. A binding is kept as typed, so one memory may be noted under two spellings.
fn legacy_bindings(root: &Path) -> Result<Vec<(String, Vec<String>)>> {
    let mut by_label: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for (spec, ids) in registry::bindings(root)?.0 {
        let label = memory::Memory::parse(&spec).label();
        by_label
            .entry(label)
            .or_insert_with(|| (spec, Vec::new()))
            .1
            .extend(ids);
    }
    Ok(by_label.into_values().collect())
}

/// The sentence for agents an older funes bound to different memories, with its cure.
fn disagreement(groups: &[(String, Vec<String>)]) -> String {
    let parts: Vec<String> = groups
        .iter()
        .enumerate()
        .map(|(i, (spec, ids))| {
            if i == 0 {
                format!("{} bound to {spec}", are(ids))
            } else {
                format!("{} to {spec}", ids.join(", "))
            }
        })
        .collect();
    format!(
        "{}; this host reads local until `funes bind <memory>` binds it to one",
        parts.join(", ")
    )
}

/// `ids`, as the subject of "bound": "claude is", "claude, codex are".
fn are(ids: &[String]) -> String {
    format!("{} {}", ids.join(", "), if ids.len() == 1 { "is" } else { "are" })
}

/// Validate an explicitly-named memory: fine if it exists or the Hub is unreachable; offer to create
/// it if missing (default **no**, to catch typos). Returns whether it created the repo.
async fn ensure_remote_exists(remote: &str) -> Result<bool> {
    let target = memory::Memory::parse(remote);
    let memory::Memory::Remote { uri } = &target else {
        return Ok(false); // a local path — nothing to check on the Hub
    };
    match memory::remote_reachability(uri).await {
        memory::Reachability::Ok | memory::Reachability::Offline => Ok(false),
        memory::Reachability::Missing => {
            let (owner, name, _) = hub::parse_hf(uri)?;
            if std::io::stdin().is_terminal()
                && confirm(
                    &format!("{remote} doesn't exist on the Hub. Create it as a private dataset? [y/N] "),
                    false,
                )
            {
                hub::create_dataset_repo(&owner, &name).await?;
                eprintln!("created {owner}/{name} as a private dataset.");
                Ok(true)
            } else {
                Err(target.missing_error())
            }
        }
    }
}

/// With no memory named, offer to set one up on the Hub — but only when a token is present and we can
/// prompt. Suggests `<user>/funes-memory`: use it if it exists, offer to create it if not. Returns the
/// memory to bind (with whether it was just created), or `None` to stay local.
async fn offer_hub_memory() -> Result<Option<Resolved>> {
    let interactive = std::io::stdin().is_terminal();
    if !hub::has_token() {
        if interactive {
            eprintln!("staying local — set HF_TOKEN (or run `hf auth login`) and re-run `funes add …` to sync across machines or a team.");
        }
        return Ok(None);
    }
    // A scripted (non-interactive) add can't prompt, so it stays local unless a memory was named.
    if !interactive
        || !confirm(
            "Push your memory to a Hugging Face dataset, so it follows you across machines? [Y/n] ",
            true,
        )
    {
        return Ok(None);
    }
    let user = match hub::whoami().await {
        Ok(u) => u,
        Err(e) => {
            eprintln!("couldn't read your Hugging Face identity ({e:#}) — staying local.");
            return Ok(None);
        }
    };
    let memory = format!("{user}/funes-memory");
    let uri = format!("hf://datasets/{memory}");
    match memory::remote_reachability(&uri).await {
        memory::Reachability::Ok => {
            Ok(confirm(&format!("Use your memory {memory}? [Y/n] "), true)
                .then_some(Resolved { memory, created: false }))
        }
        memory::Reachability::Offline => {
            eprintln!("can't reach the Hub right now — staying local; re-run when you're online.");
            Ok(None)
        }
        memory::Reachability::Missing => {
            if confirm(
                &format!("Create {memory} as a private dataset for your memory? [Y/n] "),
                true,
            ) {
                hub::create_dataset_repo(&user, "funes-memory").await?;
                eprintln!("created {memory} as a private dataset.");
                Ok(Some(Resolved { memory, created: true }))
            } else {
                Ok(None)
            }
        }
    }
}

/// Prompt for yes/no on stderr and read a line from stdin. Empty input takes `default_yes`; EOF or
/// a read error declines, so an unattended run never proceeds.
fn confirm(prompt: &str, default_yes: bool) -> bool {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    match std::io::stdin().read_line(&mut answer) {
        Ok(n) if n > 0 => parse_confirm(&answer, default_yes),
        _ => false,
    }
}

/// Pure core of [`confirm`]: empty → the default; `y`/`yes` → yes; anything else → no (conservative,
/// so an unrecognized answer never creates a repo or publishes).
fn parse_confirm(input: &str, default_yes: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// The embedding model `memory` records, `None` when it holds no dataset or is unreachable.
/// When the local memory `takes` its model from it, a memory whose model can't be read is an error:
/// a guess could pin the local memory to another model.
async fn bound_model(memory: &str, takes: bool) -> Result<Option<EmbeddingModel>> {
    let target = memory::Memory::parse(memory);
    async {
        let ds = match target.state().await? {
            memory::MemoryState::Ready(ds) => ds,
            // A dataset with no rows still records its model.
            memory::MemoryState::Empty => match target.open().await {
                Ok(ds) => ds,
                Err(_) => return Ok(None),
            },
            memory::MemoryState::Offline if takes => bail!(
                "it is unreachable, and the local memory takes its embedding model: run `funes add` again once it is back"
            ),
            memory::MemoryState::Offline => {
                eprintln!("note: can't reach {memory} right now — proceeding; it'll be used once it's back.");
                return Ok(None);
            }
            memory::MemoryState::Unauthorized if takes => return Err(target.unauthorized_error()),
            _ => return Ok(None),
        };
        memory::dataset::embedding_model(&ds).map(Some)
    }
    .await
    .with_context(|| format!("can't read {memory}"))
}

fn ask_model(default: EmbeddingModel) -> EmbeddingModel {
    let choices = if default == memory::dataset::model::MULTILINGUAL {
        "[e/M]"
    } else {
        "[E/m]"
    };
    eprint!("Index your sessions as English, or multilingual for other languages too? {choices} ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    parse_model(&answer, default)
}

fn parse_model(input: &str, default: EmbeddingModel) -> EmbeddingModel {
    match input.trim().to_ascii_lowercase().as_str() {
        "e" | "english" => memory::dataset::model::ENGLISH,
        "m" | "multilingual" => memory::dataset::model::MULTILINGUAL,
        _ => default,
    }
}

fn host_model() -> EmbeddingModel {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|value| !value.is_empty()));
    model_for_locale(locale.as_deref())
}

fn model_for_locale(locale: Option<&str>) -> EmbeddingModel {
    let language = locale.and_then(|l| l.split(['_', '.', '@']).next()).unwrap_or_default();
    match language.to_ascii_lowercase().as_str() {
        "" | "c" | "posix" | "en" => memory::dataset::model::ENGLISH,
        _ => memory::dataset::model::MULTILINGUAL,
    }
}

/// `funes add <agent> [memory]`: bootstrap the one-time steps the hooks can't do unattended,
/// around the integration's own `setup add` (converts the agent's history into its spool,
/// registers hooks + MCP).
///
/// 1. on a first add (no rows in the local memory yet), ask — the first index is about a minute of
///    work, and declining aborts the add before anything is installed, so nothing is wired up;
/// 2. bind the host when the memory was named or offered, every other agent following — once the
///    add is going ahead, so a declined one leaves the host as it was;
/// 3. `install` — writes the spool, bakes the memory in;
/// 4. build the first index from that spool, so recall and the push have content;
/// 5. first push if a memory is bound — clears the overlap guard so the push hook works thereafter.
async fn bootstrap_add<F, Fut>(agent: &str, resolved: Option<Resolved>, binds: bool, install: F) -> Result<()>
where
    F: FnOnce(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let local = index::local_memory_model().await?;
    let bound = match &resolved {
        Some(Resolved { memory, .. }) => bound_model(memory, local.is_none())
            .await?
            .map(|model| (memory.as_str(), model)),
        None => None,
    };
    if let (Some((memory, there)), Some(here)) = (bound, local) {
        if there != here {
            bail!(
                "{memory} is embedded with {}, and the local memory with {}: a memory takes one embedding model",
                there.id(),
                here.id()
            );
        }
    }
    let first_add = !matches!(memory::Memory::local().state().await, Ok(memory::MemoryState::Ready(_)));
    if first_add
        && !confirm(
            &format!("funes will index your existing {agent} sessions, if any, so recall works (about a minute). Proceed? [Y/n] "),
            true,
        )
    {
        eprintln!("funes: skipped — nothing was wired up. Run `funes add {agent}` again when you're ready.");
        return Ok(());
    }
    if local.is_none() {
        let model = match bound {
            Some((memory, model)) => {
                eprintln!(
                    "funes: embedding the local memory with {}, the model of {memory}.",
                    model.id()
                );
                model
            }
            None if std::io::stdin().is_terminal() => ask_model(host_model()),
            None => {
                let model = host_model();
                eprintln!(
                    "funes: embedding the local memory with {}, this host's default off a terminal.",
                    model.id()
                );
                model
            }
        };
        index::ensure_local_memory(model).await?;
    }
    if binds {
        bind_host(resolved.clone(), Some(agent)).await?;
    }
    install(resolved.as_ref().map(|r| r.memory.clone())).await?;
    if first_add {
        seed_local_index(agent).await;
    }
    // A memory at a local path is read where it is: push only publishes to the Hub.
    if let Some(Resolved { memory, created }) = resolved {
        if matches!(memory::Memory::parse(&memory), memory::Memory::Remote { .. }) {
            // The integration is in place by now; only the push is owed, and it takes a terminal.
            first_push(&memory, created).await.with_context(|| {
                format!(
                    "funes is added to {agent}, bound to {memory}, but the first push there did not go \
                     through — run `funes push {memory}` at a terminal to publish there"
                )
            })?;
        }
    }
    Ok(())
}

/// Build the first index from the spool `agent`'s setup has just converted its history into,
/// `$FUNES_HOME/spool/<agent>`. An empty or absent spool and a build error are notes, not
/// failures: the hooks are in, and they drain whatever lands there.
async fn seed_local_index(agent: &str) {
    let spool = spool::spool_dir(agent);
    let has_sessions = std::fs::read_dir(&spool).is_ok_and(|mut entries| entries.next().is_some());
    if !has_sessions {
        eprintln!("funes: no {agent} sessions to index yet.");
        return;
    }
    eprintln!("funes: indexing your recent {agent} sessions…");
    if let Err(e) = index::run_index_seed(&spool).await {
        eprintln!(
            "funes: initial index didn't complete ({e:#}) — the hooks are installed; run `funes index` to build it."
        );
    }
}

/// The one-time first publish `add` performs when a memory is bound (the push hook can't, off a
/// terminal — the overlap guard fails closed there). The guard prompts before publishing to a memory
/// that holds none of this host's chunks — unless funes just `created` the memory this run, which is
/// plainly the user's own empty repo, so the push proceeds without re-asking. Errors that aren't
/// fatal to the install (a read-only token, held-back secrets) are reported without failing `add`.
async fn first_push(remote: &str, created: bool) -> Result<()> {
    let confirm = if created {
        push::Confirm::Yes
    } else {
        push::Confirm::Ask(prompt_new_memory)
    };
    match push::run_push(memory::Memory::parse(remote), false, confirm, &[]).await {
        Ok(pushed) => {
            print!("{}", pushed.report);
            Ok(())
        }
        Err(e) if push::is_read_only(&e) => {
            eprintln!("{remote} is read-only for your token — recall can read it, but publishing needs write access (check your HF token).");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// A stderr spinner for the wait before results: braille frames plus a phase label, redrawn in
/// place and erased when dropped — nothing lands in the output. [`Spinner::start`] returns None
/// when stderr isn't a terminal, so piped and scripted runs stay silent.
struct Spinner {
    label: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    fn start(label: &str) -> Option<Spinner> {
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let label = Arc::new(Mutex::new(label.to_string()));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (label.clone(), stop.clone());
        let color = std::env::var_os("NO_COLOR").is_none();
        let handle = std::thread::spawn(move || {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            for i in 0.. {
                if s.load(Ordering::Relaxed) {
                    break;
                }
                let text = l.lock().map(|g| g.clone()).unwrap_or_default();
                let frame = FRAMES[i % FRAMES.len()];
                if color {
                    eprint!("\r\x1b[K\x1b[36m{frame}\x1b[0m {text}");
                } else {
                    eprint!("\r\x1b[K{frame} {text}");
                }
                let _ = std::io::stderr().flush();
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
            eprint!("\r\x1b[K");
            let _ = std::io::stderr().flush();
        });
        Some(Spinner {
            label,
            stop,
            handle: Some(handle),
        })
    }

    /// Swap the label; the next frame shows it.
    fn set(&self, label: &str) {
        if let Ok(mut l) = self.label.lock() {
            label.clone_into(&mut l);
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The push confirmation for a target holding none of the chunks to publish. Fails closed (returns
/// false) off a terminal, so an unattended push can't silently publish to the wrong memory — there it
/// must be re-run with `--yes`.
fn prompt_new_memory(label: &str, chunks: usize, why: push::Disjoint) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!("{}", disjoint_refusal(label, chunks, why));
        return false;
    }
    eprint!("{}", disjoint_prompt(label, chunks, why));
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// What the push found about the target and the chunks to publish, in the words of that case.
fn disjoint_finding(label: &str, chunks: usize, why: push::Disjoint) -> String {
    match why {
        push::Disjoint::Selection { sessions } => {
            format!("{label} holds none of the {chunks} chunk(s) of the {sessions} named session(s)")
        }
        push::Disjoint::FirstPublish => {
            format!("{label} is empty, so these {chunks} chunk(s) would be its first publish")
        }
        push::Disjoint::NewHost { remote_chunks } => {
            format!("{label} holds {remote_chunks} chunk(s), none from this host: a new host of yours")
        }
        push::Disjoint::Unexplained { remote_chunks } => format!(
            "{label} holds {remote_chunks} chunk(s) and none of the {chunks} to publish, although it held \
             chunks from this host before: check that it is the right memory"
        ),
    }
}

/// The question put to the user at a terminal: the finding, then a question that fits it.
fn disjoint_prompt(label: &str, chunks: usize, why: push::Disjoint) -> String {
    let question = match why {
        push::Disjoint::Selection { .. } => "Publish them?",
        push::Disjoint::FirstPublish => "Publish?",
        push::Disjoint::NewHost { .. } | push::Disjoint::Unexplained { .. } => "Publish anyway?",
    };
    format!("{}. {question} [y/N] ", disjoint_finding(label, chunks, why))
}

/// The refusal off a terminal, where no question can be asked.
fn disjoint_refusal(label: &str, chunks: usize, why: push::Disjoint) -> String {
    format!(
        "refusing to push: {}. Re-run with `--yes` to confirm.",
        disjoint_finding(label, chunks, why)
    )
}

#[cfg(test)]
mod tests {
    use super::{disjoint_prompt, disjoint_refusal, model_for_locale, parse_confirm, parse_model, Cli, Cmd};
    use clap::Parser;
    use funes::commands::push::Disjoint;
    use funes::memory::dataset::model::{ENGLISH, MULTILINGUAL};

    #[test]
    fn the_push_prompt_says_which_case_it_found() {
        let label = "hf://datasets/acme/memory";
        assert_eq!(
            disjoint_prompt(label, 1339, Disjoint::Selection { sessions: 3 }),
            "hf://datasets/acme/memory holds none of the 1339 chunk(s) of the 3 named session(s). Publish them? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::FirstPublish),
            "hf://datasets/acme/memory is empty, so these 40 chunk(s) would be its first publish. Publish? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::NewHost { remote_chunks: 67339 }),
            "hf://datasets/acme/memory holds 67339 chunk(s), none from this host: a new host of yours. Publish anyway? [y/N] "
        );
        assert_eq!(
            disjoint_prompt(label, 40, Disjoint::Unexplained { remote_chunks: 67339 }),
            "hf://datasets/acme/memory holds 67339 chunk(s) and none of the 40 to publish, although it held chunks from this host before: check that it is the right memory. Publish anyway? [y/N] "
        );
        assert_eq!(
            disjoint_refusal(label, 1339, Disjoint::Selection { sessions: 3 }),
            "refusing to push: hf://datasets/acme/memory holds none of the 1339 chunk(s) of the 3 named session(s). Re-run with `--yes` to confirm."
        );
    }

    #[test]
    fn an_answer_names_the_model_and_anything_else_takes_the_default() {
        for default in [ENGLISH, MULTILINGUAL] {
            for answer in ["\n", "nope"] {
                assert_eq!(parse_model(answer, default), default, "{answer:?}");
            }
            for answer in ["e", "English\n"] {
                assert_eq!(parse_model(answer, default), ENGLISH, "{answer:?}");
            }
            for answer in ["m\n", " M ", "multilingual"] {
                assert_eq!(parse_model(answer, default), MULTILINGUAL, "{answer:?}");
            }
        }
    }

    #[test]
    fn a_locale_in_another_language_than_english_defaults_to_multilingual() {
        for locale in [
            None,
            Some("C"),
            Some("C.UTF-8"),
            Some("POSIX"),
            Some("en_US.UTF-8"),
            Some("en"),
        ] {
            assert_eq!(model_for_locale(locale), ENGLISH, "{locale:?}");
        }
        for locale in ["zh_CN.UTF-8", "fr_FR", "ja_JP.eucJP", "de_DE@euro", "pt"] {
            assert_eq!(model_for_locale(Some(locale)), MULTILINGUAL, "{locale}");
        }
    }

    #[test]
    fn parse_confirm_honors_default_and_answers() {
        // Empty takes the default either way.
        assert!(parse_confirm("\n", true));
        assert!(!parse_confirm("  ", false));
        // Explicit yes/no, case- and whitespace-insensitive.
        assert!(parse_confirm("y", false));
        assert!(parse_confirm(" YES \n", false));
        assert!(!parse_confirm("n", true));
        // Anything unrecognized is a conservative no, even under a yes default.
        assert!(!parse_confirm("nope", true));
        assert!(!parse_confirm("maybe", true));
    }

    #[test]
    fn push_still_takes_force_reindex() {
        for flag in ["--compact", "--force-reindex"] {
            let cli = Cli::try_parse_from(["funes", "push", "org/memory", flag]).unwrap();
            assert!(matches!(cli.cmd, Cmd::Push { compact: true, .. }), "{flag}");
        }
    }
}
