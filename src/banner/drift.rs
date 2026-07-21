//! The drift animation: a parallax field of data points streaming past — memories going by.
//! Every event surges the drift for a couple of seconds and launches a swarm of bright motes;
//! between events the field settles to a calm glide, and dots re-enter at a fresh height each
//! lap so the pattern never repeats. Pure state → rows, so tests drive it deterministically.

use ratatui::style::Color;
use ratatui::widgets::canvas::Points;

use super::{canvas_rows, Animation, Palette};

/// Canvas rows of the strip; braille packs 4 dots of vertical resolution into each.
const ROWS: u16 = 2;
/// The strip's ideal width; it uses fewer columns when the terminal is narrower.
const WAVE_W: usize = 48;
/// Drifting dots, spread over the three parallax layers.
const DOTS: usize = 36;
/// Event bursts kept alive at once; the eldest die first.
const MAX_BURSTS: usize = 6;
/// A burst multiplies the drift rate by up to 1+SURGE, bleeding off as e^(-DECAY·age).
const SURGE: f64 = 2.5;
const DECAY: f64 = 0.9;

/// Depth layers, far to near: slow and dim in the distance, fast and bright up close.
const LAYER_SPEED: [f64; 3] = [5.0, 9.0, 15.0];
const LAYER_COLOR: [(u8, u8, u8); 3] = [(110, 55, 10), (200, 110, 30), (255, 170, 40)];
/// What the near layer heats to at full surge, and the color of an event's mote swarm.
const NEAR_HOT: (u8, u8, u8) = (255, 225, 120);
const SWARM: (u8, u8, u8) = (255, 235, 140);

pub(super) struct Drift {
    t: f64,
    dist: f64,
    /// Birth times of live event bursts, oldest first.
    bursts: Vec<f64>,
}

impl Drift {
    pub(super) fn new() -> Drift {
        Drift {
            t: 0.0,
            dist: 0.0,
            // Born surging: the launch itself is an event.
            bursts: vec![0.0],
        }
    }

    /// The combined strength of the live bursts, 1 at an event and bleeding off after it.
    fn boost(&self) -> f64 {
        self.bursts
            .iter()
            .map(|born| (-(self.t - born) * DECAY).exp())
            .sum::<f64>()
            .min(1.0)
    }
}

impl Animation for Drift {
    fn advance(&mut self, dt: f64) {
        // The distance integrates the surge, so an event accelerates the field, not teleports it.
        self.dist += dt * (1.0 + SURGE * self.boost());
        self.t += dt;
        // A burst is done once its swarm has crossed and its surge has bled off.
        self.bursts.retain(|born| self.t - born < 6.0);
    }

    fn excite(&mut self) {
        if self.bursts.len() == MAX_BURSTS {
            self.bursts.remove(0);
        }
        self.bursts.push(self.t);
    }

    fn render(&self, width: usize, palette: Palette) -> Vec<String> {
        let cells = WAVE_W.min(width);
        let span = (cells + 8) as f64;
        let mut layers: [Vec<(f64, f64)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for seed in 0..DOTS {
            let layer = seed % 3;
            let travel = self.dist * LAYER_SPEED[layer] + seed as f64 * 7.3;
            let lap = (travel / span).floor();
            let x = (cells + 4) as f64 - travel % span;
            layers[layer].push((x, 1.0 + 8.0 * hash(seed as f64 * 31.7 + lap * 7.77)));
        }
        let mut swarm = Vec::new();
        for born in &self.bursts {
            let age = self.t - born;
            for j in 0..6 {
                let x = (cells + 6) as f64 - age * (26.0 + 7.0 * (j % 3) as f64) - j as f64 * 1.9;
                if x > -4.0 {
                    swarm.push((x, 1.0 + 8.0 * hash(born * 13.3 + j as f64 * 3.1)));
                }
            }
        }
        let near = mix(LAYER_COLOR[2], NEAR_HOT, self.boost());
        canvas_rows(cells, ROWS, [0.0, 10.0], palette, |ctx| {
            for (i, coords) in layers.iter().enumerate() {
                let (r, g, b) = if i == 2 { near } else { LAYER_COLOR[i] };
                ctx.draw(&Points {
                    coords,
                    color: Color::Rgb(r, g, b),
                });
            }
            ctx.draw(&Points {
                coords: &swarm,
                color: Color::Rgb(SWARM.0, SWARM.1, SWARM.2),
            });
        })
    }
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), f: f64) -> (u8, u8, u8) {
    let f = f.clamp(0.0, 1.0);
    let ch = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * f) as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

fn hash(v: f64) -> f64 {
    ((v * 12.9898).sin() * 43758.5453).fract().abs()
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
    fn the_field_is_drawn() {
        let mut d = Drift::new();
        for _ in 0..25 {
            d.advance(0.08);
        }
        let rows = d.render(48, Palette::True);
        assert_eq!(rows.len(), ROWS as usize);
        assert!(dots(&rows) >= 10, "the field is populated");
    }

    #[test]
    fn an_event_hurries_the_field() {
        let (mut calm, mut hurried) = (Drift::new(), Drift::new());
        // Past the launch surge on both.
        for _ in 0..80 {
            calm.advance(0.08);
            hurried.advance(0.08);
        }
        hurried.excite();
        for _ in 0..12 {
            calm.advance(0.08);
            hurried.advance(0.08);
        }
        assert!(
            hurried.dist - calm.dist > 1.0,
            "the surge covers extra ground: {} vs {}",
            hurried.dist,
            calm.dist
        );
    }

    #[test]
    fn bursts_are_capped_and_retire() {
        let mut d = Drift::new();
        for _ in 0..20 {
            d.excite();
        }
        assert_eq!(d.bursts.len(), MAX_BURSTS);
        for _ in 0..90 {
            d.advance(0.08);
            d.render(48, Palette::True);
        }
        assert!(d.bursts.is_empty(), "bursts retire after their sweep");
    }

    #[test]
    fn frames_are_deterministic() {
        let (mut a, mut b) = (Drift::new(), Drift::new());
        for _ in 0..10 {
            a.advance(0.08);
            b.advance(0.08);
        }
        assert_eq!(a.render(48, Palette::True), b.render(48, Palette::True));
    }
}
