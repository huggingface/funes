//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::harness::Harness;
use funes::{ask, claude, codex, curate, hermes, hub, index, mcp, pi, push, recall, render, scrub, update};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Turns around a `get` target when no window is given.
const DEFAULT_WINDOW: i64 = 3;

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
        /// Restrict to a harness: claude | codex | pi | hermes.
        #[arg(long)]
        harness: Option<String>,
        #[command(flatten)]
        memory: MemoryOpts,
    },
    /// Drill down on a recall hit: a turn plus the turns around it, reassembled.
    Get {
        /// Session id (from a recall hit's `→ get` line).
        session_id: String,
        /// Turn uuid (from a recall hit's `→ get` line).
        turn_uuid: String,
        /// Turns within this seq window of the target are included.
        #[arg(long, default_value_t = DEFAULT_WINDOW)]
        window: i64,
        /// Output format. Default: human in a terminal, agent when piped.
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
        /// Highlight this text in the human rendering (matched whitespace-insensitively).
        #[arg(long)]
        highlight: Option<String>,
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
        /// A transcript tree or `.parquet` file, or a Hub trace repo `<org>/<repo>`. Omit — in a
        /// terminal — to index every known harness dir (~/.claude/projects, ~/.codex/sessions,
        /// ~/.pi/agent/sessions); `--harness <name>` alone targets one. An automated (non-terminal)
        /// run must name a target.
        path: Option<String>,
        /// Override harness auto-detection for PATH: claude | codex | pi | hermes.
        #[arg(long)]
        harness: Option<String>,
        /// Exclude thinking blocks.
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
    },
    /// Curate a project memory: a memory that ships only the sessions you've reviewed and marked
    /// `include`. Your review alone decides what `funes push` ships there.
    Curate {
        /// The memory — the Hub dataset it lives in: `<org>/<repo>` or a full `hf://…` URI.
        #[arg(value_name = "MEMORY")]
        memory: String,
        /// The project the memory is of — the git repo it's about (`huggingface/funes`) or a plain
        /// label. Give it the first time to name the memory; omit to review.
        project: Option<String>,
        /// Mark these sessions `include` — they ship on the next push to this memory.
        #[arg(long, value_name = "SESSION")]
        include: Vec<String>,
        /// Mark these sessions `exclude` — held back from this memory.
        #[arg(long, value_name = "SESSION")]
        exclude: Vec<String>,
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
    /// Installs the `recall`/`get` tools for any agent — and, for claude, codex, and hermes,
    /// automatic per-turn indexing. Name a memory the agent recalls from — and, for claude, codex,
    /// and hermes, publishes to — an `<org>/<repo>` shorthand or an `hf://…` URI; omit it to stay
    /// local (the default).
    #[command(
        subcommand_value_name = "AGENT",
        subcommand_help_heading = "Agents",
        override_usage = "funes add <AGENT> [MEMORY]"
    )]
    Add {
        #[command(subcommand)]
        agent: AddAgent,
    },
}

// Flattened into every agent so they share one optional `[MEMORY]` positional; the user-facing help
// comes from the field doc below.
#[derive(Args)]
struct AddMemory {
    /// Memory this agent recalls from — `<org>/<repo>`, an `hf://…` URI, or `local` (default).
    #[arg(value_name = "MEMORY")]
    memory: Option<String>,
}

#[derive(Subcommand)]
enum AddAgent {
    Claude {
        #[command(flatten)]
        memory: AddMemory,
    },
    Codex {
        #[command(flatten)]
        memory: AddMemory,
    },
    Pi {
        #[command(flatten)]
        memory: AddMemory,
        /// Reinstall even if the on-disk copy is already up to date.
        #[arg(long)]
        force: bool,
    },
    Hermes {
        #[command(flatten)]
        memory: AddMemory,
    },
}

/// The memory to bake into an agent's `funes mcp` registration: `None`/blank/`local` → the local
/// memory (a bare `funes mcp`), else the named remote/explicit memory (`funes mcp <memory>`).
fn baked_memory(memory: AddMemory) -> Option<String> {
    memory
        .memory
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "local")
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
    fn resolve(self) -> hub::Memory {
        hub::Memory::resolve(self.memory)
    }
}

/// The two output layouts for `get`.
#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    /// A numbered list with a hit selector.
    Human,
    /// The stable agent layout: multi-line hits with provenance, previews, and neighbors.
    Agent,
}

impl OutputFormat {
    /// Resolve the effective format: an explicit flag wins; otherwise human when both stdin and
    /// stdout are terminals (the hit selector needs both), agent when piped or scripted.
    fn resolve(flag: Option<OutputFormat>) -> OutputFormat {
        flag.unwrap_or_else(|| {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                OutputFormat::Human
            } else {
                OutputFormat::Agent
            }
        })
    }
}

/// Color and width for the human renderings: color needs a terminal and no `NO_COLOR`; width
/// follows `$COLUMNS` when exported, else 100.
fn human_io() -> (bool, usize) {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let width = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .map(|c| c.clamp(40, 120))
        .unwrap_or(100);
    (color, width)
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
        Cmd::Get {
            session_id,
            turn_uuid,
            window,
            format,
            highlight,
            memory,
        } => {
            let format = OutputFormat::resolve(format);
            let (note, turns) =
                recall::get_turns(memory.resolve(), session_id.clone(), turn_uuid.clone(), window).await?;
            if turns.is_empty() {
                print!("{note}");
                println!("turn {turn_uuid} not found in session {session_id}");
            } else if matches!(format, OutputFormat::Human) {
                let (color, width) = human_io();
                print!(
                    "{}",
                    render::get_human(&note, &turns, color, width, highlight.as_deref())
                );
            } else {
                print!("{}", render::get_agent(&note, &turns));
            }
            Ok(())
        }
        Cmd::Ask { agent } => match agent {
            AskAgent::Claude { args } => ask::claude(args.question.join(" "), args.memory.resolve()).await,
            AskAgent::Codex { args } => ask::codex(args.question.join(" "), args.memory.resolve()).await,
        },
        Cmd::Index {
            path,
            harness,
            no_thinking,
            limit,
            yes,
        } => {
            let harness = harness.map(|h| Harness::parse(&h)).transpose()?;
            // A harness-dirs refresh (no explicit path — the per-turn hook and the terminal "keep
            // me fresh" case) is budgeted and text-first; an explicit path or Hub repo is indexed
            // in full.
            let budgeted = path.is_none();
            let roots: Vec<(PathBuf, Option<Harness>)> = match path {
                // An existing local path wins over reading the same string as a repo ref.
                Some(p) if PathBuf::from(&p).exists() => vec![(PathBuf::from(p), harness)],
                Some(p) if p.starts_with("hf://") || hub::is_remote_shorthand(&p) => {
                    // A Hub trace dataset: resolve to `hf://datasets/<owner>/<name>` and index its
                    // auto-converted parquet.
                    let hub::Memory::Remote { uri } = hub::Memory::parse(&p) else {
                        return Err(anyhow!("expected a Hub repo, got {p:?}"));
                    };
                    return index::run_index_remote(&uri, no_thinking).await;
                }
                Some(p) => return Err(anyhow!("no such path: {p}")),
                // `--harness X` with no path targets that harness's known session dir — the
                // per-target form a session-end hook uses (index only its own harness's sessions).
                None if harness.is_some() => {
                    let h = harness.unwrap();
                    funes::harness::known_harness_roots()
                        .into_iter()
                        .find(|(_, kh)| *kh == h)
                        .map(|(dir, _)| vec![(dir, Some(h))])
                        .ok_or_else(|| anyhow!("no {} session dir found on this machine", h.cli_name()))?
                }
                // No target at all: index every known harness root — but only in a terminal. An
                // automated run (no TTY) must name a target, so a session-end hook indexes just its
                // own harness — a Claude session-end shouldn't pull in Codex or pi sessions.
                None => {
                    if !std::io::stdin().is_terminal() {
                        return Err(anyhow!(
                            "automated `funes index` needs a target — pass a path or `--harness <claude|codex|pi|hermes>`; \
                             refusing to index all harness roots unattended"
                        ));
                    }
                    funes::harness::known_harness_roots()
                        .into_iter()
                        .map(|(dir, h)| (dir, Some(h)))
                        .collect()
                }
            };
            if roots.is_empty() {
                return Err(anyhow!(
                    "no local sessions found — looked in ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions, ~/.hermes/state.db"
                ));
            }
            if budgeted {
                index::run_index_budgeted(&roots, no_thinking, limit, yes).await
            } else {
                index::run_index_roots(&roots, no_thinking, limit, yes).await
            }
        }
        Cmd::Status { memory } => {
            print!("{}", recall::status(hub::Memory::resolve(memory)).await?);
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
        } => {
            let confirm = if yes {
                push::Confirm::Yes
            } else {
                push::Confirm::Ask(prompt_new_memory)
            };
            match push::run_push(hub::Memory::parse(&remote), force_reindex, confirm).await {
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
        Cmd::Curate {
            memory,
            project,
            include,
            exclude,
        } => {
            let memory = hub::Memory::parse(&memory);
            // A project memory is of a git repo — funes attributes sessions to it by their
            // checkout's remotes, so the project must be a repo identity (`owner/name`). A bare
            // name gets a "did you mean", inferring the owner from local repos with that name.
            if let Some(project) = project.as_deref() {
                if !project.contains('/') {
                    return Err(match curate::projects_named(project).await?.as_slice() {
                        [] => anyhow!("a project is a repo, like `huggingface/transformers` — got `{project}`"),
                        [one] => anyhow!(
                            "a project is a repo — did you mean `{one}`?  run: funes curate {} {one}",
                            memory.label()
                        ),
                        many => anyhow!("a project is a repo — did you mean one of: {}", many.join(", ")),
                    });
                }
            }
            // With no decision flags, a terminal gets the interactive review; scripts and pipes
            // (and the FUNES_NO_TUI opt-out) get the plain text listing.
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if include.is_empty() && exclude.is_empty() && interactive && std::env::var_os("FUNES_NO_TUI").is_none() {
                curate_review(&memory, project.as_deref()).await
            } else {
                print!(
                    "{}",
                    curate::run(&memory, project.as_deref(), &include, &exclude).await?
                );
                Ok(())
            }
        }
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update::run(force).await,
        Cmd::Mcp { memory } => mcp::run(memory).await,
        Cmd::Add { agent } => match agent {
            // Claude, Codex, and Hermes have the full local pipeline (index + hooks + push), so `add`
            // bootstraps it: build the first index and do the first push — the two one-time steps
            // the hooks can't do unattended — so nothing is left to run by hand.
            AddAgent::Claude { memory } => {
                bootstrap_add(Harness::Claude, resolve_add_memory(memory).await?, claude::install).await
            }
            AddAgent::Codex { memory } => {
                bootstrap_add(Harness::Codex, resolve_add_memory(memory).await?, codex::install).await
            }
            AddAgent::Hermes { memory } => {
                bootstrap_add(Harness::Hermes, resolve_add_memory(memory).await?, hermes::install).await
            }
            // The rest register a read-side integration only (no local pipeline to bootstrap), so
            // they just take the resolved memory — the `created` flag only matters to the first push.
            AddAgent::Pi { memory, force } => pi::install(resolve_add_memory(memory).await?.map(|r| r.memory), force),
        },
    }
}

/// A resolved memory binding: the memory spec, and whether funes just created the repo this run — the
/// signal the first push uses to skip the wrong-memory guard (an empty repo funes made for the user
/// is plainly not "the wrong memory").
struct Resolved {
    memory: String,
    created: bool,
}

/// Resolve the memory `funes add` binds. An explicitly-named memory is validated — offer to create it
/// if it's missing on the Hub (a typo guard). With no memory, offer to set one up on the Hub when a
/// token is present (`<user>/funes-memory`); otherwise stay local.
async fn resolve_add_memory(raw: AddMemory) -> Result<Option<Resolved>> {
    match baked_memory(raw) {
        Some(memory) => {
            let created = ensure_remote_exists(&memory).await?;
            Ok(Some(Resolved { memory, created }))
        }
        None => offer_hub_memory().await,
    }
}

/// The deferred creation the interactive review runs once you've included sessions: materialize
/// `memory` as the project memory of `project`, with consent. `create_repo` makes the Hub repo first
/// (the memory was absent); otherwise it exists (a personal memory) and we only stamp it. Returns
/// whether it happened — declining stops cleanly, publishing nothing.
/// The interactive review behind `funes curate <memory>` in a terminal: the project's candidate
/// sessions in the in-process [`funes::tui`] picker where `→` includes a session and `←` excludes it
/// (the same arrow again clears to pending), the preview showing each session's user prompts.
/// Decisions persist as they're made; leaving summarizes, and — once something is included — offers
/// the push, materializing the memory as the project memory first when it isn't one yet.
async fn curate_review(memory: &hub::Memory, project: Option<&str>) -> Result<()> {
    // Resolve without creating: `materialize` is None when the memory is already the project memory,
    // else Some(create_repo) — the deferred creation to run at the close if anything is included.
    let (uri, project, materialize) = match curate::prepare(memory, project).await? {
        curate::Prepared::Ready { uri, project } => (uri, project, None),
        curate::Prepared::Absent { uri, project } => (uri, project, Some(true)),
        curate::Prepared::Personal { uri, project } => (uri, project, Some(false)),
    };
    let found = curate::candidates(memory, &uri, &project, true).await?;
    if found.matched.is_empty() {
        let skipped = found.other.len() + found.unresolvable.len();
        if skipped > 0 {
            println!("project memory of {project} — no local session resolves to {project}");
            println!("  ({skipped} session(s) resolve to other repos or have no resolvable checkout)");
        } else {
            println!("project memory of {project} — nothing new to review");
        }
        return Ok(());
    }

    // Pre-render each candidate's user prompts (scaffolding dropped) — the preview pane, and
    // (whitespace-collapsed) the surface the fuzzy filter searches beyond the visible row.
    let ids: Vec<String> = found.matched.iter().map(|s| s.session_id.clone()).collect();
    let mut previews = recall::session_prompts(&hub::Memory::local(), &ids).await?;
    for turns in previews.values_mut() {
        for turn in turns.iter_mut() {
            turn.blocks.retain(|b| !curate::is_scaffolding(b));
        }
        turns.retain(|turn| !turn.blocks.is_empty());
    }
    let (color, width) = human_io();
    let items: Vec<funes::tui::curate::Candidate> = found
        .matched
        .iter()
        .map(|s| {
            let body = previews
                .get(&s.session_id)
                .map(|turns| render::get_human("", turns, color, width, None))
                .unwrap_or_default();
            let filter: String = body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(2000)
                .collect();
            funes::tui::curate::Candidate {
                id: s.session_id.clone(),
                date: s.date().to_string(),
                prompt: s.first_prompt.clone(),
                comment: format!("{} {}", s.date(), s.first_prompt).trim().to_string(),
                filter,
                chunks: s.chunks,
                preview: funes::tui::ansi_to_text(&body),
            }
        })
        .collect();
    funes::tui::curate::run(uri.clone(), project.clone(), items)?;

    // The review persisted every decision, so leaving just summarizes and offers the push.
    let curation = curate::load(&uri)?.unwrap_or_default();
    // A stale include (the session grew since it was reviewed) counts as pending here, not as a
    // fresh include — it won't ship until it's reviewed again.
    let inc = found
        .matched
        .iter()
        .filter(|s| curation.include.contains(&s.session_id) && !curation.is_stale(&s.session_id, s.chunks))
        .count();
    let exc = found
        .matched
        .iter()
        .filter(|s| curation.exclude.contains(&s.session_id))
        .count();
    println!(
        "project memory of {project} — {inc} include, {exc} exclude, {} pending",
        found.matched.len() - inc - exc
    );
    if inc == 0 {
        return Ok(()); // nothing included — nothing to publish, and no memory created
    }
    // Now there's something to publish. A memory that already exists just needs the push; a missing
    // or personal one is materialized as the project memory first (its consent doubles as the
    // publish consent). Either way, ship on a yes.
    let publish = match materialize {
        None => confirm(&format!("push {} now? [Y/n] ", memory.label()), true),
        Some(create_repo) => create_project_memory(memory, &project, create_repo, inc).await?,
    };
    if publish {
        match push::run_push(memory.clone(), false, push::Confirm::Yes).await {
            Ok(pushed) => print!("{}", pushed.report),
            Err(e) if push::is_read_only(&e) => eprintln!(
                "{} is read-only for your token — recall can read it, but publishing needs write access.",
                memory.label()
            ),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn create_project_memory(
    memory: &hub::Memory,
    project: &str,
    create_repo: bool,
    includes: usize,
) -> Result<bool> {
    let hub::Memory::Remote { uri } = memory else {
        return Ok(false);
    };
    let prompt = format!(
        "publish {includes} session(s) to {} as the project memory of {project}? [Y/n] ",
        memory.label()
    );
    if !confirm(&prompt, true) {
        eprintln!("nothing published; {} left as is.", memory.label());
        return Ok(false);
    }
    if create_repo {
        let (owner, name, _) = hub::parse_hf(uri)?;
        hub::create_dataset_repo(&owner, &name).await?;
    }
    curate::name_project(memory, project).await?;
    eprintln!("{} is now the project memory of {project}.", memory.label());
    Ok(true)
}

/// Validate an explicitly-named memory: fine if it exists; offer to create it if missing (default
/// **no**, to catch typos); warn but proceed if the Hub is unreachable. Returns whether it created
/// the repo.
async fn ensure_remote_exists(remote: &str) -> Result<bool> {
    let hub::Memory::Remote { uri } = hub::Memory::parse(remote) else {
        return Ok(false); // a local path — nothing to check on the Hub
    };
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Ok => Ok(false),
        hub::Reachability::Offline => {
            eprintln!("note: can't reach {remote} right now — proceeding; it'll be used once it's back.");
            Ok(false)
        }
        hub::Reachability::Missing => {
            let (owner, name, _) = hub::parse_hf(&uri)?;
            if std::io::stdin().is_terminal()
                && confirm(&format!("{remote} doesn't exist on the Hub. Create it? [y/N] "), false)
            {
                hub::create_dataset_repo(&owner, &name).await?;
                eprintln!("created {owner}/{name}.");
                Ok(true)
            } else {
                Err(hub::missing_remote(&uri))
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
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Ok => {
            Ok(confirm(&format!("Use your memory {memory}? [Y/n] "), true)
                .then_some(Resolved { memory, created: false }))
        }
        hub::Reachability::Offline => {
            eprintln!("can't reach the Hub right now — staying local; re-run when you're online.");
            Ok(None)
        }
        hub::Reachability::Missing => {
            if confirm(&format!("Create {memory} for your memory? [Y/n] "), true) {
                hub::create_dataset_repo(&user, "funes-memory").await?;
                eprintln!("created {memory}.");
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

/// `funes add claude|codex [memory]` for the agents with a full local pipeline: bootstrap the
/// one-time steps the hooks can't do unattended, around the per-agent `install` (hooks + MCP).
///
/// 1. ask, then build the first index if the local memory is missing (so recall/push have content);
///    declining aborts the add — nothing is installed;
/// 2. `install` — register hooks + MCP (bakes the memory);
/// 3. first push if a memory is bound — clears the overlap guard so the push hook works thereafter.
async fn bootstrap_add(
    harness: Harness,
    resolved: Option<Resolved>,
    install: impl FnOnce(Option<String>) -> Result<()>,
) -> Result<()> {
    if !ensure_local_index(harness).await {
        eprintln!(
            "funes: skipped — nothing installed. Run `funes add {}` again when you're ready.",
            harness.cli_name()
        );
        return Ok(());
    }
    install(resolved.as_ref().map(|r| r.memory.clone()))?;
    // First push only when there's actually a local index to publish. Without one (a failed first
    // build, or no sessions yet) there's nothing to push, and running it would just error on the
    // absent memory.
    if let Some(Resolved { memory, created }) = resolved {
        if hub::Memory::local().open().await.is_ok() {
            first_push(&memory, created).await?;
        } else {
            eprintln!("funes: nothing indexed yet — nothing to publish to {memory} yet. Run `funes index`, and the hooks keep it current from there.");
        }
    }
    Ok(())
}

/// Build the first index from `harness`'s sessions when the local memory is missing — asking first,
/// since it's about a minute of work. Returns whether `add` should proceed: declining (or EOF — a
/// no-TTY run reads none) returns `false`, so the caller installs nothing. An empty/absent session
/// dir or a build error is a note, not a decline: the hooks still go in and `funes index` builds
/// it later.
async fn ensure_local_index(harness: Harness) -> bool {
    if hub::Memory::local().open().await.is_ok() {
        return true; // already have a local index
    }
    let Some(root) = funes::harness::known_harness_roots()
        .into_iter()
        .find(|(_, h)| *h == harness)
    else {
        eprintln!(
            "funes: no {} sessions found yet — the hooks are installed; run `funes index` once you've used it.",
            harness.cli_name()
        );
        return true;
    };
    if !confirm(
        &format!(
            "funes will index your recent {} sessions so recall works (about a minute). Proceed? [Y/n] ",
            harness.cli_name()
        ),
        true,
    ) {
        return false;
    }
    eprintln!("funes: indexing your recent {} sessions…", harness.cli_name());
    if let Err(e) = index::run_index_seed(&root.0, harness).await {
        eprintln!(
            "funes: initial index didn't complete ({e:#}) — the hooks are installed; run `funes index` to build it."
        );
    }
    true
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
    match push::run_push(hub::Memory::parse(remote), false, confirm).await {
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
