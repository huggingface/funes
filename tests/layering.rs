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

/// `home.rs` is the one reader of `$FUNES_HOME`: every path under the home comes from it.
#[test]
fn only_home_reads_funes_home() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offences = Vec::new();
    for entry in walk(&root) {
        if entry.ends_with("home.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&entry).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.contains("var(\"FUNES_HOME\")") || line.contains("var_os(\"FUNES_HOME\")") {
                offences.push(format!("{}:{}", entry.display(), n + 1));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "$FUNES_HOME read outside home.rs:\n{}",
        offences.join("\n")
    );
}

/// Every `.rs` file under `dir`.
fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

/// The sources the Lance rule covers: every command.
fn command_sources() -> Vec<std::path::PathBuf> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = walk(&root.join("commands"));
    files.sort();
    files
}
