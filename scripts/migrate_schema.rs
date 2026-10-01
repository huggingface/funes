//! One-off migration: bring a memory from before the `workdir` rename up to today's schema, in
//! place. Renames `project` to `workdir` and adds the `harness` and `repo` columns a reader
//! projects, all null. Both steps are metadata-only, so text, vectors, ids and indexes are
//! untouched, and a null facet reads the way an absent column did. Without `--apply` it only
//! prints what each memory needs. A remote memory lands each step in one head-guarded commit (a
//! moved head fails that memory rather than retrying, so run it with no concurrent writers). The
//! default local memory is altered under the memory lock.
//!
//!   cargo run --example migrate_schema -- [--apply] <memory>…
//!
//! `<memory>` is `local`, a directory holding `chunks.lance`, `<org>/<repo>` or an `hf://` URI.
//!
//! Disposable: delete this file, its `[[example]]` entry and `remote::rename_column` once every
//! memory is migrated.

use anyhow::{bail, ensure, Context, Result};
use arrow_schema::{DataType, Field, Schema};
use funes::hub;
use funes::memory::{dataset, lock, remote, Memory};
use hf_hub::HFClient;
use lance::dataset::{ColumnAlteration, Dataset, NewColumnTransform};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let mut specs: Vec<String> = std::env::args().skip(1).collect();
    let apply = specs.iter().any(|a| a == "--apply");
    specs.retain(|a| a != "--apply");
    if specs.is_empty() {
        bail!("usage: migrate_schema [--apply] <memory>…");
    }

    let mut failures = 0usize;
    for spec in &specs {
        match migrate_one(spec, apply).await {
            Ok(msg) => println!("{spec}: {msg}"),
            Err(e) => {
                failures += 1;
                println!("{spec}: FAILED: {e:#}");
            }
        }
    }
    if failures > 0 {
        bail!("{failures} memory(s) failed");
    }
    Ok(())
}

/// What a memory's schema lacks against today's.
#[derive(Default)]
struct Plan {
    rename_project: bool,
    add: Vec<&'static str>,
}

impl Plan {
    fn is_empty(&self) -> bool {
        !self.rename_project && self.add.is_empty()
    }

    fn describe(&self) -> String {
        let mut steps = Vec::new();
        if self.rename_project {
            steps.push("rename project to workdir".to_string());
        }
        if !self.add.is_empty() {
            steps.push(format!("add {}", self.add.join(", ")));
        }
        steps.join(", ")
    }
}

/// The steps that bring `ds` to today's schema. A memory with neither facet column, or with
/// both, is not one this migration knows.
fn plan(ds: &Dataset) -> Result<Plan> {
    let schema = arrow_schema::Schema::from(ds.schema());
    let has = |name: &str| schema.column_with_name(name).is_some();
    let mut plan = Plan::default();
    match (has("project"), has("workdir")) {
        (true, false) => plan.rename_project = true,
        (false, true) => {}
        (true, true) => bail!("carries both `project` and `workdir`"),
        (false, false) => bail!("has neither `project` nor `workdir`: not a funes memory?"),
    }
    for name in ["harness", "repo"] {
        if !has(name) {
            plan.add.push(name);
        }
    }
    Ok(plan)
}

fn rename_project() -> ColumnAlteration {
    ColumnAlteration::new("project".into()).rename("workdir".into())
}

/// New nullable utf8 columns, added as metadata only: every existing row reads null.
fn null_columns(names: &[&str]) -> NewColumnTransform {
    let fields: Vec<Field> = names.iter().map(|n| Field::new(*n, DataType::Utf8, true)).collect();
    NewColumnTransform::AllNulls(Arc::new(Schema::new(fields)))
}

async fn migrate_one(spec: &str, apply: bool) -> Result<String> {
    let memory = Memory::parse(spec.trim());
    match &memory {
        Memory::Local { path } => {
            let uri = dataset::table_uri(&path.to_string_lossy());
            migrate_local(&uri, memory.is_default_local(), apply).await
        }
        Memory::Remote { uri } => migrate_remote(uri, apply).await,
    }
}

async fn migrate_local(uri: &str, is_default: bool, apply: bool) -> Result<String> {
    let mut ds = dataset::open(uri, HashMap::new()).await?;
    let rows = ds.count_rows(None).await?;
    let steps = plan(&ds)?;
    if steps.is_empty() {
        return Ok(format!("current ({rows} rows)"));
    }
    if !apply {
        return Ok(format!("needs: {} ({rows} rows); pass --apply", steps.describe()));
    }

    // The default local memory has other writers (the index hooks), so be the only one.
    let _lock = if is_default {
        Some(lock::MemoryLock::acquire()?)
    } else {
        None
    };
    if steps.rename_project {
        ds.alter_columns(&[rename_project()])
            .await
            .context("renaming project")?;
    }
    if !steps.add.is_empty() {
        ds.add_columns(null_columns(&steps.add), None, None)
            .await
            .context("adding the facet columns")?;
    }

    let ds = dataset::open(uri, HashMap::new()).await?;
    ensure!(plan(&ds)?.is_empty(), "post-migration schema is still behind");
    Ok(format!("migrated: {} ({rows} rows)", steps.describe()))
}

async fn migrate_remote(uri: &str, apply: bool) -> Result<String> {
    let token = hub::hf_token().context("no Hugging Face token: set HF_TOKEN, or run `hf auth login`")?;
    let (owner, name, _prefix) = hub::parse_hf(uri)?;
    let dataset_uri = dataset::table_uri(uri);
    let rev = "main".to_string();
    let opts = HashMap::from([
        ("hf_token".to_string(), token.clone()),
        ("revision".to_string(), rev.clone()),
    ]);

    let ds = dataset::open(&dataset_uri, opts.clone()).await?;
    let rows = ds.count_rows(None).await?;
    let steps = plan(&ds)?;
    drop(ds);
    if steps.is_empty() {
        return Ok(format!("current ({rows} rows)"));
    }
    if !apply {
        return Ok(format!("needs: {} ({rows} rows); pass --apply", steps.describe()));
    }

    let repo = HFClient::builder().token(token).build()?.dataset(owner, name);
    let mut oids = Vec::new();
    if steps.rename_project {
        let oid = remote::rename_column(
            &repo,
            &dataset_uri,
            opts.clone(),
            &rev,
            "migrate: rename the facet column from project to workdir".to_string(),
            "project",
            "workdir",
        )
        .await?;
        oids.push(oid);
    }
    if !steps.add.is_empty() {
        let oid = remote::add_column(
            &repo,
            &dataset_uri,
            opts.clone(),
            &rev,
            format!("migrate: add the {} column(s)", steps.add.join(" and ")),
            null_columns(&steps.add),
            Vec::new(),
        )
        .await?;
        oids.push(oid);
    }

    let ds = dataset::open(&dataset_uri, opts).await?;
    ensure!(
        plan(&ds)?.is_empty(),
        "post-migration schema is still behind (commits {})",
        oids.join(", ")
    );
    Ok(format!(
        "migrated: {} ({rows} rows) in commit(s) {}",
        steps.describe(),
        oids.join(", ")
    ))
}
