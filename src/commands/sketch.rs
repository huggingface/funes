//! `funes sketch`: what a session contains.
//!
//! Selects the passages most distinctive within one session, from the vectors already stored,
//! always keeping the opening ask and the last word of the range digested.

use crate::memory::Memory;
use crate::session_sketch::{self, SketchOptions};
use anyhow::{bail, Result};

/// Places a sketch shows when the caller doesn't say.
const DEFAULT_UNITS: usize = 8;

/// Characters a sketch renders when the caller doesn't say — cheap by default, since a caller can
/// always ask for more and an over-fetch is paid for by whoever reads the reply.
const DEFAULT_CHARS: usize = 8_000;

/// Digest one session, rendered in the agent format. `units` and `max_chars` are requests: both are
/// clamped to what a reply can carry, and any narrowing is reported rather than applied quietly.
pub async fn run(
    memory: Memory,
    session_id: String,
    from: Option<i64>,
    to: Option<i64>,
    units: Option<usize>,
    max_chars: Option<usize>,
) -> Result<String> {
    let asked = SketchOptions {
        budget: units.unwrap_or(DEFAULT_UNITS),
        char_budget: max_chars.unwrap_or(DEFAULT_CHARS),
    };
    let (options, clamped) = asked.clamp();
    // A whole-session digest is deterministic in the session's content, so it is worth caching: a
    // second pass over the same candidate costs nothing. A window is not cached — a cache entry
    // keyed on the session would otherwise answer for a stretch of it.
    let sketch = if from.is_none() && to.is_none() {
        let mut batch =
            session_sketch::generate_many_cached(&memory, std::slice::from_ref(&session_id), options).await?;
        let failure = batch.failures.remove(&session_id);
        match batch.sketches.remove(&session_id) {
            Some(sketch) => sketch,
            None => bail!(
                "{}",
                failure.unwrap_or_else(|| format!("no session {session_id} in {}", memory.label()))
            ),
        }
    } else {
        session_sketch::generate(&memory, &session_id, from, to, options).await?
    };
    if sketch.evidence.is_empty() {
        bail!("nothing to sketch in session {session_id} — it holds no text worth selecting");
    }
    Ok(crate::ui::render::sketch_agent(
        "",
        &super::recall::memory_hint(Some(&memory.label())),
        &sketch,
        clamped,
    ))
}
