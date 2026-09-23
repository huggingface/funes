//! What indexing a turns file must hold to: a grown file adds only its new turns, and a thread no
//! agent wrote — a tracker's roles, no `assistant`, no `thinking`, no `cwd`, an edit as a new turn —
//! indexes and lists. Each integration round-trips its own converter against its own fixture, in
//! its bundle, which is where a native transcript is compared with the turns file it becomes.
//! Own test binary, one test at a time: `$FUNES_HOME` is process-global.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use arrow_array::{Array, ArrayRef, Int64Array, StringArray};
use funes::commands::index::run_index;
use funes::commands::recall::{self, SessionFilter};
use funes::memory::{dataset, Memory};
use funes::traces::{funes_jsonl, Turn};
use tokio::sync::Mutex;

static HOME: Mutex<()> = Mutex::const_new(());

/// Every stored column but `source_path` and the vector.
const COLS: [&str; 15] = [
    "id",
    "text",
    "session_id",
    "workdir",
    "turn_uuid",
    "parent_uuid",
    "seq",
    "ts",
    "role",
    "block_type",
    "tool_name",
    "block_idx",
    "split_idx",
    "harness",
    "repo",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// A fixture's turns, read as the turns-file source reads them.
fn turns_of(name: &str) -> Vec<Turn> {
    funes_jsonl::read_turns(&fixture(name)).unwrap()
}

fn write_turns(path: &Path, turns: &[Turn]) {
    let mut f = std::fs::File::create(path).unwrap();
    for t in turns {
        writeln!(f, "{}", serde_json::to_string(t).unwrap()).unwrap();
    }
}

fn append_turns(path: &Path, turns: &[Turn]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for t in turns {
        writeln!(f, "{}", serde_json::to_string(t).unwrap()).unwrap();
    }
}

fn cell(col: &ArrayRef, i: usize) -> String {
    if col.is_null(i) {
        return "∅".into();
    }
    if let Some(a) = col.as_any().downcast_ref::<StringArray>() {
        return a.value(i).to_string();
    }
    if let Some(a) = col.as_any().downcast_ref::<Int64Array>() {
        return a.value(i).to_string();
    }
    panic!("unexpected column type {:?}", col.data_type())
}

/// The local memory's rows over [`COLS`], one tab-joined line each, sorted.
async fn rows() -> Vec<String> {
    let ds = dataset::open(&dataset::table_uri(&dataset::local_memory_dir()), Default::default())
        .await
        .expect("the memory exists");
    let mut out = Vec::new();
    for batch in dataset::scan_rows(&ds, &COLS, None, None).await.unwrap() {
        for i in 0..batch.num_rows() {
            let row: Vec<String> = (0..batch.num_columns()).map(|c| cell(batch.column(c), i)).collect();
            out.push(row.join("\t"));
        }
    }
    out.sort();
    out
}

fn column<'a>(rows: &'a [String], name: &str) -> BTreeSet<&'a str> {
    let at = COLS.iter().position(|c| *c == name).unwrap();
    rows.iter().map(|r| r.split('\t').nth(at).unwrap()).collect()
}

/// Index `path` into a fresh home and return the rows it wrote.
async fn index_fresh(path: &Path) -> (tempfile::TempDir, Vec<String>) {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("FUNES_HOME", home.path());
    run_index(path, false, None).await.unwrap();
    let rows = rows().await;
    (home, rows)
}

#[tokio::test]
async fn a_grown_turns_file_adds_only_its_new_turns() {
    let _one_at_a_time = HOME.lock().await;
    let out = tempfile::tempdir().unwrap();
    let turns = turns_of("funes_jsonl/valid.funes.jsonl");
    let (head, tail) = turns.split_at(turns.len() / 2);
    assert!(!head.is_empty() && !tail.is_empty());

    let whole = out.path().join("whole.funes.jsonl");
    write_turns(&whole, &turns);
    let (_whole_home, from_whole) = index_fresh(&whole).await;

    let grown = out.path().join("grown.funes.jsonl");
    write_turns(&grown, head);
    let (home, first) = index_fresh(&grown).await;
    assert!(first.len() < from_whole.len());
    append_turns(&grown, tail);
    std::env::set_var("FUNES_HOME", home.path());
    run_index(&grown, false, None).await.unwrap();
    let second = rows().await;
    assert_eq!(
        second, from_whole,
        "re-indexing the grown file must converge on the one-shot index"
    );
    assert!(column(&first, "id").is_subset(&column(&second, "id")));
}

#[tokio::test]
async fn a_tracker_thread_indexes_and_lists_without_agent_roles() {
    let _one_at_a_time = HOME.lock().await;
    let (_home, rows) = index_fresh(&fixture("funes_jsonl/github_issue.funes.jsonl")).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(column(&rows, "role"), BTreeSet::from(["contributor", "member"]));
    assert_eq!(column(&rows, "block_type"), BTreeSet::from(["text"]));
    assert_eq!(column(&rows, "harness"), BTreeSet::from(["github"]));
    assert_eq!(
        column(&rows, "workdir"),
        BTreeSet::from([""]),
        "no cwd, no workdir facet"
    );
    assert_eq!(column(&rows, "repo"), BTreeSet::from([""]), "no cwd, no repo facet");
    // The edit is its own turn: three turns, three ids.
    assert_eq!(column(&rows, "turn_uuid").len(), 3);

    let filter = SessionFilter {
        repo: None,
        since: None,
        until: None,
        limit: None,
        offset: 0,
    };
    let listing = recall::sessions(Memory::local(), filter).await.unwrap();
    assert!(listing.contains("gh/huggingface/transformers#31234"), "{listing}");
    assert!(
        listing.contains("**@someone** opened: Static cache"),
        "the thread opens on its contributor's text:\n{listing}"
    );
}
