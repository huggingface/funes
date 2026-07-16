//! One-off migration: rename a remote store's facet column from `project` to `workdir`, in place
//! — one metadata-only, head-guarded commit per store; data files, vectors, ids, and indexes are
//! untouched. Skips a store that already carries `workdir`. Run with no concurrent writers (it is
//! a single guarded attempt: a moved head fails the store rather than retrying).
//!
//!   cargo run --example migrate_remote_workdir -- <org/repo> [<org/repo>…]
//!
//! Disposable: delete this file and its `[[example]]` entry once every store is migrated.

use anyhow::{bail, Context, Result};
use funes::{dataset, hf_dataset, hub};
use hf_hub::HFClient;
use std::collections::HashMap;

#[tokio::main]
async fn main() -> Result<()> {
    let specs: Vec<String> = std::env::args().skip(1).collect();
    if specs.is_empty() {
        bail!("usage: migrate_remote_workdir <org/repo> [<org/repo>…]");
    }
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to commit")?;

    let mut failures = 0usize;
    for spec in &specs {
        match migrate_one(spec, &token).await {
            Ok(msg) => println!("{spec}: {msg}"),
            Err(e) => {
                failures += 1;
                println!("{spec}: FAILED — {e:#}");
            }
        }
    }
    if failures > 0 {
        bail!("{failures} store(s) failed — rerun them after fixing the cause");
    }
    Ok(())
}

/// Rename one store's column, verifying the schema before (skip when already migrated) and after
/// (re-open at the new head and check `workdir` is there).
async fn migrate_one(spec: &str, token: &str) -> Result<String> {
    let store = hub::Store::parse(spec);
    let uri = match &store {
        hub::Store::Remote { uri } => uri.clone(),
        hub::Store::Local { .. } => bail!("not a remote store"),
    };
    let (owner, name, _prefix) = hub::parse_hf(&uri)?;
    let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
    let rev = "main".to_string();
    let opts = HashMap::from([
        ("hf_token".to_string(), token.to_string()),
        ("revision".to_string(), rev.clone()),
    ]);

    let ds = dataset::open(&dataset_uri, opts.clone()).await?;
    let schema = arrow_schema::Schema::from(ds.schema());
    if schema.column_with_name("workdir").is_some() {
        return Ok("already migrated".to_string());
    }
    if schema.column_with_name("project").is_none() {
        bail!("has neither `project` nor `workdir` — not a funes store?");
    }
    drop(ds);

    let client = HFClient::builder().token(token.to_string()).build()?;
    let repo = client.dataset(owner, name);
    let oid = hf_dataset::rename_column(
        &repo,
        &dataset_uri,
        opts.clone(),
        &rev,
        "migrate: rename the facet column from project to workdir".to_string(),
        "project",
        "workdir",
    )
    .await?;

    // Verify at the new head: the renamed schema must be what readers now see.
    let ds = dataset::open(&dataset_uri, opts).await?;
    let schema = arrow_schema::Schema::from(ds.schema());
    anyhow::ensure!(
        schema.column_with_name("workdir").is_some(),
        "post-rename schema still lacks `workdir` (commit {oid})"
    );
    Ok(format!("renamed in commit {oid}"))
}
