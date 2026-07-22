//! The `funes curate` interactive review over the generic [`crate::tui`] engine: a fuzzy-filterable
//! list of the project's candidate sessions, each carrying a decision glyph, where `→` includes a
//! session and `←` excludes it (the same arrow again clears to pending). The preview shows the
//! session's user prompts. Decisions persist to the memory's curation file as they're made; Enter or
//! Esc ends the review — the caller then summarizes and offers the push. `Tab` switches the preview
//! between the existing user-prompts view and a deterministic session sketch.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::curate::{self, Decision};
use crate::tui::{run_root, Ctx, Flow, PickerModel, RunOpts};

/// One reviewable session: what its row shows (date + opening prompt), the richer text the fuzzy
/// filter matches, the comment a decision records, and both pre-rendered previews.
pub struct Candidate {
    pub id: String,
    pub date: String,
    pub prompt: String,
    pub filter: String,
    pub comment: String,
    /// The session's current local chunk count — recorded with an `include` as its growth
    /// watermark, so a later push can tell whether the session has grown since this review.
    pub chunks: usize,
    /// User prompts with scaffolding dropped. This remains the default review view.
    pub prompts_preview: Text<'static>,
    /// Deterministically selected evidence, or an inline explanation when sketching failed.
    pub sketch_preview: Text<'static>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PreviewMode {
    #[default]
    Prompts,
    Sketch,
}

impl PreviewMode {
    fn toggle(self) -> Self {
        match self {
            Self::Prompts => Self::Sketch,
            Self::Sketch => Self::Prompts,
        }
    }
}

/// Run the arrow review for `project`'s `items` against the memory at `uri`, seeding each row from
/// the curation file's current decision. Writes persist as decisions are made; returns when the
/// user ends the review, propagating the first write error if any.
pub fn run(uri: String, project: String, items: Vec<Candidate>) -> anyhow::Result<()> {
    let existing = curate::load(&uri)?.unwrap_or_default();
    let decision = items
        .iter()
        .map(|c| {
            // A stale include — the session grew since it was reviewed — seeds as pending, so it
            // reads as needing a fresh look rather than a settled ✓.
            if existing.include.contains(&c.id) && !existing.is_stale(&c.id, c.chunks) {
                Some(Decision::Include)
            } else if existing.exclude.contains(&c.id) {
                Some(Decision::Exclude)
            } else {
                None
            }
        })
        .collect();
    let mut picker = CuratePicker {
        uri,
        project,
        items,
        decision,
        preview_mode: PreviewMode::default(),
        err: None,
    };
    run_root(&mut picker, RunOpts::default())?; // Back and Quit both mean "done reviewing"
    match picker.err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct CuratePicker {
    uri: String,
    project: String,
    items: Vec<Candidate>,
    decision: Vec<Option<Decision>>, // parallel to `items`; the in-memory mirror of the file
    preview_mode: PreviewMode,       // global view choice; decisions are independent of it
    err: Option<anyhow::Error>,      // first persist failure, surfaced when the review ends
}

impl CuratePicker {
    /// Set item `i` to `want`, or clear it to pending if it already holds that decision, then
    /// persist the new state to the curation file.
    fn toggle(&mut self, i: usize, want: Decision) {
        let next = match (self.decision[i], want) {
            (Some(Decision::Include), Decision::Include) | (Some(Decision::Exclude), Decision::Exclude) => None,
            _ => Some(want),
        };
        self.decision[i] = next;
        if self.err.is_none() {
            let c = &self.items[i];
            if let Err(e) = curate::set_decision(&self.uri, &c.id, next, c.chunks, &c.comment) {
                self.err = Some(e);
            }
        }
    }
}

impl PickerModel for CuratePicker {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn filter_key(&self, i: usize) -> &str {
        &self.items[i].filter
    }

    fn row(&self, i: usize) -> Line<'static> {
        let c = &self.items[i];
        let rest = format!(" {}  {}", c.date, c.prompt);
        match self.decision[i] {
            Some(Decision::Include) => Line::from(vec![
                Span::styled("✓", Style::default().fg(Color::Green)),
                Span::raw(rest),
            ]),
            Some(Decision::Exclude) => {
                Line::from(format!("✗{rest}")).style(Style::default().add_modifier(Modifier::DIM))
            }
            None => Line::from(format!("·{rest}")),
        }
    }

    fn preview(&self, i: usize) -> Text<'static> {
        let preview = match self.preview_mode {
            PreviewMode::Prompts => self.items[i].prompts_preview.clone(),
            PreviewMode::Sketch => self.items[i].sketch_preview.clone(),
        };
        if matches!(self.decision[i], Some(Decision::Exclude)) {
            preview.style(Style::default().add_modifier(Modifier::DIM))
        } else {
            preview
        }
    }

    fn header(&self, _query: &str) -> Line<'static> {
        // The prominent line: how to act. `→`/`←` aren't discoverable, so they lead here rather
        // than hide on the dim prompt line; `→ include` wears the green of the ✓ it sets.
        Line::from(vec![
            Span::styled("→ include", Style::default().fg(Color::Green)),
            Span::raw("    ← exclude"),
            Span::styled(
                match self.preview_mode {
                    PreviewMode::Prompts => "    tab sketch",
                    PreviewMode::Sketch => "    tab prompts",
                },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                "    · same arrow again clears · enter/esc when done",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])
    }

    fn hints(&self) -> String {
        // Which memory this is — context, on the dim prompt line beside the match counter.
        format!("ctrl-u/d scroll · project memory of {}", self.project)
    }

    fn on_key(&mut self, key: KeyEvent, sel: Option<usize>, _ctx: &mut Ctx) -> Flow {
        match key.code {
            KeyCode::Right => {
                if let Some(i) = sel {
                    self.toggle(i, Decision::Include);
                }
                Flow::Continue
            }
            KeyCode::Left => {
                if let Some(i) = sel {
                    self.toggle(i, Decision::Exclude);
                }
                Flow::Continue
            }
            KeyCode::Tab => {
                self.preview_mode = self.preview_mode.toggle();
                Flow::ResetPreview
            }
            KeyCode::Enter | KeyCode::Esc => Flow::Back,
            _ => Flow::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn header_shows_how_to_include_and_exclude() {
        let picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: Vec::new(),
            decision: Vec::new(),
            preview_mode: PreviewMode::Prompts,
            err: None,
        };
        let header = line_text(&picker.header(""));
        assert!(
            header.contains('→') && header.contains("include"),
            "no include hint: {header}"
        );
        assert!(
            header.contains('←') && header.contains("exclude"),
            "no exclude hint: {header}"
        );
        assert!(header.contains("tab sketch"), "no sketch hint: {header}");
    }

    #[test]
    fn preview_mode_toggles_without_changing_decisions() {
        let mut picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: vec![Candidate {
                id: "session".into(),
                date: "2026-07-22".into(),
                prompt: "prompt".into(),
                filter: "prompt".into(),
                comment: "comment".into(),
                chunks: 1,
                prompts_preview: Text::raw("PROMPTS"),
                sketch_preview: Text::raw("SKETCH"),
            }],
            decision: vec![Some(Decision::Include)],
            preview_mode: PreviewMode::Prompts,
            err: None,
        };

        assert_eq!(picker.preview(0).lines[0].spans[0].content, "PROMPTS");
        picker.preview_mode = picker.preview_mode.toggle();
        assert_eq!(picker.preview(0).lines[0].spans[0].content, "SKETCH");
        assert!(matches!(picker.decision.as_slice(), [Some(Decision::Include)]));
        assert!(line_text(&picker.header("")).contains("tab prompts"));
    }
}
