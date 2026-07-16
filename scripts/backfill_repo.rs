//! One-off backfill: add the `repo` column to a local store in place via Lance `add_columns` —
//! each row's value resolved per session from its transcript's cwd (the same resolver the indexer
//! uses). Additive: no data rewrite, no re-embed, no reindex — one column-append commit, vectors
//! and indexes untouched. Skips a store that already carries `repo`. Local store only
//! (`$FUNES_HOME` selects which).
//!
//!   cargo run --example backfill_repo
//!
//! Disposable: delete this file and its `[[example]]` entry once every store carries `repo`.

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use funes::{dataset, lock, repo};
use lance::dataset::{BatchUDF, Dataset, NewColumnTransform};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // add_columns is a schema-evolution commit — hold the store lock to be the sole writer.
    let _lock = lock::StoreLock::acquire()?;
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let Ok(mut ds) = dataset::open(&uri, HashMap::new()).await else {
        println!("no local store to backfill");
        return Ok(());
    };
    if arrow_schema::Schema::from(ds.schema())
        .column_with_name("repo")
        .is_some()
    {
        println!("already carries `repo`");
        return Ok(());
    }

    eprintln!("resolving repos from transcripts…");
    let by_session = session_repos(&ds).await?;
    let resolved = by_session.values().filter(|r| !r.is_empty()).count();
    eprintln!("resolved {resolved}/{} session(s) to a repo", by_session.len());

    // Append `repo`: a UDF maps each row's session_id to its resolved value (empty = unresolved).
    let out_schema = Arc::new(Schema::new(vec![Field::new("repo", DataType::Utf8, true)]));
    let mapper_schema = out_schema.clone();
    ds.add_columns(
        NewColumnTransform::BatchUDF(BatchUDF {
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
        }),
        Some(vec!["session_id".to_string()]),
        None,
    )
    .await?;

    println!(
        "added `repo` to {} chunk(s) ({resolved} session(s) resolved)",
        ds.count_rows(None).await?
    );
    Ok(())
}

/// session_id → resolved repo, from a scan of `(session_id, source_path)`. A session's rows share
/// one transcript, so resolve once per session; `git` runs once per distinct cwd.
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
