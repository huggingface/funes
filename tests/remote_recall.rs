//! Gated live test: open the shared funes fixture hosted on the HF Hub over `hf://` and read
//! from it. Skipped unless `HF_FUNES_TEST_TOKEN` is in the environment — to run it:
//!
//!   export HF_FUNES_TEST_TOKEN="$(cat hf_funes_test_token.txt)"   # or CI secret
//!   cargo test --test remote_recall -- --nocapture
//!
//! The fixture is a stable, synthetic, read-only dataset (no real data). Generating an
//! ephemeral dataset per run is deferred to Step 5, once funes can push/delete itself.

use arrow_array::{Array, StringArray};
use funes::hub::Source;
use futures::TryStreamExt;
use lance_index::scalar::FullTextSearchQuery;
use lancedb::query::{ExecutableQuery, QueryBase};

const FIXTURE_URI: &str = "hf://datasets/optimum-internal-testing/funes-test/fixture/lancedb";
const MARKER: &str = "UNIQUEMARKERXYZZY";

#[tokio::test]
async fn recall_from_remote_fixture() {
    // Gate on a non-empty token. In CI, `${{ secrets.X }}` for a fork/unset secret expands to
    // an empty string (so env::var returns Ok("")), which must also skip.
    let token = std::env::var("HF_FUNES_TEST_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        eprintln!("skip: HF_FUNES_TEST_TOKEN not set");
        return;
    }
    // funes' Source::open authenticates via HF_TOKEN.
    std::env::set_var("HF_TOKEN", token);

    let tbl = Source::parse(FIXTURE_URI, None)
        .open()
        .await
        .expect("open remote fixture over hf://");

    // Open + read over the wire (also exercises the dim guard in open()).
    let n = tbl.count_rows(None).await.expect("count_rows");
    assert!(n > 0, "remote fixture is empty");
    eprintln!("remote rows = {n}");

    // FTS path: the remote inverted index surfaces the unique marker chunk.
    let mut fts = tbl
        .query()
        .full_text_search(FullTextSearchQuery::new(MARKER.to_string()))
        .limit(5)
        .execute()
        .await
        .expect("full-text search");
    let mut found = false;
    while let Some(batch) = fts.try_next().await.expect("fts batch") {
        if let Some(col) = batch
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        {
            for i in 0..col.len() {
                if col.value(i).contains(MARKER) {
                    found = true;
                }
            }
        }
    }
    assert!(found, "FTS did not surface the marker chunk from the remote fixture");

    // Vector path: a nearest_to query exercises the remote IVF_PQ read (lazy, Xet-cached).
    let mut vq = tbl
        .query()
        .nearest_to(vec![0.0f32; 384])
        .expect("nearest_to")
        .limit(5)
        .execute()
        .await
        .expect("vector query");
    let mut vrows = 0;
    while let Some(batch) = vq.try_next().await.expect("vec batch") {
        vrows += batch.num_rows();
    }
    assert!(vrows > 0, "vector query returned no rows from the remote fixture");
    eprintln!("remote vector-query rows = {vrows}");
}
