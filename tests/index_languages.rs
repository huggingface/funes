//! The language counts indexing keeps in the memory. Own test binary: it sets `$FUNES_HOME`.

use funes::commands::index;
use funes::home;
use funes::inference::EmbeddingModel;
use funes::memory::dataset::{self, Languages};
use std::collections::BTreeMap;
use std::path::Path;

const PORTUGUESE: &str = "Precisamos decidir se o atalho apaga o caractere à frente do cursor no campo de texto.";
const ENGLISH: &str = "We decided to roll back the deploy first, then look into the configuration drift.";

fn session(dir: &Path, id: &str, turns: &[(&str, &str)]) {
    let lines: Vec<String> = turns
        .iter()
        .enumerate()
        .map(|(seq, (role, text))| {
            serde_json::json!({
                "session_id": id,
                "turn_uuid": format!("{id}-{seq}"),
                "seq": seq,
                "ts": "2026-01-02T00:00:00Z",
                "role": role,
                "harness": "test",
                "blocks": [{"block_type": "text", "text": text}],
            })
            .to_string()
        })
        .collect();
    std::fs::write(dir.join(format!("{id}.funes.jsonl")), lines.join("\n")).unwrap();
}

async fn recorded() -> Option<Languages> {
    let ds = dataset::open(&dataset::table_uri(home::memory_dir()), Default::default())
        .await
        .unwrap();
    dataset::languages(&ds).unwrap()
}

fn counted(rows: u64, eng: u64, por: u64) -> Option<Languages> {
    Some(Languages {
        rows,
        counts: BTreeMap::from([("eng".to_string(), eng), ("por".to_string(), por)]),
    })
}

#[tokio::test]
async fn indexing_keeps_the_memory_language_counts() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    let turns = home.path().join("turns");
    std::fs::create_dir(&turns).unwrap();
    index::ensure_local_memory(EmbeddingModel::BgeSmallEn).await.unwrap();

    session(&turns, "a", &[("user", PORTUGUESE), ("assistant", ENGLISH)]);
    index::run_index(&turns, false, None).await.unwrap();
    assert_eq!(recorded().await, counted(2, 1, 1));

    // A later run adds its own.
    session(&turns, "b", &[("user", PORTUGUESE)]);
    index::run_index(&turns, false, None).await.unwrap();
    assert_eq!(recorded().await, counted(3, 1, 2));

    // A rewritten memory records none, and its next run counts every row, even with nothing to index.
    let uri = dataset::table_uri(home::memory_dir());
    let ds = dataset::open(&uri, Default::default()).await.unwrap();
    let rows = dataset::scan_rows(&ds, &[], None, None).await.unwrap();
    dataset::overwrite(&uri, rows, EmbeddingModel::BgeSmallEn)
        .await
        .unwrap();
    assert_eq!(recorded().await, None);
    index::run_index(&turns, false, None).await.unwrap();
    assert_eq!(recorded().await, counted(3, 1, 2));
}
