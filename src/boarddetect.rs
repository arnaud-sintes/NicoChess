//! Finds the chessboard in a camera picture by itself.
//!
//! The 49 inner corners of a board (where four squares meet) look like saddle points
//! in the picture. They are found first; then the lattice they form is fitted with a
//! homography, which gives the four outer corners of the board by extrapolation.
//!
//! * [`find_board`] searches the whole picture (the "auto-detect" button).
//! * [`refine`] starts from a known calibration and only looks for the inner corners
//!   near where they should be: cheap enough to run during a game, to follow a camera
//!   that was moved a little.
//!
//! No OpenCV: only the `image` crate and a small amount of arithmetic.

use crate::calibration::Calibration;
use anyhow::{Result, bail};
use image::{RgbImage, imageops};
use std::collections::{HashMap, HashSet};

/// The picture is reduced to this width before it is searched.
const WORK_WIDTH: u32 = 640;
/// Inner corners on one side of a board.
const N: i32 = 7;
/// Lowest [`Found::alternation`] accepted.
const MIN_ALTERNATION: f32 = 0.4;
/// Fewest inner corners (of 49) that must be matched.
const MIN_MATCHED: usize = 24;

#[derive(Clone, Copy, Debug)]
struct Saddle {
    x: f32,
    y: f32,
    strength: f32,
}

/// The board found in a picture.
#[derive(Debug, Clone)]
pub struct Found {
    /// a1, h1, h8, a8 in picture pixels.
    pub corners: [[f32; 2]; 4],
    /// Inner corners matched, out of 49.
    pub matched: usize,
    /// True when the side of White was read from the colours of the pieces. False when
    /// that was not clear and a1 was put at the corner nearest the bottom left.
    pub sides_from_pieces: bool,
    /// 0..=1: how consistently the squares alternate light and dark around the inner corners
    /// (1 for a real board). A board seen in a picture that is not one scores low.
    pub alternation: f32,
}

// ---- Homography -----------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Homography {
    h: [f64; 8],
}

type Pair = ((f64, f64), (f64, f64));

/// Saddles matched to the places of a lattice, by lattice coordinates (i, j).
type Matches = HashMap<(i32, i32), usize>;
/// A lattice found: its matches and the map from lattice to picture.
type Lattice = (Matches, Homography);
/// The best lattice seen so far: matches, origin of its 7x7 window, how many it holds, their strength.
type BestWindow = (Matches, (i32, i32), usize, f32);
/// An outer corner of the board: place in the lattice, place in the picture, is its square dark.
type RingCorner = ((f64, f64), (f64, f64), bool);

impl Homography {
    /// Least squares fit of a projective map from lattice (u, v) to picture (x, y).
    fn fit(pairs: &[Pair]) -> Option<Homography> {
        if pairs.len() < 4 {
            return None;
        }
        let mut ata = [[0f64; 8]; 8];
        let mut atb = [0f64; 8];
        {
            let mut add = |row: [f64; 8], rhs: f64| {
                for i in 0..8 {
                    for j in 0..8 {
                        ata[i][j] += row[i] * row[j];
                    }
                    atb[i] += row[i] * rhs;
                }
            };
            for &((u, v), (x, y)) in pairs {
                let (x, y) = (x / 100.0, y / 100.0);
                add([u, v, 1.0, 0.0, 0.0, 0.0, -x * u, -x * v], x);
                add([0.0, 0.0, 0.0, u, v, 1.0, -y * u, -y * v], y);
            }
        }
        let h = solve(ata, atb)?;
        Some(Homography { h })
    }

    fn apply(&self, u: f64, v: f64) -> (f64, f64) {
        let h = &self.h;
        let w = h[6] * u + h[7] * v + 1.0;
        ((h[0] * u + h[1] * v + h[2]) / w * 100.0, (h[3] * u + h[4] * v + h[5]) / w * 100.0)
    }

    /// Distance in the picture between neighbouring lattice points near (u, v).
    fn spacing(&self, u: f64, v: f64) -> f64 {
        let p = self.apply(u, v);
        let a = self.apply(u + 1.0, v);
        let b = self.apply(u, v + 1.0);
        let d = |q: (f64, f64)| (q.0 - p.0).hypot(q.1 - p.1);
        d(a).min(d(b))
    }
}

/// Gaussian elimination with partial pivoting.
#[allow(clippy::needless_range_loop)] // plain numerical code: indexing two matrices at once reads best
fn solve(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    for col in 0..8 {
        let pivot = (col..8).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-9 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..8 {
            let f = a[row][col] / a[col][col];
            for k in col..8 {
                a[row][k] -= f * a[col][k];
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0f64; 8];
    for row in (0..8).rev() {
        let mut sum = b[row];
        for k in row + 1..8 {
            sum -= a[row][k] * x[k];
        }
        x[row] = sum / a[row][row];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

// ---- Saddle points ----------------------------------------------------------------

/// A picture reduced and smoothed, with its saddle points.
struct Prepared {
    w: usize,
    h: usize,
    blurred: Vec<f32>,
    saddles: Vec<Saddle>,
    /// Work picture pixels per original pixel.
    scale: f32,
}

fn prepare(image: &RgbImage) -> Result<Prepared> {
    let mut gray = imageops::grayscale(image);
    let mut scale = 1.0;
    if gray.width() > WORK_WIDTH {
        scale = WORK_WIDTH as f32 / gray.width() as f32;
        let height = ((gray.height() as f32 * scale).round() as u32).max(1);
        gray = imageops::resize(&gray, WORK_WIDTH, height, imageops::FilterType::Triangle);
    }
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    if w < 64 || h < 64 {
        bail!("the picture is too small");
    }
    let float: Vec<f32> = gray.as_raw().iter().map(|&v| f32::from(v)).collect();
    let blurred = blur(&float, w, h, 1.6);
    let saddles = find_saddles(&blurred, w, h);
    Ok(Prepared { w, h, blurred, saddles, scale })
}

fn blur(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil() as isize;
    let kernel: Vec<f32> = (-r..=r).map(|i| (-((i * i) as f32) / (2.0 * sigma * sigma)).exp()).collect();
    let total: f32 = kernel.iter().sum();
    let kernel: Vec<f32> = kernel.iter().map(|k| k / total).collect();
    let mut tmp = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let xx = (x as isize + k as isize - r).clamp(0, w as isize - 1) as usize;
                sum += weight * src[y * w + xx];
            }
            tmp[y * w + x] = sum;
        }
    }
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let yy = (y as isize + k as isize - r).clamp(0, h as isize - 1) as usize;
                sum += weight * tmp[yy * w + x];
            }
            out[y * w + x] = sum;
        }
    }
    out
}

/// Where four squares meet, the picture is a saddle: its Hessian has one positive and
/// one negative eigenvalue, so `Ixy^2 - Ixx*Iyy` is large and positive. Edges, blobs and
/// flat areas give zero or negative values.
fn find_saddles(img: &[f32], w: usize, h: usize) -> Vec<Saddle> {
    const D: usize = 2;
    const RADIUS: usize = 4;
    let margin = D + 1;
    let mut response = vec![0f32; w * h];
    let mut top = 0f32;
    for y in margin..h - margin {
        for x in margin..w - margin {
            let at = |dx: isize, dy: isize| img[(y as isize + dy) as usize * w + (x as isize + dx) as usize];
            let d = D as isize;
            let c = at(0, 0);
            let ixx = at(d, 0) - 2.0 * c + at(-d, 0);
            let iyy = at(0, d) - 2.0 * c + at(0, -d);
            let ixy = (at(d, d) - at(d, -d) - at(-d, d) + at(-d, -d)) / 4.0;
            let r = ixy * ixy - ixx * iyy;
            response[y * w + x] = r;
            top = top.max(r);
        }
    }
    if top <= 0.0 {
        return Vec::new();
    }
    let floor = top * 0.01;
    let mut found = Vec::new();
    for y in margin + RADIUS..h.saturating_sub(margin + RADIUS) {
        for x in margin + RADIUS..w.saturating_sub(margin + RADIUS) {
            let r = response[y * w + x];
            if r <= floor {
                continue;
            }
            let mut is_max = true;
            'window: for yy in y - RADIUS..=y + RADIUS {
                for xx in x - RADIUS..=x + RADIUS {
                    let other = response[yy * w + xx];
                    if other > r || (other == r && (yy, xx) < (y, x)) {
                        is_max = false;
                        break 'window;
                    }
                }
            }
            if !is_max {
                continue;
            }
            // Parabola through the neighbours: position to a fraction of a pixel.
            let offset = |a: f32, b: f32, c: f32| {
                let denominator = a - 2.0 * b + c;
                if denominator.abs() < 1e-9 { 0.0 } else { (0.5 * (a - c) / denominator).clamp(-0.5, 0.5) }
            };
            let dx = offset(response[y * w + x - 1], r, response[y * w + x + 1]);
            let dy = offset(response[(y - 1) * w + x], r, response[(y + 1) * w + x]);
            found.push(Saddle { x: x as f32 + dx, y: y as f32 + dy, strength: r });
        }
    }
    found.sort_by(|a, b| b.strength.total_cmp(&a.strength));
    // Keep the clear ones: a board gives at least 49 of about the same strength.
    let reference = found.get(30).or(found.last()).map_or(0.0, |s| s.strength);
    found.retain(|s| s.strength >= 0.12 * reference);
    found.truncate(500);
    found
}

fn bilinear(img: &[f32], w: usize, h: usize, x: f64, y: f64) -> Option<f32> {
    if x < 0.0 || y < 0.0 || x > (w - 1) as f64 || y > (h - 1) as f64 {
        return None;
    }
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
    let top = img[y0 * w + x0] * (1.0 - fx) + img[y0 * w + x1] * fx;
    let bottom = img[y1 * w + x0] * (1.0 - fx) + img[y1 * w + x1] * fx;
    Some(top * (1.0 - fy) + bottom * fy)
}

// ---- Lattice ------------------------------------------------------------------------

fn nearest(points: &[Saddle], at: (f64, f64), tolerance: f64, used: &HashSet<usize>) -> Option<usize> {
    let mut best = None;
    let mut best_distance = tolerance;
    for (k, s) in points.iter().enumerate() {
        if used.contains(&k) {
            continue;
        }
        let d = (f64::from(s.x) - at.0).hypot(f64::from(s.y) - at.1);
        if d < best_distance {
            best_distance = d;
            best = Some(k);
        }
    }
    best
}

fn pairs_of(points: &[Saddle], matches: &HashMap<(i32, i32), usize>, shift: (i32, i32)) -> Vec<Pair> {
    matches
        .iter()
        .map(|(&(i, j), &k)| (((i - shift.0) as f64, (j - shift.1) as f64), (f64::from(points[k].x), f64::from(points[k].y))))
        .collect()
}

/// Drops the matches that the fitted map does not explain, and fits again.
fn prune(points: &[Saddle], matches: &mut HashMap<(i32, i32), usize>, mut h: Homography) -> Option<Homography> {
    for _ in 0..2 {
        let spacing = h.spacing(0.0, 0.0).max(h.spacing(3.0, 3.0));
        let bad: Vec<(i32, i32)> = matches
            .iter()
            .filter(|&(&(i, j), &k)| {
                let (x, y) = h.apply(f64::from(i), f64::from(j));
                (x - f64::from(points[k].x)).hypot(y - f64::from(points[k].y)) > 0.2 * spacing
            })
            .map(|(key, _)| *key)
            .collect();
        if bad.is_empty() {
            break;
        }
        for key in bad {
            matches.remove(&key);
        }
        h = Homography::fit(&pairs_of(points, matches, (0, 0)))?;
    }
    Some(h)
}

/// Grows a lattice from a seed saddle and two neighbours that give its two directions.
fn grow(points: &[Saddle], seed: usize, a: (f64, f64), b: (f64, f64)) -> Option<Lattice> {
    let s = points[seed];
    let spacing = a.0.hypot(a.1).min(b.0.hypot(b.1));
    let mut matches = HashMap::from([((0, 0), seed)]);
    let mut used = HashSet::from([seed]);
    for i in -1i32..=1 {
        for j in -1i32..=1 {
            if (i, j) == (0, 0) {
                continue;
            }
            let at = (f64::from(s.x) + f64::from(i) * a.0 + f64::from(j) * b.0, f64::from(s.y) + f64::from(i) * a.1 + f64::from(j) * b.1);
            if let Some(k) = nearest(points, at, 0.3 * spacing, &used) {
                matches.insert((i, j), k);
                used.insert(k);
            }
        }
    }
    if matches.len() < 6 {
        return None;
    }
    let mut h = Homography::fit(&pairs_of(points, &matches, (0, 0)))?;
    for ring in 2i32..=8 {
        let mut added = 0;
        for i in -ring..=ring {
            for j in -ring..=ring {
                if i.abs().max(j.abs()) != ring {
                    continue;
                }
                let local = h.spacing(f64::from(i), f64::from(j));
                if !local.is_finite() || local < 4.0 {
                    continue;
                }
                let at = h.apply(f64::from(i), f64::from(j));
                if let Some(k) = nearest(points, at, 0.3 * local, &used) {
                    matches.insert((i, j), k);
                    used.insert(k);
                    added += 1;
                }
            }
        }
        if added == 0 {
            break;
        }
        h = Homography::fit(&pairs_of(points, &matches, (0, 0)))?;
    }
    let h = prune(points, &mut matches, h)?;
    Some((matches, h))
}

/// The 7x7 window of the lattice that holds most matches (ties: the strongest).
fn best_window(points: &[Saddle], matches: &HashMap<(i32, i32), usize>) -> Option<((i32, i32), usize, f32)> {
    let min_i = matches.keys().map(|k| k.0).min()?;
    let max_i = matches.keys().map(|k| k.0).max()?;
    let min_j = matches.keys().map(|k| k.1).min()?;
    let max_j = matches.keys().map(|k| k.1).max()?;
    let mut best: Option<((i32, i32), usize, f32)> = None;
    for i0 in min_i..=max_i {
        for j0 in min_j..=max_j {
            let (mut count, mut strength) = (0, 0f32);
            for (&(i, j), &k) in matches {
                if (i0..i0 + N).contains(&i) && (j0..j0 + N).contains(&j) {
                    count += 1;
                    strength += points[k].strength;
                }
            }
            if best.is_none_or(|(_, c, s)| count > c || (count == c && strength > s)) {
                best = Some(((i0, j0), count, strength));
            }
        }
    }
    best
}

/// True when the matches cover at least five rows and five columns of the window.
fn spread_enough(matches: &HashMap<(i32, i32), usize>) -> bool {
    let rows: HashSet<i32> = matches.keys().map(|k| k.1).collect();
    let cols: HashSet<i32> = matches.keys().map(|k| k.0).collect();
    rows.len() >= 5 && cols.len() >= 5
}

/// Fits the 7x7 window, matches the corners that are still missing, and fits again.
/// Keys of the result are window coordinates, 0..=6.
fn settle_window(points: &[Saddle], window: &HashMap<(i32, i32), usize>) -> Option<Lattice> {
    let mut matches = window.clone();
    let mut h = Homography::fit(&pairs_of(points, &matches, (0, 0)))?;
    for _ in 0..2 {
        let mut used: HashSet<usize> = matches.values().copied().collect();
        for u in 0..N {
            for v in 0..N {
                if matches.contains_key(&(u, v)) {
                    continue;
                }
                let local = h.spacing(f64::from(u), f64::from(v));
                if !local.is_finite() || local < 4.0 {
                    continue;
                }
                let at = h.apply(f64::from(u), f64::from(v));
                if let Some(k) = nearest(points, at, 0.25 * local, &used) {
                    matches.insert((u, v), k);
                    used.insert(k);
                }
            }
        }
        h = Homography::fit(&pairs_of(points, &matches, (0, 0)))?;
        h = prune(points, &mut matches, h)?;
    }
    let fitted = Homography::fit(&pairs_of(points, &matches, (0, 0)))?;
    Some((matches, fitted))
}

/// Root mean square distance between the matched saddles and where the map puts them,
/// in squares.
fn residual(points: &[Saddle], matches: &HashMap<(i32, i32), usize>, h: &Homography) -> f64 {
    let spacing = h.spacing(3.0, 3.0);
    let sum: f64 = matches
        .iter()
        .map(|(&(i, j), &k)| {
            let (x, y) = h.apply(f64::from(i), f64::from(j));
            (x - f64::from(points[k].x)).powi(2) + (y - f64::from(points[k].y)).powi(2)
        })
        .sum();
    (sum / matches.len() as f64).sqrt() / spacing
}

// ---- Searching the whole picture ----------------------------------------------------------

/// Finds the board anywhere in the picture. Corners come back as a1, h1, h8, a8.
pub fn find_board(image: &RgbImage) -> Result<Found> {
    let prepared = prepare(image)?;
    let points = &prepared.saddles;
    if points.len() < MIN_MATCHED {
        bail!("no chessboard pattern: not enough corners between squares (is the picture sharp and the board in view?)");
    }

    let mut best: Option<BestWindow> = None;
    'seeds: for seed in 0..points.len().min(150) {
        let s = points[seed];
        let mut near: Vec<(f64, usize)> = points
            .iter()
            .enumerate()
            .filter(|&(k, _)| k != seed)
            .map(|(k, p)| ((f64::from(p.x - s.x)).hypot(f64::from(p.y - s.y)), k))
            .filter(|&(d, _)| d >= 5.0 && d < f64::from(prepared.w as u32) * 0.5)
            .collect();
        near.sort_by(|a, b| a.0.total_cmp(&b.0));
        near.truncate(6);
        for x in 0..near.len() {
            for y in x + 1..near.len() {
                let (p, q) = (points[near[x].1], points[near[y].1]);
                let a = (f64::from(p.x - s.x), f64::from(p.y - s.y));
                let b = (f64::from(q.x - s.x), f64::from(q.y - s.y));
                let sine = (a.0 * b.1 - a.1 * b.0).abs() / (near[x].0 * near[y].0);
                if sine < 0.5 {
                    continue;
                }
                let Some((matches, _)) = grow(points, seed, a, b) else { continue };
                let Some((origin, count, strength)) = best_window(points, &matches) else { continue };
                if best.as_ref().is_none_or(|(_, _, c, st)| count > *c || (count == *c && strength > *st)) {
                    best = Some((matches, origin, count, strength));
                }
                if best.as_ref().is_some_and(|(_, _, c, _)| *c >= 46) {
                    break 'seeds;
                }
            }
        }
    }

    let Some((matches, origin, count, _)) = best else {
        bail!("no chessboard pattern found: make sure the whole board is in view, in focus and well lit");
    };
    if count < MIN_MATCHED {
        bail!(
            "only {count} of the 49 inner corners of the board could be seen (need {MIN_MATCHED}): show the whole board, in focus and well lit"
        );
    }
    let window: HashMap<(i32, i32), usize> = matches
        .iter()
        .filter(|&(&(i, j), _)| (origin.0..origin.0 + N).contains(&i) && (origin.1..origin.1 + N).contains(&j))
        .map(|(&(i, j), &k)| ((i - origin.0, j - origin.1), k))
        .collect();
    if !spread_enough(&window) {
        bail!("the corners found are in too few rows or columns to be a board");
    }
    let (window, h) = settle_window(points, &window).ok_or_else(|| anyhow::anyhow!("the corners found do not form a grid"))?;
    if window.len() < MIN_MATCHED || !spread_enough(&window) {
        bail!("only {} of the 49 inner corners of the board could be seen: show the whole board, in focus and well lit", window.len());
    }
    let error = residual(points, &window, &h);
    if error > 0.12 {
        bail!("the corners found do not line up well enough to be a board (error {error:.2} squares)");
    }
    label(&prepared, &window, &h, image)
}

// ---- Which corner is a1? -----------------------------------------------------------------------

/// Outer corners of the board in lattice coordinates (u, v): the lattice of inner corners is
/// 0..=6, so the board runs from -1 to 7.
const OUTER: [(f64, f64); 4] = [(-1.0, -1.0), (7.0, -1.0), (7.0, 7.0), (-1.0, 7.0)];

fn label(prepared: &Prepared, window: &HashMap<(i32, i32), usize>, h: &Homography, image: &RgbImage) -> Result<Found> {
    let (w, hh) = (prepared.w, prepared.h);
    let grey = |u: f64, v: f64| {
        let (x, y) = h.apply(u, v);
        bilinear(&prepared.blurred, w, hh, x, y)
    };

    // Which parity of squares is the lighter one? At each inner corner the two diagonal pairs of
    // squares differ: a patch near the corner in each of the four squares tells which pair is
    // lighter, and the sign alternates from one corner to the next.
    let (mut vote, mut total) = (0f64, 0f64);
    for u in 0..N {
        for v in 0..N {
            let (fu, fv) = (f64::from(u), f64::from(v));
            let patch = |du: f64, dv: f64| grey(fu + du, fv + dv);
            let (Some(a), Some(b), Some(c), Some(d)) = (patch(0.25, 0.25), patch(-0.25, -0.25), patch(-0.25, 0.25), patch(0.25, -0.25))
            else {
                continue;
            };
            let s = f64::from(a + b - c - d);
            vote += if (u + v) % 2 == 0 { s } else { -s };
            total += s.abs();
        }
    }
    // vote > 0: the squares whose coordinates (p + q) are even are the lighter ones.
    let even_dark = vote < 0.0;

    // The outer corners in order around the board; counter-clockwise on screen (the order that
    // a1, h1, h8, a8 have from any side). Each remembers its place in the lattice and whether the
    // square in that corner is dark.
    let mut ring: Vec<RingCorner> = OUTER
        .iter()
        .enumerate()
        .map(|(n, &uv)| {
            // Corner squares (p, q): (0,0) even, (7,0) odd, (7,7) even, (0,7) odd.
            let even = n % 2 == 0;
            (uv, h.apply(uv.0, uv.1), even == even_dark)
        })
        .collect();
    let cross = (ring[1].1.0 - ring[0].1.0) * (ring[2].1.1 - ring[1].1.1) - (ring[1].1.1 - ring[0].1.1) * (ring[2].1.0 - ring[1].1.0);
    if cross > 0.0 {
        ring.swap(1, 3);
    }

    // Candidates for a1: the two dark corners, opposite each other. Compare the pieces on the
    // two sides next to the corners: White's are the lighter ones.
    let k = if ring[0].2 { 0 } else { 1 };
    let band = |from: usize| -> Option<f32> {
        let origin = ring[from % 4].0;
        let along = ring[(from + 1) % 4].0;
        let inward = ring[(from + 3) % 4].0;
        let (mut sum, mut count) = (0f32, 0);
        for step in 0..8 {
            for depth in [0.5, 1.5] {
                let f = (f64::from(step) + 0.5) / 8.0;
                let g = depth / 8.0;
                let u = origin.0 + f * (along.0 - origin.0) + g * (inward.0 - origin.0);
                let v = origin.1 + f * (along.1 - origin.1) + g * (inward.1 - origin.1);
                if let Some(value) = grey(u, v) {
                    sum += value;
                    count += 1;
                }
            }
        }
        (count >= 8).then(|| sum / count as f32)
    };
    let difference = match (band(k), band(k + 2)) {
        (Some(near), Some(far)) => near - far,
        _ => 0.0,
    };
    let (a1, from_pieces) = if difference > 15.0 {
        (k, true)
    } else if difference < -15.0 {
        (k + 2, true)
    } else {
        // Not clear (no pieces, or a game in progress): a1 at the lower left, as seen from White's side.
        let low_left = |n: usize| ring[n % 4].1.1 - ring[n % 4].1.0;
        (if low_left(k) >= low_left(k + 2) { k } else { k + 2 }, false)
    };

    let scale = f64::from(prepared.scale);
    let mut corners = [[0f32; 2]; 4];
    for (n, corner) in corners.iter_mut().enumerate() {
        let (x, y) = ring[(a1 + n) % 4].1;
        *corner = [(x / scale) as f32, (y / scale) as f32];
    }
    let alternation = if total > 0.0 { (vote.abs() / total) as f32 } else { 0.0 };
    if alternation < MIN_ALTERNATION {
        bail!(
            "a grid was found but it does not alternate light and dark like a chessboard (score {alternation:.2}): is the whole board in view, in focus and evenly lit?"
        );
    }
    let found = Found { corners, matched: window.len(), sides_from_pieces: from_pieces, alternation };
    Calibration::new(corners).validate(image.width(), image.height()).map_err(|e| {
        anyhow::anyhow!("the board was found, but not all of it is in the picture or it looks wrong ({e:#}): show the whole board")
    })?;
    Ok(found)
}

// ---- Following a board that was moved -----------------------------------------------------------

/// Looks for the inner corners near where `current` puts them and works out where the
/// four corners of the board are now. The order a1, h1, h8, a8 is kept.
pub fn refine(image: &RgbImage, current: &Calibration) -> Result<Found> {
    let prepared = prepare(image)?;
    let points = &prepared.saddles;
    let scale = f64::from(prepared.scale);
    // Lattice (u, v) = 0..=6 are the inner corners; the board is -1..=7.
    let known: Vec<Pair> =
        OUTER.iter().zip(&current.corners).map(|(&uv, c)| (uv, (f64::from(c[0]) * scale, f64::from(c[1]) * scale))).collect();
    let mut h = Homography::fit(&known).ok_or_else(|| anyhow::anyhow!("the calibration does not define a projection"))?;

    let mut matches: HashMap<(i32, i32), usize> = HashMap::new();
    for pass in 0..3 {
        let tolerance = [0.35, 0.25, 0.2][pass];
        matches.clear();
        let mut used = HashSet::new();
        for u in 0..N {
            for v in 0..N {
                let local = h.spacing(f64::from(u), f64::from(v));
                if !local.is_finite() || local < 4.0 {
                    continue;
                }
                let at = h.apply(f64::from(u), f64::from(v));
                if let Some(k) = nearest(points, at, tolerance * local, &used) {
                    matches.insert((u, v), k);
                    used.insert(k);
                }
            }
        }
        if matches.len() < MIN_MATCHED || !spread_enough(&matches) {
            bail!("only {} inner corners of the board seen", matches.len());
        }
        let fitted = Homography::fit(&pairs_of(points, &matches, (0, 0))).ok_or_else(|| anyhow::anyhow!("no fit"))?;
        h = prune(points, &mut matches, fitted).ok_or_else(|| anyhow::anyhow!("no fit"))?;
    }
    if matches.len() < MIN_MATCHED || !spread_enough(&matches) {
        bail!("only {} inner corners of the board seen", matches.len());
    }
    let error = residual(points, &matches, &h);
    if error > 0.1 {
        bail!("the corners seen do not line up (error {error:.2} squares)");
    }
    let mut corners = [[0f32; 2]; 4];
    for (corner, &(u, v)) in corners.iter_mut().zip(&OUTER) {
        let (x, y) = h.apply(u, v);
        *corner = [(x / scale) as f32, (y / scale) as f32];
    }
    Ok(Found { corners, matched: matches.len(), sides_from_pieces: false, alternation: 1.0 })
}

/// How far the board moved between two calibrations, in squares: the largest shift of
/// a corner, divided by the length of a square on the picture.
pub fn shift_in_squares(from: &Calibration, to: &Calibration) -> f32 {
    let c = &from.corners;
    let edge = |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).hypot(a[1] - b[1]);
    let square = (edge(c[0], c[1]) + edge(c[1], c[2]) + edge(c[2], c[3]) + edge(c[3], c[0])) / 32.0;
    let moved = c.iter().zip(&to.corners).map(|(a, b)| edge(*a, *b)).fold(0.0, f32::max);
    moved / square.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picture;
    use crate::synthetic;

    fn close(found: &[[f32; 2]; 4], truth: &[[f32; 2]; 4], tolerance: f32) {
        for (n, (f, t)) in found.iter().zip(truth).enumerate() {
            let d = (f[0] - t[0]).hypot(f[1] - t[1]);
            assert!(d < tolerance, "corner {n}: found {f:?}, expected {t:?} ({d:.1} px away)");
        }
    }

    #[test]
    fn finds_the_synthetic_board() {
        let found = find_board(&synthetic::frame()).unwrap();
        close(&found.corners, &synthetic::corners().corners, 2.0);
        assert!(found.matched >= 45, "{}", found.matched);
        assert!(found.sides_from_pieces, "the pieces tell where White is");
        assert!(found.alternation > 0.9, "{}", found.alternation);
    }

    #[test]
    fn white_is_found_from_any_side() {
        let truth = synthetic::corners().corners;
        let frame = synthetic::frame();
        let (w, h) = (frame.width(), frame.height());
        // The picture turned by 0, 90, 180, 270 degrees: the labels follow White.
        for turns in 0..4 {
            let mut image = frame.clone();
            for _ in 0..turns {
                image = imageops::rotate90(&image);
            }
            let (expected, nw, nh) = picture::turn_points(&truth, w, h, turns);
            assert_eq!((image.width(), image.height()), (nw, nh));
            let found = find_board(&image).unwrap();
            close(&found.corners, &expected, 2.5);
            assert!(found.sides_from_pieces, "turns {turns}");
        }
    }

    #[test]
    fn copes_with_noise_and_a_dull_picture() {
        let mut image = synthetic::frame();
        synthetic::degrade(&mut image, -25, 12, 7);
        let found = find_board(&image).unwrap();
        close(&found.corners, &synthetic::corners().corners, 3.0);
    }

    #[test]
    fn works_on_a_bigger_picture() {
        let frame = synthetic::frame();
        let big = imageops::resize(&frame, frame.width() * 2, frame.height() * 2, imageops::FilterType::Triangle);
        let found = find_board(&big).unwrap();
        let truth = synthetic::corners().corners.map(|c| [c[0] * 2.0, c[1] * 2.0]);
        close(&found.corners, &truth, 4.0);
    }

    #[test]
    fn finds_the_board_in_the_middle_of_a_game() {
        let steps = synthetic::game_steps(&synthetic::OPERA_GAME);
        let image = synthetic::frame_from_grid(&steps[20].after);
        let found = find_board(&image).unwrap();
        // The corner squares may not tell the side now, but the four corners are the board's.
        let truth = synthetic::corners().corners;
        for f in &found.corners {
            assert!(truth.iter().any(|t| (f[0] - t[0]).hypot(f[1] - t[1]) < 2.5), "{f:?}");
        }
    }

    #[test]
    fn a_blank_picture_has_no_board() {
        let image = RgbImage::from_pixel(640, 480, image::Rgb([120, 120, 120]));
        assert!(find_board(&image).is_err());
    }

    #[test]
    fn refine_follows_a_board_that_moved() {
        let truth = synthetic::corners();
        let image = synthetic::frame();
        // The calibration is a few pixels off, as if the camera had been bumped.
        let off = Calibration::new(truth.corners.map(|c| [c[0] + 4.0, c[1] - 3.0]));
        let found = refine(&image, &off).unwrap();
        close(&found.corners, &truth.corners, 1.5);
        assert!(shift_in_squares(&off, &Calibration::new(found.corners)) > 0.05);
        // A calibration that is right stays where it is.
        let again = refine(&image, &truth).unwrap();
        assert!(shift_in_squares(&truth, &Calibration::new(again.corners)) < 0.04);
    }
}
