//! Sketches of whole sessions, cached under funes's home. An entry is used only when the session's
//! rows, the embedding model, the selector, the schema and the budgets all match; a corrupt or
//! unwritable cache degrades to building the sketch again.

use super::select::SELECTOR_VERSION;
use super::{SessionSketch, SketchOptions, SKETCH_SCHEMA_VERSION};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub(super) fn cache_path(root: &Path, memory: &str, session_id: &str) -> PathBuf {
    let mut hash = Sha256::new();
    hash.update(memory.as_bytes());
    hash.update([0]);
    hash.update(session_id.as_bytes());
    root.join(format!("{}.json", hex::encode(hash.finalize())))
}

pub(super) fn load_cached_sketch(
    path: &Path,
    memory: &str,
    session_id: &str,
    source_fingerprint: &str,
    embedding_fingerprint: &str,
    options: SketchOptions,
) -> Option<SessionSketch> {
    let file = std::fs::File::open(path).ok()?;
    let sketch: SessionSketch = serde_json::from_reader(file).ok()?;
    cache_matches(
        &sketch,
        memory,
        session_id,
        source_fingerprint,
        embedding_fingerprint,
        options,
    )
    .then_some(sketch)
}

fn cache_matches(
    sketch: &SessionSketch,
    memory: &str,
    session_id: &str,
    source_fingerprint: &str,
    embedding_fingerprint: &str,
    options: SketchOptions,
) -> bool {
    sketch.schema_version == SKETCH_SCHEMA_VERSION
        && sketch.selector_version == SELECTOR_VERSION
        && sketch.memory == memory
        && sketch.session_id == session_id
        && sketch.source_fingerprint == source_fingerprint
        && sketch.embedding_fingerprint == embedding_fingerprint
        && sketch.diagnostics.budget == options.budget
        && sketch.diagnostics.char_budget == options.char_budget
}

pub(super) fn store_cached_sketch(path: &Path, sketch: &SessionSketch) -> Result<()> {
    let parent = path.parent().context("session-sketch cache path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating session-sketch cache at {}", parent.display()))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("staging session-sketch cache in {}", parent.display()))?;
    serde_json::to_writer(&mut staged, sketch).context("serializing session-sketch cache")?;
    staged.flush().context("flushing session-sketch cache")?;
    staged
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replacing session-sketch cache at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Diagnostics, DEFAULT_CHARS, DEFAULT_UNITS};
    use super::*;

    fn cached_sketch_fixture() -> SessionSketch {
        SessionSketch {
            schema_version: SKETCH_SCHEMA_VERSION,
            selector_version: SELECTOR_VERSION.into(),
            memory: "local-memory".into(),
            session_id: "session".into(),
            source_fingerprint: "source".into(),
            embedding_fingerprint: "embedding".into(),
            source_chunks: 1,
            eligible_units: 1,
            evidence: Vec::new(),
            diagnostics: Diagnostics {
                axes: 0,
                transitions: 0,
                near_duplicate_groups: 0,
                duplicate_strategy: "exact".into(),
                duplicate_vector_comparisons: 0,
                candidates: 0,
                rendered_characters: 0,
                budget: DEFAULT_UNITS,
                char_budget: DEFAULT_CHARS,
                elapsed_ms: 1,
                fallback: None,
            },
        }
    }

    #[test]
    fn sketch_cache_round_trips_and_rejects_stale_keys() {
        let temp = tempfile::tempdir().unwrap();
        let path = cache_path(temp.path(), "local-memory", "session");
        let sketch = cached_sketch_fixture();
        let options = SketchOptions {
            budget: DEFAULT_UNITS,
            char_budget: DEFAULT_CHARS,
        };
        store_cached_sketch(&path, &sketch).unwrap();

        assert!(load_cached_sketch(&path, "local-memory", "session", "source", "embedding", options,).is_some());
        assert!(load_cached_sketch(
            &path,
            "local-memory",
            "session",
            "rewritten-source",
            "embedding",
            options,
        )
        .is_none());
        assert!(load_cached_sketch(
            &path,
            "local-memory",
            "session",
            "source",
            "embedding",
            SketchOptions {
                budget: options.budget + 1,
                ..options
            },
        )
        .is_none());
    }
}
