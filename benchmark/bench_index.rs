//! Benchmark `funes index <source>`: build time, embedding throughput, and the compactness of the
//! resulting store. Useful for tracking the parquet bulk-import path (one append, one Lance fragment) and
//! catching regressions like per-session appends bloating the store.
//!
//! The index is built into a throwaway `$FUNES_HOME` so your real store and config are untouched
//! (no remote is attached there, so no push fires).
//!
//! ```text
//! cargo run --release --example bench_index -- path/to/traces.parquet
//! cargo run --release --example bench_index -- ~/.claude/projects   # a JSONL tree
//! ```
//!
//! Elapsed includes the one-time embedding-model load (~1-2s), so reported throughput is a slight
//! under-estimate on small inputs and accurate on large ones.

use anyhow::Result;
use arrow_array::{Array, StringArray};
use clap::Parser;
use funes::hub::Store;
use funes::index::run_index;
use futures::TryStreamExt;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(about = "Benchmark index build: time, throughput, memory compactness")]
struct Args {
    /// Source to index: a `.parquet` trace dataset or a directory of JSONL transcripts.
    source: PathBuf,
    /// Index at most this many sessions, to bound build time. Raise it for a longer, steadier run.
    #[arg(long, default_value_t = 500)]
    sessions: usize,
    /// Exclude thinking blocks (passed through to `index`).
    #[arg(long)]
    no_thinking: bool,
}

/// Total bytes of every file under `dir`.
fn dir_bytes(dir: &std::path::Path) -> u64 {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();

    // Build into a throwaway FUNES_HOME (held alive for the run): the real store/config are untouched
    // and, with no remote attached there, `index` won't push.
    let home = tempfile::Builder::new().prefix("funes-bench-index-").tempdir()?;
    std::env::set_var("FUNES_HOME", home.path());

    let t = Instant::now();
    run_index(a.source.as_path(), a.no_thinking, Some(a.sessions)).await?;
    let secs = t.elapsed().as_secs_f64();

    // Inspect the built store: chunks (rows), distinct sessions, on-disk size, and Lance fragments —
    // three different granularities, so the output shows they don't coincide.
    let store = home.path().join("store");
    let ds = Store::local().open().await?;
    let chunks = ds.count_rows(None).await?;
    let mut scan = ds.scan();
    scan.project(&["session_id"])?;
    let mut stream = scan.try_into_stream().await?;
    let mut sids = HashSet::new();
    while let Some(batch) = stream.try_next().await? {
        if let Some(col) = batch
            .column_by_name("session_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            (0..col.len()).filter(|&i| !col.is_null(i)).for_each(|i| {
                sids.insert(col.value(i).to_string());
            });
        }
    }
    let sessions = sids.len();
    let mb = dir_bytes(&store) as f64 / 1e6;
    let data = store.join("chunks.lance").join("data");
    let fragments = std::fs::read_dir(&data).map(|d| d.count()).unwrap_or(0);

    println!("\n=== index benchmark ===");
    println!("source:           {}", a.source.display());
    println!("elapsed:          {secs:.1}s  (incl. model load)");
    println!("sessions:         {sessions}");
    println!("chunks:           {chunks}");
    println!("throughput:       {:.0} chunks/s", chunks as f64 / secs.max(0.001));
    println!("memory size:      {mb:.0} MB");
    println!("lance fragments:  {fragments}");
    Ok(())
}
