//! Gated live test: open the shared funes fixture hosted on the HF Hub over `hf://` and read
//! from it. Skipped unless `HF_FUNES_TEST_TOKEN` is in the environment — to run it:
//!
//!   export HF_FUNES_TEST_TOKEN=<your HF token>   # or a CI secret
//!   cargo test --test remote_recall -- --nocapture
//!
//! The fixture is a stable, synthetic, read-only dataset (no real data).

use funes::hub::Store;

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
    // funes' Store::open authenticates via HF_TOKEN.
    std::env::set_var("HF_TOKEN", token);

    // Open + read over the wire (also exercises the dim guard in open()).
    let ds = Store::parse(FIXTURE_URI)
        .open()
        .await
        .expect("open remote fixture over hf://");
    let n = ds.count_rows(None).await.expect("count_rows");
    assert!(n > 0, "remote fixture is empty");
    eprintln!("remote rows = {n}");

    // End-to-end: the full recall pipeline (hybrid vector + BM25 → rerank → recency → format) over
    // the remote store surfaces the marker chunk — exercising both the remote IVF_PQ and inverted-
    // index reads (lazy, Xet-cached). recency off, no neighbors, to keep the assertion tight.
    let out = funes::recall::recall(Store::parse(FIXTURE_URI), MARKER.to_string(), 5, 30, 0.0, 0, None, None)
        .await
        .expect("recall over remote fixture");
    assert!(out.contains(MARKER), "recall did not surface the marker chunk: {out}");
}
