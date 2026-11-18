//! Depth beside the colour picture (a camera such as the Intel RealSense D415 gives both).
//!
//! The depth is not read the way the colour is. A piece is a few centimetres taller than the board, so what
//! tells a piece from an empty square is how far each point of the picture stands above the plane of the board,
//! whatever the light or the colour of the piece. That is turned into a grey picture, the *height picture*: the
//! board plane is a middle grey, what stands above it is brighter, a point with no depth reading is black. The
//! tracker reads this picture with the same per-square models as a colour picture (`vision`), so it learns by
//! itself what an empty square and an occupied one look like in it, including the black pieces that give no
//! reading at all.
//!
//! The depth camera sees the board from another spot than the colour one, so its squares are somewhere else in
//! its picture. [`align`] finds where, from the position the board is known to be in: the occupied squares
//! must be the tall ones.

use crate::calibration::Calibration;
use image::{Rgb, RgbImage};
use imageproc::geometric_transformations::Projection;

/// The grey of the board plane in a height picture, and the grey added for each millimetre above it.
pub const REST: f32 = 60.0;
pub const PER_MM: f32 = 2.0;
/// A board must agree with the height picture at least this well (see `score_of`) for the depth camera to count as
/// lined up with it.
const MIN_ALIGN_SCORE: f32 = 0.55;
/// Depth readings (millimetres) that are believed: nearer or farther than this is noise.
const NEAREST_MM: u16 = 100;
const FARTHEST_MM: u16 = 4000;

/// One depth picture: distance in millimetres from the camera, 0 where there is no reading.
#[derive(Clone, Debug)]
pub struct DepthFrame {
    pub width: u32,
    pub height: u32,
    pub mm: Vec<u16>,
}

impl DepthFrame {
    pub fn new(width: u32, height: u32, mm: Vec<u16>) -> DepthFrame {
        debug_assert_eq!(mm.len(), (width * height) as usize);
        DepthFrame { width, height, mm }
    }

    pub fn at(&self, x: u32, y: u32) -> u16 {
        self.mm[(y * self.width + x) as usize]
    }

    /// Turned clockwise by 0, 90, 180 or 270 degrees, the way `Picture::rotate` turns the colour picture: the two
    /// sensors are in one body, so a camera that stands on its side is turned for both.
    pub fn rotated(&self, degrees: u16) -> DepthFrame {
        let (w, h) = (self.width, self.height);
        let (nw, nh) = if matches!(degrees % 360, 90 | 270) { (h, w) } else { (w, h) };
        if nw == w && nh == h && degrees % 360 != 180 {
            return self.clone();
        }
        let mut mm = vec![0u16; (nw * nh) as usize];
        for ny in 0..nh {
            for nx in 0..nw {
                let (x, y) = match degrees % 360 {
                    90 => (ny, h - 1 - nx),
                    180 => (w - 1 - nx, h - 1 - ny),
                    _ => (w - 1 - ny, nx),
                };
                mm[(ny * nw + nx) as usize] = self.at(x, y);
            }
        }
        DepthFrame { width: nw, height: nh, mm }
    }
}

/// The picture with the readings outside `near..=far` (millimetres) taken out: they become "no reading". What is beyond
/// the board (walls, screens, the room) only confuses the search for the plane of the board.
pub fn clip(frame: &DepthFrame, near: u16, far: u16) -> DepthFrame {
    let mm = frame.mm.iter().map(|&z| if z < near || z > far { 0 } else { z }).collect();
    DepthFrame { width: frame.width, height: frame.height, mm }
}

/// The share of the picture (0 to 1) that has a reading that is believed.
pub fn valid_share(frame: &DepthFrame) -> f32 {
    if frame.mm.is_empty() {
        return 0.0;
    }
    frame.mm.iter().filter(|z| (NEAREST_MM..=FARTHEST_MM).contains(z)).count() as f32 / frame.mm.len() as f32
}

/// Steadies the readings of a depth camera, which flicker: a point with no reading for a moment takes its last
/// one for a few pictures, and the readings of a point are smoothed, unless the distance really changed.
#[derive(Default)]
pub struct DepthFilter {
    state: Vec<f32>,
    age: Vec<u8>,
    size: (u32, u32),
}

/// A point with no reading keeps its last value for this many pictures.
const HOLD_FRAMES: u8 = 8;
/// A change of more than this (millimetres) is a real change of distance, not noise: it is taken at once.
const REAL_CHANGE_MM: f32 = 25.0;
const SMOOTH: f32 = 0.4;

impl DepthFilter {
    pub fn push(&mut self, frame: &DepthFrame) -> DepthFrame {
        let n = frame.mm.len();
        if self.size != (frame.width, frame.height) {
            self.state = vec![0.0; n];
            self.age = vec![0; n];
            self.size = (frame.width, frame.height);
        }
        let mut out = vec![0u16; n];
        for (i, value) in out.iter_mut().enumerate() {
            let z = frame.mm[i];
            if (NEAREST_MM..=FARTHEST_MM).contains(&z) {
                let z = f32::from(z);
                let s = self.state[i];
                self.state[i] = if s == 0.0 || (z - s).abs() > REAL_CHANGE_MM { z } else { s + SMOOTH * (z - s) };
                self.age[i] = 0;
            } else {
                self.age[i] = self.age[i].saturating_add(1);
                if self.age[i] > HOLD_FRAMES {
                    self.state[i] = 0.0;
                }
            }
            *value = self.state[i].round() as u16;
        }
        DepthFrame { width: frame.width, height: frame.height, mm: out }
    }
}

/// The plane of the board in a depth picture: `1 / z = a * x + b * y + c` for the point at pixel (x, y) (for a flat
/// surface the inverse of the distance is a linear function of the pixel).
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    a: f32,
    b: f32,
    c: f32,
}

impl Plane {
    /// The distance of the plane at a pixel, in millimetres (0 when the plane is not in front of the camera there).
    pub fn depth_at(&self, x: f32, y: f32) -> f32 {
        let inverse = self.a * x + self.b * y + self.c;
        if inverse > 1e-7 { 1.0 / inverse } else { 0.0 }
    }

    /// Finds the largest flat surface in the picture (the board, and the table around it), inside `region` (four
    /// corners, made larger) when it is given. Pieces and hands are not on it, and do not count. `None` when
    /// there is too little depth to say.
    pub fn fit(frame: &DepthFrame, region: Option<&[[f32; 2]; 4]>) -> Option<Plane> {
        let area = region.map(|corners| enlarged(corners, 1.4));
        let mut points: Vec<(f32, f32, f32)> = Vec::new();
        for y in (0..frame.height).step_by(3) {
            for x in (0..frame.width).step_by(3) {
                let z = frame.at(x, y);
                if !(NEAREST_MM..=FARTHEST_MM).contains(&z) {
                    continue;
                }
                if area.as_ref().is_some_and(|a| !inside(a, x as f32, y as f32)) {
                    continue;
                }
                points.push((x as f32, y as f32, f32::from(z)));
            }
        }
        if points.len() < 120 {
            return None;
        }
        // The search for a plane looks at a few thousand points at most; the plane found is then made exact on all.
        let sample: Vec<(f32, f32, f32)> = if points.len() > 1500 {
            let step = points.len() as f32 / 1500.0;
            (0..1500).map(|i| points[(i as f32 * step) as usize]).collect()
        } else {
            points.clone()
        };
        // A fixed sequence of "random" choices: the same picture always gives the same plane.
        let mut seed = 0x2545_f491u32;
        let mut next = move |n: usize| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as usize % n
        };
        let close = |plane: &Plane, p: &(f32, f32, f32)| (plane.depth_at(p.0, p.1) - p.2).abs() < 5.0 + 0.004 * p.2;
        let mut best: Option<(Plane, usize)> = None;
        for _ in 0..200 {
            let (p, q, r) = (sample[next(sample.len())], sample[next(sample.len())], sample[next(sample.len())]);
            let Some(plane) = through(p, q, r) else { continue };
            let count = sample.iter().filter(|s| close(&plane, s)).count();
            if best.as_ref().is_none_or(|(_, c)| count > *c) {
                best = Some((plane, count));
            }
        }
        let (mut plane, count) = best?;
        if count < (sample.len() * 3 / 10).max(60) {
            return None;
        }
        // Refine on everything close to the plane found.
        for _ in 0..3 {
            let inliers: Vec<&(f32, f32, f32)> = points.iter().filter(|s| close(&plane, s)).collect();
            plane = least_squares(&inliers).unwrap_or(plane);
        }
        Some(plane)
    }
}

/// The plane through three points (x, y, distance).
fn through(p: (f32, f32, f32), q: (f32, f32, f32), r: (f32, f32, f32)) -> Option<Plane> {
    let rows = [(p.0, p.1, 1.0 / p.2), (q.0, q.1, 1.0 / q.2), (r.0, r.1, 1.0 / r.2)];
    solve3(&[[rows[0].0, rows[0].1, 1.0], [rows[1].0, rows[1].1, 1.0], [rows[2].0, rows[2].1, 1.0]], [rows[0].2, rows[1].2, rows[2].2])
        .map(|[a, b, c]| Plane { a, b, c })
}

/// The plane that is closest to these points, in the inverse of the distance.
fn least_squares(points: &[&(f32, f32, f32)]) -> Option<Plane> {
    // Pixels are centred first: the sums of squares of large coordinates would lose all precision in f32.
    let n = points.len() as f64;
    if n < 3.0 {
        return None;
    }
    let (mx, my) = (points.iter().map(|p| f64::from(p.0)).sum::<f64>() / n, points.iter().map(|p| f64::from(p.1)).sum::<f64>() / n);
    let (mut sxx, mut sxy, mut syy, mut sx, mut sy, mut sxw, mut syw, mut sw) = (0.0f64, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
    for p in points {
        let (x, y, w) = (f64::from(p.0) - mx, f64::from(p.1) - my, 1.0 / f64::from(p.2));
        sxx += x * x;
        sxy += x * y;
        syy += y * y;
        sx += x;
        sy += y;
        sxw += x * w;
        syw += y * w;
        sw += w;
    }
    let m = [[sxx, sxy, sx], [sxy, syy, sy], [sx, sy, n]];
    let [a, b, c0] = solve3_f64(&m, [sxw, syw, sw])?;
    // w = a (x - mx) + b (y - my) + c0
    Some(Plane { a: a as f32, b: b as f32, c: (c0 - a * mx - b * my) as f32 })
}

fn solve3(m: &[[f32; 3]; 3], v: [f32; 3]) -> Option<[f32; 3]> {
    let m64 = m.map(|r| r.map(f64::from));
    solve3_f64(&m64, v.map(f64::from)).map(|s| s.map(|x| x as f32))
}

fn solve3_f64(m: &[[f64; 3]; 3], v: [f64; 3]) -> Option<[f64; 3]> {
    let det = |a: &[[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let d = det(m);
    if d.abs() < 1e-12 {
        return None;
    }
    let mut out = [0.0f64; 3];
    for (k, slot) in out.iter_mut().enumerate() {
        let mut t = *m;
        for r in 0..3 {
            t[r][k] = v[r];
        }
        *slot = det(&t) / d;
    }
    Some(out)
}

/// The four corners moved away from their centre by this factor.
fn enlarged(corners: &[[f32; 2]; 4], factor: f32) -> [[f32; 2]; 4] {
    let (cx, cy) = (corners.iter().map(|c| c[0]).sum::<f32>() / 4.0, corners.iter().map(|c| c[1]).sum::<f32>() / 4.0);
    corners.map(|c| [cx + (c[0] - cx) * factor, cy + (c[1] - cy) * factor])
}

/// Is the point inside the convex quadrilateral (corners in either direction)?
fn inside(corners: &[[f32; 2]; 4], x: f32, y: f32) -> bool {
    let mut sign = 0.0f32;
    for i in 0..4 {
        let (a, b) = (corners[i], corners[(i + 1) % 4]);
        let cross = (b[0] - a[0]) * (y - a[1]) - (b[1] - a[1]) * (x - a[0]);
        if cross == 0.0 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

/// The height picture: how far each point stands above the plane, as a grey (see the module).
pub fn height_picture(frame: &DepthFrame, plane: &Plane) -> RgbImage {
    let (w, h) = (frame.width, frame.height);
    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let z = frame.at(x, y);
            let grey = if !(NEAREST_MM..=FARTHEST_MM).contains(&z) {
                0
            } else {
                let rest = plane.depth_at(x as f32, y as f32);
                (REST + PER_MM * (rest - f32::from(z))).clamp(1.0, 255.0) as u8
            };
            out.put_pixel(x, y, Rgb([grey, grey, grey]));
        }
    }
    out
}

/// A plain picture of the depth, for a person to look at when there is no board plane to measure from: near is bright,
/// far is dark, no reading is black. Scaled between the nearest and the farthest 2% of the readings.
pub fn preview(frame: &DepthFrame) -> RgbImage {
    let mut valid: Vec<u16> = frame.mm.iter().copied().filter(|z| (NEAREST_MM..=FARTHEST_MM).contains(z)).collect();
    valid.sort_unstable();
    let (near, far) = match (valid.get(valid.len() / 50), valid.get(valid.len().saturating_sub(1 + valid.len() / 50))) {
        (Some(&n), Some(&f)) if f > n => (f32::from(n), f32::from(f)),
        _ => (0.0, 1.0),
    };
    let mut out = RgbImage::new(frame.width, frame.height);
    for y in 0..frame.height {
        for x in 0..frame.width {
            let z = frame.at(x, y);
            let grey = if !(NEAREST_MM..=FARTHEST_MM).contains(&z) {
                0
            } else {
                (255.0 - 254.0 * ((f32::from(z) - near) / (far - near)).clamp(0.0, 1.0)) as u8
            };
            out.put_pixel(x, y, Rgb([grey, grey, grey]));
        }
    }
    out
}

// ---- Finding the board in the height picture ---------------------------------------------------------------------

/// Where the board is in the height picture, and how sure: the share of the squares that look as they should (a
/// number up to 1; the occupied squares are tall or without reading, the empty ones flat).
#[derive(Clone, Copy, Debug)]
pub struct Alignment {
    pub calibration: Calibration,
    pub score: f32,
}

/// Points read in each square (a grid of this many on a side, in the middle of the square).
const FINE: usize = 4;
const COARSE: usize = 2;

/// A point of the height picture is tall when it is this much above the board plane in grey (24 is 12 mm), and flat
/// when it is within this much of it (14 is 7 mm). Anything between, or lower, is neither.
const TALL_GREY: f32 = 24.0;
const FLAT_GREY: f32 = 14.0;

struct Grey<'a> {
    picture: &'a RgbImage,
}

/// What a point of the height picture is: tall, flat, without reading, or none of these (or outside the picture).
const TALL: usize = 0;
const FLAT: usize = 1;
const HOLE: usize = 2;
const OTHER: usize = 3;

impl Grey<'_> {
    fn kind(&self, x: f32, y: f32) -> usize {
        let (xi, yi) = (x.round(), y.round());
        if xi < 0.0 || yi < 0.0 || xi >= self.picture.width() as f32 || yi >= self.picture.height() as f32 {
            return OTHER;
        }
        let g = self.picture.get_pixel(xi as u32, yi as u32)[0];
        if g == 0 {
            return HOLE;
        }
        let above = f32::from(g) - REST;
        if above >= TALL_GREY {
            TALL
        } else if above.abs() <= FLAT_GREY {
            FLAT
        } else {
            OTHER
        }
    }
}

/// The positions in the squares that are read, in board units (a file or a rank is one unit), square by square.
fn probe_points(per_side: usize) -> Vec<(f32, f32)> {
    let mut points = Vec::with_capacity(64 * per_side * per_side);
    for square in 0..64 {
        let (file, rank) = ((square % 8) as f32, (square / 8) as f32);
        for j in 0..per_side {
            for i in 0..per_side {
                let at = |k: usize| 0.3 + 0.4 * (k as f32 + 0.5) / per_side as f32;
                points.push((file + at(i), rank + at(j)));
            }
        }
    }
    points
}

/// How well a board with these corners agrees with the height picture: for each square, an occupied one is right when
/// it is tall (or has no reading: a dark piece may give none), and an empty one when it is flat and has readings.
/// Looking at each square, not at the average height, keeps the objects of a cluttered scene from passing for a board:
/// they can make some squares tall, but not make the empty ones flat as well. 1 is a perfect match.
fn score_of(grey: &Grey, corners: &[[f32; 2]; 4], points: &[(f32, f32)], occupied: &[bool; 64]) -> f32 {
    let from = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
    let to = corners.map(|c| (c[0], c[1]));
    let Some(projection) = Projection::from_control_points(from, to) else { return f32::MIN };
    if occupied.iter().all(|&o| o) || occupied.iter().all(|&o| !o) {
        return f32::MIN;
    }
    let per_square = points.len() / 64;
    let mut total = 0.0f32;
    for (square, chunk) in points.chunks_exact(per_square).enumerate() {
        let mut counts = [0.0f32; 4];
        for &(u, v) in chunk {
            let (x, y) = projection * (u, v);
            counts[grey.kind(x, y)] += 1.0;
        }
        let share = counts.map(|c| c / per_square as f32);
        total += if occupied[square] {
            share[TALL] + 0.6 * share[HOLE] - 0.8 * share[FLAT]
        } else {
            share[FLAT] - share[TALL] - 0.8 * share[HOLE]
        };
    }
    total / 64.0
}

/// The corners moved as a whole: turned, scaled and shifted about their centre.
fn moved(base: &[[f32; 2]; 4], shift: (f32, f32), scale: f32, turn_degrees: f32) -> [[f32; 2]; 4] {
    let (cx, cy) = (base.iter().map(|c| c[0]).sum::<f32>() / 4.0, base.iter().map(|c| c[1]).sum::<f32>() / 4.0);
    let (sin, cos) = turn_degrees.to_radians().sin_cos();
    base.map(|c| {
        let (x, y) = ((c[0] - cx) * scale, (c[1] - cy) * scale);
        [cx + x * cos - y * sin + shift.0, cy + x * sin + y * cos + shift.1]
    })
}

/// Finds the squares of the board in a height picture. `guess` is where they are expected to be (the colour
/// camera's corners, taken over to the depth picture roughly); `occupied` says which squares hold a piece. The
/// search shifts, scales and turns the guess, then moves each corner by itself, for the best match between the tall
/// squares and the occupied ones. `None` when no match is clear enough.
pub fn align(picture: &RgbImage, guess: &[[f32; 2]; 4], occupied: &[bool; 64]) -> Option<Alignment> {
    let grey = Grey { picture };
    let coarse = probe_points(COARSE);
    let fine = probe_points(FINE);
    let span = 0.16 * picture.width().max(picture.height()) as f32;
    let step = 8.0;
    let steps = (span / step) as i32;

    // A wide search with few points, keeping the best few.
    let mut candidates: Vec<(f32, (f32, f32), f32, f32)> = Vec::new();
    for iy in -steps..=steps {
        for ix in -steps..=steps {
            let shift = (ix as f32 * step, iy as f32 * step);
            for scale in [0.85, 0.9, 0.95, 1.0, 1.05, 1.1, 1.15] {
                for turn in [-6.0, -3.0, 0.0, 3.0, 6.0] {
                    let score = score_of(&grey, &moved(guess, shift, scale, turn), &coarse, occupied);
                    candidates.push((score, shift, scale, turn));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Each of the best is made finer, and the best of those is kept.
    let mut best: Option<(f32, [[f32; 2]; 4])> = None;
    let mut tried: Vec<(f32, f32)> = Vec::new();
    for &(_, shift, scale, turn) in candidates.iter() {
        // Candidates next to one already refined lead to the same place.
        if tried.iter().any(|t| (t.0 - shift.0).abs() < 2.5 * step && (t.1 - shift.1).abs() < 2.5 * step) {
            continue;
        }
        tried.push(shift);
        if tried.len() > 5 {
            break;
        }
        let (mut shift, mut scale, mut turn) = (shift, scale, turn);
        let mut score = score_of(&grey, &moved(guess, shift, scale, turn), &fine, occupied);
        for (dx, ds, dt) in [(4.0, 0.025, 1.5), (2.0, 0.0125, 0.75), (1.0, 0.006, 0.4)] {
            let mut improved = true;
            let mut rounds = 0;
            while improved && rounds < 12 {
                improved = false;
                rounds += 1;
                let options = [
                    ((shift.0 + dx, shift.1), scale, turn),
                    ((shift.0 - dx, shift.1), scale, turn),
                    ((shift.0, shift.1 + dx), scale, turn),
                    ((shift.0, shift.1 - dx), scale, turn),
                    (shift, scale + ds, turn),
                    (shift, scale - ds, turn),
                    (shift, scale, turn + dt),
                    (shift, scale, turn - dt),
                ];
                for (s, k, t) in options {
                    let candidate = score_of(&grey, &moved(guess, s, k, t), &fine, occupied);
                    if candidate > score + 0.0005 {
                        (shift, scale, turn, score) = (s, k, t, candidate);
                        improved = true;
                    }
                }
            }
        }
        let corners = moved(guess, shift, scale, turn);
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, corners));
        }
    }
    let (mut score, mut corners) = best?;

    // Each corner on its own: the two cameras do not see the board from the same spot, so the shape is not quite
    // the guess's.
    for dx in [3.0f32, 1.5, 0.75] {
        for _ in 0..8 {
            let mut improved = false;
            for corner in 0..4 {
                for (ox, oy) in [(dx, 0.0), (-dx, 0.0), (0.0, dx), (0.0, -dx)] {
                    let mut trial = corners;
                    trial[corner][0] += ox;
                    trial[corner][1] += oy;
                    let candidate = score_of(&grey, &trial, &fine, occupied);
                    if candidate > score + 0.0005 {
                        (corners, score, improved) = (trial, candidate, true);
                    }
                }
            }
            if !improved {
                break;
            }
        }
    }
    if score < MIN_ALIGN_SCORE {
        return None;
    }
    let calibration = Calibration::for_frame(corners, picture.width(), picture.height());
    calibration.validate(picture.width(), picture.height()).ok()?;
    Some(Alignment { calibration, score })
}

/// A camera whose own picture is not the colour camera's: where the colour calibration's corners are expected to
/// be in a depth picture, taking the two sensors as looking the same way from the same spot, with their own fields
/// of view. The remaining difference (the sensors are some centimetres apart) is what [`align`] finds.
pub fn guess_corners(
    colour: &Calibration,
    colour_size: (u32, u32),
    depth_size: (u32, u32),
    colour_fov: f32,
    depth_fov: f32,
) -> [[f32; 2]; 4] {
    // The length of a focal distance in pixels is half the width over the tangent of half the angle.
    let focal = |width: u32, fov_degrees: f32| width as f32 / 2.0 / (fov_degrees.to_radians() / 2.0).tan();
    let ratio = focal(depth_size.0, depth_fov) / focal(colour_size.0, colour_fov);
    let (ccx, ccy) = (colour_size.0 as f32 / 2.0, colour_size.1 as f32 / 2.0);
    let (dcx, dcy) = (depth_size.0 as f32 / 2.0, depth_size.1 as f32 / 2.0);
    colour.corners.map(|c| [dcx + (c[0] - ccx) * ratio, dcy + (c[1] - ccy) * ratio])
}

pub mod fake {
    //! A depth picture of a board, drawn from a position: the plane of the board, and a column on every occupied
    //! square. For the tests, and for trying the depth without a depth camera (`--source demo --depth-camera`).
    use super::*;

    /// How tall the piece on a square is, in millimetres: pawns, then the back rank by file.
    fn piece_mm(square: usize) -> f32 {
        let (file, rank) = (square % 8, square / 8);
        if rank == 1 || rank == 6 { 42.0 } else { [58.0, 50.0, 52.0, 68.0, 90.0, 52.0, 50.0, 58.0][file] }
    }

    /// A cheap repeatable noise in -1..1.
    fn noise(x: u32, y: u32, seed: u32) -> f32 {
        let mut h = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca6b) ^ seed.wrapping_mul(0xc2b2_ae35);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2c1b_3c6d);
        h ^= h >> 12;
        (h & 0xffff) as f32 / 32767.5 - 1.0
    }

    /// The board is at `corners` (a1, h1, h8, a8 in the picture), the plane is 600 mm away at the middle and leans.
    /// `occupied` says where there are pieces; `holes` is the share of the readings that are missing; the black
    /// pieces (`absorbing`) give no reading at all on their top, as a dark matte surface can do.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        width: u32,
        height: u32,
        corners: &[[f32; 2]; 4],
        occupied: &[bool; 64],
        black: &[bool; 64],
        absorbing: bool,
        holes: f32,
        seed: u32,
    ) -> DepthFrame {
        let from = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
        let to = corners.map(|c| (c[0], c[1]));
        let to_board = Projection::from_control_points(to, from).expect("corners");
        let mut mm = vec![0u16; (width * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let rest = 600.0 + 0.35 * (y as f32 - height as f32 / 2.0) - 0.08 * (x as f32 - width as f32 / 2.0);
                let (u, v) = to_board * (x as f32, y as f32);
                let mut z = rest;
                let mut hidden = false;
                if (0.0..8.0).contains(&u) && (0.0..8.0).contains(&v) {
                    let square = (v as usize) * 8 + u as usize;
                    let (du, dv) = (u.fract() - 0.5, v.fract() - 0.5);
                    if occupied[square] && du * du + dv * dv < 0.3 * 0.3 {
                        z -= piece_mm(square);
                        hidden = absorbing && black[square];
                    }
                }
                let flicker = noise(x, y, seed);
                let missing = noise(y, x, seed ^ 0x55) * 0.5 + 0.5 < holes;
                mm[(y * width + x) as usize] = if hidden || missing { 0 } else { (z + 2.0 * flicker).round() as u16 };
            }
        }
        DepthFrame { width, height, mm }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picture::Picture;

    const W: u32 = 424;
    const H: u32 = 240;

    fn corners() -> [[f32; 2]; 4] {
        // a1 near the bottom left, seen from the side of white, as in a camera picture.
        [[120.0, 215.0], [330.0, 212.0], [312.0, 38.0], [140.0, 40.0]]
    }

    fn start_occupied() -> ([bool; 64], [bool; 64]) {
        let mut occupied = [false; 64];
        let mut black = [false; 64];
        for square in 0..64 {
            occupied[square] = !(16..48).contains(&square);
            black[square] = square >= 48;
        }
        (occupied, black)
    }

    fn start_frame(absorbing: bool, holes: f32, seed: u32) -> DepthFrame {
        let (occupied, black) = start_occupied();
        fake::frame(W, H, &corners(), &occupied, &black, absorbing, holes, seed)
    }

    #[test]
    fn a_picture_with_no_reading_at_all_can_be_shown() {
        // `/depth.jpg` of a camera that sees nothing (a cap on the lens) must not panic.
        let empty = DepthFrame::new(8, 4, vec![0; 32]);
        let shown = preview(&empty);
        assert_eq!((shown.width(), shown.height()), (8, 4));
        assert!(shown.pixels().all(|p| p[0] == 0));
        let none = DepthFrame::new(0, 0, vec![]);
        assert_eq!(preview(&none).width(), 0);
    }

    #[test]
    fn readings_outside_the_range_are_taken_out() {
        let frame = DepthFrame::new(5, 1, vec![0, 200, 600, 1200, 3000]);
        let kept = clip(&frame, 300, 1500);
        assert_eq!(kept.mm, vec![0, 0, 600, 1200, 0]);
        assert!((valid_share(&kept) - 0.4).abs() < 1e-6);
        assert!((valid_share(&frame) - 0.8).abs() < 1e-6, "the reading of 0 is not a reading");
        assert_eq!(valid_share(&DepthFrame::new(0, 0, vec![])), 0.0);
    }

    #[test]
    fn the_plane_is_found_although_pieces_stand_on_it() {
        let frame = start_frame(false, 0.0, 1);
        let plane = Plane::fit(&frame, Some(&corners())).expect("a plane");
        // The plane was 600 mm away at the middle, leaning a little.
        let rest = plane.depth_at(W as f32 / 2.0, H as f32 / 2.0);
        assert!((rest - 600.0).abs() < 4.0, "{rest}");
        let near = plane.depth_at(W as f32 / 2.0, 0.0);
        assert!((near - (600.0 - 0.35 * H as f32 / 2.0)).abs() < 5.0, "{near}");
    }

    #[test]
    fn pieces_are_bright_in_the_height_picture_and_empty_squares_are_the_rest_grey() {
        let frame = start_frame(false, 0.0, 2);
        let plane = Plane::fit(&frame, None).expect("a plane");
        let picture = height_picture(&frame, &plane);
        let (occupied, _) = start_occupied();
        let from = [(0.0, 0.0), (8.0, 0.0), (8.0, 8.0), (0.0, 8.0)];
        let to_picture = Projection::from_control_points(from, corners().map(|c| (c[0], c[1]))).unwrap();
        let mut sums = [0.0f32; 2];
        let mut counts = [0.0f32; 2];
        for square in 0..64 {
            let (x, y) = to_picture * ((square % 8) as f32 + 0.5, (square / 8) as f32 + 0.5);
            let g = f32::from(picture.get_pixel(x as u32, y as u32)[0]);
            sums[usize::from(occupied[square])] += g;
            counts[usize::from(occupied[square])] += 1.0;
        }
        let (empty, taken) = (sums[0] / counts[0], sums[1] / counts[1]);
        assert!((empty - REST).abs() < 8.0, "empty squares are {empty}");
        assert!(taken > REST + 50.0, "occupied squares are {taken}");
    }

    #[test]
    fn a_point_with_no_reading_is_black_and_the_filter_fills_flicker_for_a_while() {
        let mut filter = DepthFilter::default();
        let steady = DepthFrame::new(2, 1, vec![500, 600]);
        for _ in 0..3 {
            filter.push(&steady);
        }
        // One picture where the first point has no reading: it keeps its value.
        let out = filter.push(&DepthFrame::new(2, 1, vec![0, 600]));
        assert_eq!(out.mm, vec![500, 600]);
        // A real change is taken at once.
        let out = filter.push(&DepthFrame::new(2, 1, vec![500, 540]));
        assert_eq!(out.mm, vec![500, 540]);
        // A long silence is forgotten.
        let mut last = out;
        for _ in 0..12 {
            last = filter.push(&DepthFrame::new(2, 1, vec![0, 540]));
        }
        assert_eq!(last.mm[0], 0);
    }

    #[test]
    fn turning_a_depth_frame_is_turning_a_picture() {
        let (w, h) = (5u32, 3u32);
        let mut image = RgbImage::new(w, h);
        let mut mm = vec![0u16; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let value = (y * w + x + 1) as u8;
                image.put_pixel(x, y, Rgb([value, 0, 0]));
                mm[(y * w + x) as usize] = u16::from(value);
            }
        }
        let frame = DepthFrame::new(w, h, mm);
        for degrees in [0u16, 90, 180, 270] {
            let picture = Picture { rotation: degrees, ..Picture::default() }.rotate(image.clone());
            let turned = frame.rotated(degrees);
            assert_eq!((turned.width, turned.height), (picture.width(), picture.height()), "{degrees}");
            for y in 0..turned.height {
                for x in 0..turned.width {
                    assert_eq!(turned.at(x, y), u16::from(picture.get_pixel(x, y)[0]), "{degrees} at {x},{y}");
                }
            }
        }
    }

    fn mean_corner_error(found: &[[f32; 2]; 4], truth: &[[f32; 2]; 4]) -> f32 {
        found.iter().zip(truth).map(|(a, b)| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()).sum::<f32>() / 4.0
    }

    #[test]
    fn the_board_is_found_in_the_height_picture_from_a_guess_that_is_off() {
        let (occupied, _) = start_occupied();
        for (seed, shift, scale, turn) in [(3u32, (38.0, -24.0), 0.93, 3.0), (4, (-45.0, 30.0), 1.08, -4.0), (5, (0.0, 0.0), 1.0, 0.0)] {
            let frame = start_frame(false, 0.04, seed);
            let plane = Plane::fit(&frame, None).expect("a plane");
            let picture = height_picture(&frame, &plane);
            let guess = moved(&corners(), shift, scale, turn);
            let found = align(&picture, &guess, &occupied).unwrap_or_else(|| panic!("no alignment for guess {seed}"));
            let error = mean_corner_error(&found.calibration.corners, &corners());
            assert!(error < 4.0, "seed {seed}: off by {error} px, score {}", found.score);
            assert!(found.score > 0.7, "{}", found.score);
        }
    }

    #[test]
    fn black_pieces_that_give_no_reading_do_not_stop_the_alignment() {
        let (occupied, black) = start_occupied();
        let frame = fake::frame(W, H, &corners(), &occupied, &black, true, 0.05, 6);
        let plane = Plane::fit(&frame, None).expect("a plane");
        let picture = height_picture(&frame, &plane);
        let guess = moved(&corners(), (30.0, 20.0), 0.95, 2.0);
        let found = align(&picture, &guess, &occupied).expect("aligned on the white pieces alone");
        assert!(mean_corner_error(&found.calibration.corners, &corners()) < 8.0);
    }

    #[test]
    fn an_empty_scene_is_not_mistaken_for_a_board() {
        // No piece at all: whatever is tried, no square stands out.
        let (occupied, black) = start_occupied();
        let empty = fake::frame(W, H, &corners(), &[false; 64], &black, false, 0.02, 7);
        let plane = Plane::fit(&empty, None).expect("a plane");
        let picture = height_picture(&empty, &plane);
        assert!(align(&picture, &corners(), &occupied).is_none());
    }

    #[test]
    fn the_colour_corners_are_taken_over_to_the_depth_picture_by_the_fields_of_view() {
        let colour = Calibration::for_frame([[400.0, 620.0], [900.0, 610.0], [860.0, 150.0], [440.0, 160.0]], 1280, 720);
        let guess = guess_corners(&colour, (1280, 720), (848, 480), 69.4, 63.4);
        // The centre of the colour picture is the centre of the depth picture, and what is farther from it is
        // scaled by about the ratio of the focal lengths.
        let ratio = (848.0 / 2.0 / (63.4f32.to_radians() / 2.0).tan()) / (1280.0 / 2.0 / (69.4f32.to_radians() / 2.0).tan());
        assert!((guess[0][0] - (424.0 + (400.0 - 640.0) * ratio)).abs() < 0.01);
        assert!((guess[0][1] - (240.0 + (620.0 - 360.0) * ratio)).abs() < 0.01);
        assert!(guess.iter().all(|c| c[0] > 0.0 && c[0] < 848.0 && c[1] > 0.0 && c[1] < 480.0));
    }
}
