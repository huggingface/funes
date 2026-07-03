//! One-off: backfill a nullable `harness` column (= "claude_code") on a local funes store built
//! before the harness facet existed, so recall attributes those old rows and a later `funes index`
//! append lines up with the current schema (which now carries `harness` as its last column).
//!
//! Run once per host — `FUNES_HOME` selects the store:
//!   cargo run --release --example migrate
//!
//! The shared remote is migrated by rebuilding it: delete the dataset on the Hub, then `funes push`
//! the migrated local store — a fresh push re-creates it with the `harness` column.
//!
//! Throwaway: delete this file once every store is migrated.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use funes::dataset;
use lance::dataset::{BatchUDF, NewColumnTransform};

const HARNESS: &str = "claude_code";

#[tokio::main]
async fn main() -> Result<()> {
    let uri = dataset::table_uri(&dataset::local_store_dir());
    let mut ds = dataset::open(&uri, HashMap::new())
        .await
        .with_context(|| format!("no local store to migrate at {uri}"))?;

    if arrow_schema::Schema::from(ds.schema())
        .column_with_name("harness")
        .is_some()
    {
        println!("{uri}: already has a harness column — nothing to migrate");
        return Ok(());
    }
    let rows = ds.count_rows(None).await.unwrap_or(0);

    // A nullable Utf8 `harness` column set to the constant. Nullable so it matches a freshly-built
    // store's schema exactly (a SqlExpressions literal would be non-nullable and break the next
    // append); a BatchUDF constant (not AllNulls) so old rows read `claude_code`, not blank.
    let output_schema = Arc::new(Schema::new(vec![Field::new("harness", DataType::Utf8, true)]));
    let schema = output_schema.clone();
    let mapper = Box::new(move |batch: &RecordBatch| -> lance::Result<RecordBatch> {
        let col = StringArray::from(vec![HARNESS; batch.num_rows()]);
        Ok(RecordBatch::try_new(schema.clone(), vec![Arc::new(col)])?)
    });
    ds.add_columns(
        NewColumnTransform::BatchUDF(BatchUDF {
            mapper,
            output_schema,
            result_checkpoint: None,
        }),
        None,
        None,
    )
    .await
    .context("adding the harness column")?;

    println!("{uri}: backfilled harness={HARNESS:?} on {rows} rows");
    Ok(())
}
