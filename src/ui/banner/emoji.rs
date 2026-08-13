//! The emoji bob wave: a ribbon of Hugging Face and memory-themed emojis cycling left→right
//! while bobbing between the two rows as a wave travels through; each event sparkles the crests.
//! Pure state → rows, so tests drive it deterministically.

use super::{Animation, Palette};

/// Columns per emoji slot: a ~2-cell emoji plus three spaces of breathing room.
const SLOT: usize = 5;
const ROWS: usize = 2;
/// Ribbon scroll speed, in slots per second.
const DRIFT: f64 = 2.0;
/// The bobbing wave's travel speed and how many humps span the row.
const WAVE_SPEED: f64 = 3.5;
const WAVE_FREQ: f64 = 0.7;

const HUG: &str = "🤗";
const SPARKLE: &str = "✨";
/// The memory-themed pool 🤗 shares the ribbon with — a broad mix for variety.
const POOL: [&str; 10] = ["🧠", "💭", "📚", "📖", "💡", "🔥", "🌟", "🧩", "📝", "🔖"];

fn hash(v: i64) -> f64 {
    let x = (v.wrapping_mul(2654435761) & 0x7fff_ffff) as f64;
    ((x * 0.000_001).sin() * 43758.5453).fract().abs()
}

/// The emoji at ribbon position `i` — 🤗 about a third of the time, otherwise a mote of memory;
/// stable per position so each emoji keeps its identity as the ribbon scrolls.
fn emoji_at(i: i64) -> &'static str {
    if hash(i) < 0.35 {
        HUG
    } else {
        POOL[(hash(i * 7 + 3) * POOL.len() as f64) as usize % POOL.len()]
    }
}

fn slot(glyph: &str) -> String {
    format!("{glyph}   ")
}

pub(super) struct Emoji {
    t: f64,
    /// Birth times of live event bursts; a fresh one sparkles the wave's crests.
    bursts: Vec<f64>,
}

impl Emoji {
    pub(super) fn new() -> Emoji {
        // Born sparkling: the launch itself is an event.
        Emoji {
            t: 0.0,
            bursts: vec![0.0],
        }
    }

    fn recent(&self) -> bool {
        self.bursts.iter().any(|born| self.t - born < 1.0)
    }
}

impl Animation for Emoji {
    fn advance(&mut self, dt: f64) {
        self.t += dt;
        self.bursts.retain(|born| self.t - born < 1.5);
    }

    fn excite(&mut self) {
        self.bursts.push(self.t);
    }

    fn render(&self, width: usize, _palette: Palette) -> Vec<String> {
        let slots = (width / SLOT).max(1);
        let base = (self.t * DRIFT) as i64;
        let recent = self.recent();
        let blank = " ".repeat(SLOT);
        let mut rows = vec![String::new(); ROWS];
        for k in 0..slots {
            let wave = (k as f64 * WAVE_FREQ - self.t * WAVE_SPEED).sin();
            // A crest (near the wave's peak) sparkles while an event is fresh.
            let glyph = if wave.abs() > 0.9 && recent {
                SPARKLE
            } else {
                emoji_at(k as i64 - base)
            };
            // The sign of the wave sends the emoji to the top or bottom row.
            let (fill, empty) = if wave > 0.0 { (0, 1) } else { (1, 0) };
            rows[fill].push_str(&slot(glyph));
            rows[empty].push_str(&blank);
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_emoji(s: &str) -> bool {
        s.contains(HUG) || POOL.iter().any(|e| s.contains(e))
    }

    #[test]
    fn two_rows_within_the_width() {
        let mut e = Emoji::new();
        for _ in 0..25 {
            e.advance(0.08);
        }
        let rows = e.render(60, Palette::True);
        assert_eq!(rows.len(), ROWS);
        for row in &rows {
            assert!(row.chars().count() <= 60, "row fits the width");
        }
        assert!(has_emoji(&rows[0]) || has_emoji(&rows[1]), "the ribbon is drawn");
    }

    #[test]
    fn a_face_rides_the_top_or_bottom_but_not_both() {
        // The wave splits each slot to exactly one row, so the two rows never both fill a slot.
        let e = Emoji::new();
        let rows = e.render(60, Palette::True);
        for (a, b) in rows[0].chars().zip(rows[1].chars()) {
            assert!(a == ' ' || b == ' ', "a slot column is filled on at most one row");
        }
    }

    #[test]
    fn an_event_sparkles_the_crests() {
        let mut e = Emoji::new();
        // Let the launch burst lapse, confirm the resting wave carries no sparkle…
        for _ in 0..40 {
            e.advance(0.08);
        }
        let calm = e.render(80, Palette::True);
        assert!(!calm.iter().any(|r| r.contains(SPARKLE)), "no sparkle at rest");
        // …then an event lights the crests.
        e.excite();
        let lit = e.render(80, Palette::True);
        assert!(lit.iter().any(|r| r.contains(SPARKLE)), "an event sparkles");
    }

    #[test]
    fn the_ribbon_scrolls() {
        let early = Emoji::new();
        let mut late = Emoji::new();
        for _ in 0..20 {
            late.advance(0.08);
        }
        // Far enough apart that the base index has moved several slots.
        assert_ne!(early.render(60, Palette::True), late.render(60, Palette::True));
    }

    #[test]
    fn frames_are_deterministic() {
        let (mut a, mut b) = (Emoji::new(), Emoji::new());
        for _ in 0..10 {
            a.advance(0.08);
            b.advance(0.08);
        }
        assert_eq!(a.render(60, Palette::True), b.render(60, Palette::True));
    }
}
