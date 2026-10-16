//! Regression: recall must still work on a memory built before the `harness` column existed (an
//! un-migrated memory). Build a normal memory, drop `harness` to reproduce that schema, then recall —
//! projecting a column the dataset lacks would error, so recall's projection must adapt. Guards the
//! same path the gated `remote_recall` covers, but unconditionally.
//!
//! Its own binary (not folded into `index_recall.rs`) because it sets the process-global
//! `FUNES_HOME`, and cargo runs a file's tests concurrently — two such tests would clobber each
//! other's memory.

mod support;

use funes::home;
use std::collections::HashMap;

#[tokio::test]
async fn recall_tolerates_a_memory_without_the_harness_column() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());

    let (session, _) = support::write_transcript(source.path());
    funes::commands::index::run_index(source.path(), false, None)
        .await
        .unwrap();

    // Drop the harness column in place — the schema an un-migrated memory has.
    let uri = funes::memory::dataset::table_uri(home::memory_dir());
    let mut ds = funes::memory::dataset::open(&uri, HashMap::new()).await.unwrap();
    ds.drop_columns(&["harness"]).await.unwrap();
    assert!(
        arrow_schema::Schema::from(ds.schema())
            .column_with_name("harness")
            .is_none(),
        "harness column should be gone"
    );

    let out = funes::commands::recall::recall(
        funes::memory::Memory::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        1,
        Default::default(),
    )
    .await
    .expect("recall over a memory without the harness column");
    assert!(
        out.contains(&session),
        "recall should surface the session even without a harness column: {out}"
    );

    // But a `--harness` filter needs the column: refuse with a clear message, not an opaque Lance
    // schema error.
    let err = funes::commands::recall::recall(
        funes::memory::Memory::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        1,
        funes::commands::recall::RecallFilter {
            harness: Some("pi".into()),
            ..Default::default()
        },
    )
    .await
    .expect_err("--harness on a column-less memory should error");
    assert!(
        err.to_string().contains("predates the harness facet"),
        "error should explain the memory predates the harness facet: {err}"
    );
}
