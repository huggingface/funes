//! Benchmark `recall()` latency, local vs remote, cold vs warm — over the **same dataset** so the
//! comparison isolates the I/O path, not differences in the data.
//!
//! The CPU stages of recall (query embed, cross-encoder rerank) are identical whatever the store;
//! the local↔remote difference is entirely in the I/O stages (store open, vector ANN scan, BM25
//! scan, neighbor scan). To compare them fairly the bench downloads the `--remote` dataset to a
//! temp dir and benchmarks that local copy against the same dataset over `hf://`.
//!
//! ```text
//! cargo run --release --example bench_recall -- "your query" \
//!     --remote dacorvo/funes-Glint-Research-Fable-5 --iters 5 --cold
//! ```
//!
//! `--cold` points the hf-hub file cache (`HF_HUB_CACHE`) at a throwaway temp dir so the remote cold
//! call is a true download, leaving your real `~/.cache/huggingface/hub` untouched. (It does not, and
//! cannot portably, drop the OS page cache, so the local cold figure is only as cold as the page
//! cache happens to be.) The dataset is fetched through the hf-hub crate — no external CLI — and a
//! private repo authenticates with the token from the environment (the same one recall uses).

use anyhow::{Context, Result};
use clap::Parser;
use funes::hub::Store;
use funes::recall::recall;
use hf_hub::HFClient;
use std::path::Path;
use std::time::Instant;
use tempfile::TempDir;

#[derive(Parser)]
#[command(about = "Benchmark recall latency over one dataset: local vs remote, cold vs warm")]
struct Args {
    /// Query to recall.
    #[arg(default_value = "how does recall rerank candidates")]
    query: String,
    /// Dataset to benchmark (`org/repo` or `hf://…`), used for BOTH legs.
    #[arg(long, default_value = "dacorvo/funes-Glint-Research-Fable-5")]
    remote: String,
    /// Warm iterations timed per memory (after the one cold call).
    #[arg(long, default_value_t = 5)]
    iters: usize,
    /// Give the remote leg a throwaway temp HF cache so its cold call is a true download — your real
    /// `~/.cache/huggingface/hub` is left untouched.
    #[arg(long)]
    cold: bool,
    /// Results to return (recall `k`).
    #[arg(long, default_value_t = 8)]
    k: usize,
    /// Fused candidates to rerank.
    #[arg(long, default_value_t = 30)]
    candidates: usize,
    /// Neighbor chunks attached per hit.
    #[arg(long, default_value_t = 1)]
    neighbors: i64,
}

/// One timed `recall()` call: elapsed millis and the number of hits in the output.
async fn time_recall(store: &Store, a: &Args) -> Result<(f64, usize)> {
    let t = Instant::now();
    let out = recall(
        store.clone(),
        a.query.clone(),
        a.k,
        a.candidates,
        0.0,
        a.neighbors,
        None,
        None,
    )
    .await?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    // Each hit prints a `→ get <session> <turn>` line.
    let hits = out.matches("→ get ").count();
    Ok((ms, hits))
}

/// Download the `--remote` dataset's files into `dest` via the hf-hub crate, so the same data can be
/// opened as a local store for the local leg. `snapshot_download` with `local_dir` writes straight to
/// `dest` and bypasses the hub cache, so it can't pre-warm the cache the remote leg reads — the
/// remote "cold" call stays a true download. The client reads the token from the environment
/// (`HF_TOKEN`, then the token file), the same source recall uses, so a private repo authenticates.
async fn download_dataset(remote: &str, dest: &Path) -> Result<()> {
    let repo = remote.strip_prefix("hf://datasets/").unwrap_or(remote);
    let mut seg = repo.split('/');
    let (owner, name) = match (seg.next(), seg.next()) {
        (Some(o), Some(n)) if !o.is_empty() && !n.is_empty() => (o, n),
        _ => anyhow::bail!("--remote must be <owner>/<name>, got {remote}"),
    };
    eprintln!("downloading {owner}/{name} → {} (local leg)…", dest.display());
    HFClient::builder()
        .build()
        .context("building hf-hub client")?
        .dataset(owner, name)
        .snapshot_download()
        .local_dir(dest.to_path_buf())
        .send()
        .await
        .with_context(|| format!("downloading {owner}/{name} via hf-hub"))?;
    Ok(())
}

/// Point the hf-hub file cache at a fresh temp dir so the first remote read is a genuine download,
/// without touching the user's real `~/.cache/huggingface/hub`. `HF_HUB_CACHE` relocates only the
/// download cache — the token and `HF_HOME` are untouched, so auth still works. The returned dir must
/// stay alive for the run; dropping it removes the temp cache.
fn isolate_hub_cache() -> Result<TempDir> {
    let dir = tempfile::Builder::new().prefix("funes-bench-hub-").tempdir()?;
    std::env::set_var("HF_HUB_CACHE", dir.path());
    eprintln!(
        "cold: isolated HF cache at {} (real cache untouched)",
        dir.path().display()
    );
    Ok(dir)
}

/// The timings the x-factor summary compares (milliseconds). The full row (incl. warm_hi, hits) is
/// printed live by `bench_store`; only these are needed afterwards.
struct Stats {
    cold: f64,
    warm_lo: f64,
    warm_med: f64,
}

async fn bench_store(name: &str, store: &Store, a: &Args) -> Result<Stats> {
    let (cold, hits) = time_recall(store, a).await?;

    let mut warm: Vec<f64> = Vec::with_capacity(a.iters);
    for _ in 0..a.iters {
        warm.push(time_recall(store, a).await?.0);
    }
    warm.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let (warm_lo, warm_med, warm_hi) = if warm.is_empty() {
        (cold, cold, cold)
    } else {
        (warm[0], warm[warm.len() / 2], warm[warm.len() - 1])
    };

    // Print the row live, so the local leg shows before the slow remote one runs.
    println!("{name:<8} {cold:>9.1} {warm_lo:>9.1} {warm_med:>9.1} {warm_hi:>9.1} {hits:>5}");
    Ok(Stats {
        cold,
        warm_lo,
        warm_med,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let remote = Store::parse(a.remote.trim());

    // With --cold, redirect the hf-hub file cache to a temp dir for the whole run (held alive here)
    // so the remote leg's first read is a genuine download. The local-leg download writes via
    // `local_dir`, which bypasses this cache, so it can't pre-warm the remote leg.
    let _hub_guard = if a.cold { Some(isolate_hub_cache()?) } else { None };

    // The local leg is a fresh download of the SAME dataset, so local vs remote isolates the hf://
    // I/O path rather than comparing two different stores. `local_dir` is held to the end of the run
    // so the downloaded copy isn't removed mid-benchmark.
    let local_dir = tempfile::Builder::new().prefix("funes-bench-local-").tempdir()?;
    download_dataset(a.remote.trim(), local_dir.path()).await?;
    let local = Store::Local {
        path: local_dir.path().to_path_buf(),
    };

    // Warm up the embed + rerank models once (against the local copy), so the one-time model load
    // (hundreds of ms, identical for every store) is excluded from all timings below.
    eprintln!("loading models…");
    let _ = recall(local.clone(), a.query.clone(), 1, 5, 0.0, 0, None, None).await;

    println!(
        "\ndataset: {}   query: {:?}   k={} candidates={} neighbors={}   warm iters={}\n",
        a.remote, a.query, a.k, a.candidates, a.neighbors, a.iters
    );
    println!(
        "{:<8} {:>9} {:>9} {:>9} {:>9} {:>5}",
        "memory", "cold(ms)", "warm_lo", "warm_med", "warm_hi", "hits"
    );

    let l = bench_store("local", &local, &a).await?;
    let r = bench_store("remote", &remote, &a).await?;

    // Headline: how much the remote tier costs over a local copy of the same data.
    let x = |remote: f64, local: f64| if local > 0.0 { remote / local } else { f64::NAN };
    println!(
        "\nremote vs local:  {:.1}× slower cold,  {:.1}× slower warm (median),  {:.1}× warm best-case",
        x(r.cold, l.cold),
        x(r.warm_med, l.warm_med),
        x(r.warm_lo, l.warm_lo),
    );

    if a.cold {
        println!("\nnote: --cold gave the remote leg an empty (temp) hf-hub file cache, so its cold call");
        println!("      is a true download. The OS page cache isn't dropped, so the local cold figure is");
        println!("      only as cold as the page cache already was.");
    } else {
        println!("\nnote: without --cold the remote leg used your existing hf-hub cache, so its 'cold' call");
        println!("      may already be warm. Pass --cold for a true cold-download measurement.");
    }
    Ok(())
}
