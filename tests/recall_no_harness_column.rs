//! Regression: recall must still work on a store built before the `harness` column existed (an
//! un-migrated store). Build a normal store, drop `harness` to reproduce that schema, then recall —
//! projecting a column the dataset lacks would error, so recall's projection must adapt. Guards the
//! same path the gated `remote_recall` covers, but unconditionally.
//!
//! Its own binary (not folded into `index_recall.rs`) because it sets the process-global
//! `FUNES_HOME`, and cargo runs a file's tests concurrently — two such tests would clobber each
//! other's store.

use std::collections::HashMap;
use std::io::Write;

#[tokio::test]
async fn recall_tolerates_a_store_without_the_harness_column() {
    let db_dir = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", db_dir.path());

    // A minimal Claude transcript so parse → chunk → embed → store produces a real recallable row.
    let session = "test-session-0001";
    let dir = source.path().join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
    for l in [
        r#"{"type":"user","uuid":"t1","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"how do we parse transcripts into turns"}}"#,
        r#"{"type":"assistant","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:05Z","message":{"role":"assistant","content":[{"type":"text","text":"We parse each JSONL line into a turn with typed blocks."}]}}"#,
    ] {
        writeln!(f, "{l}").unwrap();
    }

    funes::index::run_index(source.path(), false, None).await.unwrap();

    // Drop the harness column in place — the schema an un-migrated store has.
    let uri = funes::dataset::table_uri(&funes::dataset::local_store_dir());
    let mut ds = funes::dataset::open(&uri, HashMap::new()).await.unwrap();
    ds.drop_columns(&["harness"]).await.unwrap();
    assert!(
        arrow_schema::Schema::from(ds.schema())
            .column_with_name("harness")
            .is_none(),
        "harness column should be gone"
    );

    let out = funes::recall::recall(
        funes::hub::Store::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
    )
    .await
    .expect("recall over a store without the harness column");
    assert!(
        out.contains(session),
        "recall should surface the session even without a harness column: {out}"
    );

    // But a `--harness` filter needs the column: refuse with a clear message, not an opaque Lance
    // schema error.
    let err = funes::recall::recall(
        funes::hub::Store::local(),
        "parse transcripts into turns".into(),
        5,
        30,
        30.0,
        1,
        None,
        Some("pi".into()),
    )
    .await
    .expect_err("--harness on a column-less store should error");
    assert!(
        err.to_string().contains("predates the harness facet"),
        "error should explain the store predates the harness facet: {err}"
    );
}
