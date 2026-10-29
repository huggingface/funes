//! How much a rerank helps at each pool size, on a set of `dacorvo/funes-recall-benchmark`'s
//! questions:
//!
//! ```text
//! cargo run --release --example sweep_rerank -- questions.jsonl --memory mem/funes-memory --out sweep.jsonl
//! ```
//!
//! For each question and each `--pools` size N, it asks `recall_hits` for N hits from a pool of N (the
//! fused order of the pool, which funes reads whole for N hits, never reranked) and scores every
//! candidate with funes's own reranker, as `funes recall --candidates N` would before its cut.
//! One JSON line per question and pool: the candidates in fused order, each with its fused score and
//! its reranker score, and how long the retrieval and the rerank took. The dataset's scorers compare
//! the fused order with the reranked order at any cut. By default a question's union of pools is
//! scored in one reranker call and a chunk's score looked up by its id. `--per-pool` scores each pool
//! as its own call, as funes does, which is what to time, and `--cpu` then also times the CPU
//! reranker. The output is appended to, and a (question, pool) already in it is skipped, so an
//! interrupted run resumes.

use anyhow::{bail, Context, Result};
use clap::Parser;
use funes::commands::recall::{recall_hits, RecallFilter};
use funes::inference::{self, Reranker};
use funes::memory::Memory;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(about = "How much a rerank helps at each pool size: the fused pool and its reranker scores, per question")]
struct Args {
    /// Questions file, one JSON object a line with an `id` and a `question`.
    questions: PathBuf,
    /// Memory to ask: `<org>/<repo>`, an `hf://…` URI, a local path, or `local`.
    #[arg(long)]
    memory: String,
    /// Pool sizes to sweep.
    #[arg(long, num_args = 1.., default_values_t = [10usize, 12, 16, 20, 24, 30, 32, 50, 100])]
    pools: Vec<usize>,
    /// Output file, one JSON line per question and pool, appended to.
    #[arg(long)]
    out: PathBuf,
    /// Score each pool as its own reranker call, as funes would, instead of each question's union of
    /// pools once. Slower, and what the latencies of a pool are measured on.
    #[arg(long)]
    per_pool: bool,
    /// Also time the CPU reranker on every pool (with --per-pool).
    #[arg(long)]
    cpu: bool,
    /// Stop after this many questions.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct Question {
    id: String,
    question: String,
}

#[derive(Deserialize)]
struct Done {
    id: String,
    pool: usize,
}

#[derive(Serialize)]
struct Row<'a> {
    id: &'a str,
    pool: usize,
    retrieve_ms: u128,
    /// The reranker call's time: this pool's own with --per-pool, else the question's union of pools.
    rerank_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    union: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_ms: Option<u128>,
    hits: Vec<Candidate>,
}

#[derive(Serialize)]
struct Candidate {
    id: String,
    session_id: String,
    seq: i64,
    fused: f32,
    score: f32,
}

/// The CPU reranker `--cpu` times beside the one `inference::reranker()` builds.
fn cpu_reranker() -> Result<Box<dyn Reranker>> {
    #[cfg(feature = "blas")]
    {
        Ok(Box::new(inference::blas::GteReranker::new()?))
    }
    #[cfg(not(feature = "blas"))]
    {
        bail!("--cpu times the blas reranker, which this build leaves out")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let text =
        std::fs::read_to_string(&args.questions).with_context(|| format!("reading {}", args.questions.display()))?;
    let questions = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Question>(l).with_context(|| format!("parsing {l}")))
        .collect::<Result<Vec<_>>>()?;
    let questions = &questions[..args.limit.unwrap_or(questions.len()).min(questions.len())];
    let memory = Memory::resolve(Some(args.memory.clone()));

    let mut done: HashSet<(String, usize)> = HashSet::new();
    if args.out.exists() {
        for line in BufReader::new(std::fs::File::open(&args.out)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let d: Done = serde_json::from_str(&line).with_context(|| format!("parsing {}", args.out.display()))?;
            done.insert((d.id, d.pool));
        }
    }
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&args.out)?;

    let t = Instant::now();
    let mut reranker = inference::reranker()?;
    eprintln!("reranker built in {} ms", t.elapsed().as_millis());
    let t = Instant::now();
    let mut cpu: Option<Box<dyn Reranker>> = if args.cpu { Some(cpu_reranker()?) } else { None };
    if args.cpu {
        eprintln!("CPU reranker built in {} ms", t.elapsed().as_millis());
    }

    // Warm up: the embedder, the dataset and the GPU graph, so no row's timing carries them.
    if let Some(q) = questions.first() {
        let t = Instant::now();
        let (_, hits) = recall_hits(
            memory.clone(),
            q.question.clone(),
            32,
            32,
            0,
            RecallFilter::default(),
            &|_| (),
        )
        .await?;
        eprintln!("warm-up retrieval in {} ms", t.elapsed().as_millis());
        let docs: Vec<&str> = hits.iter().map(|(h, _)| h.text.as_str()).collect();
        let t = Instant::now();
        reranker.rerank(&q.question, &docs)?;
        eprintln!(
            "warm-up rerank of {} pairs in {} ms",
            docs.len(),
            t.elapsed().as_millis()
        );
        if let Some(cpu) = cpu.as_mut() {
            let t = Instant::now();
            cpu.rerank(&q.question, &docs[..docs.len().min(8)])?;
            eprintln!("warm-up CPU rerank of 8 pairs in {} ms", t.elapsed().as_millis());
        }
    }

    for (n, q) in questions.iter().enumerate() {
        let mut line = format!("{} ({}/{})", q.id, n + 1, questions.len());
        let mut pools = Vec::new();
        for &pool in &args.pools {
            if done.contains(&(q.id.clone(), pool)) {
                continue;
            }
            let t = Instant::now();
            let (note, hits) = recall_hits(
                memory.clone(),
                q.question.clone(),
                pool,
                pool,
                0,
                RecallFilter::default(),
                &|_| (),
            )
            .await?;
            if !note.is_empty() {
                bail!("{}: {}", q.id, note.trim());
            }
            pools.push((pool, t.elapsed().as_millis(), hits));
        }
        // The union of the pools, scored once; a chunk's score is its own whatever batch it sits in.
        let mut union: Vec<(&str, &str)> = Vec::new();
        let mut seen = HashSet::new();
        for (_, _, hits) in &pools {
            for (h, _) in hits {
                if seen.insert(h.id.as_str()) {
                    union.push((h.id.as_str(), h.text.as_str()));
                }
            }
        }
        let mut by_id: HashMap<&str, f32> = HashMap::new();
        let mut union_ms = 0;
        if !args.per_pool && !union.is_empty() {
            let docs: Vec<&str> = union.iter().map(|(_, text)| *text).collect();
            let t = Instant::now();
            let scores = reranker.rerank(&q.question, &docs)?;
            union_ms = t.elapsed().as_millis();
            by_id.extend(union.iter().map(|(id, _)| *id).zip(scores));
        }
        for (pool, retrieve_ms, hits) in &pools {
            let docs: Vec<&str> = hits.iter().map(|(h, _)| h.text.as_str()).collect();
            let (scores, rerank_ms) = if args.per_pool {
                let t = Instant::now();
                let scores = reranker.rerank(&q.question, &docs)?;
                (scores, t.elapsed().as_millis())
            } else {
                (hits.iter().map(|(h, _)| by_id[h.id.as_str()]).collect(), union_ms)
            };
            let cpu_ms = match cpu.as_mut() {
                Some(cpu) => {
                    let t = Instant::now();
                    let cpu_scores = cpu.rerank(&q.question, &docs)?;
                    let drift = scores
                        .iter()
                        .zip(&cpu_scores)
                        .map(|(a, b)| (a - b).abs())
                        .fold(0.0f32, f32::max);
                    if drift > 0.5 {
                        eprintln!("{} pool {pool}: GPU and CPU scores differ by up to {drift:.3}", q.id);
                    }
                    Some(t.elapsed().as_millis())
                }
                None => None,
            };
            let row = Row {
                id: &q.id,
                pool: *pool,
                retrieve_ms: *retrieve_ms,
                rerank_ms,
                union: (!args.per_pool).then_some(union.len()),
                cpu_ms,
                hits: hits
                    .iter()
                    .zip(&scores)
                    .map(|((h, _), &score)| Candidate {
                        id: h.id.clone(),
                        session_id: h.session_id.clone(),
                        seq: h.seq,
                        fused: h.fused,
                        score,
                    })
                    .collect(),
            };
            writeln!(out, "{}", serde_json::to_string(&row)?)?;
            out.flush()?;
            line.push_str(&format!("  {pool}:{}+{}ms", retrieve_ms, rerank_ms));
        }
        eprintln!("{line}");
    }
    Ok(())
}
