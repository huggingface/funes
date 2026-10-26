//! One-off backfill: count the languages of a memory's text and record them, for a memory indexed
//! before funes counted them. A remote memory gets them in one head-guarded commit, a local one in a
//! commit of its own. Skips a memory whose counts already cover all of its rows. `--dry-run` prints
//! the counts and writes nothing. Run with no concurrent writers (a single guarded attempt).
//!
//!   cargo run --release --example backfill_languages -- [--dry-run] <memory> [<memory>…]
//!
//! Disposable: delete this file (and `remote::count_languages`) once every memory records its
//! languages.

use anyhow::{bail, Context, Result};
use funes::hub;
use funes::memory::{dataset, remote, Memory};
use hf_hub::HFClient;
use std::cmp::Reverse;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let specs: Vec<&String> = args.iter().filter(|a| *a != "--dry-run").collect();
    if specs.is_empty() {
        bail!("usage: backfill_languages [--dry-run] <memory> [<memory>…]");
    }

    let mut failures = 0usize;
    for spec in specs {
        match backfill_one(spec, dry_run).await {
            Ok(msg) => println!("{spec}: {msg}"),
            Err(e) => {
                failures += 1;
                println!("{spec}: FAILED — {e:#}");
            }
        }
    }
    if failures > 0 {
        bail!("{failures} memory(s) failed — rerun them after fixing the cause");
    }
    Ok(())
}

async fn backfill_one(spec: &str, dry_run: bool) -> Result<String> {
    let memory = Memory::parse(spec);
    let mut ds = memory.open().await?;
    let rows = dataset::count_rows(&ds, None).await? as u64;
    if dataset::languages(&ds).is_some_and(|languages| languages.rows == rows) {
        return Ok("already counted".to_string());
    }
    if dry_run {
        return Ok(format!("would record {}", shown(&dataset::scan_languages(&ds).await?)));
    }
    match memory {
        Memory::Local { .. } => {
            let languages = dataset::scan_languages(&ds).await?;
            dataset::stamp_languages(&mut ds, &languages).await?;
            Ok(format!("recorded {}", shown(&languages)))
        }
        Memory::Remote { uri } => {
            drop(ds);
            let (languages, oid) = count_remote(&uri).await?;
            Ok(format!("recorded {} in commit {oid}", shown(&languages)))
        }
    }
}

/// Count the languages of the remote memory at `uri` and record them in one guarded commit.
async fn count_remote(uri: &str) -> Result<(dataset::Languages, String)> {
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to commit")?;
    let (owner, name, _prefix) = hub::parse_hf(uri)?;
    let rev = "main".to_string();
    let opts = HashMap::from([
        ("hf_token".to_string(), token.clone()),
        ("revision".to_string(), rev.clone()),
    ]);
    // A commit retried after a lost response reports a conflict for a commit that landed.
    let repo = HFClient::builder()
        .token(token)
        .retry_max_attempts(0)
        .build()?
        .dataset(owner, name);
    remote::count_languages(
        &repo,
        &dataset::table_uri(uri),
        opts,
        &rev,
        "backfill: count the languages".to_string(),
    )
    .await
}

/// `languages` as `eng 3 (75.0%), por 1 (25.0%) over 9 rows`.
fn shown(languages: &dataset::Languages) -> String {
    let total: u64 = languages.counts.values().sum();
    let mut counts: Vec<(&String, &u64)> = languages.counts.iter().collect();
    counts.sort_by_key(|&(_, n)| Reverse(*n));
    let counts: Vec<String> = counts
        .iter()
        .map(|(code, n)| format!("{code} {n} ({:.1}%)", **n as f64 * 100.0 / total as f64))
        .collect();
    format!("{} over {} rows", counts.join(", "), languages.rows)
}
