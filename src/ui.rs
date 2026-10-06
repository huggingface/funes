//! Terminal presentation for the commands: [`render`] formats the read verbs' results, and
//! [`banner`] plays the wait animation `ask` runs behind a borrowed agent.

use crate::memory::dataset::IndexBuildEvent;

pub mod banner;
pub mod render;

/// Write index-build progress and vector-index failures to stderr.
pub(crate) fn index_progress(event: IndexBuildEvent) {
    match event {
        IndexBuildEvent::Building(phase) => eprintln!("building {phase}…"),
        IndexBuildEvent::MergingFragments(n) => eprintln!("merging {n} fragments…"),
        IndexBuildEvent::Compacting { index, deltas } => {
            eprintln!("compacting {index} ({deltas} delta sub-indexes)…");
        }
        IndexBuildEvent::VectorIndexFailed(error) => {
            eprintln!("note: vector index skipped — {error:#}");
        }
    }
}
