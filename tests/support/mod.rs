// Shared by the test binaries that drive `funes`; each uses the subset it needs.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Output;

/// The model cache the embedder and reranker read, for a run whose `$HOME` is fake.
pub fn hf_home() -> PathBuf {
    std::env::var_os("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("a home")).join(".cache/huggingface"))
}

pub fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| e.unwrap().into_path())
        .filter(|p| p.is_file())
        .filter(|p| {
            std::fs::read(p)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
        })
        .collect()
}
