//! Score `funes recall` at its defaults against a labeled set of questions and print the receipt:
//!
//! ```text
//! cargo run --release --example score_recall -- questions.jsonl --memory huggingface/funes-memory
//! ```
//!
//! `--candidates` asks from a deeper pool instead. A pool larger than the hits is reranked before the
//! cut, so `--candidates 8` scores the fused order alone.
//!
//! A question is one JSON object a line: an `id`, the `question` to ask, `gold`, its answering turns
//! (each a `session_id` and a `turn`: the answer and each restatement), and an optional `kind` the
//! receipt breaks the scores down by. A question's rank is the position of its first hit showing an
//! answering turn, as the hit itself or in its exchange frame. The receipt names the funes version,
//! the memory's revision and the file's hash, then reports hit@1, hit@5, found@k, the mean reciprocal
//! rank and the answers that are a hit's own turn, overall and by kind, with the median reply size
//! and latency, then each question's rank and the turns its reply showed, so it can be rescored. The
//! hits come from the call `funes recall` makes, not from its printed text, which can quote an
//! earlier recall, `→ get` lines included.

use anyhow::{bail, Context, Result};
use clap::Parser;
use funes::commands::recall::{recall_hits, reranks, Hit, RecallFilter, DEFAULT_CANDIDATES, DEFAULT_K};
use funes::hub::parse_hf;
use funes::memory::{Memory, MemoryState};
use funes::ui::render::recall_reply;
use hf_hub::HFClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "Score a default funes recall against a labeled set of questions")]
struct Args {
    /// Questions file, one JSON object a line with an `id`, a `question` and its `gold` turns.
    questions: PathBuf,
    /// Memory to ask: `<org>/<repo>`, an `hf://…` URI, a local path, or `local`.
    #[arg(long, default_value = "huggingface/funes-memory")]
    memory: String,
    /// Size of the fused pool the hits are cut from.
    #[arg(long, default_value_t = DEFAULT_CANDIDATES)]
    candidates: usize,
}

#[derive(Deserialize)]
struct Question {
    id: String,
    question: String,
    kind: Option<String>,
    gold: Vec<Answer>,
}

/// A turn that answers the question, the answer or a restatement.
#[derive(Deserialize)]
struct Answer {
    session_id: String,
    turn: i64,
}

impl Answer {
    /// Whether the hit's own turn is this one.
    fn is_hit(&self, hit: &Hit) -> bool {
        hit.session_id == self.session_id && hit.seq == self.turn
    }

    /// Whether the hit shows this turn, as itself or in its frame.
    fn shown_by(&self, hit: &Hit) -> bool {
        self.is_hit(hit) || (hit.session_id == self.session_id && hit.exchange.iter().any(|e| e.seq == self.turn))
    }
}

/// A question's rank, and whether an answering turn is a hit's own turn.
struct Score {
    rank: Option<usize>,
    as_hit: bool,
}

/// The memory's revision for the receipt: the Hub commit a remote's reads pin to, or a local
/// memory's dataset version.
async fn revision(memory: &Memory) -> Result<String> {
    match memory {
        Memory::Remote { uri } => {
            let (owner, name, _) = parse_hf(uri)?;
            let refs = HFClient::builder()
                .build()
                .context("building hf-hub client")?
                .dataset(owner, name)
                .list_refs()
                .send()
                .await
                .context("listing the remote refs")?;
            refs.branches
                .iter()
                .find(|b| b.name == "main")
                .map(|b| b.target_commit.clone())
                .context("no main branch on the remote")
        }
        Memory::Local { .. } => match memory.state().await? {
            MemoryState::Ready(ds) => Ok(format!("v{}", ds.version().version)),
            _ => bail!("no memory at {}", memory.label()),
        },
    }
}

fn dash(rank: Option<usize>) -> String {
    rank.map_or_else(|| "-".to_string(), |r| (r + 1).to_string())
}

/// The turns a reply showed, one word a hit: its own turn, then its frame's.
fn shown(hits: &[(Hit, f64)]) -> String {
    hits.iter()
        .map(|(h, _)| {
            let frame: String = h.exchange.iter().map(|e| format!("+{}", e.seq)).collect();
            format!("{}:{}{frame}", h.session_id, h.seq)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn row(label: &str, scores: &[&Score]) {
    let within = |n: usize| scores.iter().filter(|s| s.rank.is_some_and(|r| r < n)).count();
    let mrr = scores
        .iter()
        .map(|s| s.rank.map_or(0.0, |r| 1.0 / (r as f64 + 1.0)))
        .sum::<f64>()
        / scores.len() as f64;
    println!(
        "{label:16} {:5} {:5} {:7}  {mrr:.3} {:10}   (of {})",
        within(1),
        within(5),
        scores.iter().filter(|s| s.rank.is_some()).count(),
        scores.iter().filter(|s| s.as_hit).count(),
        scores.len()
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let bytes = std::fs::read(&args.questions).with_context(|| format!("reading {}", args.questions.display()))?;
    let digest = hex::encode(Sha256::digest(&bytes));
    let questions = std::str::from_utf8(&bytes)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Question>(l).with_context(|| format!("parsing {l}")))
        .collect::<Result<Vec<_>>>()?;
    if questions.is_empty() {
        bail!("no questions in {}", args.questions.display());
    }
    let memory = Memory::resolve(Some(args.memory));
    let revision = revision(&memory).await?;

    let mut scores = Vec::with_capacity(questions.len());
    let mut replies = Vec::with_capacity(questions.len());
    let mut ms = Vec::with_capacity(questions.len());
    let mut chars = Vec::with_capacity(questions.len());
    for q in &questions {
        let t = Instant::now();
        let (note, hits) = recall_hits(
            memory.clone(),
            q.question.clone(),
            DEFAULT_K,
            args.candidates,
            RecallFilter::default(),
            &|_| (),
        )
        .await?;
        let took = t.elapsed().as_millis();
        // A degraded read answers from another memory (the local one when the Hub times out), so
        // its hits would be scored against labels they were never drawn from.
        if !note.is_empty() {
            bail!("{}: {}", q.id, note.trim());
        }
        let score = Score {
            rank: hits.iter().position(|(h, _)| q.gold.iter().any(|a| a.shown_by(h))),
            as_hit: hits.iter().any(|(h, _)| q.gold.iter().any(|a| a.is_hit(h))),
        };
        eprintln!("{} rank {} {took} ms", q.id, dash(score.rank));
        chars.push(recall_reply(&note, &hits, None).chars().count());
        replies.push(shown(&hits));
        scores.push(score);
        ms.push(took);
    }
    ms.sort_unstable();
    chars.sort_unstable();

    println!(
        "funes {}, memory {} @ {revision}, questions {} sha256 {digest}",
        env!("CARGO_PKG_VERSION"),
        memory.label(),
        args.questions.display()
    );
    println!(
        "{} questions, {DEFAULT_K} hits a question from a pool of {}{}, p50 {} ms, reply p50 {} chars",
        questions.len(),
        args.candidates,
        if reranks(DEFAULT_K, args.candidates) {
            " reranked"
        } else {
            ""
        },
        ms[ms.len() / 2],
        chars[chars.len() / 2]
    );
    println!();
    println!("{:16} hit@1 hit@5 found@{DEFAULT_K}    MRR    as a hit", "");
    row("all", &scores.iter().collect::<Vec<_>>());
    let kinds: BTreeSet<&str> = questions.iter().filter_map(|q| q.kind.as_deref()).collect();
    for kind in kinds {
        let of_kind: Vec<&Score> = questions
            .iter()
            .zip(&scores)
            .filter(|(q, _)| q.kind.as_deref() == Some(kind))
            .map(|(_, s)| s)
            .collect();
        row(&format!("  {kind}"), &of_kind);
    }
    println!();
    println!(
        "rank per question (- = not in the reply), then the turns the reply showed, a hit's own turn and its frame's:"
    );
    for ((q, score), reply) in questions.iter().zip(&scores).zip(&replies) {
        println!("  {}  {}  {reply}", q.id, dash(score.rank));
    }
    Ok(())
}
