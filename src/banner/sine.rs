//! Thin oscilloscope traces: one clean sine, or three harmonics layered as separate 1px lines
//! (drawn with the Canvas `Line` shape, not filled). Events swell the amplitude and quicken the
//! sweep, settling between. Pure state → rows, so tests drive it deterministically.

use ratatui::style::Color;
use ratatui::widgets::canvas::Line;

use super::{canvas_rows, Animation, Palette};

const ROWS: u16 = 2;
/// The trace lives in y ∈ [-Y_LIM, Y_LIM]; a little headroom past ±1 keeps crests off the edge.
const Y_LIM: f64 = 1.2;
/// Horizontal sampling step, in canvas columns — fine enough for a smooth polyline.
const STEP: f64 = 0.25;

const ORANGE: (u8, u8, u8) = (255, 138, 40);
const GOLD: (u8, u8, u8) = (255, 205, 70);
const RUST: (u8, u8, u8) = (198, 84, 22);

/// A single trace: its colour and the height it draws at each column.
type Wave = ((u8, u8, u8), Box<dyn Fn(f64) -> f64>);

/// Which trace to draw; the two share all the state and only differ in their wave set.
enum Kind {
    Single,
    Triple,
}

pub(super) struct Sine {
    t: f64,
    /// Birth times of live event bursts; a fresh one swells the trace.
    bursts: Vec<f64>,
    kind: Kind,
}

impl Sine {
    pub(super) fn single() -> Sine {
        Sine::new(Kind::Single)
    }
    pub(super) fn triple() -> Sine {
        Sine::new(Kind::Triple)
    }

    fn new(kind: Kind) -> Sine {
        // Born surging: the launch itself is an event.
        Sine {
            t: 0.0,
            bursts: vec![0.0],
            kind,
        }
    }

    /// How excited the trace is now, 0 at rest rising toward 1 just after an event.
    fn surge(&self) -> f64 {
        self.bursts
            .iter()
            .map(|born| (-(self.t - born) * 1.3).exp())
            .sum::<f64>()
            .min(1.0)
    }

    /// The wave set for this trace: each is (colour, height-of-column). `t` is the clock and `s`
    /// the surge, baked in so the closures are self-contained.
    fn waves(&self) -> Vec<Wave> {
        let (t, s) = (self.t, self.surge());
        match self.kind {
            Kind::Single => {
                vec![(
                    GOLD,
                    Box::new(move |x: f64| (0.55 + 0.45 * s) * (x * 0.28 - t * (2.2 + 1.6 * s)).sin()),
                )]
            }
            Kind::Triple => {
                let amp = 0.4 + 0.4 * s;
                vec![
                    (RUST, Box::new(move |x: f64| amp * 0.8 * (x * 0.19 + t * 1.5).sin())),
                    (ORANGE, Box::new(move |x: f64| amp * 0.6 * (x * 0.47 - t * 3.0).sin())),
                    (
                        GOLD,
                        Box::new(move |x: f64| amp * (x * 0.30 - t * (2.4 + 2.0 * s)).sin()),
                    ),
                ]
            }
        }
    }
}

impl Animation for Sine {
    fn advance(&mut self, dt: f64) {
        self.t += dt;
        // A burst's swell has decayed to nothing well within three seconds.
        self.bursts.retain(|born| self.t - born < 3.0);
    }

    fn excite(&mut self) {
        self.bursts.push(self.t);
    }

    fn render(&self, width: usize, palette: Palette) -> Vec<String> {
        let waves = self.waves();
        canvas_rows(width, ROWS, [-Y_LIM, Y_LIM], palette, |ctx| {
            for (color, f) in &waves {
                let (mut px, mut py) = (0.0, f(0.0));
                let mut x = STEP;
                while x <= width as f64 {
                    let y = f(x);
                    ctx.draw(&Line {
                        x1: px,
                        y1: py,
                        x2: x,
                        y2: y,
                        color: Color::Rgb(color.0, color.1, color.2),
                    });
                    (px, py) = (x, y);
                    x += STEP;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dots(rows: &[String]) -> usize {
        rows.iter()
            .flat_map(|r| r.chars())
            .filter(|c| ('\u{2801}'..='\u{28FF}').contains(c))
            .count()
    }

    #[test]
    fn both_kinds_draw_a_two_row_trace() {
        for mut s in [Sine::single(), Sine::triple()] {
            for _ in 0..25 {
                s.advance(0.08);
            }
            let rows = s.render(56, Palette::True);
            assert_eq!(rows.len(), ROWS as usize);
            assert!(dots(&rows) >= 10, "the trace is drawn");
        }
    }

    #[test]
    fn an_event_swells_the_trace() {
        let mut s = Sine::single();
        for _ in 0..40 {
            s.advance(0.08);
        }
        let calm = s.surge();
        s.excite();
        assert!(s.surge() > calm, "an event raises the surge");
        for _ in 0..40 {
            s.advance(0.08);
        }
        assert!(s.surge() < 0.1, "the swell settles");
    }

    #[test]
    fn frames_are_deterministic() {
        let (mut a, mut b) = (Sine::triple(), Sine::triple());
        for _ in 0..10 {
            a.advance(0.08);
            b.advance(0.08);
        }
        assert_eq!(a.render(56, Palette::True), b.render(56, Palette::True));
    }
}
