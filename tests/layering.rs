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
