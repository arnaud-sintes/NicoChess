//! Looks at the camera image and decides, for each of the 64 squares, whether it is
//! empty or holds a white or a black piece.
//!
//! Piece types are not recognised: they come from the game state (see `tracker`).
//!
//! The image is not warped as a whole: a grid of points inside each square is read directly
//! through the calibration (about 9000 pixel reads per frame), and summarised as a few numbers:
//! the colour of the middle of the square, the colour of its edge, and how much texture the
//! middle has. A piece is in the middle; an empty square is the same all over.
//!
//! How a square looks depends on the light, on the square and on the piece, so nothing is fixed
//! in advance. [`Models`] learns, for every square, what it looks like empty, with a white piece
//! and with a black piece, starting from the position the board is known to be in, and goes on
//! learning as the game is followed. What a frame says about a square is a likelihood: how well
//! its numbers fit each of the three possibilities, given what that square has looked like.
// Numbers per feature are kept in parallel arrays: indexing them together reads best.
#![allow(clippy::needless_range_loop)]

use crate::calibration::{Calibration, MARGIN_PX, SQUARE_PX};
use anyhow::{Result, bail};
use image::RgbImage;

/// Samples per side inside each square.
const GRID: usize = 12;
/// A neighbouring square is below another in the picture when the direction to it points down by at least this
/// fraction of its length (0: level, 1: straight down). See [`Sampler::lean_sources`].
const LEAN_BELOW: f32 = 0.5;
/// The part of the square that is read, as a fraction of its side (the border is left out: it
/// holds the neighbours of tall pieces).
const SPAN: (f32, f32) = (0.12, 0.88);
/// The part of the square that counts as its middle; the rest of what is read is the edge.
const MIDDLE: (f32, f32) = (0.30, 0.70);

/// Numbers per square.
pub const DIM: usize = 8;

/// What stands on a square, as far as colour tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Empty,
    White,
    Black,
}

impl Cell {
    pub const ALL: [Cell; 3] = [Cell::Empty, Cell::White, Cell::Black];

    pub fn index(self) -> usize {
        self as usize
    }
}

/// The 64 squares, indexed a1 = 0, b1 = 1, ... h8 = 63.
pub type Grid = [Cell; 64];

/// Dark squares are 0, light squares are 1 (a1 is dark).
pub fn shade(square: usize) -> usize {
    let (file, rank) = (square % 8, square / 8);
    if (file + rank) % 2 == 0 { 0 } else { 1 }
}

/// What one square looks like, as [`DIM`] numbers:
/// 0-2 the red, green and blue of the middle, 3-5 the same for the edge,
/// 6 the spread of the brightness in the middle, 7 how rough the middle is (differences between neighbouring points).
#[derive(Clone, Copy, Debug)]
pub struct Features {
    pub v: [f32; DIM],
}

impl Default for Features {
    fn default() -> Self {
        Features { v: [0.0; DIM] }
    }
}

impl Features {
    /// The colour of the middle: what tells that a hand or a shadow is moving over the board.
    pub fn middle(&self) -> [f32; 3] {
        [self.v[0], self.v[1], self.v[2]]
    }
}

/// Where to read the pixels for every square, for one calibration.
pub struct Sampler {
    /// `64 * GRID * GRID` points in camera pixels, square by square, row by row.
    points: Vec<(f32, f32)>,
    /// For each of the `GRID * GRID` points of a square: is it in the middle?
    middle: Vec<bool>,
}

impl Sampler {
    pub fn new(calibration: &Calibration) -> Option<Self> {
        // Points are laid out in the warped (top-down) image, then mapped back to
        // the camera image.
        let to_camera = calibration.projection()?.invert();
        let square = SQUARE_PX as f32;
        let margin = MARGIN_PX as f32;
        let at = |k: usize| SPAN.0 + (SPAN.1 - SPAN.0) * (k as f32 + 0.5) / GRID as f32;
        let mut points = Vec::with_capacity(64 * GRID * GRID);
        for index in 0..64 {
            let (file, rank) = ((index % 8) as f32, (index / 8) as f32);
            let row = 7.0 - rank;
            for j in 0..GRID {
                for i in 0..GRID {
                    let x = margin + (file + at(i)) * square;
                    let y = margin + (row + at(j)) * square;
                    points.push(to_camera * (x, y));
                }
            }
        }
        let inside = |u: f32| (MIDDLE.0..=MIDDLE.1).contains(&u);
        let middle = (0..GRID * GRID).map(|k| inside(at(k % GRID)) && inside(at(k / GRID))).collect();
        Some(Sampler { points, middle })
    }

    /// For each square, the squares whose piece may lean over it in the picture: the neighbours that lie below it
    /// (toward the bottom of the picture). A camera that looks at the board from the side and low sees the head of a
    /// tall piece above its foot, over the square behind it, so an empty square can show a piece's colour.
    ///
    /// Only the direction in the picture counts, so this holds for any way the board is turned in front of the camera.
    pub fn lean_sources(&self) -> Vec<Vec<u8>> {
        let per_square = GRID * GRID;
        let centres: Vec<(f32, f32)> = self
            .points
            .chunks_exact(per_square)
            .map(|square| {
                let n = per_square as f32;
                (square.iter().map(|p| p.0).sum::<f32>() / n, square.iter().map(|p| p.1).sum::<f32>() / n)
            })
            .collect();
        (0..64usize)
            .map(|square| {
                let (file, rank) = ((square % 8) as i32, (square / 8) as i32);
                let mut sources = Vec::new();
                for (df, dr) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                    let (f, r) = (file + df, rank + dr);
                    if !(0..8).contains(&f) || !(0..8).contains(&r) {
                        continue;
                    }
                    let other = (r * 8 + f) as usize;
                    let (dx, dy) = (centres[other].0 - centres[square].0, centres[other].1 - centres[square].1);
                    let length = dx.hypot(dy);
                    // Below in the picture: the piece stands there and its head reaches up to this square.
                    if length > 0.0 && dy / length > LEAN_BELOW {
                        sources.push(other as u8);
                    }
                }
                sources
            })
            .collect()
    }

    /// Reads the features of all 64 squares.
    pub fn features(&self, image: &RgbImage) -> Vec<Features> {
        let (w, h) = (image.width() as i32, image.height() as i32);
        let per_square = GRID * GRID;
        let mut out = Vec::with_capacity(64);
        for square in self.points.chunks_exact(per_square) {
            // Colour and brightness of every point.
            let mut pixels = [[0.0f32; 4]; GRID * GRID];
            for (k, &(x, y)) in square.iter().enumerate() {
                // Every point is the mean of a small block, not one pixel: the sensor noise of a dim room would
                // otherwise swamp the two features that measure texture (spread and roughness).
                let (cx, cy) = (x.round() as i32, y.round() as i32);
                let mut sum = [0.0f32; 3];
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let px = image.get_pixel((cx + dx).clamp(0, w - 1) as u32, (cy + dy).clamp(0, h - 1) as u32);
                        for c in 0..3 {
                            sum[c] += px[c] as f32;
                        }
                    }
                }
                let (r, g, b) = (sum[0] / 9.0, sum[1] / 9.0, sum[2] / 9.0);
                pixels[k] = [r, g, b, 0.299 * r + 0.587 * g + 0.114 * b];
            }
            let (mut mid, mut edge) = ([0.0f32; 3], [0.0f32; 3]);
            let (mut mid_n, mut edge_n) = (0.0f32, 0.0f32);
            let (mut lum_sum, mut lum_sq) = (0.0f32, 0.0f32);
            for (k, p) in pixels.iter().enumerate() {
                if self.middle[k] {
                    mid_n += 1.0;
                    lum_sum += p[3];
                    lum_sq += p[3] * p[3];
                    for c in 0..3 {
                        mid[c] += p[c];
                    }
                } else {
                    edge_n += 1.0;
                    for c in 0..3 {
                        edge[c] += p[c];
                    }
                }
            }
            let mean_lum = lum_sum / mid_n;
            let spread = (lum_sq / mid_n - mean_lum * mean_lum).max(0.0).sqrt();
            // Roughness: brightness steps between neighbouring points of the middle.
            let (mut steps, mut count) = (0.0f32, 0.0f32);
            for j in 0..GRID {
                for i in 0..GRID {
                    let k = j * GRID + i;
                    if !self.middle[k] {
                        continue;
                    }
                    if i + 1 < GRID && self.middle[k + 1] {
                        steps += (pixels[k][3] - pixels[k + 1][3]).abs();
                        count += 1.0;
                    }
                    if j + 1 < GRID && self.middle[k + GRID] {
                        steps += (pixels[k][3] - pixels[k + GRID][3]).abs();
                        count += 1.0;
                    }
                }
            }
            let mut v = [0.0f32; DIM];
            for c in 0..3 {
                v[c] = mid[c] / mid_n;
                v[3 + c] = edge[c] / edge_n.max(1.0);
            }
            v[6] = spread;
            v[7] = if count > 0.0 { steps / count } else { 0.0 };
            out.push(Features { v });
        }
        out
    }
}

// ---- What a square looks like: models that learn ------------------------------------------------------------

/// How much two pictures of the same thing differ because of the sensor alone, per number.
const NOISE_SD: [f32; DIM] = [5.0, 5.0, 5.0, 6.0, 6.0, 6.0, 3.0, 2.5];
/// The light is never quite the same twice: this fraction of a number is allowed on top of the noise.
const LIGHT_SD: f32 = 0.06;
/// A difference of more than this many standard deviations counts as this many: one odd number (a
/// reflection, a shadow edge) must not decide alone.
const Z2_MAX: f32 = 25.0;
/// How much a picture counts in the models when the board was found to look different from what was
/// learned (the light drifted) or when a person set the position: more than an ordinary confirmed move.
pub const ADAPT_WEIGHT: f32 = 3.0;
/// What a square costs when nothing is known about what it would look like with this content.
const UNKNOWN_NLL: f32 = 80.0;
/// The most a square's own look can weigh against what is learned later (about a window of this many frames).
const WINDOW: f32 = 10.0;
/// How low the spread of a square's look may go, as a fraction of the spread of its kind at the start:
/// pieces of one colour differ (a pawn is not a queen), so a square seen with one piece is not sure for the next.
const SPREAD_FLOOR: [f32; 3] = [0.10, 0.35, 0.35];

#[derive(Clone, Copy, Debug)]
struct Stat {
    mean: [f32; DIM],
    var: [f32; DIM],
    /// How many pictures this stands on (counted with their weights), at most [`WINDOW`].
    weight: f32,
}

fn stat_of(vectors: &[[f32; DIM]]) -> Option<Stat> {
    if vectors.is_empty() {
        return None;
    }
    let n = vectors.len() as f32;
    let mut mean = [0.0f32; DIM];
    for v in vectors {
        for d in 0..DIM {
            mean[d] += v[d] / n;
        }
    }
    let mut var = [0.0f32; DIM];
    for v in vectors {
        for d in 0..DIM {
            var[d] += (v[d] - mean[d]) * (v[d] - mean[d]) / n;
        }
    }
    for d in 0..DIM {
        var[d] = var[d].max(NOISE_SD[d] * NOISE_SD[d]);
    }
    Some(Stat { mean, var, weight: n.min(WINDOW) })
}

/// The cost of seeing `v` where `stat` is expected: the negative log-likelihood, in nats (so costs add up
/// over squares, and a difference of a few nats between two explanations is a real preference).
fn nll_of(stat: &Stat, v: &[f32; DIM]) -> f32 {
    let mut total = 0.0f32;
    for d in 0..DIM {
        // The spread is the learned one plus the light's own uncertainty, which grows with the value.
        let light = LIGHT_SD * stat.mean[d];
        let var = stat.var[d] + light * light;
        let z2 = (v[d] - stat.mean[d]) * (v[d] - stat.mean[d]) / var;
        total += 0.5 * z2.min(Z2_MAX) + 0.5 * var.ln();
    }
    total
}

/// What a picture costs on average when it is drawn from the model itself: the spread terms plus half a nat
/// per number. What a square costs beyond this (or beyond what it has usually cost) is a misfit.
fn expected_nll(stat: &Stat) -> f32 {
    let mut total = 0.0f32;
    for d in 0..DIM {
        let light = LIGHT_SD * stat.mean[d];
        total += 0.5 * (stat.var[d] + light * light).ln() + 0.5;
    }
    total
}

/// How much two looks of the same square differ, in the sum over the numbers of squared differences
/// in units of their spread (each capped, so that one odd number does not decide): about [`DIM`] when nothing
/// changed, far more when a piece came or went. The tracker compares a picture with the last confirmed one this way:
/// a move changes two squares by a lot, whatever the light.
pub fn difference(now: &Features, before: &Features) -> f32 {
    let mut total = 0.0f32;
    for d in 0..DIM {
        let light = LIGHT_SD * before.v[d];
        // Two noisy readings: twice the noise.
        let var = 2.0 * NOISE_SD[d] * NOISE_SD[d] + light * light;
        total += ((now.v[d] - before.v[d]).powi(2) / var).min(Z2_MAX);
    }
    total
}

/// How much a square changed between two pictures taken one after the other: the same as [`difference`], but
/// with only the noise of the sensor as the spread, because the light is the same in both. A white piece leaving
/// a light square is a small change in colour, which the uncertainty of the light would hide.
pub fn frame_change(now: &Features, before: &Features) -> f32 {
    let mut total = 0.0f32;
    for d in 0..DIM {
        let var = 2.0 * NOISE_SD[d] * NOISE_SD[d];
        total += ((now.v[d] - before.v[d]).powi(2) / var).min(Z2_MAX);
    }
    total
}

/// What every square looks like empty, with a white piece and with a black piece, as learned so far.
#[derive(Clone, Debug)]
pub struct Models {
    /// What each square looked like with each content, where it has been seen so.
    squares: Vec<[Option<Stat>; 3]>,
    /// The look of each content on dark and light squares in general: what a square falls back on
    /// for a content it has not been seen with.
    prior: [[Option<Stat>; 3]; 2],
    /// The same for all squares.
    pooled: [Option<Stat>; 3],
    /// The lowest spread allowed, by shade and content.
    floor: [[[f32; DIM]; 3]; 2],
    /// What each square usually costs, by content: more than that is unexplained. It starts as what the
    /// model itself predicts, and follows what the square has really cost since.
    typical: Vec<[f32; 3]>,
}

impl Models {
    /// Learns from a picture where the content of every square is known.
    pub fn learn(features: &[Features], labels: &Grid) -> Result<Models> {
        if features.len() != 64 {
            bail!("expected 64 squares, got {}", features.len());
        }
        let mut groups: [[Vec<[f32; DIM]>; 3]; 2] = Default::default();
        let mut all: [Vec<[f32; DIM]>; 3] = Default::default();
        for (square, (f, label)) in features.iter().zip(labels).enumerate() {
            groups[shade(square)][label.index()].push(f.v);
            all[label.index()].push(f.v);
        }
        let has = |cell: Cell| !all[cell.index()].is_empty();
        if !has(Cell::Empty) || (!has(Cell::White) && !has(Cell::Black)) {
            bail!("the position needs empty squares and at least one piece to learn from");
        }

        let pooled = [stat_of(&all[0]), stat_of(&all[1]), stat_of(&all[2])];
        let mut prior = [[None; 3]; 2];
        let mut floor = [[[0.0f32; DIM]; 3]; 2];
        for s in 0..2 {
            for cell in Cell::ALL {
                let c = cell.index();
                // A handful of squares is not enough to tell a spread: use the one of all squares.
                prior[s][c] = if groups[s][c].len() >= 4 { stat_of(&groups[s][c]) } else { pooled[c] };
                if let Some(p) = prior[s][c] {
                    for d in 0..DIM {
                        floor[s][c][d] = (SPREAD_FLOOR[c] * p.var[d]).max(NOISE_SD[d] * NOISE_SD[d]);
                    }
                }
            }
        }
        let mut models = Models { squares: vec![[None; 3]; 64], prior, pooled, floor, typical: vec![[0.0; 3]; 64] };
        // Each square starts from what it is seen to be, pulled a little toward the look of its kind.
        for (square, (f, label)) in features.iter().zip(labels).enumerate() {
            let c = label.index();
            if let Some(p) = models.prior[shade(square)][c] {
                let mut mean = [0.0f32; DIM];
                for d in 0..DIM {
                    mean[d] = 0.25 * p.mean[d] + 0.75 * f.v[d];
                }
                models.squares[square][c] = Some(Stat { mean, var: p.var, weight: 1.0 });
            }
        }
        for square in 0..64 {
            for cell in Cell::ALL {
                models.typical[square][cell.index()] = models.stat(square, cell).map_or(UNKNOWN_NLL, |stat| expected_nll(&stat));
            }
        }
        Ok(models)
    }

    /// The model of a square for a content: its own, else the one of its kind (less sure of itself).
    fn stat(&self, square: usize, cell: Cell) -> Option<Stat> {
        let c = cell.index();
        self.squares[square][c].or_else(|| {
            self.prior[shade(square)][c].or(self.pooled[c]).map(|mut p| {
                for d in 0..DIM {
                    p.var[d] *= 2.0;
                }
                p.weight = 0.0;
                p
            })
        })
    }

    /// What a square costs for each content: `[empty, white, black]`, in nats.
    pub fn costs(&self, features: &[Features]) -> Vec<[f32; 3]> {
        features
            .iter()
            .enumerate()
            .take(64)
            .map(|(square, f)| {
                let mut out = [UNKNOWN_NLL; 3];
                for cell in Cell::ALL {
                    if let Some(stat) = self.stat(square, cell) {
                        out[cell.index()] = nll_of(&stat, &f.v);
                    }
                }
                out
            })
            .collect()
    }

    /// What each square is most likely to hold.
    pub fn read(costs: &[[f32; 3]]) -> Grid {
        let mut grid = [Cell::Empty; 64];
        for (square, c) in costs.iter().enumerate().take(64) {
            let mut best = 0;
            for k in 1..3 {
                if c[k] < c[best] {
                    best = k;
                }
            }
            grid[square] = Cell::ALL[best];
        }
        grid
    }

    /// What each square usually costs, by content (see the field).
    pub fn typical(&self) -> &[[f32; 3]] {
        &self.typical
    }

    /// Corrects a change of the light that is the same all over the board: how much brighter or darker, and how
    /// much redder or bluer, the squares are than they were when learned. The estimate is the median over the
    /// squares, with the content the game says they have, so a few moved pieces do not matter. (A shadow over
    /// part of the board is not corrected here: the models follow it, by learning from every confirmed move.)
    /// `features` are corrected in place. Returns the factor found for red, green and blue.
    pub fn compensate_light(&self, features: &mut [Features], expected: &Grid) -> [f32; 3] {
        let mut ratios: [Vec<f32>; 3] = Default::default();
        for (square, f) in features.iter().enumerate().take(64) {
            let Some(stat) = self.stat(square, expected[square]) else { continue };
            for c in 0..3 {
                if stat.mean[c] > 15.0 {
                    ratios[c].push(f.v[c] / stat.mean[c]);
                }
            }
        }
        let mut gain = [1.0f32; 3];
        for c in 0..3 {
            if ratios[c].len() >= 12 {
                ratios[c].sort_by(f32::total_cmp);
                gain[c] = ratios[c][ratios[c].len() / 2].clamp(0.4, 2.5);
            }
        }
        let brightness = 0.299 * gain[0] + 0.587 * gain[1] + 0.114 * gain[2];
        for f in features.iter_mut() {
            for c in 0..3 {
                f.v[c] /= gain[c];
                f.v[3 + c] /= gain[c];
            }
            f.v[6] /= brightness;
            f.v[7] /= brightness;
        }

        gain
    }

    /// Learns from a picture where the content of every square is known (the position the game has
    /// reached). `weight` says how much this picture counts: 1 for an ordinary confirmed move, more for
    /// a position the person has set by hand.
    pub fn update(&mut self, features: &[Features], labels: &Grid, weight: f32) {
        self.update_except(features, labels, weight, &[]);
    }

    /// The same, but the squares marked in `skip` (a hand over them, a picture that does not fit what they should
    /// show) are left as they are: what a hand looks like must not become the look of a square.
    pub fn update_except(&mut self, features: &[Features], labels: &Grid, weight: f32, skip: &[bool]) {
        for square in 0..64.min(features.len()) {
            if skip.get(square).copied().unwrap_or(false) {
                continue;
            }
            let cell = labels[square];
            let c = cell.index();
            let v = features[square].v;
            let s = shade(square);
            let Some(mut stat) = self.stat(square, cell) else { continue };
            // What this picture cost, before it is learned: an honest measure of what the square usually costs.
            let honest = nll_of(&stat, &v);
            let alpha = weight / (stat.weight + weight);
            for d in 0..DIM {
                stat.mean[d] += alpha * (v[d] - stat.mean[d]);
                let r = v[d] - stat.mean[d];
                stat.var[d] = ((1.0 - alpha) * stat.var[d] + alpha * r * r).max(self.floor[s][c][d]);
            }
            stat.weight = (stat.weight + weight).min(WINDOW);
            // A picture that was confirmed by a person (a heavy weight) is the look from now on: the cost
            // after learning it. An ordinary one is judged as it was before: what the square might do again.
            let sample = if weight >= ADAPT_WEIGHT { nll_of(&stat, &v) } else { honest };
            let follow = weight / (weight + 4.0);
            self.typical[square][c] = (1.0 - follow) * self.typical[square][c] + follow * sample;
            self.squares[square][c] = Some(stat);
            // The look of this kind of square follows, slowly: it is what an unseen square falls back on.
            if let Some(p) = self.prior[s][c].as_mut() {
                let a = (0.05 * weight).min(0.5);
                for d in 0..DIM {
                    p.mean[d] += a * (v[d] - p.mean[d]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic::{self, PieceGrid};
    use shakmaty::Color;

    fn cell_grid(pieces: &PieceGrid) -> Grid {
        let mut grid = [Cell::Empty; 64];
        for (i, p) in pieces.iter().enumerate() {
            grid[i] = match p {
                None => Cell::Empty,
                Some(Color::White) => Cell::White,
                Some(Color::Black) => Cell::Black,
            };
        }
        grid
    }

    #[test]
    fn shades_match_the_board() {
        assert_eq!(shade(0), 0, "a1 is dark");
        assert_eq!(shade(7), 1, "h1 is light");
        assert_eq!(shade(63), 0, "h8 is dark");
        assert_eq!(shade(56), 1, "a8 is light");
    }

    #[test]
    fn learns_the_start_position_and_reads_it_back() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let labels = cell_grid(&synthetic::start_grid());
        let features = sampler.features(&synthetic::frame());
        let models = Models::learn(&features, &labels).unwrap();
        let costs = models.costs(&features);
        assert_eq!(Models::read(&costs), labels);
        // What it was learned from costs little: the square's own content is far cheaper than the others.
        for (square, c) in costs.iter().enumerate() {
            let own = c[labels[square].index()];
            let other = c.iter().enumerate().filter(|(k, _)| *k != labels[square].index()).map(|(_, v)| *v).fold(f32::MAX, f32::min);
            assert!(other - own > 10.0, "square {square}: {c:?}");
        }
    }

    #[test]
    fn reads_a_different_position() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let models = Models::learn(&sampler.features(&synthetic::frame()), &start).unwrap();

        // After 1. e4 the pawn has left e2 and stands on e4.
        let steps = synthetic::game_steps(&["e4"]);
        let features = sampler.features(&synthetic::frame_from_grid(&steps[0].after));
        assert_eq!(Models::read(&models.costs(&features)), cell_grid(&steps[0].after));
    }

    #[test]
    fn survives_brightness_change_and_noise() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let models = Models::learn(&sampler.features(&synthetic::frame()), &start).unwrap();

        let mut frame = synthetic::frame();
        synthetic::degrade(&mut frame, -25, 12, 7);
        // As in the tracker: a change of the light is compensated before the squares are read.
        let mut features = sampler.features(&frame);
        models.compensate_light(&mut features, &start);
        assert_eq!(Models::read(&models.costs(&features)), start);
    }

    #[test]
    fn a_change_of_the_light_all_over_the_board_is_compensated() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let models = Models::learn(&sampler.features(&synthetic::frame()), &start).unwrap();
        // Much darker and warmer: without compensation the squares no longer look like what was learned.
        let dark = Conditions { final_gain: 0.6, final_warmth: 0.12, ..Conditions::EASY };
        let mut features = sampler.features(&synthetic::frame_from_position(&shakmaty::Chess::default(), &dark, 1.0, 1));
        let gain = models.compensate_light(&mut features, &start);
        assert!(gain[1] < 0.7 && gain[0] > gain[2], "{gain:?}");
        assert_eq!(Models::read(&models.costs(&features)), start);
    }

    use crate::synthetic::Conditions;

    #[test]
    fn learning_from_new_pictures_lowers_what_a_square_costs() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let mut models = Models::learn(&sampler.features(&synthetic::frame()), &start).unwrap();
        // A changed look that stays (a glare spot, say).
        let glare = Conditions { glare: 0.5, glare_square: shakmaty::Square::D4, ..Conditions::EASY };
        let features = sampler.features(&synthetic::frame_from_position(&shakmaty::Chess::default(), &glare, 0.0, 1));
        let before = models.costs(&features)[27][0];
        for _ in 0..4 {
            models.update(&features, &start, 1.0);
        }
        let after = models.costs(&features)[27][0];
        assert!(after < before - 5.0, "{before} -> {after}");
    }

    #[test]
    fn refuses_to_learn_without_pieces_or_empty_squares() {
        let features = vec![Features::default(); 64];
        assert!(Models::learn(&features, &[Cell::Empty; 64]).is_err());
        assert!(Models::learn(&features, &[Cell::White; 64]).is_err());
    }
}
