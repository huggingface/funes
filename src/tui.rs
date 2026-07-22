//! An in-process list+preview picker: a filterable list on the left, a live preview on the right,
//! a query line at the bottom. It is generic over a [`PickerModel`] the caller implements, so a
//! screen supplies its own rows, preview, and custom key handling without the engine knowing
//! anything about the domain. The curate review drives it; other commands can drive their own.
//!
//! The engine owns all *view* state — the live query, the ranked survivor list, the cursor and its
//! scroll, the preview scroll. The model owns only *domain* state. That split is what lets a
//! model's [`PickerModel::on_key`] take `&mut self` and mutate itself (and any caller state it
//! holds) each keypress with no aliasing, and stay open across those mutations.

pub mod curate;

use std::io::{Stdout, Write};

use anyhow::Result;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

/// The concrete terminal the picker draws to.
pub type Term = Terminal<CrosstermBackend<Stdout>>;

const PAGE: i32 = 10;
const PREVIEW_STEP: i32 = 8;
const DEFAULT_PREVIEW_PCT: u16 = 60;

/// A screen the picker can drive. The engine calls these; a consumer implements them.
#[allow(clippy::len_without_is_empty)]
pub trait PickerModel {
    /// Number of items before filtering.
    fn len(&self) -> usize;
    /// Whether item `i` belongs to the model's current scope. A screen can change this from
    /// [`PickerModel::on_key`] and return [`Flow::Refilter`] to rebuild the visible list without
    /// conflating a structural scope with the user's fuzzy query.
    fn visible(&self, _i: usize) -> bool {
        true
    }
    /// The text nucleo scores the live query against for item `i` — usually hidden content richer
    /// than the visible row (so a query matches beyond what the scent shows).
    fn filter_key(&self, i: usize) -> &str;
    /// The list row for item `i`, fully styled (glyph, dim, text). The engine adds the cursor
    /// pointer and selection highlight itself.
    fn row(&self, i: usize) -> Line<'static>;
    /// The preview pane for item `i`.
    fn preview(&self, i: usize) -> Text<'static>;
    /// The top line: context and key hints. Given the live query in case it wants to echo it.
    fn header(&self, query: &str) -> Line<'static>;
    /// A trailing hint shown dim on the query line.
    fn hints(&self) -> String {
        String::new()
    }
    /// Keys the engine did not consume (everything but query editing and list/preview navigation):
    /// Enter, Esc, ←/→, Ctrl-y, … `sel` is the model index under the cursor (None on an empty
    /// list). The returned [`Flow`] decides what happens next.
    fn on_key(&mut self, key: KeyEvent, sel: Option<usize>, ctx: &mut Ctx) -> Flow;
}

/// What a model's [`PickerModel::on_key`] tells the engine to do next.
pub enum Flow {
    /// Stay open; redraw (the model may have mutated itself).
    Continue,
    /// Stay open, redraw, and return the preview pane to its first line.
    ResetPreview,
    /// Re-run the model's scope and fuzzy filters, resetting the cursor and preview.
    Refilter,
    /// Return `Accept(i)` from [`run`] — a select-one result for the caller.
    Accept(usize),
    /// Pop this screen: `run` returns `Back` to its caller (an outer picker, or the top).
    Back,
    /// Tear the whole picker down.
    Quit,
}

/// Handed to [`PickerModel::on_key`] so a model can copy to the clipboard or flash a transient
/// frame — without ever touching engine view state.
pub struct Ctx<'t> {
    term: &'t mut Term,
    clipboard: Option<&'static str>,
}

impl Ctx<'_> {
    /// Copy `text` to the system clipboard via the discovered writer (pbcopy/wl-copy/xclip/xsel);
    /// false when the box has none or the writer fails. Spawns the writer directly — no shell.
    pub fn copy(&self, text: &str) -> bool {
        let Some(pipe) = self.clipboard else { return false };
        let mut parts = pipe.split_whitespace();
        let Some(prog) = parts.next() else { return false };
        let args: Vec<&str> = parts.collect();
        match std::process::Command::new(prog)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                child.wait().map(|s| s.success()).unwrap_or(false)
            }
            Err(_) => false,
        }
    }

    /// Draw a one-off full-screen message (e.g. while a blocking load runs).
    pub fn flash(&mut self, msg: &str) -> Result<()> {
        self.term.draw(|f| {
            f.render_widget(Paragraph::new(msg.to_string()).style(dim_style()), f.area());
        })?;
        Ok(())
    }
}

/// Options for one [`run`] of a screen.
pub struct RunOpts {
    /// Initial cursor position (model index) — e.g. land on the hit's turn.
    pub start: usize,
    /// Initial query.
    pub query: String,
    /// Preview pane width, percent of the body.
    pub preview_pct: u16,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            start: 0,
            query: String::new(),
            preview_pct: DEFAULT_PREVIEW_PCT,
        }
    }
}

/// Enter raw mode + the alternate screen, run `model` to completion, and restore the terminal on
/// the way out (including on panic).
pub fn run_root<M: PickerModel>(model: &mut M, opts: RunOpts) -> Result<Flow> {
    install_panic_hook();
    let clipboard = clipboard_pipe();
    let mut guard = TermGuard::enter()?;
    run(&mut guard.term, model, opts, clipboard)
}

/// The engine loop over one screen. Consumes query editing and list/preview navigation itself;
/// hands every other key to `model.on_key` and acts on the returned [`Flow`].
pub fn run<M: PickerModel>(
    term: &mut Term,
    model: &mut M,
    opts: RunOpts,
    clipboard: Option<&'static str>,
) -> Result<Flow> {
    let mut view = View::new(model, &opts);
    loop {
        term.draw(|f| draw(f, &mut view, &*model))?;
        let Event::Key(key) = event::read()? else {
            continue; // resize/mouse/paste: just redraw
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => return Ok(Flow::Quit),
            KeyCode::Char('u') if ctrl => view.scroll_preview(-PREVIEW_STEP),
            KeyCode::Char('d') if ctrl => view.scroll_preview(PREVIEW_STEP),
            KeyCode::Char(c) if !ctrl && !alt => view.set_query_push(c, model),
            KeyCode::Backspace => view.set_query_pop(model),
            KeyCode::Up => view.move_cursor(-1),
            KeyCode::Down => view.move_cursor(1),
            KeyCode::PageUp => view.move_cursor(-PAGE),
            KeyCode::PageDown => view.move_cursor(PAGE),
            KeyCode::Home => view.move_cursor(i32::MIN),
            KeyCode::End => view.move_cursor(i32::MAX),
            _ => {
                let sel = view.sel_model_idx();
                let mut ctx = Ctx {
                    term: &mut *term,
                    clipboard,
                };
                match model.on_key(key, sel, &mut ctx) {
                    Flow::Continue => {}
                    Flow::ResetPreview => view.preview_off = 0,
                    Flow::Refilter => view.refilter(model),
                    other => return Ok(other),
                }
            }
        }
    }
}

/// Engine-owned view state for one screen.
struct View {
    query: String,
    filtered: Vec<usize>, // model indices surviving the query, best-first
    list: ListState,      // cursor (index into `filtered`) + scroll offset
    preview_off: u16,
    preview_pct: u16,
    filter: Filter,
}

impl View {
    fn new<M: PickerModel>(model: &M, opts: &RunOpts) -> Self {
        let mut filter = Filter::new();
        let filtered = rank(&mut filter, &opts.query, model);
        let cursor = if opts.query.is_empty() {
            opts.start.min(model.len().saturating_sub(1))
        } else {
            0
        };
        let mut list = ListState::default();
        if !filtered.is_empty() {
            list.select(Some(cursor.min(filtered.len() - 1)));
        }
        View {
            query: opts.query.clone(),
            filtered,
            list,
            preview_off: 0,
            preview_pct: if opts.preview_pct == 0 {
                DEFAULT_PREVIEW_PCT
            } else {
                opts.preview_pct
            },
            filter,
        }
    }

    fn refilter<M: PickerModel>(&mut self, model: &M) {
        self.filtered = rank(&mut self.filter, &self.query, model);
        self.preview_off = 0;
        self.list.select((!self.filtered.is_empty()).then_some(0));
    }

    fn set_query_push<M: PickerModel>(&mut self, c: char, model: &M) {
        self.query.push(c);
        self.refilter(model);
    }

    fn set_query_pop<M: PickerModel>(&mut self, model: &M) {
        self.query.pop();
        self.refilter(model);
    }

    fn move_cursor(&mut self, delta: i32) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        let cur = self.list.selected().unwrap_or(0) as i32;
        let next = cur.saturating_add(delta).clamp(0, n as i32 - 1) as usize;
        if Some(next) != self.list.selected() {
            self.preview_off = 0;
        }
        self.list.select(Some(next));
    }

    fn scroll_preview(&mut self, delta: i32) {
        self.preview_off = (self.preview_off as i32 + delta).clamp(0, u16::MAX as i32) as u16;
    }

    fn sel_model_idx(&self) -> Option<usize> {
        self.list.selected().and_then(|c| self.filtered.get(c).copied())
    }
}

/// Rank a query over a model's filter keys into surviving model indices (best-first). An empty
/// query passes every item in original order — the initial full list.
fn rank<M: PickerModel>(filter: &mut Filter, query: &str, model: &M) -> Vec<usize> {
    if query.is_empty() {
        return (0..model.len()).filter(|&i| model.visible(i)).collect();
    }
    filter.rank(
        query,
        (0..model.len())
            .filter(|&i| model.visible(i))
            .map(|i| (i, model.filter_key(i))),
    )
}

fn draw<M: PickerModel>(f: &mut Frame, view: &mut View, model: &M) {
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).split(f.area());
    f.render_widget(Paragraph::new(model.header(&view.query)), rows[0]);

    let body = Layout::horizontal([
        Constraint::Percentage(100 - view.preview_pct),
        Constraint::Percentage(view.preview_pct),
    ])
    .split(rows[1]);

    let items: Vec<ListItem> = view.filtered.iter().map(|&i| ListItem::new(model.row(i))).collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("❯ ")
        .highlight_spacing(HighlightSpacing::Always);
    f.render_stateful_widget(list, body[0], &mut view.list);

    let preview = view.sel_model_idx().map(|i| model.preview(i)).unwrap_or_default();
    let paragraph = Paragraph::new(preview).wrap(Wrap { trim: false });
    // Paragraph scroll offsets address rendered rows, not source lines. A prose-heavy preview can
    // contain one logical line that wraps into dozens of terminal rows, so `Text::lines.len()` is
    // not a valid bound here.
    view.preview_off = view.preview_off.min(preview_scroll_limit(&paragraph, body[1]));
    f.render_widget(paragraph.scroll((view.preview_off, 0)), body[1]);

    let meta = format!("  [{}/{}]  {}", view.filtered.len(), model.len(), model.hints());
    let prompt = Line::from(vec![
        Span::raw(format!("❯ {}", view.query)),
        Span::styled(meta, dim_style()),
    ]);
    f.render_widget(Paragraph::new(prompt), rows[2]);
    let cx = rows[2].x + 2 + view.query.chars().count() as u16;
    f.set_cursor_position((cx.min(rows[2].x + rows[2].width.saturating_sub(1)), rows[2].y));
}

fn preview_scroll_limit(paragraph: &Paragraph<'_>, area: Rect) -> u16 {
    paragraph
        .line_count(area.width)
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16
}

// --- terminal lifecycle ---

/// RAII terminal: raw mode + alt screen on [`TermGuard::enter`], restored on `Drop` (survives a
/// `?`-early-return and a panic unwind).
struct TermGuard {
    term: Term,
}

impl TermGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        // Raw mode is now on but no guard owns the restore yet, so undo it by hand if entering the
        // alt screen or building the terminal fails — otherwise an error here would leave the tty
        // raw (the Drop restore and the panic hook only cover a live guard / an unwind).
        let build = || -> Result<Term> {
            execute!(std::io::stdout(), EnterAlternateScreen)?;
            Ok(Terminal::new(CrosstermBackend::new(std::io::stdout()))?)
        };
        match build() {
            Ok(term) => Ok(TermGuard { term }),
            Err(e) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(e)
            }
        }
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.term.backend_mut(), LeaveAlternateScreen);
        let _ = self.term.show_cursor();
    }
}

/// Restore the terminal before the default panic hook prints, so a panic mid-picker never leaves
/// the tty in raw mode / the alternate screen.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        prev(info);
    }));
}

// --- nucleo filtering ---

/// A reusable nucleo fuzzy matcher over bounded lists.
struct Filter {
    matcher: Matcher,
    pattern: Pattern,
    buf: Vec<char>,
}

impl Filter {
    fn new() -> Self {
        Filter {
            matcher: Matcher::new(Config::DEFAULT),
            pattern: Pattern::default(),
            buf: Vec::new(),
        }
    }

    /// Score `query` against each `(index, key)` and return the surviving indices, best score
    /// first; ties keep the caller's order (the sort is stable).
    fn rank<'a>(&mut self, query: &str, keys: impl Iterator<Item = (usize, &'a str)>) -> Vec<usize> {
        self.pattern.reparse(query, CaseMatching::Smart, Normalization::Smart);
        let (matcher, pattern, buf) = (&mut self.matcher, &self.pattern, &mut self.buf);
        let mut scored: Vec<(usize, u32)> = keys
            .filter_map(|(i, k)| pattern.score(Utf32Str::new(k, buf), matcher).map(|s| (i, s)))
            .collect();
        scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
        scored.into_iter().map(|(i, _)| i).collect()
    }
}

// --- ANSI → ratatui ---

fn dim_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn sgr_style(dim: bool, reverse: bool) -> Style {
    let mut s = Style::default();
    if dim {
        s = s.add_modifier(Modifier::DIM);
    }
    if reverse {
        s = s.add_modifier(Modifier::REVERSED);
    }
    s
}

fn apply_sgr(params: &str, dim: &mut bool, reverse: &mut bool) {
    if params.is_empty() {
        // `ESC[m` is `ESC[0m`.
        *dim = false;
        *reverse = false;
        return;
    }
    for p in params.split(';') {
        match p.parse::<u16>() {
            Ok(0) => {
                *dim = false;
                *reverse = false;
            }
            Ok(2) => *dim = true,
            Ok(7) => *reverse = true,
            Ok(22) => *dim = false,
            Ok(27) => *reverse = false,
            _ => {} // color/other SGR funes never emits — ignore, keep the text
        }
    }
}

/// Convert a string carrying render.rs's SGR escapes (only dim `2`, reverse `7`, and resets) into
/// styled ratatui `Text`. Splits on newlines; unknown escapes are dropped, their text kept.
pub fn ansi_to_text(s: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let (mut dim, mut reverse) = (false, false);
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next(); // '['
                let mut params = String::new();
                for pc in chars.by_ref() {
                    if pc == 'm' {
                        break;
                    }
                    params.push(pc);
                }
                if !cur.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur), sgr_style(dim, reverse)));
                }
                apply_sgr(&params, &mut dim, &mut reverse);
            }
            '\n' => {
                if !cur.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur), sgr_style(dim, reverse)));
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, sgr_style(dim, reverse)));
    }
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    Text::from(lines)
}

/// The first clipboard writer on PATH, as a command string; None when the box has none.
pub fn clipboard_pipe() -> Option<&'static str> {
    const WRITERS: [(&str, &str); 4] = [
        ("pbcopy", "pbcopy"),
        ("wl-copy", "wl-copy"),
        ("xclip", "xclip -selection clipboard"),
        ("xsel", "xsel --input --clipboard"),
    ];
    let path = std::env::var_os("PATH")?;
    WRITERS
        .iter()
        .find(|(bin, _)| std::env::split_paths(&path).any(|d| d.join(bin).is_file()))
        .map(|(_, pipe)| *pipe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn ansi_converts_dim_and_reverse() {
        let t = ansi_to_text("\x1b[2mmeta\x1b[0m tail");
        let spans = &t.lines[0].spans;
        assert_eq!(spans[0].content, "meta");
        assert!(spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(spans[1].content, " tail");
        assert!(!spans[1].style.add_modifier.contains(Modifier::DIM));

        let r = ansi_to_text("a \x1b[7mhit\x1b[0m b");
        assert!(r.lines[0]
            .spans
            .iter()
            .any(|s| s.content == "hit" && s.style.add_modifier.contains(Modifier::REVERSED)));
    }

    #[test]
    fn ansi_ignores_unknown_codes_keeps_text() {
        // A 256-color SGR funes never emits: the code is dropped, the text survives unstyled.
        let t = ansi_to_text("\x1b[38;5;6mcyan\x1b[0m");
        let joined: String = t.lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "cyan");
        assert!(t.lines[0].spans.iter().all(|s| s.style.add_modifier.is_empty()));
    }

    #[test]
    fn ansi_splits_lines() {
        let t = ansi_to_text("one\ntwo");
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[0].spans[0].content, "one");
        assert_eq!(t.lines[1].spans[0].content, "two");
    }

    #[test]
    fn filter_empty_query_passes_all_in_order() {
        let mut f = Filter::new();
        let keys = ["alpha", "beta", "gamma"];
        assert_eq!(f.rank("", keys.iter().copied().enumerate()), vec![0, 1, 2]);
    }

    #[test]
    fn filter_ranks_and_drops_non_matches() {
        let mut f = Filter::new();
        let keys = ["mask future keys", "top-k selection", "unrelated prose"];
        let got = f.rank("mask", keys.iter().copied().enumerate());
        assert_eq!(got, vec![0]);
        // A query that matches nothing yields nothing.
        assert!(f.rank("zzzzz", keys.iter().copied().enumerate()).is_empty());
    }

    /// A minimal model for exercising the render path with `TestBackend`.
    struct Mock {
        rows: Vec<String>,
    }
    impl PickerModel for Mock {
        fn len(&self) -> usize {
            self.rows.len()
        }
        fn visible(&self, i: usize) -> bool {
            !self.rows[i].starts_with("hidden:")
        }
        fn filter_key(&self, i: usize) -> &str {
            &self.rows[i]
        }
        fn row(&self, i: usize) -> Line<'static> {
            Line::raw(self.rows[i].clone())
        }
        fn preview(&self, i: usize) -> Text<'static> {
            Text::raw(format!("preview of {}", self.rows[i]))
        }
        fn header(&self, _q: &str) -> Line<'static> {
            Line::raw("HEADER")
        }
        fn on_key(&mut self, _k: KeyEvent, _s: Option<usize>, _c: &mut Ctx) -> Flow {
            Flow::Continue
        }
    }

    fn buf_text(term: &Terminal<TestBackend>) -> String {
        term.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn draws_header_rows_preview_and_prompt() {
        let model = Mock {
            rows: vec!["first row".into(), "second row".into()],
        };
        let mut term = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut view = View::new(&model, &RunOpts::default());
        term.draw(|f| draw(f, &mut view, &model)).unwrap();
        let text = buf_text(&term);
        assert!(text.contains("HEADER"), "header missing: {text}");
        assert!(text.contains("first row"), "row missing: {text}");
        assert!(text.contains("preview of first row"), "preview missing: {text}");
        assert!(text.contains("[2/2]"), "count missing: {text}");
        assert!(text.contains('❯'), "pointer/prompt missing: {text}");
    }

    #[test]
    fn typing_filters_the_list() {
        let model = Mock {
            rows: vec!["mask future keys".into(), "top-k selection".into()],
        };
        let mut view = View::new(&model, &RunOpts::default());
        view.set_query_push('m', &model);
        view.set_query_push('a', &model);
        view.set_query_push('s', &model);
        view.set_query_push('k', &model);
        assert_eq!(view.filtered, vec![0]);
        assert_eq!(view.sel_model_idx(), Some(0));
    }

    #[test]
    fn model_scope_applies_before_and_during_fuzzy_filtering() {
        let model = Mock {
            rows: vec!["alpha visible".into(), "hidden: alpha".into(), "beta".into()],
        };
        let mut view = View::new(&model, &RunOpts::default());
        assert_eq!(view.filtered, vec![0, 2]);
        for c in "alpha".chars() {
            view.set_query_push(c, &model);
        }
        assert_eq!(view.filtered, vec![0]);
    }

    #[test]
    fn initial_selection_lands_on_start() {
        let model = Mock {
            rows: (0..5).map(|i| format!("row {i}")).collect(),
        };
        let opts = RunOpts {
            start: 3,
            ..Default::default()
        };
        let view = View::new(&model, &opts);
        assert_eq!(view.sel_model_idx(), Some(3));
    }

    #[test]
    fn preview_scroll_limit_counts_wrapped_visual_lines() {
        let paragraph = Paragraph::new("wrapped prose ".repeat(40)).wrap(Wrap { trim: false });
        let area = Rect::new(0, 0, 12, 3);
        let rendered = paragraph.line_count(area.width);
        assert!(rendered > area.height as usize);
        assert_eq!(
            preview_scroll_limit(&paragraph, area),
            (rendered - area.height as usize) as u16
        );
        assert_eq!(preview_scroll_limit(&paragraph, Rect::new(0, 0, 1_000, 3)), 0);
    }
}
