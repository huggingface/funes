//! Recall over a memory embedded with multilingual-e5-small, through the real models.

mod support;

use funes::commands::recall;
use funes::inference::EmbeddingModel;
use funes::memory::Memory;
use tempfile::TempDir;

async fn memory_of(model: EmbeddingModel, texts: &[&str]) -> (TempDir, Memory) {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("memory");
    support::memory_of(model, texts, &path).await;
    (home, Memory::parse(&path.to_string_lossy()))
}

#[tokio::test]
async fn recall_embeds_the_query_with_the_memorys_model() {
    let (_home, memory) = memory_of(
        EmbeddingModel::MultilingualE5Small,
        &[
            "给表加上索引",
            "周末我们和几个老朋友一起开车去郊外的山上徒步露营",
            "这只橘色的猫每天早上六点准时跑到厨房门口等着吃鱼",
            "奶奶在厨房里一边听收音机一边包了两百多个饺子",
            "火车因为大雪晚点了两个多小时，站台上挤满了人",
            "孩子们放学后在学校的操场上踢足球一直踢到天黑",
            "他把用了五年的旧手机擦干净送给了上大学的弟弟",
            "今年夏天雨水特别多，河边的路被淹了好几次",
        ],
    )
    .await;

    // No query token is a chunk token and the pool is too small to rerank: only the vectors decide.
    let (_, hits) = recall::recall_hits(memory, "怎样让查询变快".into(), 1, 3, 0, Default::default(), &|_| {})
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.session_id, "s-0", "{}", hits[0].0.text);
}

#[tokio::test]
async fn a_search_reads_memories_of_one_model() {
    let (_e5_home, e5) = memory_of(EmbeddingModel::MultilingualE5Small, &["给表加上索引"]).await;
    let (_bge_home, bge) = memory_of(EmbeddingModel::BgeSmallEn, &["add an index to the table"]).await;
    let quiet = |_: &str| ();
    let search = recall::Search::new("怎样让查询变快".into(), 3, Default::default()).unwrap();
    search.candidates(&e5, &quiet).await.unwrap();
    let err = search.candidates(&bge, &quiet).await.err().unwrap().to_string();
    assert!(
        err.contains(EmbeddingModel::BgeSmallEn.id()) && err.contains(EmbeddingModel::MultilingualE5Small.id()),
        "{err}"
    );
}
