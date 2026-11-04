//! Looks at the camera image and decides, for each of the 64 squares, whether it is
//! empty or holds a white or a black piece.
//!
//! Piece types are not recognised: they come from the game state (see `tracker`).
//!
//! Cost matters on the target (Atom N270), so the image is not warped as a whole.
//! A small patch in the middle of each square is sampled directly through the
//! calibration, which is about 4000 pixel reads per frame.
//!
//! How a square looks depends on the lighting, so the classifier is trained at the
//! start of a game from the position the board is known to be in.

use crate::calibration::{Calibration, MARGIN_PX, SQUARE_PX};
use anyhow::{Result, bail};
use image::RgbImage;

/// Samples per side inside each square.
const SAMPLES: usize = 8;
/// Part of the square that is sampled, as a fraction of its side. The middle is
/// used because tall pieces lean over their neighbours in the warped view.
const REGION: (f32, f32) = (0.30, 0.70);
/// Weight of the texture measure against the colour distance.
const STD_WEIGHT: f32 = 1.5;

/// What stands on a square, as far as colour tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Empty,
    White,
    Black,
}

impl Cell {
    const ALL: [Cell; 3] = [Cell::Empty, Cell::White, Cell::Black];

    fn index(self) -> usize {
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

/// What the sampled patch of one square looks like.
#[derive(Clone, Copy, Debug, Default)]
pub struct Features {
    /// Mean red, green and blue.
    pub mean: [f32; 3],
    /// Spread of the brightness: low for a flat square, higher where a piece edge is.
    pub std: f32,
}

impl Features {
    fn vector(&self) -> [f32; 4] {
        [self.mean[0], self.mean[1], self.mean[2], self.std * STD_WEIGHT]
    }
}

/// Where to read the pixels for every square, for one calibration.
pub struct Sampler {
    /// `64 * SAMPLES * SAMPLES` points in camera pixels, square by square.
    points: Vec<(f32, f32)>,
}

impl Sampler {
    pub fn new(calibration: &Calibration) -> Option<Self> {
        // Points are laid out in the warped (top-down) image, then mapped back to
        // the camera image.
        let to_camera = calibration.projection()?.invert();
        let square = SQUARE_PX as f32;
        let margin = MARGIN_PX as f32;
        let mut points = Vec::with_capacity(64 * SAMPLES * SAMPLES);
        for index in 0..64 {
            let (file, rank) = ((index % 8) as f32, (index / 8) as f32);
            let row = 7.0 - rank;
            for j in 0..SAMPLES {
                let v = REGION.0 + (REGION.1 - REGION.0) * (j as f32 + 0.5) / SAMPLES as f32;
                for i in 0..SAMPLES {
                    let u = REGION.0 + (REGION.1 - REGION.0) * (i as f32 + 0.5) / SAMPLES as f32;
                    let x = margin + (file + u) * square;
                    let y = margin + (row + v) * square;
                    points.push(to_camera * (x, y));
                }
            }
        }
        Some(Sampler { points })
    }

    /// Reads the features of all 64 squares.
    pub fn features(&self, image: &RgbImage) -> Vec<Features> {
        let (w, h) = (image.width() as i32, image.height() as i32);
        let per_square = SAMPLES * SAMPLES;
        let mut out = Vec::with_capacity(64);
        for square in self.points.chunks_exact(per_square) {
            let (mut sum, mut lum_sum, mut lum_sq) = ([0.0f32; 3], 0.0f32, 0.0f32);
            for &(x, y) in square {
                let px = image.get_pixel(
                    (x.round() as i32).clamp(0, w - 1) as u32,
                    (y.round() as i32).clamp(0, h - 1) as u32,
                );
                let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
                sum[0] += r;
                sum[1] += g;
                sum[2] += b;
                let lum = 0.299 * r + 0.587 * g + 0.114 * b;
                lum_sum += lum;
                lum_sq += lum * lum;
            }
            let n = per_square as f32;
            let mean_lum = lum_sum / n;
            out.push(Features {
                mean: [sum[0] / n, sum[1] / n, sum[2] / n],
                std: (lum_sq / n - mean_lum * mean_lum).max(0.0).sqrt(),
            });
        }
        out
    }
}

type Centroid = [f32; 4];

/// Tells empty, white and black squares apart, separately on dark and light squares.
#[derive(Clone, Debug)]
pub struct Classifier {
    /// By shade (dark, light), then by cell.
    centroids: [[Option<Centroid>; 3]; 2],
    /// Fallback by cell when a shade had no example.
    pooled: [Option<Centroid>; 3],
}

fn mean_of(vectors: &[[f32; 4]]) -> Option<Centroid> {
    if vectors.is_empty() {
        return None;
    }
    let mut sum = [0.0f32; 4];
    for v in vectors {
        for k in 0..4 {
            sum[k] += v[k];
        }
    }
    let n = vectors.len() as f32;
    Some([sum[0] / n, sum[1] / n, sum[2] / n, sum[3] / n])
}

fn distance(a: &Centroid, b: &Centroid) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
}

impl Classifier {
    /// Learns the look of each kind of square from a frame where the content of
    /// every square is known.
    pub fn learn(features: &[Features], labels: &Grid) -> Result<Classifier> {
        if features.len() != 64 {
            bail!("expected 64 squares, got {}", features.len());
        }
        let mut groups: [[Vec<[f32; 4]>; 3]; 2] = Default::default();
        let mut pooled: [Vec<[f32; 4]>; 3] = Default::default();
        for (square, (f, label)) in features.iter().zip(labels).enumerate() {
            groups[shade(square)][label.index()].push(f.vector());
            pooled[label.index()].push(f.vector());
        }
        let has = |cell: Cell| !pooled[cell.index()].is_empty();
        if !has(Cell::Empty) || (!has(Cell::White) && !has(Cell::Black)) {
            bail!("the position needs empty squares and at least one piece to learn from");
        }

        let mut centroids = [[None; 3]; 2];
        for (s, group) in groups.iter().enumerate() {
            for cell in Cell::ALL {
                centroids[s][cell.index()] = mean_of(&group[cell.index()]);
            }
        }
        let pooled = [mean_of(&pooled[0]), mean_of(&pooled[1]), mean_of(&pooled[2])];
        Ok(Classifier { centroids, pooled })
    }

    /// The most likely content of a square and how sure that is, from 0 to 1.
    pub fn classify(&self, features: &Features, square: usize) -> (Cell, f32) {
        let v = features.vector();
        let mut best: Option<(Cell, f32)> = None;
        let mut second = f32::INFINITY;
        for cell in Cell::ALL {
            let Some(centroid) = self.centroids[shade(square)][cell.index()].or(self.pooled[cell.index()]) else {
                continue;
            };
            let d = distance(&v, &centroid);
            match best {
                Some((_, bd)) if d >= bd => second = second.min(d),
                Some((_, bd)) => {
                    second = bd;
                    best = Some((cell, d));
                }
                None => best = Some((cell, d)),
            }
        }
        let (cell, d) = best.unwrap_or((Cell::Empty, 0.0));
        let confidence = if second.is_finite() { (second - d) / (second + d + 1e-3) } else { 1.0 };
        (cell, confidence.clamp(0.0, 1.0))
    }

    /// Classifies all 64 squares.
    pub fn classify_all(&self, features: &[Features]) -> (Grid, [f32; 64]) {
        let mut grid = [Cell::Empty; 64];
        let mut confidence = [0.0f32; 64];
        for (square, f) in features.iter().enumerate().take(64) {
            let (cell, c) = self.classify(f, square);
            grid[square] = cell;
            confidence[square] = c;
        }
        (grid, confidence)
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
        let classifier = Classifier::learn(&features, &labels).unwrap();
        let (grid, confidence) = classifier.classify_all(&features);
        assert_eq!(grid, labels);
        assert!(confidence.iter().all(|&c| c > 0.3), "{confidence:?}");
    }

    #[test]
    fn reads_a_different_position() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let classifier = Classifier::learn(&sampler.features(&synthetic::frame()), &start).unwrap();

        // After 1. e4 the pawn has left e2 and stands on e4.
        let steps = synthetic::game_steps(&["e4"]);
        let features = sampler.features(&synthetic::frame_from_grid(&steps[0].after));
        let (grid, _) = classifier.classify_all(&features);
        assert_eq!(grid, cell_grid(&steps[0].after));
    }

    #[test]
    fn survives_brightness_change_and_noise() {
        let sampler = Sampler::new(&synthetic::corners()).unwrap();
        let start = cell_grid(&synthetic::start_grid());
        let classifier = Classifier::learn(&sampler.features(&synthetic::frame()), &start).unwrap();

        let mut frame = synthetic::frame();
        synthetic::degrade(&mut frame, -25, 12, 7);
        let (grid, _) = classifier.classify_all(&sampler.features(&frame));
        assert_eq!(grid, start);
    }

    #[test]
    fn refuses_to_learn_without_pieces_or_empty_squares() {
        let features = vec![Features::default(); 64];
        assert!(Classifier::learn(&features, &[Cell::Empty; 64]).is_err());
        assert!(Classifier::learn(&features, &[Cell::White; 64]).is_err());
    }
}
