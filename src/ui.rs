//! The terminal side of the commands: [`render`] formats the read verbs' results, [`banner`]
//! plays the wait display while a command works, and [`prompt`] puts a command's questions to
//! the user.

use crate::memory::dataset::IndexBuildEvent;

pub mod banner;
pub mod prompt;
pub mod render;

/// Write index-build progress and vector-index failures to stderr.
pub(crate) fn index_progress(event: IndexBuildEvent) {
    match event {
        IndexBuildEvent::Building(phase) => eprintln!("building {phase}…"),
        IndexBuildEvent::MergingFragments(n) => eprintln!("merging {n} fragments…"),
        IndexBuildEvent::MergingDeltas { index, deltas } => {
            eprintln!("merging {deltas} delta sub-indexes of {index}…");
        }
        IndexBuildEvent::VectorIndexFailed(error) => {
            eprintln!("note: vector index skipped — {error:#}");
        }
    }
}
