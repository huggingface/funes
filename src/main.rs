//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::harness::Harness;
use funes::recall::Hit;
use funes::{claude, codex, config, hello, hermes, hub, index, mcp, opencode, pi, push, recall, render, scrub, update};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

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
        /// Restrict to a project (the path segment under `projects`).
        #[arg(long)]
        project: Option<String>,
        /// Restrict to a harness: claude | codex | pi.
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
        #[arg(long, default_value_t = DEFAULT_WINDOW)]
        window: i64,
        /// Output format. Default: human in a terminal, agent when piped.
        #[arg(long, value_enum)]
        format: Option<OutputFormat>,
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
        /// Override harness auto-detection for PATH: claude | codex | pi.
        #[arg(long)]
        harness: Option<String>,
        /// Exclude thinking blocks.
        #[arg(long)]
        no_thinking: bool,
        /// Index only the most recent N sessions per source. Omit to index all.
        #[arg(long)]
        limit: Option<usize>,
        /// Skip the first-index size confirmation, and allow a first index from a non-interactive
        /// run (a hook/cron) — which is otherwise refused so the long initial build stays manual.
        #[arg(long)]
        yes: bool,
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
        /// URI. Pass `local` to detach and go back to your local store.
        store: String,
    },
    /// Publish your local store's new chunks to a remote store on the HF Hub — the active store by
    /// default, or `[store]` to publish elsewhere.
    Push {
        /// Store to publish to: `<org>/<repo>` or a full `hf://…` URI. Defaults to the active store.
        store: Option<String>,
        /// Skip the confirmation when the target shares no chunks with your local store.
        #[arg(short, long)]
        yes: bool,
        /// Refresh the remote index after pushing (retrying on conflict) even if the unindexed
        /// backlog is below the auto-reindex threshold. With nothing new to push, reindex only.
        #[arg(long)]
        force_reindex: bool,
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
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Add funes to a coding agent.
    #[command(subcommand_value_name = "AGENT", subcommand_help_heading = "Agents")]
    Add {
        #[command(subcommand)]
        agent: AddAgent,
    },
}

#[derive(Subcommand)]
enum AddAgent {
    /// claude: register funes as an MCP server with Claude Code (native MCP client, user scope).
    Claude,
    /// codex: register funes as an MCP server with Codex (native MCP client, user scope).
    Codex,
    /// pi: install funes as a pi extension user-wide (pi has no MCP client of its own).
    Pi {
        /// Reinstall even if the on-disk copy is already up to date.
        #[arg(long)]
        force: bool,
    },
    /// hermes: register funes as an MCP server (hermes has a native MCP client).
    Hermes,
    /// opencode: register funes as an MCP server (user scope).
    Opencode,
}

/// Which store the read commands act on. Shared by `recall`/`list`/`get`/`status` and `mcp`.
#[derive(Args)]
struct StoreOpts {
    /// The store to read — an `<org>/<repo>` shorthand, an `hf://…` URI, a local path, or `local`
    /// — overriding the active store. Defaults to the active store, else your local store.
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
    /// The stable machine layout.
    Agent,
}

impl OutputFormat {
    /// Resolve the effective format: an explicit flag wins; otherwise human when both stdin and
    /// stdout are terminals (the hit selector needs both), agent when piped or scripted.
    fn human(flag: Option<OutputFormat>) -> bool {
        match flag {
            Some(OutputFormat::Human) => true,
            Some(OutputFormat::Agent) => false,
            None => std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        }
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
            project,
            harness,
            format,
            store,
        } => {
            let store = store.resolve();
            let (note, store_label, hits) = recall::recall_hits(
                store.clone(),
                query.join(" "),
                k,
                candidates,
                half_life,
                neighbors,
                block_type,
                project,
                harness,
            )
            .await?;
            if !OutputFormat::human(format) {
                if hits.is_empty() {
                    print!("{note}no results");
                } else {
                    print!(
                        "{}",
                        render::recall_agent(&note, &recall::store_hint(store_label.as_deref()), &hits)
                    );
                }
                return Ok(());
            }
            if hits.is_empty() {
                println!("{note}no results");
                return Ok(());
            }
            let (color, width) = human_io();
            print!("{note}");
            select_hits(&store, &hits, color, width).await
        }
        Cmd::List { project, limit, store } => {
            print!("{}", recall::list(store.resolve(), project, limit).await?);
            Ok(())
        }
        Cmd::Get {
            session_id,
            turn_uuid,
            window,
            format,
            store,
        } => {
            let (note, turns) =
                recall::get_turns(store.resolve(), session_id.clone(), turn_uuid.clone(), window).await?;
            if turns.is_empty() {
                print!("{note}");
                println!("turn {turn_uuid} not found in session {session_id}");
            } else if OutputFormat::human(format) {
                let (color, width) = human_io();
                print!("{}", render::get_human(&note, &turns, color, width));
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
                            "automated `funes index` needs a target — pass a path or `--harness <claude|codex|pi>`; \
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
                    "no local sessions found — looked in ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions"
                ));
            }
            index::run_index_roots(&roots, no_thinking, limit, yes).await
        }
        Cmd::Status { store } => {
            print!("{}", recall::status(store.resolve()).await?);
            // Show the status body before the (bounded, best-effort) update check, so a slow or
            // offline Hub can't delay the useful output.
            std::io::stdout().flush().ok();
            if let Some(notice) = update::upgrade_notice().await {
                print!("{notice}");
            }
            Ok(())
        }
        Cmd::Use { store } => use_store(store).await,
        Cmd::Push {
            store,
            yes,
            force_reindex,
        } => {
            let remote = match store {
                Some(s) => s,
                None => config::load().remote.ok_or_else(|| {
                    anyhow!("no active store — attach one with `funes use <org>/<repo>`, or name a store: `funes push <org>/<repo>`")
                })?,
            };
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
        Cmd::Scrub => scrub::run().await,
        Cmd::Update { force } => update::run(force).await,
        Cmd::Mcp {
            store: StoreOpts { store },
        } => mcp::run(store).await,
        Cmd::Add { agent } => match agent {
            AddAgent::Claude => claude::install(),
            AddAgent::Codex => codex::install(),
            AddAgent::Pi { force } => pi::install(force),
            AddAgent::Hermes => hermes::install(),
            AddAgent::Opencode => opencode::install(),
        },
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
                        print!("{}", render::get_human(&note, &turns, color, width));
                        println!(
                            "{}",
                            render::dim(
                                &format!(
                                    "funes get {} {} --window {window} --store {}",
                                    h.session_id,
                                    h.turn_uuid,
                                    store.label()
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

/// `funes use <store>`: attach a remote (or `local` to detach) and report the next step.
async fn use_store(spec: String) -> Result<()> {
    let mut cfg = config::load();
    if spec == "local" {
        cfg.remote = None;
        config::save(&cfg)?;
        println!("active store: local store");
        return Ok(());
    }
    let uri = match hub::Store::parse(&spec) {
        hub::Store::Remote { uri } => uri,
        hub::Store::Local { .. } => {
            return Err(anyhow!(
                "`funes use` takes a remote (e.g. `acme/kb` or `hf://datasets/<org>/<repo>`); for the \
                 local store use `funes use local`, or relocate funes's home with $FUNES_HOME"
            ))
        }
    };
    cfg.remote = Some(uri.clone());
    config::save(&cfg)?;
    println!("active store: {uri}");

    // The overlap heuristic below reads the remote's chunk ids; a remote we can't read would come
    // back empty and wrongly hint at pushing. Attach anyway (the intent is stored) but skip the
    // heuristic and say what's wrong — caught here at attach time rather than at the next push.
    match hub::remote_reachability(&uri).await {
        hub::Reachability::Offline => {
            println!("remote currently unreachable — recall will use your local store until it's back");
            return Ok(());
        }
        hub::Reachability::Missing => {
            println!("note: {}", hub::missing_remote(&uri));
            return Ok(());
        }
        hub::Reachability::Ok => {}
    }

    let local = push::store_ids(&hub::Store::local()).await;
    let remote = push::store_ids(&hub::Store::parse(&uri)).await;
    let unpushed = local.difference(&remote).count();
    println!("{}", attach_hint(local.len(), remote.len(), unpushed));
    Ok(())
}

/// The next-step hint for `funes use`.
fn attach_hint(local: usize, remote: usize, unpushed: usize) -> String {
    if local == 0 && remote == 0 {
        "no memories indexed yet — run `funes index` to build your local store, then `funes push` to publish it here."
            .to_string()
    } else if local == 0 {
        format!("the remote holds {remote} chunks — recall reads them now. if you own this remote store, run `funes index` then `funes push` to add this machine's sessions.")
    } else if unpushed == 0 {
        format!("local store: {local} chunks, all present on the remote.")
    } else if remote == 0 {
        format!("local store: {local} chunks, none on the remote yet — run `funes push`.")
    } else if local > unpushed {
        // Shares chunks with the remote → it's yours to add to; publish the extras.
        format!("local store: {local} chunks, {unpushed} not yet on the remote — run `funes push`.")
    } else {
        // No shared chunks with a populated remote: a fresh host of yours, or a store you only read.
        format!("local store: {local} chunks, remote: {remote} — no shared chunks: a new host of yours, or a store you only read. `funes push` to contribute, skip if it's not yours.")
    }
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
    use super::{attach_hint, parse_selection, Selection};

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

    #[test]
    fn attach_hint_covers_each_state() {
        // empty local + empty remote -> index
        assert!(attach_hint(0, 0, 0).contains("funes index"));
        // empty local + populated remote -> recall now, index to add local
        let h = attach_hint(0, 5, 0);
        assert!(h.contains("recall reads them") && h.contains("funes index"));
        // local fully published -> no push hint
        assert!(attach_hint(8, 8, 0).contains("all present"));
        // empty remote -> first publish
        assert!(attach_hint(8, 0, 8).contains("funes push"));
        // local overlaps the remote (overlap = 8-5 = 3) -> push the extras
        let h = attach_hint(8, 3, 5);
        assert!(h.contains("5 not yet") && h.contains("funes push"));
        // no overlap with a populated remote -> cautious, may be read-only
        let h = attach_hint(8, 5, 8);
        assert!(h.contains("no shared chunks") && h.contains("skip if it's not yours"));
    }
}
