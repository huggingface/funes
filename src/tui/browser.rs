//! Recall's picker screens over the generic [`crate::tui`] engine: a hit picker whose Enter drills
//! into a per-session turn browser, and Esc backs out. Both reuse [`crate::render`]'s human layout
//! (the tested single source of truth) via the ANSI→ratatui converter — no span reimplementation.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use tokio::runtime::Handle;

use crate::recall::{self, Hit, Turn};
use crate::tui::{ansi_line, ansi_to_text, copy_cmd, run_root, Ctx, Flow, PickerModel, RunOpts};
use crate::{hub, render};

/// The interactive recall browser: a fuzzy-filterable list of hits with a live preview, Enter to
/// browse a hit's session, Ctrl-y to copy its `get` command, Esc to quit. Runs on the caller's
/// thread (a plain OS thread, not a runtime worker) so `rt.block_on` can load a session on drill.
pub fn run(
    store: hub::Store,
    note: String,
    query: String,
    hits: &[(Hit, f64)],
    color: bool,
    width: usize,
    rt: Handle,
) -> anyhow::Result<()> {
    // The list opens unfiltered in recall order — the recall query is shown in the header for
    // context, NOT seeded as the live filter (that would fuzzy-AND every word against each chunk
    // and hide the semantically-ranked hits until the user cleared it).
    let mut picker = HitPicker::new(store, note, query, hits, color, width, rt);
    run_root(&mut picker, RunOpts::default())?; // Back and Quit both mean "done browsing"
    Ok(())
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// True when a clipboard writer is present — tailors the key hints.
fn can_copy() -> bool {
    crate::tui::clipboard_pipe().is_some()
}

struct HitPicker<'a> {
    store: hub::Store,
    label: String,
    note: String,
    query: String, // the recall query, shown in the header for context (not the live filter)
    hits: &'a [(Hit, f64)],
    rows: Vec<String>,  // render::recall_rows — one ANSI row per hit
    blobs: Vec<String>, // whitespace-collapsed matched chunk: the search surface and the preview
    color: bool,
    width: usize,
    copy: bool,
    rt: Handle,
}

impl<'a> HitPicker<'a> {
    fn new(
        store: hub::Store,
        note: String,
        query: String,
        hits: &'a [(Hit, f64)],
        color: bool,
        width: usize,
        rt: Handle,
    ) -> Self {
        let rows = render::recall_rows(hits, color, width, chrono::Utc::now());
        let blobs = hits
            .iter()
            .map(|(h, _)| {
                h.text
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(2000)
                    .collect()
            })
            .collect();
        HitPicker {
            label: store.label(),
            store,
            note,
            query,
            hits,
            rows,
            blobs,
            color,
            width,
            copy: can_copy(),
            rt,
        }
    }
}

impl PickerModel for HitPicker<'_> {
    fn len(&self) -> usize {
        self.hits.len()
    }

    fn filter_key(&self, i: usize) -> &str {
        &self.blobs[i]
    }

    fn row(&self, i: usize) -> Line<'static> {
        ansi_line(&self.rows[i])
    }

    fn preview(&self, i: usize) -> Text<'static> {
        // The matched chunk verbatim — the turn browser behind Enter owns the surrounding context.
        ansi_to_text(&self.blobs[i])
    }

    fn header(&self, _query: &str) -> Line<'static> {
        // `_query` is the live filter (the engine echoes it on the prompt line); the header shows
        // the recall query so the user keeps sight of what these hits answer.
        let mut spans = Vec::new();
        let note = self.note.trim();
        if !note.is_empty() {
            spans.push(Span::styled(format!("{note}  "), dim()));
        }
        spans.push(Span::raw(format!("recall  {}", self.query)));
        spans.push(Span::styled(format!("  ·  {}", self.label), dim()));
        Line::from(spans)
    }

    fn hints(&self) -> String {
        if self.copy {
            "enter browses · ctrl-y copies · esc quits".into()
        } else {
            "enter browses · esc quits".into()
        }
    }

    fn on_key(&mut self, key: KeyEvent, sel: Option<usize>, ctx: &mut Ctx) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                let Some(i) = sel else { return Flow::Continue };
                let h = &self.hits[i].0;
                let _ = ctx.flash("loading session…");
                match self.rt.block_on(recall::get_turns(
                    self.store.clone(),
                    h.session_id.clone(),
                    h.turn_uuid.clone(),
                    i64::MAX,
                )) {
                    Ok((note, turns)) if !turns.is_empty() => {
                        // Mark the matched chunk so it stands out of the surrounding turns.
                        let mark: String = h.text.split_whitespace().collect::<Vec<_>>().join(" ");
                        let mut browser = TurnBrowser::new(
                            self.label.clone(),
                            note,
                            h.session_id.clone(),
                            turns,
                            h.turn_uuid.clone(),
                            Some(mark),
                            self.color,
                            self.width,
                            self.copy,
                        );
                        let start = browser.center;
                        let opts = RunOpts {
                            start,
                            ..Default::default()
                        };
                        match ctx.drill(&mut browser, opts) {
                            Ok(Flow::Quit) => Flow::Quit,
                            _ => Flow::Continue, // Back (or a transient error) returns to the picker
                        }
                    }
                    _ => Flow::Continue,
                }
            }
            KeyCode::Char('y') if ctrl => {
                if let Some(i) = sel {
                    let h = &self.hits[i].0;
                    ctx.copy(&copy_cmd(&self.label, &h.session_id, &h.turn_uuid));
                }
                Flow::Continue
            }
            KeyCode::Esc => Flow::Quit,
            _ => Flow::Continue,
        }
    }
}

/// The turn browser behind Enter: the session's turns oldest-first, the recall hit's turn marked
/// `▶` and landed on, the selected turn shown whole in the preview with the matched chunk
/// reverse-videoed. Esc walks back to the hit picker.
struct TurnBrowser {
    label: String,
    note: String,
    session_id: String,
    turns: Vec<Turn>,
    rows: Vec<String>, // render::turn_rows — one ANSI row per turn (display)
    keys: Vec<String>, // the same rows without SGR — the fuzzy-search surface
    center: usize,     // index of the hit's turn (landing position + `▶` marker)
    mark: Option<String>,
    color: bool,
    width: usize,
    copy: bool,
}

impl TurnBrowser {
    #[allow(clippy::too_many_arguments)]
    fn new(
        label: String,
        note: String,
        session_id: String,
        turns: Vec<Turn>,
        center_uuid: String,
        mark: Option<String>,
        color: bool,
        width: usize,
        copy: bool,
    ) -> Self {
        let rows = render::turn_rows(&turns, color, width);
        // Search over the plain text (color=false emits no SGR), not the ANSI-laden display rows.
        let keys = render::turn_rows(&turns, false, width);
        let center = turns.iter().position(|t| t.turn_uuid == center_uuid).unwrap_or(0);
        TurnBrowser {
            label,
            note,
            session_id,
            turns,
            rows,
            keys,
            center,
            mark,
            color,
            width,
            copy,
        }
    }
}

impl PickerModel for TurnBrowser {
    fn len(&self) -> usize {
        self.turns.len()
    }

    fn filter_key(&self, i: usize) -> &str {
        &self.keys[i]
    }

    fn row(&self, i: usize) -> Line<'static> {
        let glyph = if i == self.center { "▶ " } else { "  " };
        let mut line = ansi_line(&self.rows[i]);
        line.spans.insert(0, Span::raw(glyph));
        line
    }

    fn preview(&self, i: usize) -> Text<'static> {
        let body = render::get_human(
            "",
            std::slice::from_ref(&self.turns[i]),
            self.color,
            self.width,
            self.mark.as_deref(),
        );
        ansi_to_text(&body)
    }

    fn header(&self, _query: &str) -> Line<'static> {
        let s8 = &self.session_id[..self.session_id.len().min(8)];
        let mut spans = Vec::new();
        let note = self.note.trim();
        if !note.is_empty() {
            spans.push(Span::styled(format!("{note}  "), dim()));
        }
        spans.push(Span::raw(format!("session {s8}")));
        spans.push(Span::styled(format!("  ·  {}", self.label), dim()));
        Line::from(spans)
    }

    fn hints(&self) -> String {
        if self.copy {
            "ctrl-y copies · esc goes back".into()
        } else {
            "esc goes back".into()
        }
    }

    fn on_key(&mut self, key: KeyEvent, sel: Option<usize>, ctx: &mut Ctx) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Flow::Back,
            KeyCode::Char('y') if ctrl => {
                if let Some(i) = sel {
                    ctx.copy(&copy_cmd(&self.label, &self.session_id, &self.turns[i].turn_uuid));
                }
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle only to construct the models; the pure methods under test never touch it.
    fn handle() -> Handle {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .handle()
            .clone()
    }

    fn hit(text: &str, sid: &str, tid: &str) -> Hit {
        Hit {
            text: text.into(),
            session_id: sid.into(),
            workdir: "-home-u-funes".into(),
            turn_uuid: tid.into(),
            seq: 1,
            ts: "2026-06-19T01:29:59.000Z".into(),
            block_type: "text".into(),
            harness: "claude_code".into(),
            neighbors: vec![],
        }
    }

    fn turn(uuid: &str, role: &str, ts: &str, block: &str) -> Turn {
        Turn {
            seq: 1,
            turn_uuid: uuid.into(),
            ts: ts.into(),
            role: role.into(),
            blocks: vec![block.into()],
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn text_body(text: &Text<'static>) -> String {
        text.lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn hit_picker_searches_the_collapsed_chunk_and_names_the_store() {
        let hits = vec![(hit("mask   future\n keys before top-k", "s1", "t1"), 0.9)];
        let p = HitPicker::new(
            hub::Store::local(),
            String::new(),
            "sparse attention".into(),
            &hits,
            false,
            100,
            handle(),
        );
        assert_eq!(p.len(), 1);
        // The search surface is the whitespace-collapsed chunk, not the visible scent.
        assert_eq!(p.filter_key(0), "mask future keys before top-k");
        // The preview leads with that same chunk.
        assert!(text_body(&p.preview(0)).contains("mask future keys"));
        // The header shows the recall query and the store (the engine echoes the live filter
        // separately, on the prompt line).
        let header = line_text(&p.header(""));
        assert!(header.contains("sparse attention"), "got {header:?}");
        assert!(header.contains(&hub::Store::local().label()), "got {header:?}");
    }

    #[test]
    fn turn_browser_marks_and_lands_on_the_hit_turn() {
        let turns = vec![
            turn("a", "user", "2026-06-19T01:29:00.000Z", "the question"),
            turn("b", "assistant", "2026-06-19T01:30:00.000Z", "the answer we want"),
        ];
        let b = TurnBrowser::new(
            "local".into(),
            String::new(),
            "0123456789abcdef".into(),
            turns,
            "b".into(),
            Some("the answer we want".into()),
            false,
            100,
            false,
        );
        // Lands on the hit's turn.
        assert_eq!(b.center, 1);
        // The hit turn is glyphed `▶`; the others are indented to align.
        assert!(line_text(&b.row(1)).starts_with("▶ "));
        assert!(line_text(&b.row(0)).starts_with("  "));
        // The preview renders the selected turn's body.
        assert!(text_body(&b.preview(1)).contains("the answer we want"));
        // The header carries the 8-char session prefix.
        assert!(line_text(&b.header("")).contains("01234567"));
    }

    #[test]
    fn turn_browser_search_key_is_ansi_free() {
        let turns = vec![turn("a", "assistant", "2026-06-19T01:29:00.000Z", "compute the mask")];
        // Color on → the display rows carry SGR; the fuzzy-search key must not.
        let b = TurnBrowser::new(
            "local".into(),
            String::new(),
            "sid".into(),
            turns,
            "a".into(),
            None,
            true,
            100,
            false,
        );
        assert!(
            !b.filter_key(0).contains('\u{1b}'),
            "search key carries ANSI: {:?}",
            b.filter_key(0)
        );
        assert!(b.filter_key(0).contains("compute the mask"));
    }
}
