//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::harness::Harness;
use funes::recall::Hit;
use funes::{claude, codex, curate, hello, hermes, hub, index, mcp, pi, push, recall, render, scrub, update};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Turns around a `get` target when no window is given — shared by the CLI flag and the hit selector.
const DEFAULT_WINDOW: i64 = 3;

#[derive(Parser)]
#[command(name = "funes", version, about = "Recall over your past AI agent sessions.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print a short, human-readable guide to funes — the friendly first run (no index needed).
    Guide,
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
        /// Output format. Default: human in a terminal, agent when piped.
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// List indexed sessions, newest activity first.
    List {
        /// Store to read — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
        /// Defaults to your local store.
        store: Option<String>,
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
        #[arg(long, default_value_t = DEFAULT_WINDOW)]
        window: i64,
        /// Output format. Default: human in a terminal, agent when piped.
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
        /// Highlight this text in the human rendering (matched whitespace-insensitively).
        #[arg(long)]
        highlight: Option<String>,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Build or update your local store from session transcripts.
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
        /// Store to inspect — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or
        /// `local`. Defaults to your local store.
        store: Option<String>,
    },
    /// Publish your local store's new chunks to a remote store on the HF Hub.
    Push {
        /// Store to publish to: `<org>/<repo>` or a full `hf://…` URI.
        store: String,
        /// Skip the confirmation when the target shares no chunks with your local store.
        #[arg(short, long)]
        yes: bool,
        /// Refresh the remote index after pushing (retrying on conflict) even if the unindexed
        /// backlog is below the auto-reindex threshold. With nothing new to push, reindex only.
        #[arg(long)]
        force_reindex: bool,
    },
    /// Curate a project memory: a store that ships only the sessions you've reviewed and marked
    /// `include`. Your review alone decides what `funes push` ships there.
    Curate {
        /// The store — the Hub dataset the memory lives in: `<org>/<repo>` or a full `hf://…` URI.
        store: String,
        /// The project the store is the memory of — the git repo it's about (`huggingface/funes`)
        /// or a plain label. Give it the first time to name the store; omit to review.
        project: Option<String>,
        /// Mark these sessions `include` — they ship on the next push to this store.
        #[arg(long, value_name = "SESSION")]
        include: Vec<String>,
        /// Mark these sessions `exclude` — held back from this store.
        #[arg(long, value_name = "SESSION")]
        exclude: Vec<String>,
    },
    /// Redact secrets from your local store in place — for rows indexed before redaction existed (or
    /// flagged by an updated ruleset); needs no source transcript. Cleans the local store only: it
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
        /// Store to serve — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
        /// Defaults to your local store.
        store: Option<String>,
    },
    /// Add funes to a coding agent.
    ///
    /// Installs the `recall`/`get` tools for any agent — and, for claude, codex, and hermes,
    /// automatic per-turn indexing. Name a store the agent recalls from — and, for claude, codex,
    /// and hermes, publishes to — an `<org>/<repo>` shorthand or an `hf://…` URI; omit it to stay
    /// local (the default).
    #[command(
        subcommand_value_name = "AGENT",
        subcommand_help_heading = "Agents",
        override_usage = "funes add <AGENT> [STORE]"
    )]
    Add {
        #[command(subcommand)]
        agent: AddAgent,
    },
}

// Flattened into every agent so they share one optional `[STORE]` positional; the user-facing help
// comes from the field doc below.
#[derive(Args)]
struct AddStore {
    /// Store this agent recalls from — `<org>/<repo>`, an `hf://…` URI, or `local` (default).
    store: Option<String>,
}

#[derive(Subcommand)]
enum AddAgent {
    Claude {
        #[command(flatten)]
        store: AddStore,
    },
    Codex {
        #[command(flatten)]
        store: AddStore,
    },
    Pi {
        #[command(flatten)]
        store: AddStore,
        /// Reinstall even if the on-disk copy is already up to date.
        #[arg(long)]
        force: bool,
    },
    Hermes {
        #[command(flatten)]
        store: AddStore,
    },
}

/// The store to bake into an agent's `funes mcp` registration: `None`/blank/`local` → the local
/// store (a bare `funes mcp`), else the named remote/explicit store (`funes mcp <store>`).
fn baked_store(store: AddStore) -> Option<String> {
    store
        .store
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "local")
}

/// Which store the read commands act on. Shared by `recall`/`list`/`get`/`status` and `mcp`.
#[derive(Args)]
struct StoreOpts {
    /// The store to read — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`.
    /// Defaults to your local store.
    #[arg(long)]
    store: Option<String>,
}

impl StoreOpts {
    fn resolve(self) -> hub::Store {
        hub::Store::resolve(self.store)
    }
}

/// The two output layouts for the read commands.
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
        Cmd::Guide => {
            print!("{}", hello::guide());
            Ok(())
        }
        Cmd::Recall {
            query,
            k,
            candidates,
            half_life,
            neighbors,
            block_type,
            harness,
            format,
            store,
        } => {
            let store = store.resolve();
            let query = query.join(" ");
            let spinner = Spinner::start("recalling…");
            let progress = |label: &str| {
                if let Some(s) = &spinner {
                    s.set(label);
                }
            };
            let (note, store_label, hits) = recall::recall_hits(
                store.clone(),
                query.clone(),
                k,
                candidates,
                half_life,
                neighbors,
                block_type,
                harness,
                &progress,
            )
            .await?;
            drop(spinner);
            match OutputFormat::resolve(format) {
                OutputFormat::Agent => {
                    if hits.is_empty() {
                        print!("{note}no results");
                    } else {
                        print!(
                            "{}",
                            render::recall_agent(&note, &recall::store_hint(store_label.as_deref()), &hits)
                        );
                    }
                    Ok(())
                }
                OutputFormat::Human => {
                    if hits.is_empty() {
                        println!("{note}no results");
                        return Ok(());
                    }
                    let (color, width) = human_io();
                    // The in-process browser owns the whole terminal, so it needs real TTYs; a
                    // forced human format without them (or the FUNES_NO_TUI opt-out) keeps the
                    // plain line selector. It runs on its own thread — not a runtime worker — so it
                    // can `block_on` a session load when drilling into a hit.
                    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
                    if interactive && std::env::var_os("FUNES_NO_TUI").is_none() {
                        let rt = tokio::runtime::Handle::current();
                        std::thread::scope(|s| {
                            match s
                                .spawn(|| funes::tui::browser::run(store, note, query, &hits, color, width, rt))
                                .join()
                            {
                                Ok(res) => res,
                                Err(_) => Err(anyhow!("recall browser thread panicked")),
                            }
                        })
                    } else {
                        print!("{note}");
                        select_hits(&store, &hits, color, width).await
                    }
                }
            }
        }
        Cmd::List { store, limit } => {
            print!("{}", recall::list(hub::Store::resolve(store), limit).await?);
            Ok(())
        }
        Cmd::Get {
            session_id,
            turn_uuid,
            window,
            format,
            highlight,
            store,
        } => {
            let format = OutputFormat::resolve(format);
            let (note, turns) =
                recall::get_turns(store.resolve(), session_id.clone(), turn_uuid.clone(), window).await?;
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
                    let hub::Store::Remote { uri } = hub::Store::parse(&p) else {
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
        Cmd::Status { store } => {
            print!("{}", recall::status(hub::Store::resolve(store)).await?);
            // Show the status body before the (bounded, best-effort) update check, so a slow or
            // offline Hub can't delay the useful output.
            std::io::stdout().flush().ok();
            if let Some(notice) = update::upgrade_notice().await {
                print!("{notice}");
            }
            Ok(())
        }
        Cmd::Push {
            store: remote,
            yes,
            force_reindex,
        } => {
            let confirm = if yes {
                push::Confirm::Yes
            } else {
                push::Confirm::Ask(prompt_new_store)
            };
            match push::run_push(hub::Store::parse(&remote), force_reindex, confirm).await {
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
            store,
            project,
            include,
            exclude,
        } => {
            let store = hub::Store::parse(&store);
            // A project memory is of a git repo — funes attributes sessions to it by their
            // checkout's remotes, so the project must be a repo identity (`owner/name`). A bare
            // name gets a "did you mean", inferring the owner from local repos with that name.
            if let Some(project) = project.as_deref() {
                if !project.contains('/') {
                    return Err(match curate::projects_named(project).await?.as_slice() {
                        [] => anyhow!("a project is a repo, like `huggingface/transformers` — got `{project}`"),
                        [one] => anyhow!(
                            "a project is a repo — did you mean `{one}`?  run: funes curate {} {one}",
                            store.label()
                        ),
                        many => anyhow!("a project is a repo — did you mean one of: {}", many.join(", ")),
                    });
                }
            }
            // With no decision flags, a terminal gets the interactive review; scripts and pipes
            // (and the FUNES_NO_TUI opt-out) get the plain text listing.
            let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            if include.is_empty() && exclude.is_empty() && interactive && std::env::var_os("FUNES_NO_TUI").is_none() {
                curate_review(&store, project.as_deref()).await
            } else {
                print!("{}", curate::run(&store, project.as_deref(), &include, &exclude).await?);
                Ok(())
            }
        }
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update::run(force).await,
        Cmd::Mcp { store } => mcp::run(store).await,
        Cmd::Add { agent } => match agent {
            // Claude, Codex, and Hermes have the full local pipeline (index + hooks + push), so `add`
            // bootstraps it: build the first index and do the first push — the two one-time steps
            // the hooks can't do unattended — so nothing is left to run by hand.
            AddAgent::Claude { store } => {
                bootstrap_add(Harness::Claude, resolve_add_store(store).await?, claude::install).await
            }
            AddAgent::Codex { store } => {
                bootstrap_add(Harness::Codex, resolve_add_store(store).await?, codex::install).await
            }
            AddAgent::Hermes { store } => {
                bootstrap_add(Harness::Hermes, resolve_add_store(store).await?, hermes::install).await
            }
            // The rest register a read-side integration only (no local pipeline to bootstrap), so
            // they just take the resolved store — the `created` flag only matters to the first push.
            AddAgent::Pi { store, force } => pi::install(resolve_add_store(store).await?.map(|r| r.store), force),
        },
    }
}

/// A resolved store binding: the store spec, and whether funes just created the repo this run — the
/// signal the first push uses to skip the wrong-store guard (an empty repo funes made for the user
/// is plainly not "the wrong store").
struct Resolved {
    store: String,
    created: bool,
}

/// Resolve the store `funes add` binds. An explicitly-named store is validated — offer to create it
/// if it's missing on the Hub (a typo guard). With no store, offer to set one up on the Hub when a
/// token is present (`<user>/funes-store`); otherwise stay local.
async fn resolve_add_store(raw: AddStore) -> Result<Option<Resolved>> {
    match baked_store(raw) {
        Some(store) => {
            let created = ensure_remote_exists(&store).await?;
            Ok(Some(Resolved { store, created }))
        }
        None => offer_hub_store().await,
    }
}

/// The deferred creation the interactive review runs once you've included sessions: materialize
/// `store` as the project memory of `project`, with consent. `create_repo` makes the Hub repo first
/// (the store was absent); otherwise it exists (a personal memory) and we only stamp it. Returns
/// whether it happened — declining stops cleanly, publishing nothing.
/// The interactive review behind `funes curate <store>` in a terminal: the project's candidate
/// sessions in the in-process [`funes::tui`] picker where `→` includes a session and `←` excludes it
/// (the same arrow again clears to pending), the preview showing each session's user prompts.
/// Decisions persist as they're made; leaving summarizes, and — once something is included — offers
/// the push, materializing the store as the project memory first when it isn't one yet.
async fn curate_review(store: &hub::Store, project: Option<&str>) -> Result<()> {
    // Resolve without creating: `materialize` is None when the store is already the project memory,
    // else Some(create_repo) — the deferred creation to run at the close if anything is included.
    let (uri, project, materialize) = match curate::prepare(store, project).await? {
        curate::Prepared::Ready { uri, project } => (uri, project, None),
        curate::Prepared::Absent { uri, project } => (uri, project, Some(true)),
        curate::Prepared::Personal { uri, project } => (uri, project, Some(false)),
    };
    let found = curate::candidates(store, &uri, &project, true).await?;
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
    let mut previews = recall::session_prompts(&hub::Store::local(), &ids).await?;
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
        return Ok(()); // nothing included — nothing to publish, and no store created
    }
    // Now there's something to publish. A store that already exists just needs the push; a missing
    // or personal one is materialized as the project memory first (its consent doubles as the
    // publish consent). Either way, ship on a yes.
    let publish = match materialize {
        None => confirm(&format!("push {} now? [Y/n] ", store.label()), true),
        Some(create_repo) => create_project_memory(store, &project, create_repo, inc).await?,
    };
    if publish {
        match push::run_push(store.clone(), false, push::Confirm::Yes).await {
            Ok(pushed) => print!("{}", pushed.report),
            Err(e) if push::is_read_only(&e) => eprintln!(
                "{} is read-only for your token — recall can read it, but publishing needs write access.",
                store.label()
            ),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

async fn create_project_memory(store: &hub::Store, project: &str, create_repo: bool, includes: usize) -> Result<bool> {
    let hub::Store::Remote { uri } = store else {
        return Ok(false);
    };
    let prompt = format!(
        "publish {includes} session(s) to {} as the project memory of {project}? [Y/n] ",
        store.label()
    );
    if !confirm(&prompt, true) {
        eprintln!("nothing published; {} left as is.", store.label());
        return Ok(false);
    }
    if create_repo {
        let (owner, name, _) = hub::parse_hf(uri)?;
        hub::create_dataset_repo(&owner, &name).await?;
    }
    curate::name_project(store, project).await?;
    eprintln!("{} is now the project memory of {project}.", store.label());
    Ok(true)
}

/// Validate an explicitly-named store: fine if it exists; offer to create it if missing (default
/// **no**, to catch typos); warn but proceed if the Hub is unreachable. Returns whether it created
/// the repo.
async fn ensure_remote_exists(remote: &str) -> Result<bool> {
    let hub::Store::Remote { uri } = hub::Store::parse(remote) else {
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

/// With no store named, offer to set one up on the Hub — but only when a token is present and we can
/// prompt. Suggests `<user>/funes-store`: use it if it exists, offer to create it if not. Returns the
/// store to bind (with whether it was just created), or `None` to stay local.
async fn offer_hub_store() -> Result<Option<Resolved>> {
    let interactive = std::io::stdin().is_terminal();
    if !hub::has_token() {
        if interactive {
            eprintln!("staying local — set HF_TOKEN (or run `hf auth login`) and re-run `funes add …` to sync across machines or a team.");
        }
        return Ok(None);
    }
    // A scripted (non-interactive) add can't prompt, so it stays local unless a store was named.
    if !interactive
        || !confirm(
            "Sync your memory to a Hugging Face store, so it follows you across machines? [Y/n] ",
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
    let store = format!("{user}/funes-store");
    let uri = format!("hf://datasets/{store}");
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Ok => {
            Ok(confirm(&format!("Use your store {store}? [Y/n] "), true).then_some(Resolved { store, created: false }))
        }
        hub::Reachability::Offline => {
            eprintln!("can't reach the Hub right now — staying local; re-run when you're online.");
            Ok(None)
        }
        hub::Reachability::Missing => {
            if confirm(&format!("Create {store} for your memory? [Y/n] "), true) {
                hub::create_dataset_repo(&user, "funes-store").await?;
                eprintln!("created {store}.");
                Ok(Some(Resolved { store, created: true }))
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

/// `funes add claude|codex [store]` for the agents with a full local pipeline: bootstrap the
/// one-time steps the hooks can't do unattended, around the per-agent `install` (hooks + MCP).
///
/// 1. ask, then build the first index if the local store is missing (so recall/push have content);
///    declining aborts the add — nothing is installed;
/// 2. `install` — register hooks + MCP (bakes the store);
/// 3. first push if a store is bound — clears the overlap guard so the push hook works thereafter.
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
    install(resolved.as_ref().map(|r| r.store.clone()))?;
    // First push only when there's actually a local index to publish. Without one (a failed first
    // build, or no sessions yet) there's nothing to push, and running it would just error on the
    // absent store.
    if let Some(Resolved { store, created }) = resolved {
        if hub::Store::local().open().await.is_ok() {
            first_push(&store, created).await?;
        } else {
            eprintln!("funes: nothing indexed yet — nothing to publish to {store} yet. Run `funes index`, and the hooks keep it current from there.");
        }
    }
    Ok(())
}

/// Build the first index from `harness`'s sessions when the local store is missing — asking first,
/// since it's about a minute of work. Returns whether `add` should proceed: declining (or EOF — a
/// no-TTY run reads none) returns `false`, so the caller installs nothing. An empty/absent session
/// dir or a build error is a note, not a decline: the hooks still go in and `funes index` builds
/// it later.
async fn ensure_local_index(harness: Harness) -> bool {
    if hub::Store::local().open().await.is_ok() {
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

/// The one-time first publish `add` performs when a store is bound (the push hook can't, off a
/// terminal — the overlap guard fails closed there). The guard prompts before publishing to a store
/// this host shares no chunks with — unless funes just `created` the store this run, which is
/// plainly the user's own empty repo, so the push proceeds without re-asking. Errors that aren't
/// fatal to the install (a read-only token, held-back secrets) are reported without failing `add`.
async fn first_push(remote: &str, created: bool) -> Result<()> {
    let confirm = if created {
        push::Confirm::Yes
    } else {
        push::Confirm::Ask(prompt_new_store)
    };
    match push::run_push(hub::Store::parse(remote), false, confirm).await {
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

/// The hit selector after a human recall: prints the numbered menu, then a typed number expands
/// that hit via `get` (`3 10` widens the window) and the menu reprints — the walk-back. An empty
/// line, `q`, or end-of-input quits.
async fn select_hits(store: &hub::Store, hits: &[(Hit, f64)], color: bool, width: usize) -> Result<()> {
    let menu = render::recall_human("", hits, color, width, chrono::Utc::now());
    let hint = render::dim(
        "type a number to expand a hit (`3 10` widens the context) — enter or q quits",
        color,
    );
    print!("{menu}");
    println!("{hint}");
    let mut line = String::new();
    loop {
        print!("› ");
        std::io::stdout().flush().ok();
        line.clear();
        if std::io::stdin().read_line(&mut line)? == 0 {
            println!();
            return Ok(());
        }
        match parse_selection(line.trim(), hits.len()) {
            Selection::Quit => return Ok(()),
            Selection::Help => {
                println!(
                    "1–{} expands a hit (`3 10` widens the context); enter or q quits",
                    hits.len()
                )
            }
            Selection::Expand { ordinal, window } => {
                let h = &hits[ordinal - 1].0;
                match recall::get_turns(store.clone(), h.session_id.clone(), h.turn_uuid.clone(), window).await {
                    Ok((note, turns)) if turns.is_empty() => {
                        print!("{note}");
                        println!("turn {} not found in session {}", h.turn_uuid, h.session_id);
                    }
                    Ok((note, turns)) => {
                        // Mark the matched chunk so it stands out of the surrounding turns.
                        let mark: String = h.text.split_whitespace().collect::<Vec<_>>().join(" ");
                        print!("{}", render::get_human(&note, &turns, color, width, Some(&mark)));
                        println!(
                            "{}",
                            render::dim(
                                &format!(
                                    "funes get {} {} --window {window} --store {}",
                                    h.session_id,
                                    h.turn_uuid,
                                    funes::tui::sh_word(&store.label())
                                ),
                                color
                            )
                        );
                    }
                    // A transient failure (say, the remote dropped) shouldn't kill the session.
                    Err(e) => println!("get failed: {e:#}"),
                }
                // Back to the picker: the expansion pushed the menu out of view.
                println!();
                print!("{menu}");
                println!("{hint}");
            }
        }
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

/// One parsed selection.
enum Selection {
    Quit,
    Help,
    Expand { ordinal: usize, window: i64 },
}

/// `""`/`q`/`quit` quit; `N` expands hit N with the default window; `N W` widens it to W.
fn parse_selection(line: &str, hits: usize) -> Selection {
    if line.is_empty() || line.eq_ignore_ascii_case("q") || line.eq_ignore_ascii_case("quit") {
        return Selection::Quit;
    }
    let mut parts = line.split_whitespace();
    let ordinal = match parts.next().and_then(|t| t.parse::<usize>().ok()) {
        Some(o) if (1..=hits).contains(&o) => o,
        _ => return Selection::Help,
    };
    let window = match parts.next() {
        None => DEFAULT_WINDOW,
        Some(t) => match t.parse::<i64>() {
            Ok(w) if w >= 0 => w,
            _ => return Selection::Help,
        },
    };
    if parts.next().is_some() {
        return Selection::Help;
    }
    Selection::Expand { ordinal, window }
}

/// The push confirmation for a store the local index shares no chunks with. Fails closed (returns
/// false) off a terminal, so an unattended push can't silently publish to the wrong store — there it
/// must be re-run with `--yes`.
fn prompt_new_store(label: &str, chunks: usize) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "refusing to push {chunks} chunk(s) to {label}: your local store shares no chunks with it \
             (a first push, a new host, or the wrong store) — re-run with `--yes` to confirm."
        );
        return false;
    }
    eprint!(
        "{label}: your local store shares no chunks with it — a first push here, a new host of yours, \
         or the wrong store. Publish {chunks} chunk(s) anyway? [y/N] "
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
    use super::{parse_confirm, parse_selection, Selection};

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
    fn parse_selection_grammar() {
        assert!(matches!(parse_selection("", 8), Selection::Quit));
        assert!(matches!(parse_selection("q", 8), Selection::Quit));
        assert!(matches!(parse_selection("QUIT", 8), Selection::Quit));
        assert!(matches!(
            parse_selection("3", 8),
            Selection::Expand { ordinal: 3, window: 3 }
        ));
        assert!(matches!(
            parse_selection("3 10", 8),
            Selection::Expand { ordinal: 3, window: 10 }
        ));
        // Out of range, not a number, negative window, trailing junk.
        assert!(matches!(parse_selection("9", 8), Selection::Help));
        assert!(matches!(parse_selection("0", 8), Selection::Help));
        assert!(matches!(parse_selection("x", 8), Selection::Help));
        assert!(matches!(parse_selection("3 -1", 8), Selection::Help));
        assert!(matches!(parse_selection("3 10 zzz", 8), Selection::Help));
    }
}
