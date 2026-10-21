//! Score `funes recall` at its defaults against a labeled set of questions and print the receipt:
//!
//! ```text
//! cargo run --release --example score_recall -- questions.jsonl --memory huggingface/funes-memory
//! ```
//!
//! `--candidates` asks from a deeper pool instead. From four times the hits, recall reranks the pool
//! before the cut, so this scores the reranker.
//!
//! A question is one JSON object a line: an `id`, the `question` to ask, `gold`, the spans its answer
//! is in (a `session_id` with optional `from` and `to` turns, the whole session when both are absent),
//! and an optional `kind` the receipt breaks the scores down by. Other fields are ignored. A
//! question's rank is the position of its first hit in a gold span. The receipt names the funes
//! version, the memory's revision and the file's hash, then reports hit@1, hit@5, found@k (k the hits
//! a default recall returns), the mean reciprocal rank and the median latency, overall and by kind,
//! then each question's rank. The hits come from the call `funes recall` makes, not from its printed
//! text: a hit's text can quote an earlier recall, `→ get` lines included, and nothing in the text
//! tells a quoted hit from a real one.

use anyhow::{bail, Context, Result};
use clap::Parser;
use funes::commands::recall::{recall_hits, reranks, Hit, RecallFilter, DEFAULT_CANDIDATES, DEFAULT_K};
use funes::hub::parse_hf;
use funes::memory::{Memory, MemoryState};
use hf_hub::HFClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "Score a default funes recall against a labeled set of questions")]
struct Args {
    /// Questions file, one JSON object a line with an `id`, a `question` and its `gold` spans.
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
    gold: Vec<Span>,
}

/// Where a question's answer is: a session, or a range of its turns.
#[derive(Deserialize)]
struct Span {
    session_id: String,
    from: Option<i64>,
    to: Option<i64>,
}

impl Span {
    fn holds(&self, hit: &Hit) -> bool {
        hit.session_id == self.session_id
            && self.from.is_none_or(|from| from <= hit.seq)
            && self.to.is_none_or(|to| hit.seq <= to)
    }
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

fn row(label: &str, ranks: &[Option<usize>]) {
    let within = |n: usize| ranks.iter().filter(|r| r.is_some_and(|r| r < n)).count();
    let mrr = ranks
        .iter()
        .map(|r| r.map_or(0.0, |r| 1.0 / (r as f64 + 1.0)))
        .sum::<f64>()
        / ranks.len() as f64;
    println!(
        "{label:16} {:5} {:5} {:7}  {mrr:.3}   (of {})",
        within(1),
        within(5),
        ranks.iter().flatten().count(),
        ranks.len()
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

    let mut ranks = Vec::with_capacity(questions.len());
    let mut ms = Vec::with_capacity(questions.len());
    for q in &questions {
        let t = Instant::now();
        // No neighbors: they are context attached to the hits once ranked.
        let (note, hits) = recall_hits(
            memory.clone(),
            q.question.clone(),
            DEFAULT_K,
            args.candidates,
            0,
            RecallFilter::default(),
            &|_| (),
        )
        .await?;
        // A degraded read answers from another memory (the local one when the Hub times out), so
        // its hits would be scored against labels they were never drawn from.
        if !note.is_empty() {
            bail!("{}: {}", q.id, note.trim());
        }
        let rank = hits.iter().position(|(h, _)| q.gold.iter().any(|s| s.holds(h)));
        let took = t.elapsed().as_millis();
        eprintln!("{} rank {} {took} ms", q.id, dash(rank));
        ranks.push(rank);
        ms.push(took);
    }
    ms.sort_unstable();

    println!(
        "funes {}, memory {} @ {revision}, questions {} sha256 {digest}",
        env!("CARGO_PKG_VERSION"),
        memory.label(),
        args.questions.display()
    );
    println!(
        "{} questions, {DEFAULT_K} hits a question from a pool of {}{}, p50 {} ms",
        questions.len(),
        args.candidates,
        if reranks(DEFAULT_K, args.candidates) {
            " reranked"
        } else {
            ""
        },
        ms[ms.len() / 2]
    );
    println!();
    println!("{:16} hit@1 hit@5 found@{DEFAULT_K}    MRR", "");
    row("all", &ranks);
    let kinds: BTreeSet<&str> = questions.iter().filter_map(|q| q.kind.as_deref()).collect();
    for kind in kinds {
        let of_kind: Vec<Option<usize>> = questions
            .iter()
            .zip(&ranks)
            .filter(|(q, _)| q.kind.as_deref() == Some(kind))
            .map(|(_, r)| *r)
            .collect();
        row(&format!("  {kind}"), &of_kind);
    }
    println!();
    println!("rank per question (- = not in the hits):");
    for (q, rank) in questions.iter().zip(&ranks) {
        println!("  {}  {}", q.id, dash(*rank));
    }
    Ok(())
}
