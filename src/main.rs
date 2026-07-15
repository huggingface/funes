//! funes — recall over your past AI Agent sessions.
//!
//! `recall` reads the index (hybrid → rerank → recency); `index` builds/updates it from the local
//! harness session dirs (Claude Code, Codex, pi) or an explicit path/parquet/repo. funes's home is
//! `$FUNES_HOME` or `~/.funes`.

use funes::harness::Harness;
use funes::recall::Hit;
use funes::{claude, codex, hello, hermes, hub, index, mcp, opencode, pi, push, recall, render, scrub, update};

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
        /// Highlight this text in the human rendering (matched whitespace-insensitively).
        #[arg(long)]
        highlight: Option<String>,
        #[command(flatten)]
        store: StoreOpts,
    },
    /// Browse a session's turns in fzf — the hit picker's drill-down.
    #[command(hide = true)]
    Turns {
        session_id: String,
        /// The turn to mark in the list (the recall hit).
        turn_uuid: String,
        /// The matched chunk, highlighted in previews and pages.
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
            project,
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
                project,
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
                    print!("{note}");
                    // fzf owns the whole terminal, so it needs a real one; a forced human format
                    // without TTYs (or without fzf installed) keeps the line selector.
                    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
                    if interactive && use_fzf() {
                        select_hits_fzf(&store, &query, &hits, color, width)
                    } else {
                        select_hits(&store, &hits, color, width).await
                    }
                }
            }
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
        Cmd::Turns {
            session_id,
            turn_uuid,
            highlight,
            store,
        } => select_turns_fzf(store.resolve(), session_id, turn_uuid, highlight).await,
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
                                    sh_word(&store.label())
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

/// fzf-driven hit selector, the human default when fzf is installed: the list pane holds one row
/// per hit, the preview pane shows the matched chunk, enter drills into the session's turn
/// browser and leaving it returns here — the walk-back; Esc quits. Each row carries hidden tab
/// columns — session id and turn uuid for the drill-down, plus the matched chunk, which doubles
/// as search text (a query matches content beyond the visible scent) and as the preview.
fn select_hits_fzf(store: &hub::Store, query: &str, hits: &[(Hit, f64)], color: bool, width: usize) -> Result<()> {
    let rows = render::recall_rows(hits, color, width, chrono::Utc::now());
    let mut lines = String::new();
    for ((h, _), row) in hits.iter().zip(&rows) {
        // Field 4 is the matched chunk, whitespace-collapsed: fzf searches it (a query matches
        // content beyond the visible scent) and the preview leads with it.
        let blob: String = h
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(2000)
            .collect();
        lines.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            row.replace('\t', " "),
            h.session_id,
            h.turn_uuid,
            blob
        ));
    }
    let exe = std::env::current_exe()?;
    let copy = clipboard_pipe();
    let hints = match copy {
        Some(_) => "enter browses the session · ctrl-y copies its get command · esc quits",
        None => "enter browses the session · esc quits",
    };
    let mut args = fzf_style_args(color, "recall ❯ ", &format!("{query} · {}\n{hints}", store.label()));
    args.extend([
        "--preview".to_string(),
        // The matched chunk, nothing else — the turn browser behind enter owns the context.
        // Rendering the turn here too would show the match's text twice in different shapes
        // (or, for a tool hit, not at all: the turn view compresses tool blocks).
        "echo {4} | fold -s -w $FZF_PREVIEW_COLUMNS".to_string(),
        "--preview-window".to_string(),
        "right:60%:wrap".to_string(),
        "--bind".to_string(),
        // fzf shell-escapes {4} (the matched chunk) itself.
        format!(
            "enter:execute:{} turns {{2}} {{3}} --store {} --highlight {{4}}",
            sh_quote(&exe.display().to_string()),
            sh_quote(&store.label())
        ),
    ]);
    if let Some(pipe) = copy {
        args.extend(["--bind".to_string(), copy_bind(store, pipe)]);
    }
    let mut fzf = std::process::Command::new("fzf")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    fzf.stdin.take().expect("stdin is piped").write_all(lines.as_bytes())?;
    // Enter never terminates fzf (it drills into the turn browser and comes back); the picker
    // ends on Esc or Ctrl-C, so any exit status just means "done browsing".
    fzf.wait()?;
    Ok(())
}

/// The turn browser behind enter in the hit picker: one fzf row per turn of the session, oldest
/// first, the recall hit's turn marked `▶`; the highlighted turn shows whole in the preview pane
/// — the matched chunk reverse-videoed within it — and enter toggles that pane full-screen for
/// reading. Esc walks back to the hit picker.
async fn select_turns_fzf(
    store: hub::Store,
    session_id: String,
    center: String,
    highlight: Option<String>,
) -> Result<()> {
    let s8 = &session_id[..session_id.len().min(8)];
    let spinner = Spinner::start(&format!("loading session {s8}…"));
    let (note, turns) = recall::get_turns(store.clone(), session_id.clone(), center.clone(), i64::MAX).await?;
    drop(spinner);
    if turns.is_empty() {
        print!("{note}");
        println!("turn {center} not found in session {session_id}");
        return Ok(());
    }
    let (color, width) = human_io();
    let rows = render::turn_rows(&turns, color, width);
    // The session is fetched once, above; each turn's rendering is written to a file the preview
    // and the pager read back — field 4 of the row — so neither touches the store again.
    let dir = tempfile::tempdir()?;
    let mut lines = String::new();
    for (i, (t, row)) in turns.iter().zip(&rows).enumerate() {
        let path = dir.path().join(i.to_string());
        let body = render::get_human("", std::slice::from_ref(t), color, width, highlight.as_deref());
        std::fs::write(&path, body)?;
        let marker = if t.turn_uuid == center { "▶ " } else { "  " };
        lines.push_str(&format!(
            "{marker}{}\t{}\t{}\t{}\n",
            row.replace('\t', " "),
            session_id,
            t.turn_uuid,
            path.display()
        ));
    }
    let copy = clipboard_pipe();
    // Reading mode: enter grows the preview to most of the screen and back, and the arrows keep
    // walking the turns while it's grown. An fzf without change-preview-window pages the turn
    // through less instead.
    let reader = fzf_version().is_some_and(|v| v >= (0, 30));
    let enter_hint = if reader {
        "enter toggles full-screen"
    } else {
        "enter opens the turn in less"
    };
    let hints = match copy {
        Some(_) => format!("{enter_hint} · ctrl-y copies the get command · esc goes back"),
        None => format!("{enter_hint} · esc goes back"),
    };
    let mut args = fzf_style_args(color, "turns ❯ ", &format!("session {s8} · {}\n{hints}", store.label()));
    args.extend([
        "--preview".to_string(),
        "cat {4}".to_string(),
        "--preview-window".to_string(),
        "right:60%:wrap".to_string(),
        "--bind".to_string(),
        if reader {
            "enter:change-preview-window(down,90%,wrap|right,60%,wrap)".to_string()
        } else {
            // fzf runs binds via $SHELL -c, and zsh doesn't word-split an unquoted
            // `${PAGER:-less -R}` default — the command must be spelled out word by word.
            // `-R` passes the highlight's ANSI marks through.
            "enter:execute:cat {4} | less -R".to_string()
        },
    ]);
    if let Some(pipe) = copy {
        args.extend(["--bind".to_string(), copy_bind(&store, pipe)]);
    }
    let mut cmd = std::process::Command::new("fzf");
    cmd.args(&args);
    // Land on the hit's turn rather than the top of a possibly huge session. `pos` needs
    // fzf 0.36; older ones still get the `▶` marker to search for.
    if let Some(idx) = turns.iter().position(|t| t.turn_uuid == center) {
        if fzf_version().is_some_and(|v| v >= (0, 36)) {
            cmd.args(["--bind", &format!("load:pos({})", idx + 1)]);
        }
    }
    let mut fzf = cmd.stdin(std::process::Stdio::piped()).spawn()?;
    fzf.stdin.take().expect("stdin is piped").write_all(lines.as_bytes())?;
    fzf.wait()?;
    Ok(())
}

/// The presentation flags the fzf pickers share: rows are tab-delimited with only the first field
/// visible, the header carries the context and key hints, and fzf's own chrome (prompt, pointer,
/// match highlights) wears the funes accent, cyan — monochrome when `color` is off.
fn fzf_style_args(color: bool, prompt: &str, header: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--ansi",
        "--layout=reverse",
        "--no-sort",
        "--info=inline",
        "--pointer=❯",
        "--delimiter",
        "\t",
        "--with-nth",
        "1",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    args.push(format!("--prompt={prompt}"));
    args.push(format!("--header={header}"));
    args.push(if color {
        "--color=hl:6,hl+:6,prompt:6,pointer:6,info:6,spinner:6,marker:6".to_string()
    } else {
        "--color=bw".to_string()
    });
    args
}

/// The ctrl-y bind for a picker whose rows carry session id and turn uuid in fields 2 and 3:
/// copies the row's ready-to-run `funes get` command through `pipe`.
fn copy_bind(store: &hub::Store, pipe: &str) -> String {
    format!(
        "ctrl-y:execute-silent(printf 'funes get %s %s --store %s' {{2}} {{3}} {} | {pipe})",
        sh_word(&store.label())
    )
}

/// The first clipboard writer on PATH, as a shell pipe target; None when the box has none.
fn clipboard_pipe() -> Option<&'static str> {
    const WRITERS: [(&str, &str); 4] = [
        ("pbcopy", "pbcopy"),
        ("wl-copy", "wl-copy"),
        ("xclip", "xclip -selection clipboard"),
        ("xsel", "xsel --input --clipboard"),
    ];
    let path = std::env::var_os("PATH")?;
    WRITERS
        .iter()
        .find(|(bin, _)| std::env::split_paths(&path).any(|d| d.join(bin).is_file()))
        .map(|(_, pipe)| *pipe)
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

/// `s` single-quoted for a shell command line.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `s` quoted only when a shell would mangle it bare — for commands shown to be copy-pasted,
/// where quoting the common clean label is just noise.
fn sh_word(s: &str) -> String {
    let clean = |c: char| c.is_ascii_alphanumeric() || "/:.@_+=%,~-".contains(c);
    if !s.is_empty() && s.chars().all(clean) {
        s.to_string()
    } else {
        sh_quote(s)
    }
}

/// The installed fzf's (major, minor), or None when it can't be read.
fn fzf_version() -> Option<(u32, u32)> {
    let out = std::process::Command::new("fzf").arg("--version").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let mut parts = s.split_whitespace().next()?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// True when fzf is on PATH and `FUNES_NO_FZF` is unset (presence opts out, like `NO_COLOR`).
fn use_fzf() -> bool {
    std::env::var_os("FUNES_NO_FZF").is_none()
        && std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join("fzf").is_file()))
            .unwrap_or(false)
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
    use super::{fzf_style_args, parse_selection, Selection};

    #[test]
    fn fzf_style_args_wear_the_accent_only_in_color() {
        let colored = fzf_style_args(true, "recall ❯ ", "q · store\nhints");
        assert!(colored.contains(&"--prompt=recall ❯ ".to_string()));
        assert!(colored.contains(&"--header=q · store\nhints".to_string()));
        assert!(colored.iter().any(|a| a.starts_with("--color=hl:6")));
        let plain = fzf_style_args(false, "p", "h");
        assert!(plain.contains(&"--color=bw".to_string()));
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
