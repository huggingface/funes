//! A fresh install with no personal index recalls the built-in hello-world corpus, so the first
//! `funes recall` returns something useful. Mirrors index_recall.rs but builds *no* index: it
//! points `$FUNES_HOME` at an empty temp dir and exercises the read surface's fallback. Runs the
//! real BGE embedder + reranker (downloaded to the fastembed cache on first run).

use funes::hub::Store;

#[tokio::test]
async fn recall_without_an_index_uses_the_builtin_guide() {
    let empty = tempfile::tempdir().unwrap();
    // $FUNES_HOME points at an empty dir: Store::local() resolves there, there is no dataset to open,
    // so the read surface must fall back to the hello-world corpus.
    std::env::set_var("FUNES_HOME", empty.path());

    // recall: a question about wiring funes into an agent surfaces the MCP-setup passage.
    let out = funes::recall::recall(
        Store::local(),
        "how do I connect funes to claude code".into(),
        5,
        30,
        30.0,
        1,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_ne!(out, "no results", "fallback recall returned nothing: {out}");
    assert!(out.contains("funes/hello"), "expected a built-in hello hit: {out}");
    assert!(out.contains("funes install"), "expected the funes install passage: {out}");
    assert!(
        out.contains("→ get hello"),
        "hit should carry a resolvable get line: {out}"
    );

    // get: the `→ get` line resolves against the corpus and expands the turn.
    let got = funes::recall::get(Store::local(), "hello".into(), "hello-0005".into(), 3)
        .await
        .unwrap();
    assert!(got.contains("funes install"), "get should expand a corpus turn: {got}");

    // list: the synthetic session shows under the funes project.
    let list = funes::recall::list(Store::local(), None, 50).await.unwrap();
    assert!(
        list.contains("funes/hello"),
        "list should show the built-in session: {list}"
    );

    // status: guides the user to index instead of erroring on the missing store.
    let status = funes::recall::status(Store::local()).await.unwrap();
    assert!(
        status.contains("funes index"),
        "status should point at `funes index`: {status}"
    );
}
