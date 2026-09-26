//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::agents::{self, registry};
use funes::commands::{ask, index, mcp, push, recall, scrub, sketch, update};
use funes::hub;
use funes::memory;
use funes::traces::spool;
use funes::ui::render;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
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
    /// Recall passages from past sessions (hybrid → rerank → recency → neighbors).
    Recall {
        /// What to recall (free text).
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// How many results to show.
        #[arg(short, long, default_value_t = recall::DEFAULT_K)]
        k: usize,
        /// How many fused candidates to rerank.
        #[arg(long, default_value_t = recall::DEFAULT_CANDIDATES)]
        candidates: usize,
        /// Recency half-life in days (a hit this old keeps half its weight). 0 disables.
        #[arg(long, default_value_t = recall::DEFAULT_HALF_LIFE)]
        half_life: f64,
        /// Adjacent chunks (within this seq window) to attach to each hit. 0 disables.
        #[arg(long, default_value_t = recall::DEFAULT_NEIGHBORS)]
        neighbors: i64,
        /// Restrict to a block type: text | thinking | tool_use | tool_result.
        #[arg(long = "type", value_name = "BLOCK_TYPE")]
        block_type: Option<String>,
        /// Restrict to a harness facet, as the turns carry it (`claude` also matches the older
        /// `claude_code`).
        #[arg(long)]
        harness: Option<String>,
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
    /// Build or update your local memory from session transcripts.
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
    /// Show index statistics.
    Status {
        /// Memory to inspect — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or
        /// `local`. Defaults to your local memory.
        #[arg(value_name = "MEMORY")]
        memory: Option<String>,
    },
    /// Publish your local memory's new chunks to a remote memory on the HF Hub.
    Push {
        /// Memory to publish to: `<org>/<repo>` or a full `hf://…` URI.
        #[arg(value_name = "MEMORY")]
        memory: String,
        /// Skip the confirmation when the target shares no chunks with your local memory.
        #[arg(short, long)]
        yes: bool,
        /// Refresh the remote index after pushing (retrying on conflict) even if the unindexed
        /// backlog is below the auto-reindex threshold. With nothing new to push, reindex only.
        #[arg(long)]
        force_reindex: bool,
        /// Publish exactly these sessions. Omit to publish everything the remote does not already
        /// hold.
        #[arg(long, value_name = "SESSION")]
        sessions: Vec<String>,
    },
    /// Redact secrets from your local memory in place — for rows indexed before redaction existed (or
    /// flagged by an updated ruleset); needs no source transcript. Cleans the local memory only: it
    /// does NOT scrub an already-published remote, which the push gate can only stop adding to.
    Scrub,
    /// Update funes in place: download the latest release binary for this platform and replace the
    /// running executable. Idempotent — `--force` reinstalls even when already up to date.
    Update {
        /// Reinstall the latest binary even if this build is already up to date.
        #[arg(short, long)]
        force: bool,
    },
    /// Run as an MCP server over stdio (for Claude Code, Cursor, …).
    Mcp {
        /// Memory to serve — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
        /// Defaults to your local memory.
        #[arg(value_name = "MEMORY")]
        memory: Option<String>,
    },
    /// Add funes to a coding agent.
    ///
    /// Installs funes's read tools and automatic per-turn indexing. Name a memory the agent recalls
    /// from — and publishes to — an `<org>/<repo>` shorthand or an `hf://…` URI; omit it and an
    /// installed agent keeps the memory it is bound to, a first add stays local.
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
    /// Memory this agent recalls from — `<org>/<repo>`, an `hf://…` URI, or `local`. Omitted, the
    /// memory the agent is bound to stays; a first add's is local.
    #[arg(value_name = "MEMORY")]
    memory: Option<String>,
}

/// The memory to bake into an agent's `funes mcp` registration: blank/`local` → the local memory
/// (a bare `funes mcp`); none named → `bound`, the memory the install is recorded as bound to;
/// else the named remote/explicit memory (`funes mcp <memory>`).
fn baked_memory(memory: AddMemory, bound: Option<String>) -> Option<String> {
    match memory.memory.map(|s| s.trim().to_string()) {
        None => bound,
        Some(s) if s.is_empty() || s == "local" => None,
        Some(s) => Some(s),
    }
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
    /// The memory to read — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
    /// Defaults to your local memory.
    #[arg(long = "memory", value_name = "MEMORY")]
    memory: Option<String>,
}

impl MemoryOpts {
    fn resolve(self) -> memory::Memory {
        memory::Memory::resolve(self.memory)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
            | Cmd::Status { .. }
            | Cmd::Ask { .. }
    ) {
        if let Some(note) = agents::stale_install_notice(None) {
            eprint!("{note}");
        }
    }
    match cli.cmd {
        Cmd::Recall {
            query,
            k,
            candidates,
            half_life,
            neighbors,
            block_type,
            harness,
            memory,
        } => {
            let memory = memory.resolve();
            let query = query.join(" ");
            let spinner = Spinner::start("recalling…");
            let progress = |label: &str| {
                if let Some(s) = &spinner {
                    s.set(label);
                }
            };
            let (note, memory_label, hits) = recall::recall_hits(
                memory, query, k, candidates, half_life, neighbors, block_type, harness, &progress,
            )
            .await?;
            drop(spinner);
            if hits.is_empty() {
                print!("{note}no results");
            } else {
                print!(
                    "{}",
                    render::recall_agent(&note, &recall::memory_hint(memory_label.as_deref()), &hits)
                );
            }
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
                recall::scan(memory.resolve(), needle, session_id, from, to, ignore_case, context).await?
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
            print!("{}", recall::sessions(memory.resolve(), filter).await?);
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
                sketch::run(memory.resolve(), session_id, from, to, units, max_chars).await?
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
            print!("{}", recall::get(memory.resolve(), session_id, range).await?);
            Ok(())
        }
        Cmd::Ask { agent } => match agent {
            AskAgent::Claude { args } => ask::claude(args.question.join(" "), args.memory.resolve()).await,
            AskAgent::Codex { args } => ask::codex(args.question.join(" "), args.memory.resolve()).await,
        },
        Cmd::Index {
            path,
            harness,
            check,
            no_thinking,
            limit,
            yes,
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
            // fresh" case) is budgeted and text-first; an explicit path or Hub repo is indexed in
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
        Cmd::Status { memory } => {
            print!("{}", recall::status(memory::Memory::resolve(memory)).await?);
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
            force_reindex,
            sessions,
        } => {
            let confirm = if yes {
                push::Confirm::Yes
            } else {
                push::Confirm::Ask(prompt_new_memory)
            };
            match push::run_push(memory::Memory::parse(&remote), force_reindex, confirm, &sessions).await {
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
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update::run(force).await,
        Cmd::Mcp { memory } => mcp::run(memory).await,
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
        // The catalog's newest release is what a catalog install runs, so it is consulted on
        // every run; a directory or an archive named once stays until named again. Files named
        // now, and a copy this funes cannot run, are refreshed whatever is there.
        let refresh = fresh
            || from.is_some()
            || std::env::var_os("FUNES_INTEGRATIONS").is_some()
            || registry::speaks_another_contract(&root, id)
            || registry::installed_from_catalog(&root, id);
        // Read before the record is rewritten: the memory this install was last bound to.
        let bound = registry::installed(&root, id).and_then(|record| record.memory);
        let (integration, provisioned) = prepare_agent(id, refresh, from).await?;
        let resolved = resolve_add_memory(baked_memory(memory, bound)).await?;
        let memory = resolved.as_ref().map(|r| r.memory.clone());
        let record = provisioned.map(|(origin, files)| registry::Installed::new(&integration.manifest, origin, files));
        let install = |memory: Option<String>| async move {
            // Counted as run before it runs: a setup that fails part-way may have wired some of
            // the agent to these files, and `remove` must find them recorded to run unasked.
            ran.set(true);
            integration.add(memory.as_deref())?;
            // Whatever an older hook asked for, this install's hooks are the ones that ask now.
            spool::forget_missing(id)
        };
        let outcome = bootstrap_add(id, resolved, install).await;
        // Recorded once setup has run, whatever came after: a first push that failed leaves the
        // new files installed, and the record must say so — the memory bound with it. An
        // installed copy nothing refreshed keeps the record it has, rebound.
        if ran.get() {
            if let Some(mut record) = record.or_else(|| registry::installed(&root, id)) {
                record.memory = memory;
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
                    registry::Provenance::Unvouched(_)
                        if registry::installed(&root, id).is_some_and(|r| r.origin == origin && r.files == files) =>
                    {
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

/// A resolved memory binding: the memory spec, and whether funes just created the repo this run — the
/// signal the first push uses to skip the wrong-memory guard (an empty repo funes made for the user
/// is plainly not "the wrong memory").
struct Resolved {
    memory: String,
    created: bool,
}

/// Resolve the memory `funes add` binds. A named memory is validated — offer to create it if it's
/// missing on the Hub (a typo guard). With none, offer to set one up on the Hub when a token is
/// present (`<user>/funes-memory`); otherwise stay local.
async fn resolve_add_memory(memory: Option<String>) -> Result<Option<Resolved>> {
    match memory {
        Some(memory) => {
            let created = ensure_remote_exists(&memory).await?;
            Ok(Some(Resolved { memory, created }))
        }
        None => offer_hub_memory().await,
    }
}

/// Validate an explicitly-named memory: fine if it exists; offer to create it if missing (default
/// **no**, to catch typos); warn but proceed if the Hub is unreachable. Returns whether it created
/// the repo.
async fn ensure_remote_exists(remote: &str) -> Result<bool> {
    let target = memory::Memory::parse(remote);
    let memory::Memory::Remote { uri } = &target else {
        return Ok(false); // a local path — nothing to check on the Hub
    };
    match memory::remote_reachability(uri).await {
        memory::Reachability::Ok => Ok(false),
        memory::Reachability::Offline => {
            eprintln!("note: can't reach {remote} right now — proceeding; it'll be used once it's back.");
            Ok(false)
        }
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

/// `funes add <agent> [memory]`: bootstrap the one-time steps the hooks can't do unattended,
/// around the integration's own `setup add` (converts the agent's history into its spool,
/// registers hooks + MCP).
///
/// 1. on a first add (no local memory yet), ask — the first index is about a minute of work, and
///    declining aborts the add before anything is installed, so nothing is wired up;
/// 2. `install` — writes the spool, bakes the memory in;
/// 3. build the first index from that spool, so recall and the push have content;
/// 4. first push if a memory is bound — clears the overlap guard so the push hook works thereafter.
async fn bootstrap_add<F, Fut>(agent: &str, resolved: Option<Resolved>, install: F) -> Result<()>
where
    F: FnOnce(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let first_add = memory::Memory::local().open().await.is_err();
    if first_add
        && !confirm(
            &format!("funes will index your existing {agent} sessions, if any, so recall works (about a minute). Proceed? [Y/n] "),
            true,
        )
    {
        eprintln!("funes: skipped — nothing was wired up. Run `funes add {agent}` again when you're ready.");
        return Ok(());
    }
    install(resolved.as_ref().map(|r| r.memory.clone())).await?;
    if first_add {
        seed_local_index(agent).await;
    }
    // First push only when there's actually a local index to publish. Without one (a failed first
    // build, or no sessions yet) there's nothing to push, and running it would just error on the
    // absent memory.
    if let Some(Resolved { memory, created }) = resolved {
        if memory::Memory::local().open().await.is_ok() {
            // The integration is in place by now; only the push is owed, and it takes a terminal.
            first_push(&memory, created).await.with_context(|| {
                format!(
                    "funes is added to {agent}, bound to {memory}, but the first push there did not go \
                     through — run `funes push {memory}` at a terminal once it is reachable"
                )
            })?;
        } else {
            eprintln!("funes: nothing indexed yet — nothing to publish to {memory} yet.");
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
/// this host shares no chunks with — unless funes just `created` the memory this run, which is
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

/// The push confirmation for a memory the local index shares no chunks with. Fails closed (returns
/// false) off a terminal, so an unattended push can't silently publish to the wrong memory — there it
/// must be re-run with `--yes`.
fn prompt_new_memory(label: &str, chunks: usize) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "refusing to push {chunks} chunk(s) to {label}: your local memory shares no chunks with it \
             (a first push, a new host, or the wrong memory) — re-run with `--yes` to confirm."
        );
        return false;
    }
    eprint!(
        "{label}: your local memory shares no chunks with it — a first push here, a new host of yours, \
         or the wrong memory. Publish {chunks} chunk(s) anyway? [y/N] "
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::parse_confirm;

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
}
