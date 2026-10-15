//! The placement rules of CONTRIBUTING.md that a build does not check.

const MAIN: &str = include_str!("../src/main.rs");

/// `main.rs` is the CLI's definitions and dispatch: it defines no free function but `main` and
/// `run`. A flow, a decision or a prompt belongs to a command or to `ui`.
#[test]
fn main_rs_defines_only_main_and_run() {
    let production = MAIN.split("#[cfg(test)]").next().unwrap();
    let free_fns: Vec<&str> = production
        .lines()
        .filter_map(|line| line.strip_prefix("fn ").or_else(|| line.strip_prefix("async fn ")))
        .map(|rest| rest.split(['(', '<']).next().unwrap())
        .collect();
    assert_eq!(free_fns, ["main", "run"], "free functions in src/main.rs");
}

/// `memory/dataset.rs` makes every Lance call. A command holds the `Dataset` and `RecordBatch`
/// values it hands out and hands them back; it scans, reads a column, quotes a filter value,
/// writes rows or reads a version through `dataset`, never itself.
#[test]
fn commands_leave_lance_to_dataset() {
    const FORBIDDEN: &[&str] = &[
        "use arrow_",
        "use lance_",
        "use lance::dataset::",
        "use futures::TryStreamExt",
        ".scan()",
        "as_any()",
        "try_into_stream(",
        "with_row_id(",
        ".take_rows(",
        ".count_rows(",
        ".cleanup_old_versions(",
        "Dataset::write(",
        "RecordBatch::try_new(",
        "filter_record_batch(",
        ".nearest(",
        "full_text_search(",
        ".version()",
        ".schema()",
        "replace('\\'', \"''\")",
    ];
    let mut offences = Vec::new();
    for file in command_sources() {
        let text = std::fs::read_to_string(&file).unwrap();
        let production = text.split("#[cfg(test)]").next().unwrap();
        for (n, line) in production.lines().enumerate() {
            let code = line.split("//").next().unwrap();
            for token in FORBIDDEN {
                if code.contains(token) {
                    offences.push(format!("{}:{}: {token}", file.display(), n + 1));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "Lance or arrow named outside memory/dataset.rs:\n{}",
        offences.join("\n")
    );
}

/// The sources the Lance rule covers: every command, and the sketch selector.
fn command_sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(root.join("commands"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.push(root.join("session_sketch.rs"));
    files.sort();
    files
}
