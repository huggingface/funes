//! The `funes curate` interactive review over the generic [`crate::tui`] engine: a fuzzy-filterable
//! list of the project's local sessions, each carrying a decision glyph, where `→` includes a
//! reviewable session and `←` excludes it (the same arrow again clears to pending). Fully published
//! sessions remain browseable but immutable. The preview opens on the deterministic session sketch,
//! with the prompt history one `Tab` away as a fallback. Decisions persist to the memory's curation
//! file as they're made; Enter or Esc ends the review — the caller then summarizes and offers the
//! push. `Shift-Tab` switches between all local sessions and only those still requiring a decision.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::curate::{self, Decision, Publication};
use crate::curation_assist::{AssessmentArtifact, AssistRequest, CriterionSnapshot, Recommendation};
use crate::session_sketch::{self, SessionSketch};
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
    /// Structured sketch retained so cited turns can be emphasized after a background result.
    pub sketch: Option<SessionSketch>,
    /// A provider-approved request for this row. None keeps assistance read-only/disabled.
    pub assist_request: Option<AssistRequest>,
    /// Fresh cached output or the current state of an on-demand generation.
    pub assistance: Assistance,
}

#[derive(Clone, Debug, Default)]
pub enum Assistance {
    #[default]
    Unavailable,
    Pending,
    Running,
    Ready(Box<AssessmentArtifact>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PreviewMode {
    Prompts,
    #[default]
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
pub fn run(
    uri: String,
    project: String,
    criterion: Option<CriterionSnapshot>,
    items: Vec<Candidate>,
) -> anyhow::Result<()> {
    let existing = curate::load(&uri)?.unwrap_or_default();
    let decision = items
        .iter()
        .map(|c| {
            // A stale include — the session grew since it was reviewed — seeds as pending, so it
            // reads as needing a fresh look rather than a settled ✓.
            if existing.include.contains(&c.id) && (c.publication.is_read_only() || !existing.is_stale(&c.id, c.chunks))
            {
                Some(Decision::Include)
            } else if existing.exclude.contains(&c.id) {
                Some(Decision::Exclude)
            } else {
                None
            }
        })
        .collect();
    let mut picker = CuratePicker::new(uri, project, criterion, items, decision);
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
    criterion: Option<CriterionSnapshot>, // fixed before the picker opens; never edited in-place
    assist_tx: Sender<AssistMessage>,
    assist_rx: Receiver<AssistMessage>,
    active_assessments: usize,
    notice: Option<String>,
    err: Option<anyhow::Error>, // first persist failure, surfaced when the review ends
}

type AssistMessage = (usize, Result<AssessmentArtifact, String>);

impl CuratePicker {
    fn new(
        uri: String,
        project: String,
        criterion: Option<CriterionSnapshot>,
        items: Vec<Candidate>,
        decision: Vec<Option<Decision>>,
    ) -> Self {
        let (assist_tx, assist_rx) = mpsc::channel();
        Self {
            uri,
            project,
            items,
            decision,
            preview_mode: PreviewMode::default(),
            scope: SessionScope::default(),
            criterion,
            assist_tx,
            assist_rx,
            active_assessments: 0,
            notice: None,
            err: None,
        }
    }

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
            if let Err(e) = curate::set_decision(&self.uri, &c.id, next, c.chunks, &c.comment) {
                self.err = Some(e);
            }
        }
    }

    fn start_assessment(&mut self, i: usize) {
        if self.active_assessments > 0 {
            self.notice = Some("one assessment is already running; keep browsing while it finishes".into());
            return;
        }
        let Some(request) = self.items[i].assist_request.clone() else {
            self.notice = Some("assistance is disabled; reopen with --assist claude".into());
            return;
        };
        self.items[i].assistance = Assistance::Running;
        self.active_assessments += 1;
        self.notice = Some(format!(
            "assessing {} with {}/{} in the background…",
            &self.items[i].id[..self.items[i].id.len().min(8)],
            request.runner.name,
            request.runner.model
        ));
        let tx = self.assist_tx.clone();
        std::thread::spawn(move || {
            let result = request.run().map_err(|error| format!("{error:#}"));
            let _ = tx.send((i, result));
        });
    }

    fn guidance(&self, i: usize) -> Text<'static> {
        let mut lines = Vec::new();
        let Some(criterion) = &self.criterion else {
            return Text::default();
        };
        let effect = match criterion.effect {
            crate::curation_assist::CriterionEffect::Inclusion => "INCLUDE WHEN MATCHED",
            crate::curation_assist::CriterionEffect::Exclusion => "EXCLUDE WHEN MATCHED",
        };
        lines.push(Line::from(vec![
            Span::styled("CRITERION ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(effect, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(format!(
                "  · {} · {} · {}",
                criterion.id,
                criterion.name,
                criterion.short_fingerprint()
            )),
        ]));
        lines.extend(criterion.text.lines().map(|line| Line::raw(line.to_string())));
        lines.push(Line::raw(""));
        match &self.items[i].assistance {
            Assistance::Unavailable => lines.push(Line::styled(
                "No fresh assessment. Reopen with --assist claude to enable F2.",
                Style::default().add_modifier(Modifier::DIM),
            )),
            Assistance::Pending => lines.push(Line::styled(
                "ASSESSMENT READY TO RUN  · press F2",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Assistance::Running => lines.push(Line::styled(
                "ASSESSMENT RUNNING  · keep browsing; this preview will update",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Assistance::Failed(error) => {
                lines.push(Line::styled(
                    "ASSESSMENT FAILED",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::raw(error.clone()));
            }
            Assistance::Ready(artifact) if artifact.validation.status != "accepted" => {
                lines.push(Line::styled(
                    "ASSESSMENT REJECTED BY LOCAL VALIDATION",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::raw(
                    artifact
                        .validation
                        .error
                        .clone()
                        .unwrap_or_else(|| "unknown validation failure".into()),
                ));
            }
            Assistance::Ready(artifact) => {
                let Some(assessment) = artifact.assessment.as_ref() else {
                    lines.push(Line::styled(
                        "ASSESSMENT REJECTED BY LOCAL VALIDATION",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                    lines.push(Line::raw("accepted cache entry has no assessment"));
                    lines.push(Line::raw(""));
                    return Text::from(lines);
                };
                let color = match assessment.recommendation {
                    Recommendation::IncludeCandidate => Color::Green,
                    Recommendation::ExcludeCandidate => Color::Red,
                    Recommendation::NeedsFullReview => Color::Yellow,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        assessment.recommendation.to_string().to_uppercase(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  · {} match · {}/{}",
                        assessment.criterion_match, artifact.runner.name, artifact.runner.requested_model
                    )),
                ]));
                lines.push(Line::raw(assessment.rationale.clone()));
                let mut run = vec![format!(
                    "input: {} sketch block(s) from {} indexed chunk(s)",
                    artifact.sketch.evidence.len(),
                    artifact.sketch.source_chunks
                )];
                if artifact.runner.wall_seconds > 0.0 {
                    run.push(format!("{:.1}s", artifact.runner.wall_seconds));
                }
                if let Some(cost) = artifact.runner.total_cost_usd {
                    run.push(format!("${cost:.3}"));
                }
                lines.push(Line::styled(
                    run.join(" · "),
                    Style::default().add_modifier(Modifier::DIM),
                ));
                if !assessment.supports.is_empty() {
                    lines.push(Line::styled(
                        "Observed supporting evidence:",
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    for claim in &assessment.supports {
                        lines.push(Line::raw(format!("• {}", claim.claim)));
                    }
                }
                if !assessment.against.is_empty() {
                    lines.push(Line::styled(
                        "Observed evidence against a match:",
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    for claim in &assessment.against {
                        lines.push(Line::raw(format!("• {}", claim.claim)));
                    }
                }
                if !assessment.uncertainties.is_empty() {
                    lines.push(Line::styled(
                        "Uncertainties:",
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                    for uncertainty in &assessment.uncertainties {
                        lines.push(Line::raw(format!("• {uncertainty}")));
                    }
                }
            }
        }
        lines.push(Line::raw(""));
        Text::from(lines)
    }

    fn cited_turns(&self, i: usize) -> HashSet<String> {
        let Assistance::Ready(artifact) = &self.items[i].assistance else {
            return HashSet::new();
        };
        let Some(assessment) = &artifact.assessment else {
            return HashSet::new();
        };
        assessment
            .supports
            .iter()
            .chain(&assessment.against)
            .flat_map(|claim| claim.evidence.iter().cloned())
            .collect()
    }

    fn preview_body(&self, i: usize) -> Text<'static> {
        match self.preview_mode {
            PreviewMode::Prompts => self.items[i].prompts_preview.clone(),
            PreviewMode::Sketch => self.items[i]
                .sketch
                .as_ref()
                .map(|sketch| {
                    crate::tui::ansi_to_text(&session_sketch::render_preview_with_citations(
                        sketch,
                        &self.cited_turns(i),
                    ))
                })
                .unwrap_or_else(|| self.items[i].sketch_preview.clone()),
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
        let assistance = match &c.assistance {
            Assistance::Ready(artifact) if artifact.validation.status != "accepted" => {
                Some(Span::styled(" × ", Style::default().fg(Color::Red)))
            }
            Assistance::Ready(artifact) => match artifact.assessment.as_ref().map(|a| a.recommendation) {
                Some(Recommendation::IncludeCandidate) => Some(Span::styled(" + ", Style::default().fg(Color::Green))),
                Some(Recommendation::ExcludeCandidate) => Some(Span::styled(" ! ", Style::default().fg(Color::Red))),
                Some(Recommendation::NeedsFullReview) => Some(Span::styled(" ? ", Style::default().fg(Color::Yellow))),
                None => Some(Span::styled(" × ", Style::default().fg(Color::Red))),
            },
            Assistance::Running => Some(Span::styled(" … ", Style::default().fg(Color::Yellow))),
            Assistance::Pending => Some(Span::raw(" ◇ ")),
            Assistance::Failed(_) => Some(Span::styled(" × ", Style::default().fg(Color::Red))),
            Assistance::Unavailable => None,
        };
        if let Some(assistance) = assistance {
            spans.push(assistance);
        }
        let short_id = &c.id[..c.id.len().min(8)];
        spans.push(Span::raw(format!("{}  {}  {}", c.date, short_id, c.prompt)));
        let line = Line::from(spans);
        if matches!(self.decision[i], Some(Decision::Exclude)) {
            line.style(Style::default().add_modifier(Modifier::DIM))
        } else {
            line
        }
    }

    fn preview(&self, i: usize) -> Text<'static> {
        let mut preview = Text::from(vec![
            Line::from(vec![
                Span::styled("SESSION ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(self.items[i].id.clone()),
            ]),
            Line::raw(""),
        ]);
        preview.lines.extend(self.guidance(i).lines);
        preview.lines.extend(self.preview_body(i).lines);
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
                    PreviewMode::Sketch => "    tab prompts",
                },
                Style::default().fg(Color::Cyan),
            ),
        ];
        if self.items.iter().any(|item| item.assist_request.is_some()) {
            spans.push(Span::styled("    F2 assess", Style::default().fg(Color::Cyan)));
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
        let criterion = self
            .criterion
            .as_ref()
            .map(|criterion| format!(" · {}", criterion.label()))
            .unwrap_or_default();
        let notice = self
            .notice
            .as_deref()
            .map(|note| format!(" · {note}"))
            .unwrap_or_default();
        format!(
            "{scope}{criterion} · ctrl-u/d scroll · project memory of {}{notice}",
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
            KeyCode::BackTab => {
                self.scope = self.scope.toggle();
                Flow::Refilter
            }
            KeyCode::F(2) => {
                if let Some(i) = sel {
                    self.start_assessment(i);
                }
                Flow::Continue
            }
            KeyCode::Enter | KeyCode::Esc if self.active_assessments > 0 => {
                self.notice = Some("assessment still running; exit is available when it finishes".into());
                Flow::Continue
            }
            KeyCode::Enter | KeyCode::Esc => Flow::Back,
            _ => Flow::Continue,
        }
    }

    fn on_tick(&mut self) -> Flow {
        while let Ok((i, result)) = self.assist_rx.try_recv() {
            self.active_assessments = self.active_assessments.saturating_sub(1);
            match result {
                Ok(artifact) => {
                    let accepted = artifact.validation.status == "accepted";
                    self.items[i].assistance = Assistance::Ready(Box::new(artifact));
                    self.notice = Some(if accepted {
                        format!(
                            "assessment ready for {}",
                            &self.items[i].id[..self.items[i].id.len().min(8)]
                        )
                    } else {
                        format!(
                            "assessment failed validation for {}",
                            &self.items[i].id[..self.items[i].id.len().min(8)]
                        )
                    });
                }
                Err(error) => {
                    self.items[i].assistance = Assistance::Failed(error);
                    self.notice = Some(format!(
                        "assessment runner failed for {}",
                        &self.items[i].id[..self.items[i].id.len().min(8)]
                    ));
                }
            }
        }
        Flow::Continue
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
            sketch: None,
            assist_request: None,
            assistance: Assistance::Unavailable,
        }
    }

    fn picker(
        items: Vec<Candidate>,
        decision: Vec<Option<Decision>>,
        preview_mode: PreviewMode,
        scope: SessionScope,
    ) -> CuratePicker {
        let mut picker = CuratePicker::new("hf://datasets/o/r".into(), "o/r".into(), None, items, decision);
        picker.preview_mode = preview_mode;
        picker.scope = scope;
        picker
    }

    #[test]
    fn header_shows_how_to_include_and_exclude() {
        let picker = picker(Vec::new(), Vec::new(), PreviewMode::Prompts, SessionScope::All);
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
        let mut picker = picker(
            vec![candidate("session", Publication::Local)],
            vec![Some(Decision::Include)],
            PreviewMode::Prompts,
            SessionScope::All,
        );

        let preview = picker.preview(0).to_string();
        assert!(preview.contains("PROMPTS"));
        assert!(preview.contains("SESSION session"));
        picker.preview_mode = picker.preview_mode.toggle();
        let preview = picker.preview(0).to_string();
        assert!(preview.contains("SKETCH"));
        assert!(matches!(picker.decision.as_slice(), [Some(Decision::Include)]));
        assert!(line_text(&picker.header("")).contains("tab prompts"));
    }

    #[test]
    fn sketch_is_the_default_preview() {
        assert_eq!(PreviewMode::default(), PreviewMode::Sketch);
    }

    #[test]
    fn fixed_criterion_is_visible_in_every_preview() {
        let mut picker = picker(
            vec![candidate("session", Publication::Local)],
            vec![None],
            PreviewMode::Sketch,
            SessionScope::All,
        );
        picker.criterion = Some(CriterionSnapshot {
            schema_version: 1,
            id: "internal".into(),
            effect: crate::curation_assist::CriterionEffect::Exclusion,
            name: "internal.txt".into(),
            fingerprint: "sha256:1234567890".into(),
            text: "Do not disclose internal projects or people.".into(),
        });

        let preview = picker.preview(0).to_string();
        assert!(preview.contains("EXCLUDE WHEN MATCHED"));
        assert!(preview.contains("Do not disclose internal projects or people."));
        assert!(preview.contains("SKETCH"));
        assert!(picker.hints().contains("internal · exclusion · 12345678"));
    }

    #[test]
    fn a_published_session_is_visible_but_immutable() {
        let mut picker = picker(
            vec![candidate("published", Publication::Published)],
            vec![Some(Decision::Include)],
            PreviewMode::Prompts,
            SessionScope::All,
        );

        assert!(line_text(&picker.row(0)).contains('↑'));
        assert!(line_text(&picker.row(0)).contains("publishe"));
        picker.toggle(0, Decision::Exclude);
        assert!(matches!(picker.decision.as_slice(), [Some(Decision::Include)]));
    }

    #[test]
    fn pending_scope_shows_only_undecided_reviewable_sessions() {
        let picker = picker(
            vec![
                candidate("pending", Publication::Local),
                candidate("decided", Publication::Local),
                candidate("published", Publication::Published),
            ],
            vec![None, Some(Decision::Exclude), None],
            PreviewMode::Prompts,
            SessionScope::Pending,
        );

        assert!(picker.visible(0));
        assert!(!picker.visible(1), "a settled decision does not require curation");
        assert!(!picker.visible(2), "a fully published row is browse-only");
        assert!(line_text(&picker.header("")).contains("shift-tab all"));
        assert!(picker.hints().starts_with("pending"));
    }
}
