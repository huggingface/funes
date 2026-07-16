//! One-off migration: bring a store's facet column up to date, in place. Renames the column from
//! `project` to `workdir` if the store predates the rename, then recomputes each row's value from
//! its source transcript — the same per-harness derivation as the indexer, so a migrated store
//! matches a freshly rebuilt one wherever the transcript still exists; a row whose transcript is
//! gone keeps its stored facet (the store may be its only remaining record). Text and vectors are
//! untouched. Local store only (`$FUNES_HOME` selects which).
//!
//!   cargo run --example migrate_workdir_facet
//!
//! Disposable: delete this file and its `[[example]]` entry once every store is migrated.

use anyhow::{Context, Result};
use arrow_array::{Array, RecordBatch, RecordBatchIterator, StringArray};
use funes::{claude_traces, codex_traces, dataset, jsonl, lock, pi_traces};
use lance::dataset::{ColumnAlteration, Dataset, WriteMode, WriteParams};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // The rewrite replaces the whole table, so hold the store lock to be the sole writer.
    let _lock = lock::StoreLock::acquire()?;
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let Ok(mut ds) = dataset::open(&uri, HashMap::new()).await else {
        println!("no local store to migrate");
        return Ok(());
    };

    // A store from before the rename: `alter_columns` is a metadata-only commit, no data rewrite.
    let schema = arrow_schema::Schema::from(ds.schema());
    if schema.column_with_name("workdir").is_none() && schema.column_with_name("project").is_some() {
        eprintln!("renaming the `project` column to `workdir`…");
        ds.alter_columns(&[ColumnAlteration::new("project".into()).rename("workdir".into())])
            .await?;
    }

    eprintln!("loading the local store…");
    let batches = dataset::scan_rows(&ds, &[], None, None).await?;
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    if total == 0 {
        println!("store is empty");
        return Ok(());
    }

    // One derivation per distinct source_path — a session's rows all share one transcript.
    let mut derived: HashMap<String, Option<String>> = HashMap::new();
    // (old, new) → row count, for the report.
    let mut renames: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut untouched = 0usize;
    let mut changed = 0usize;

    let mut out: Vec<RecordBatch> = Vec::new();
    for b in &batches {
        let projects = col_str(b, "workdir")?;
        let sources = col_str(b, "source_path")?;
        let harnesses = col_str(b, "harness")?;
        let mut new_projects: Vec<Option<String>> = Vec::with_capacity(b.num_rows());
        for i in 0..b.num_rows() {
            let old = projects.value(i);
            let (source, harness) = (sources.value(i), harnesses.value(i));
            let new = derived
                .entry(source.to_string())
                .or_insert_with(|| derive_project(source, harness));
            match new.as_deref() {
                Some(p) if p != old => {
                    *renames.entry((old.to_string(), p.to_string())).or_default() += 1;
                    changed += 1;
                    new_projects.push(Some(p.to_string()));
                }
                Some(_) => new_projects.push(Some(old.to_string())),
                None => {
                    untouched += 1;
                    new_projects.push((!projects.is_null(i)).then(|| old.to_string()));
                }
            }
        }
        let proj_idx = b.schema().index_of("workdir")?;
        let mut cols = b.columns().to_vec();
        cols[proj_idx] = Arc::new(StringArray::from(new_projects));
        out.push(RecordBatch::try_new(b.schema(), cols)?);
    }

    if changed == 0 {
        println!("store is already migrated ({total} chunks)");
        return Ok(());
    }

    // Rewrite in a single Overwrite commit — same rationale as scrub: two commits could be
    // interrupted between them, and for a source-gone session that loss is permanent.
    eprintln!("rewriting the store…");
    let schema = out[0].schema();
    let reader = RecordBatchIterator::new(out.into_iter().map(Ok), schema);
    let mut ds = Dataset::write(
        reader,
        &uri,
        Some(WriteParams {
            mode: WriteMode::Overwrite,
            ..Default::default()
        }),
    )
    .await?;
    dataset::build_indexes(&mut ds, |phase| eprintln!("building {phase}…")).await;

    let mut msg = format!("migrated {changed} of {total} row(s):");
    for ((old, new), n) in &renames {
        msg.push_str(&format!("\n  {old} → {new}  ({n} row(s))"));
    }
    if untouched > 0 {
        msg.push_str(&format!(
            "\nleft {untouched} row(s) as-is (source transcript gone, or not a local JSONL transcript)"
        ));
    }
    println!("{msg}");
    Ok(())
}

/// The facet `source` derives to today, or `None` when it can't be re-derived — the caller leaves
/// those rows alone. The extension gate matters: a parquet-sourced row's file-stem facet is
/// already right, and the path fallback would clobber it with the parent dir. Mirrors the indexer:
/// the munged recorded cwd, else the path-derived fallback.
fn derive_project(source: &str, harness: &str) -> Option<String> {
    let p = Path::new(source);
    if !p.extension().map(|x| x == "jsonl").unwrap_or(false) {
        return None;
    }
    let records = match jsonl::read_jsonl_records(p) {
        Ok(r) => r,
        // A gone Claude transcript still derives exactly: its path's `projects` segment IS the
        // munged cwd. Other layouts carry nothing recoverable — leave their rows alone.
        Err(_) => return (harness == "claude_code").then(|| claude_traces::workdir_of(p)),
    };
    let from_cwd = match harness {
        "claude_code" => claude_traces::workdir_from_records(&records),
        "codex" => codex_traces::workdir_from_records(&records),
        "pi" => pi_traces::workdir_from_records(&records),
        _ => return None,
    };
    Some(from_cwd.unwrap_or_else(|| claude_traces::workdir_of(p)))
}

/// A named Utf8 column of `b`, or an error naming what's missing.
fn col_str<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .with_context(|| format!("store has no `{name}` column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("`{name}` column is not utf8"))
}
