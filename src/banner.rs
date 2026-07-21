//! The `ask` wait animation harness: a background thread that plays an [`Animation`] in a fixed
//! band of stderr while the agent works, then erases it without trace once the answer is ready.
//!
//! The harness is animation-agnostic — it owns the thread, the live status line, palette
//! detection, the TTY gating, the compact fallbacks, and the erase. The visual itself lives
//! behind the [`Animation`] trait (see [`drift`]); to change it, implement the trait in a new
//! module and point [`animation`] at it. Drawing is plain ANSI on the normal screen — no raw
//! mode, no alternate screen — so an interrupt mid-ask can't wedge the terminal.

mod drift;

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::crossterm::terminal;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::Marker;
use ratatui::widgets::canvas::{Canvas, Context};
use ratatui::widgets::Widget;

const FRAME: std::time::Duration = std::time::Duration::from_millis(80);
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// The band's left indent, shared by every animation row and the status line.
const INDENT: &str = "  ";
/// Below this terminal width the frame degrades to the one-line spinner.
const MIN_FULL_W: usize = 24;
/// The status line's spinner/elapsed accent.
const GOLD: (u8, u8, u8) = (255, 221, 105);

/// The animation the banner plays. Swap the body to change the visual — the harness is agnostic
/// to which [`Animation`] it drives.
fn animation() -> Box<dyn Animation> {
    Box::new(drift::Drift::new())
}

/// A wait animation, isolated from the terminal plumbing: pure state advanced by wall-clock
/// seconds, drawn into a fixed-width band of ANSI rows. The harness adds the layout indent, the
/// status line, palette fallbacks, and the erase around it.
trait Animation: Send {
    /// Advance the animation by `dt` seconds of wall-clock time.
    fn advance(&mut self, dt: f64);
    /// An agent event just landed (the status line changed) — react to it.
    fn excite(&mut self);
    /// Draw the current frame as colored ANSI rows, each at most `width` columns and no indent.
    /// The row count is the animation's fixed height.
    fn render(&self, width: usize, palette: Palette) -> Vec<String>;
}

/// The animated wait display; the status label is swapped live via [`Banner::set`], and each
/// swap excites the animation. Dropping it stops and erases the animation.
pub struct Banner {
    label: Arc<Mutex<String>>,
    pulse: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Banner {
    pub fn start(label: &str) -> Option<Banner> {
        if !std::io::stderr().is_terminal() {
            return None;
        }
        let label = Arc::new(Mutex::new(label.to_string()));
        let pulse = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, p, s) = (label.clone(), pulse.clone(), stop.clone());
        let palette = Palette::detect();
        let handle = std::thread::spawn(move || {
            let mut anim = animation();
            let mut t = 0.0f64;
            let mut drawn = 0usize;
            let mut last = Instant::now();
            while !s.load(Ordering::Relaxed) {
                if p.swap(false, Ordering::Relaxed) {
                    anim.excite();
                }
                let label = l.lock().map(|g| g.clone()).unwrap_or_default();
                redraw(&mut drawn, &compose(&*anim, t, term_width(), palette, &label));
                std::thread::sleep(FRAME);
                let dt = std::mem::replace(&mut last, Instant::now()).elapsed().as_secs_f64();
                t += dt;
                anim.advance(dt);
            }
            finish(drawn);
        });
        Some(Banner {
            label,
            pulse,
            stop,
            handle: Some(handle),
        })
    }

    /// Swap the label; the next frame shows it, and a changed label excites the animation.
    pub fn set(&self, label: &str) {
        if let Ok(mut l) = self.label.lock() {
            if *l != label {
                label.clone_into(&mut l);
                self.pulse.store(true, Ordering::Relaxed);
            }
        }
    }
}

impl Drop for Banner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// One frame: the animation's rows indented under the status line, or — off truecolor or on a
/// narrow terminal — just the one-line spinner, no animation.
fn compose(anim: &dyn Animation, t: f64, width: usize, palette: Palette, label: &str) -> Vec<String> {
    if palette == Palette::Mono || width < MIN_FULL_W {
        return vec![status_line(t, palette, width, label)];
    }
    let cw = width - INDENT.len() - 2;
    let mut lines: Vec<String> = anim
        .render(cw, palette)
        .into_iter()
        .map(|row| format!("{INDENT}{row}"))
        .collect();
    lines.push(format!("{INDENT}{}", status_line(t, palette, cw, label)));
    lines
}

/// Spinner + live label + elapsed, un-indented. Truncated to the band: a wrapped status would
/// add a row and desync the repaint height.
fn status_line(t: f64, palette: Palette, width: usize, label: &str) -> String {
    let spin = SPIN[(t / FRAME.as_secs_f64()) as usize % SPIN.len()];
    let label = clip(label, width.saturating_sub(14));
    match palette {
        Palette::Mono => format!("{spin} {label} · {}s", t as u64),
        p => format!("{}{spin}\x1b[0m {label}\x1b[2m · {}s\x1b[0m", p.fg(GOLD), t as u64),
    }
}

/// Zero-width means the terminal didn't say (headless ptys) — treat it like no answer at all.
fn term_width() -> usize {
    match terminal::size() {
        Ok((w, _)) if w > 0 => w as usize,
        _ => 80,
    }
}

/// Truncate to `max` display chars, marking the cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Repaint the region in place — per-line \x1b[K avoids whole-frame flicker; a full \x1b[J wipe
/// happens only when the frame changes height (a resize crossing the slim threshold).
fn redraw(drawn: &mut usize, lines: &[String]) {
    let mut out = String::from("\r");
    if *drawn > 1 {
        out.push_str(&format!("\x1b[{}A", *drawn - 1));
    }
    if *drawn > 0 && *drawn != lines.len() {
        out.push_str("\x1b[J");
    }
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        out.push_str("\x1b[K");
    }
    eprint!("{out}");
    let _ = std::io::stderr().flush();
    *drawn = lines.len();
}

/// The send-off: the whole region erased, leaving the answer alone on screen.
fn finish(drawn: usize) {
    if drawn == 0 {
        return;
    }
    let mut out = String::from("\r");
    if drawn > 1 {
        out.push_str(&format!("\x1b[{}A", drawn - 1));
    }
    out.push_str("\x1b[J");
    eprint!("{out}");
    let _ = std::io::stderr().flush();
}

#[derive(Clone, Copy, PartialEq)]
enum Palette {
    True,
    Indexed,
    Mono,
}

impl Palette {
    fn detect() -> Palette {
        if std::env::var_os("NO_COLOR").is_some() {
            return Palette::Mono;
        }
        match std::env::var("COLORTERM") {
            Ok(v) if v.contains("truecolor") || v.contains("24bit") => Palette::True,
            _ => Palette::Indexed,
        }
    }

    /// Foreground escape for an RGB color, degraded to the 256-color cube when needed.
    fn fg(self, (r, g, b): (u8, u8, u8)) -> String {
        match self {
            Palette::True => format!("\x1b[38;2;{r};{g};{b}m"),
            Palette::Indexed => {
                let c = |v: u8| (v as u16 * 5 / 255) as u8;
                format!("\x1b[38;5;{}m", 16 + 36 * c(r) + 6 * c(g) + c(b))
            }
            Palette::Mono => String::new(),
        }
    }
}

/// Paint a ratatui `Canvas` (braille marker) into `rows` lines of at most `width` columns and
/// serialize the cells to ANSI, encoding each `Color::Rgb` through `palette`. The reusable core
/// of a Canvas-based animation — an [`Animation::render`] supplies the shapes in `paint`.
fn canvas_rows(
    width: usize,
    rows: u16,
    y_bounds: [f64; 2],
    palette: Palette,
    paint: impl Fn(&mut Context),
) -> Vec<String> {
    let area = Rect::new(0, 0, width as u16, rows);
    let mut buf = Buffer::empty(area);
    Canvas::default()
        .marker(Marker::Braille)
        .x_bounds([0.0, width as f64])
        .y_bounds(y_bounds)
        .paint(paint)
        .render(area, &mut buf);
    (0..rows)
        .map(|y| {
            let mut line = String::new();
            let mut last = String::new();
            for x in 0..width as u16 {
                let Some(cell) = buf.cell((x, y)) else {
                    line.push(' ');
                    continue;
                };
                let symbol = cell.symbol();
                if symbol == " " {
                    line.push(' ');
                    continue;
                }
                let color = match cell.fg {
                    Color::Rgb(r, g, b) => palette.fg((r, g, b)),
                    _ => String::new(),
                };
                if color != last {
                    line.push_str(&color);
                    last = color;
                }
                line.push_str(symbol);
            }
            line.push_str("\x1b[0m");
            line
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip ANSI escapes (ESC through the final letter) so tests see display text.
    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// A fixed-height animation that ignores time — lets the harness be tested on its own.
    struct Still(usize);
    impl Animation for Still {
        fn advance(&mut self, _dt: f64) {}
        fn excite(&mut self) {}
        fn render(&self, width: usize, _palette: Palette) -> Vec<String> {
            (0..self.0).map(|_| "▪".repeat(width.min(4))).collect()
        }
    }

    #[test]
    fn full_frame_is_indented_animation_then_status() {
        let lines = compose(&Still(2), 2.0, 100, Palette::True, "recalling…");
        assert_eq!(lines.len(), 3, "two animation rows plus the status line");
        for row in &lines[..2] {
            assert!(
                row.starts_with(INDENT) && plain(row).contains('▪'),
                "indented animation row"
            );
        }
        let status = plain(&lines[2]);
        assert!(status.starts_with(INDENT), "status shares the band's indent");
        assert!(
            status.contains("recalling…") && status.trim_end().ends_with("2s"),
            "{status}"
        );
    }

    #[test]
    fn narrow_terminal_falls_back_to_one_line() {
        let lines = compose(&Still(2), 0.0, MIN_FULL_W - 1, Palette::True, "thinking…");
        assert_eq!(lines.len(), 1);
        assert!(plain(&lines[0]).contains("thinking…"));
    }

    #[test]
    fn mono_gets_plain_text() {
        let lines = compose(&Still(2), 0.0, 120, Palette::Mono, "x");
        assert_eq!(lines.len(), 1);
        assert_eq!(plain(&lines[0]), lines[0], "no escapes under NO_COLOR");
    }

    #[test]
    fn a_long_label_is_clipped_to_fit() {
        let line = plain(&status_line(0.0, Palette::True, 30, &"word ".repeat(20)));
        assert!(line.chars().count() <= 30, "clipped to width: {line:?}");
        assert!(line.contains('…'), "the cut is marked");
    }
}
