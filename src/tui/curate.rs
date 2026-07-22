//! The `funes curate` interactive review over the generic [`crate::tui`] engine: a fuzzy-filterable
//! list of the project's local sessions, each carrying a decision glyph, where `→` includes a
//! reviewable session and `←` excludes it (the same arrow again clears to pending). Fully published
//! sessions remain browseable but immutable. The preview opens on the deterministic session sketch,
//! with the prompt history one `Tab` away as a fallback and the active local criteria under `c`.
//! Decisions persist to the memory's curation file as they're made; Enter or Esc ends the review —
//! the caller then summarizes and offers the push. `Shift-Tab` switches between all local sessions
//! and only those still requiring a decision.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::curate::{self, Decision, Publication};
use crate::tui::{run_root, Ctx, Flow, PickerModel, RunOpts};

/// One local session: what its row shows (date + opening prompt), the sketch-and-prompts text the
/// fuzzy filter matches, the comment a decision records, and both pre-rendered previews.
pub struct Candidate {
    pub id: String,
    pub date: String,
    pub prompt: String,
    pub filter: String,
    pub comment: String,
    /// The session's current local chunk count — recorded with an `include` as its growth
    /// watermark, so a later push can tell whether the session has grown since this review.
    pub chunks: usize,
    /// Publication state relative to the target memory. Fully published sessions remain visible
    /// for browsing, but their decisions cannot change because an exclude cannot retract them.
    pub publication: Publication,
    /// User prompts with scaffolding dropped. This is the fallback review view.
    pub prompts_preview: Text<'static>,
    /// Deterministically selected evidence, or an inline explanation when sketching failed.
    pub sketch_preview: Text<'static>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PreviewMode {
    Prompts,
    #[default]
    Sketch,
    Criteria,
}

impl PreviewMode {
    fn toggle(self) -> Self {
        match self {
            Self::Prompts => Self::Sketch,
            Self::Sketch => Self::Prompts,
            Self::Criteria => Self::Sketch,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SessionScope {
    #[default]
    All,
    Pending,
}

impl SessionScope {
    fn toggle(self) -> Self {
        match self {
            Self::All => Self::Pending,
            Self::Pending => Self::All,
        }
    }
}

/// Run the arrow review for `project`'s `items` against the memory at `uri`, seeding each row from
/// the curation file's current decision. Writes persist as decisions are made; returns when the
/// user ends the review, propagating the first write error if any.
pub fn run(uri: String, project: String, items: Vec<Candidate>) -> anyhow::Result<()> {
    let existing = curate::load(&uri)?.unwrap_or_default();
    let criteria = curate::load_criteria(&uri)?;
    let criteria_fingerprint = criteria.as_ref().map(|snapshot| snapshot.fingerprint.clone());
    let criteria_label = criteria
        .as_ref()
        .map(|snapshot| format!("criteria {} {}", snapshot.name, snapshot.short_fingerprint()));
    let criteria_preview = criteria.as_ref().map(criteria_text);
    let decision = items
        .iter()
        .map(|c| {
            // A stale include — the session grew since it was reviewed — seeds as pending, so it
            // reads as needing a fresh look rather than a settled ✓.
            if existing.include.contains(&c.id) && (c.publication.is_read_only() || !existing.is_stale(&c.id, c.chunks))
            {
                Some(Decision::Include)
            } else if existing.exclude.contains(&c.id) && !existing.criteria_is_stale(&c.id) {
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
        scope: SessionScope::default(),
        criteria_fingerprint,
        criteria_label,
        criteria_preview,
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
    scope: SessionScope,             // all local sessions, or only undecided reviewable sessions
    criteria_fingerprint: Option<String>, // criteria new decisions are bound to
    criteria_label: Option<String>,  // compact name + fingerprint, always visible in the footer
    criteria_preview: Option<Text<'static>>, // full local Markdown snapshot, opened with `c`
    err: Option<anyhow::Error>,      // first persist failure, surfaced when the review ends
}

fn criteria_text(snapshot: &curate::CriteriaSnapshot) -> Text<'static> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("CURATION CRITERIA", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "  · {} · {} · local only",
                snapshot.name,
                snapshot.short_fingerprint()
            )),
        ]),
        Line::raw(""),
    ];
    for source in snapshot.markdown.lines() {
        let trimmed = source.trim_start();
        if trimmed.starts_with('#') {
            lines.push(Line::styled(
                source.to_string(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Color::Yellow)),
                Span::raw(item.to_string()),
            ]));
        } else {
            lines.push(Line::raw(source.to_string()));
        }
    }
    Text::from(lines)
}

impl CuratePicker {
    /// Set item `i` to `want`, or clear it to pending if it already holds that decision, then
    /// persist the new state to the curation file.
    fn toggle(&mut self, i: usize, want: Decision) {
        if self.items[i].publication.is_read_only() {
            return;
        }
        let next = match (self.decision[i], want) {
            (Some(Decision::Include), Decision::Include) | (Some(Decision::Exclude), Decision::Exclude) => None,
            _ => Some(want),
        };
        self.decision[i] = next;
        if self.err.is_none() {
            let c = &self.items[i];
            if let Err(e) = curate::set_decision(
                &self.uri,
                &c.id,
                next,
                c.chunks,
                self.criteria_fingerprint.as_deref(),
                &c.comment,
            ) {
                self.err = Some(e);
            }
        }
    }
}

impl PickerModel for CuratePicker {
    fn len(&self) -> usize {
        self.items.len()
    }

    fn visible(&self, i: usize) -> bool {
        match self.scope {
            SessionScope::All => true,
            SessionScope::Pending => self.decision[i].is_none() && !self.items[i].publication.is_read_only(),
        }
    }

    fn filter_key(&self, i: usize) -> &str {
        &self.items[i].filter
    }

    fn row(&self, i: usize) -> Line<'static> {
        let c = &self.items[i];
        let mut spans = match self.decision[i] {
            Some(Decision::Include) => vec![Span::styled("✓", Style::default().fg(Color::Green))],
            Some(Decision::Exclude) => vec![Span::styled("✗", Style::default().add_modifier(Modifier::DIM))],
            None => vec![Span::raw("·")],
        };
        spans.push(match c.publication {
            Publication::Local => Span::raw("   "),
            Publication::Partial => Span::styled(" ◐ ", Style::default().fg(Color::Yellow)),
            Publication::Published => Span::styled(" ↑ ", Style::default().fg(Color::Cyan)),
        });
        spans.push(Span::raw(format!("{}  {}", c.date, c.prompt)));
        let line = Line::from(spans);
        if matches!(self.decision[i], Some(Decision::Exclude)) {
            line.style(Style::default().add_modifier(Modifier::DIM))
        } else {
            line
        }
    }

    fn preview(&self, i: usize) -> Text<'static> {
        let preview = match self.preview_mode {
            PreviewMode::Prompts => self.items[i].prompts_preview.clone(),
            PreviewMode::Sketch => self.items[i].sketch_preview.clone(),
            PreviewMode::Criteria => self
                .criteria_preview
                .clone()
                .unwrap_or_else(|| Text::raw("No criteria snapshot. Run `funes curate <memory> --criteria <file>`.")),
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
        let mut spans = vec![
            Span::styled("→ include", Style::default().fg(Color::Green)),
            Span::raw("    ← exclude"),
            Span::styled(
                match self.preview_mode {
                    PreviewMode::Prompts => "    tab sketch",
                    PreviewMode::Sketch | PreviewMode::Criteria => "    tab prompts",
                },
                Style::default().fg(Color::Cyan),
            ),
        ];
        if self.criteria_preview.is_some() {
            spans.push(Span::styled(
                if self.preview_mode == PreviewMode::Criteria {
                    "    c sketch"
                } else {
                    "    c criteria"
                },
                Style::default().fg(Color::Cyan),
            ));
        }
        spans.extend([
            Span::styled(
                match self.scope {
                    SessionScope::All => "    shift-tab pending",
                    SessionScope::Pending => "    shift-tab all",
                },
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                "    · ↑ published/read-only · ◐ has local updates · enter/esc when done",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]);
        Line::from(spans)
    }

    fn hints(&self) -> String {
        // Which memory this is — context, on the dim prompt line beside the match counter.
        let scope = match self.scope {
            SessionScope::All => "all local",
            SessionScope::Pending => "pending",
        };
        let criteria = self.criteria_label.as_deref().unwrap_or("no criteria");
        format!(
            "{scope} · {criteria} · ctrl-u/d scroll · project memory of {}",
            self.project
        )
    }

    fn on_key(&mut self, key: KeyEvent, sel: Option<usize>, _ctx: &mut Ctx) -> Flow {
        match key.code {
            KeyCode::Right => {
                if let Some(i) = sel {
                    self.toggle(i, Decision::Include);
                }
                if self.scope == SessionScope::Pending {
                    Flow::Refilter
                } else {
                    Flow::Continue
                }
            }
            KeyCode::Left => {
                if let Some(i) = sel {
                    self.toggle(i, Decision::Exclude);
                }
                if self.scope == SessionScope::Pending {
                    Flow::Refilter
                } else {
                    Flow::Continue
                }
            }
            KeyCode::Tab => {
                self.preview_mode = self.preview_mode.toggle();
                Flow::ResetPreview
            }
            KeyCode::Char('c') if self.criteria_preview.is_some() => {
                self.preview_mode = if self.preview_mode == PreviewMode::Criteria {
                    PreviewMode::Sketch
                } else {
                    PreviewMode::Criteria
                };
                Flow::ResetPreview
            }
            KeyCode::BackTab => {
                self.scope = self.scope.toggle();
                Flow::Refilter
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

    fn candidate(id: &str, publication: Publication) -> Candidate {
        Candidate {
            id: id.into(),
            date: "2026-07-22".into(),
            prompt: "prompt".into(),
            filter: "prompt".into(),
            comment: "comment".into(),
            chunks: 1,
            publication,
            prompts_preview: Text::raw("PROMPTS"),
            sketch_preview: Text::raw("SKETCH"),
        }
    }

    #[test]
    fn header_shows_how_to_include_and_exclude() {
        let picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: Vec::new(),
            decision: Vec::new(),
            preview_mode: PreviewMode::Prompts,
            scope: SessionScope::All,
            criteria_fingerprint: None,
            criteria_label: None,
            criteria_preview: None,
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
        assert!(header.contains("shift-tab pending"), "no scope hint: {header}");
    }

    #[test]
    fn preview_mode_toggles_without_changing_decisions() {
        let mut picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: vec![candidate("session", Publication::Local)],
            decision: vec![Some(Decision::Include)],
            preview_mode: PreviewMode::Prompts,
            scope: SessionScope::All,
            criteria_fingerprint: None,
            criteria_label: None,
            criteria_preview: None,
            err: None,
        };

        assert_eq!(picker.preview(0).lines[0].spans[0].content, "PROMPTS");
        picker.preview_mode = picker.preview_mode.toggle();
        assert_eq!(picker.preview(0).lines[0].spans[0].content, "SKETCH");
        assert!(matches!(picker.decision.as_slice(), [Some(Decision::Include)]));
        assert!(line_text(&picker.header("")).contains("tab prompts"));
    }

    #[test]
    fn criteria_view_shows_the_full_named_snapshot_and_fingerprint() {
        let snapshot = curate::CriteriaSnapshot {
            schema_version: 1,
            name: "transformers-memory.md".into(),
            fingerprint: "sha256:1234567890abcdef".into(),
            markdown: "# Purpose\n\n- Explain durable decisions\n\nNever publish private plans.\n".into(),
        };
        let preview = criteria_text(&snapshot);
        let mut picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: vec![candidate("session", Publication::Local)],
            decision: vec![None],
            preview_mode: PreviewMode::Criteria,
            scope: SessionScope::All,
            criteria_fingerprint: Some(snapshot.fingerprint.clone()),
            criteria_label: Some("criteria transformers-memory.md 12345678".into()),
            criteria_preview: Some(preview),
            err: None,
        };

        let rendered: String = picker
            .preview(0)
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("CURATION CRITERIA  · transformers-memory.md · 12345678 · local only"));
        assert!(rendered.contains("# Purpose"));
        assert!(rendered.contains("• Explain durable decisions"));
        assert!(rendered.contains("Never publish private plans."));
        assert!(line_text(&picker.header("")).contains("c sketch"));
        assert!(picker.hints().contains("criteria transformers-memory.md 12345678"));

        picker.preview_mode = PreviewMode::Sketch;
        assert!(line_text(&picker.header("")).contains("c criteria"));
    }

    #[test]
    fn sketch_is_the_default_preview() {
        assert_eq!(PreviewMode::default(), PreviewMode::Sketch);
    }

    #[test]
    fn a_published_session_is_visible_but_immutable() {
        let mut picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: vec![candidate("published", Publication::Published)],
            decision: vec![Some(Decision::Include)],
            preview_mode: PreviewMode::Prompts,
            scope: SessionScope::All,
            criteria_fingerprint: None,
            criteria_label: None,
            criteria_preview: None,
            err: None,
        };

        assert!(line_text(&picker.row(0)).contains('↑'));
        picker.toggle(0, Decision::Exclude);
        assert!(matches!(picker.decision.as_slice(), [Some(Decision::Include)]));
    }

    #[test]
    fn pending_scope_shows_only_undecided_reviewable_sessions() {
        let picker = CuratePicker {
            uri: "hf://datasets/o/r".into(),
            project: "o/r".into(),
            items: vec![
                candidate("pending", Publication::Local),
                candidate("decided", Publication::Local),
                candidate("published", Publication::Published),
            ],
            decision: vec![None, Some(Decision::Exclude), None],
            preview_mode: PreviewMode::Prompts,
            scope: SessionScope::Pending,
            criteria_fingerprint: None,
            criteria_label: None,
            criteria_preview: None,
            err: None,
        };

        assert!(picker.visible(0));
        assert!(!picker.visible(1), "a settled decision does not require curation");
        assert!(!picker.visible(2), "a fully published row is browse-only");
        assert!(line_text(&picker.header("")).contains("shift-tab all"));
        assert!(picker.hints().starts_with("pending"));
    }
}
