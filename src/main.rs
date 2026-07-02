//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::harness::Harness;
use funes::{claude, codex, config, hermes, hub, index, mcp, opencode, pi, push, recall, scrub, update};

use anyhow::{anyhow, Result};
use clap::{Args, Parser, Subcommand};
use std::io::{IsTerminal, Write};
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
        /// Restrict to a harness: claude_code | codex | pi.
        #[arg(long)]
        harness: Option<String>,
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
        /// A transcript tree or `.parquet` file, or a Hub trace repo `<org>/<repo>`. Omit to index
        /// every known local harness dir present (~/.claude/projects, ~/.codex/sessions,
        /// ~/.pi/agent/sessions).
        path: Option<String>,
        /// Override harness auto-detection for PATH: claude | codex | pi.
        #[arg(long)]
        harness: Option<String>,
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
    /// Publish the local index's new chunks to a remote store on the HF Hub — the active remote by
    /// default, or `[store]` to publish elsewhere.
    Push {
        /// Store to publish to: `<org>/<repo>` or a full `hf://…` URI. Defaults to the active remote.
        store: Option<String>,
        /// Skip the confirmation when the target shares no chunks with your local index.
        #[arg(short, long)]
        yes: bool,
        /// Refresh the remote index after pushing (retrying on conflict) even if the unindexed
        /// backlog is below the auto-reindex threshold. With nothing new to push, reindex only.
        #[arg(long)]
        force_reindex: bool,
    },
    /// Redact secrets from the local index in place — for rows indexed before redaction existed (or
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
    Mcp,
    /// Add funes to a coding agent.
    #[command(subcommand_value_name = "AGENT", subcommand_help_heading = "Agents")]
    Add {
        #[command(subcommand)]
        agent: AddAgent,
    },
}

#[derive(Subcommand)]
enum AddAgent {
    /// claude: register funes as an MCP server with Claude Code (native MCP client).
    Claude {
        /// Register at user scope (all projects) instead of just the current one.
        #[arg(short, long)]
        global: bool,
    },
    /// codex: register funes as an MCP server with Codex (native MCP client, user scope).
    Codex,
    /// pi: install funes as a pi extension (pi has no MCP client of its own).
    Pi {
        /// Install user-wide instead of just the current directory.
        #[arg(short, long)]
        global: bool,
        /// Extract the extension to this directory instead of the default.
        #[arg(long, value_name = "PATH")]
        dest: Option<PathBuf>,
        /// Reinstall even if the on-disk copy is already up to date.
        #[arg(long)]
        force: bool,
    },
    /// hermes: register funes as an MCP server (hermes has a native MCP client).
    Hermes,
    /// opencode: register funes as an MCP server.
    Opencode {
        /// Write the user config instead of the current directory.
        #[arg(short, long)]
        global: bool,
    },
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
            harness,
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
                    project,
                    harness,
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
        Cmd::Index {
            path,
            harness,
            no_thinking,
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
                None if harness.is_some() => return Err(anyhow!("--harness needs a PATH")),
                None => funes::harness::known_harness_roots()
                    .into_iter()
                    .map(|(dir, h)| (dir, Some(h)))
                    .collect(),
            };
            if roots.is_empty() {
                return Err(anyhow!(
                    "no local sessions found — looked in ~/.claude/projects, ~/.codex/sessions, ~/.pi/agent/sessions"
                ));
            }
            index::run_index_roots(&roots, no_thinking, None).await
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
                    anyhow!("no active remote — attach one with `funes use <org>/<repo>`, or name a store: `funes push <org>/<repo>`")
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
        Cmd::Mcp => mcp::run().await,
        Cmd::Add { agent } => match agent {
            AddAgent::Claude { global } => claude::install(global),
            AddAgent::Codex => codex::install(),
            AddAgent::Pi { global, dest, force } => pi::install(global, dest, force),
            AddAgent::Hermes => hermes::install(),
            AddAgent::Opencode { global } => opencode::install(global),
        },
    }
}

/// `funes use <store>`: attach a remote (or `local` to detach) and report the next step.
async fn use_store(spec: String) -> Result<()> {
    let mut cfg = config::load();
    if spec == "local" {
        cfg.remote = None;
        config::save(&cfg)?;
        println!("active store: local index");
        return Ok(());
    }
    let uri = match hub::Store::parse(&spec) {
        hub::Store::Remote { uri } => uri,
        hub::Store::Local { .. } => {
            return Err(anyhow!(
                "`funes use` takes a remote (e.g. `acme/kb` or `hf://datasets/<org>/<repo>`); for the \
                 local index use `funes use local`, or relocate funes's home with $FUNES_HOME"
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
            println!("remote currently unreachable — recall will use your local index until it's back");
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
        "no memories indexed yet — run `funes index` to build your local index, then `funes push` to publish it here."
            .to_string()
    } else if local == 0 {
        format!("the remote holds {remote} chunks — recall reads them now. if you own this remote store, run `funes index` then `funes push` to add this machine's sessions.")
    } else if unpushed == 0 {
        format!("local index: {local} chunks, all present on the remote.")
    } else if remote == 0 {
        format!("local index: {local} chunks, none on the remote yet — run `funes push`.")
    } else if local > unpushed {
        // Shares chunks with the remote → it's yours to add to; publish the extras.
        format!("local index: {local} chunks, {unpushed} not yet on the remote — run `funes push`.")
    } else {
        // No shared chunks with a populated remote: a fresh host of yours, or a store you only read.
        format!("local index: {local} chunks, remote: {remote} — no shared chunks: a new host of yours, or a store you only read. `funes push` to contribute, skip if it's not yours.")
    }
}

/// The push confirmation for a store the local index shares no chunks with. Fails closed (returns
/// false) off a terminal, so an unattended push can't silently publish to the wrong store — there it
/// must be re-run with `--yes`.
fn prompt_new_store(label: &str, chunks: usize) -> bool {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "refusing to push {chunks} chunk(s) to {label}: your local index shares no chunks with it \
             (a first push, a new host, or the wrong store) — re-run with `--yes` to confirm."
        );
        return false;
    }
    eprint!(
        "{label}: your local index shares no chunks with it — a first push here, a new host of yours, \
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
    use super::attach_hint;

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
