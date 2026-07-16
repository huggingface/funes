//! One-off backfill: add the `repo` column to a remote store on the Hub via `add_columns`, landing
//! it in one head-guarded commit. Each row's value is resolved per session from its transcript's
//! cwd using the **local** transcripts (the remote mirrors this machine), with the same resolver
//! the indexer uses. Additive: no data rewrite, no re-embed, no reindex. Skips a store already
//! carrying `repo`. Run with no concurrent writers (a single guarded attempt).
//!
//!   cargo run --example backfill_remote_repo -- <org/repo> [<org/repo>…]
//!
//! Disposable: delete this file (and `backfill_repo`, `hf_dataset::add_column`) once every store
//! carries `repo`.

use anyhow::{bail, Context, Result};
use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use funes::{dataset, hf_dataset, hub, repo};
use hf_hub::HFClient;
use lance::dataset::{BatchUDF, Dataset, NewColumnTransform};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let specs: Vec<String> = std::env::args().skip(1).collect();
    if specs.is_empty() {
        bail!("usage: backfill_remote_repo <org/repo> [<org/repo>…]");
    }
    let token = hub::hf_token().context("no HF token (set HF_TOKEN) — required to commit")?;

    let mut failures = 0usize;
    for spec in &specs {
        match backfill_one(spec, &token).await {
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

async fn backfill_one(spec: &str, token: &str) -> Result<String> {
    let uri = match hub::Store::parse(spec) {
        hub::Store::Remote { uri } => uri,
        hub::Store::Local { .. } => bail!("not a remote store — use `backfill_repo` for the local store"),
    };
    let (owner, name, _prefix) = hub::parse_hf(&uri)?;
    let dataset_uri = format!("{uri}/{}.lance", dataset::TABLE);
    let rev = "main".to_string();
    let opts = HashMap::from([
        ("hf_token".to_string(), token.to_string()),
        ("revision".to_string(), rev.clone()),
    ]);

    let ds = dataset::open(&dataset_uri, opts.clone()).await?;
    if arrow_schema::Schema::from(ds.schema())
        .column_with_name("repo")
        .is_some()
    {
        return Ok("already carries `repo`".to_string());
    }

    eprintln!("  resolving repos from local transcripts…");
    let by_session = session_repos(&ds).await?;
    drop(ds);
    let resolved = by_session.values().filter(|r| !r.is_empty()).count();
    eprintln!("  resolved {resolved}/{} session(s) to a repo", by_session.len());

    let out_schema = Arc::new(Schema::new(vec![Field::new("repo", DataType::Utf8, true)]));
    let mapper_schema = out_schema.clone();
    let transform = NewColumnTransform::BatchUDF(BatchUDF {
        mapper: Box::new(move |batch: &RecordBatch| {
            let sids = batch
                .column_by_name("session_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                .expect("add_columns read session_id");
            let repos = StringArray::from_iter_values(
                (0..batch.num_rows()).map(|i| by_session.get(sids.value(i)).map(String::as_str).unwrap_or("")),
            );
            RecordBatch::try_new(mapper_schema.clone(), vec![Arc::new(repos)]).map_err(lance::Error::from)
        }),
        output_schema: out_schema,
        result_checkpoint: None,
    });

    let repo_handle = HFClient::builder()
        .token(token.to_string())
        .build()?
        .dataset(owner, name);
    let oid = hf_dataset::add_column(
        &repo_handle,
        &dataset_uri,
        opts.clone(),
        &rev,
        "backfill: add the repo column".to_string(),
        transform,
        vec!["session_id".to_string()],
    )
    .await?;

    // Verify at the new head: readers must now see the column.
    let ds = dataset::open(&dataset_uri, opts).await?;
    anyhow::ensure!(
        arrow_schema::Schema::from(ds.schema())
            .column_with_name("repo")
            .is_some(),
        "post-backfill schema still lacks `repo` (commit {oid})"
    );
    Ok(format!("added `repo` in commit {oid} ({resolved} session(s) resolved)"))
}

/// session_id → resolved repo, from a scan of `(session_id, source_path)`. A session's rows share
/// one transcript, so resolve once per session; `git` runs once per distinct cwd. `source_path`
/// values are local paths (the store was indexed here), so a still-present checkout resolves.
async fn session_repos(ds: &Dataset) -> Result<HashMap<String, String>> {
    let batches = dataset::scan_rows(ds, &["session_id", "source_path"], None, None).await?;
    let mut source_of: HashMap<String, String> = HashMap::new();
    for b in &batches {
        let sid = col(b, "session_id")?;
        let src = col(b, "source_path")?;
        for i in 0..b.num_rows() {
            source_of
                .entry(sid.value(i).to_string())
                .or_insert_with(|| src.value(i).to_string());
        }
    }
    let mut cwd_cache: HashMap<String, String> = HashMap::new();
    let mut repos = HashMap::new();
    for (session, source) in source_of {
        let r = match repo::cwd_of_transcript(Path::new(&source)) {
            Some(cwd) => cwd_cache
                .entry(cwd.clone())
                .or_insert_with(|| repo::of_cwd(&cwd))
                .clone(),
            None => String::new(),
        };
        repos.insert(session, r);
    }
    Ok(repos)
}

/// A named Utf8 column of `b`, or an error naming what's missing.
fn col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .with_context(|| format!("store has no `{name}` column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("`{name}` column is not utf8"))
}
