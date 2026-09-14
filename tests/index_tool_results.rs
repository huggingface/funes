//! The tool-result exclusion is explicit and backwards-compatible: a normal index keeps all
//! tiers, while `IndexOptions` can omit the bulky output tier and later backfill it.

use arrow_array::{Array, StringArray};
use funes::commands::index::{run_index_with_options, IndexOptions};
use funes::memory::Memory;
use std::io::Write;

fn write_session(source: &std::path::Path) {
    let dir = source.join("projects").join("-home-u-dev-demo");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("tool-result-session.jsonl")).unwrap();
    for line in [
        r#"{"type":"user","uuid":"t0","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"decide how to index transcripts"}}"#,
        r#"{"type":"assistant","uuid":"t1","parentUuid":"t0","timestamp":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        r#"{"type":"user","uuid":"t2","parentUuid":"t1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"22 tests passed"}]}]}}"#,
    ] {
        writeln!(f, "{line}").unwrap();
    }
}

async fn block_types() -> Vec<String> {
    let ds = Memory::local().open().await.unwrap();
    let batches = funes::memory::dataset::scan_rows(&ds, &["block_type"], None, None)
        .await
        .unwrap();
    batches
        .into_iter()
        .flat_map(|batch| {
            let values = batch
                .column_by_name("block_type")
                .unwrap()
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .clone();
            (0..values.len()).map(move |i| values.value(i).to_string())
        })
        .collect()
}

#[tokio::test]
async fn tool_results_are_opt_out_and_can_be_backfilled() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    write_session(source.path());

    run_index_with_options(source.path(), IndexOptions::from_flags(false, true), None)
        .await
        .unwrap();
    let without_results = block_types().await;
    assert!(without_results.iter().any(|kind| kind == "text"));
    assert!(without_results.iter().any(|kind| kind == "tool_use"));
    assert!(!without_results.iter().any(|kind| kind == "tool_result"));

    // A later run with the default policy backfills the omitted tier without duplicating the rows
    // already written by the opt-out run.
    run_index_with_options(source.path(), IndexOptions::default(), None)
        .await
        .unwrap();
    let with_results = block_types().await;
    assert_eq!(with_results.iter().filter(|kind| *kind == "text").count(), 1);
    assert_eq!(with_results.iter().filter(|kind| *kind == "tool_use").count(), 1);
    assert_eq!(with_results.iter().filter(|kind| *kind == "tool_result").count(), 1);
}
