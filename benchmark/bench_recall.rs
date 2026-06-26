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
//!     --remote dacorvo/funes-bench --iters 5 --cold
//! ```
//!
//! `--cold` points the Xet cache (`HF_XET_CACHE`) at a throwaway temp dir so the remote cold call
//! is a true download, leaving your real `~/.cache/huggingface/xet` untouched. (It does not, and
//! cannot portably, drop the OS page cache, so the local cold figure is only as cold as the page
//! cache happens to be.) Downloading the dataset needs the `hf` CLI and, for a private repo, a
//! logged-in token (the same token recall uses).

use anyhow::{Context, Result};
use clap::Parser;
use funes::hub::Store;
use funes::recall::recall;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;

#[derive(Parser)]
#[command(about = "Benchmark recall latency over one dataset: local vs remote, cold vs warm")]
struct Args {
    /// Query to recall.
    #[arg(default_value = "how does recall rerank candidates")]
    query: String,
    /// Dataset to benchmark (`org/repo` or `hf://…`), used for BOTH legs.
    #[arg(long, default_value = "dacorvo/funes-bench")]
    remote: String,
    /// Warm iterations timed per store (after the one cold call).
    #[arg(long, default_value_t = 5)]
    iters: usize,
    /// Give the remote leg a throwaway temp Xet cache so its cold call is a true download — your
    /// real `~/.cache/huggingface/xet` is left untouched.
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

/// Download the `--remote` dataset into `dest` (which then holds `chunks.lance`) so it can be opened
/// as a local store. Shells out to the `hf` CLI; a private repo uses the logged-in token.
///
/// The download gets its OWN throwaway Xet cache: otherwise it would pre-warm the cache the remote
/// leg reads (Xet reconstructs files through `HF_XET_CACHE`), and the remote "cold" call would hit
/// chunks this very download just fetched — measuring a warm read, not a cold one.
fn download_dataset(remote: &str, dest: &Path) -> Result<()> {
    let repo = remote.strip_prefix("hf://datasets/").unwrap_or(remote);
    let dl_xet = tempfile::Builder::new().prefix("funes-bench-dl-xet-").tempdir()?;
    eprintln!("downloading {repo} → {} (local leg)…", dest.display());
    let ok = Command::new("hf")
        .args(["download", repo, "--repo-type", "dataset", "--local-dir"])
        .arg(dest)
        .env("HF_XET_CACHE", dl_xet.path())
        .status()
        .context("run `hf download` (is the huggingface CLI installed?)")?
        .success();
    if !ok {
        anyhow::bail!("`hf download {repo}` failed");
    }
    Ok(())
}

/// Point the Xet chunk cache at a fresh temp dir so the first remote read is genuinely cold, without
/// touching the user's real `~/.cache/huggingface/xet`. `HF_XET_CACHE` relocates only the chunk
/// cache — the HF token and `HF_HOME` are untouched, so auth still works. The returned dir must stay
/// alive for the run; dropping it removes the temp cache.
fn isolate_xet_cache() -> Result<TempDir> {
    let dir = tempfile::Builder::new().prefix("funes-bench-xet-").tempdir()?;
    std::env::set_var("HF_XET_CACHE", dir.path());
    eprintln!(
        "cold: isolated Xet cache at {} (real cache untouched)",
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

    // With --cold, redirect the Xet cache to a temp dir for the whole run (held alive here) so the
    // remote leg starts cold without disturbing the user's real cache. The local leg uses no Xet.
    let _xet_guard = if a.cold { Some(isolate_xet_cache()?) } else { None };

    // The local leg is a fresh download of the SAME dataset, so local vs remote isolates the hf://
    // I/O path rather than comparing two different stores. `local_dir` is held to the end of the run
    // so the downloaded copy isn't removed mid-benchmark.
    let local_dir = tempfile::Builder::new().prefix("funes-bench-local-").tempdir()?;
    download_dataset(a.remote.trim(), local_dir.path())?;
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
        "store", "cold(ms)", "warm_lo", "warm_med", "warm_hi", "hits"
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
        println!("\nnote: --cold gave the remote leg an empty (temp) Xet cache, so its cold call is a true");
        println!("      download. The OS page cache isn't dropped, so the local cold figure is only as");
        println!("      cold as the page cache already was.");
    } else {
        println!("\nnote: without --cold the remote leg used your existing Xet cache, so its 'cold' call may");
        println!("      already be partly warm. Pass --cold for a true cold-download measurement.");
    }
    Ok(())
}
