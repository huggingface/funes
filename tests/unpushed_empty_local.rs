//! A push receipt for a remote, with an empty local memory behind it: what a first push with
//! nothing indexed leaves. The local memory adds nothing to that remote's recall, and does not
//! fail it.

use funes::commands::{index, mcp, recall};
use funes::inference::EmbeddingModel;
use funes::memory::Memory;

#[tokio::test]
async fn an_empty_local_memory_adds_no_unpushed_pool() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    index::ensure_local_memory(EmbeddingModel::MultilingualE5Small)
        .await
        .unwrap();
    // The receipt push keeps for hf://datasets/acme/memory, holding no chunk.
    let receipt = home.path().join("pushed/hf___datasets_acme_memory");
    std::fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    std::fs::write(&receipt, "").unwrap();

    let search = recall::Search::new("an index on the table".into(), 30, Default::default()).unwrap();
    let pool = mcp::unpushed(&search, &Memory::parse("hf://datasets/acme/memory"))
        .await
        .unwrap();
    assert!(pool.is_none());
}
