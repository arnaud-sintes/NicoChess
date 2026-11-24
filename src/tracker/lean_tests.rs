//! A tall piece seen from the side by a low camera leans over the square behind it: that empty square then looks
//! occupied. The tracker must not be stopped by it, and must not take a piece that really comes there for it.

use super::*;
use crate::calibration::{MARGIN_PX, SQUARE_PX};
use crate::game::uci_of;
use crate::synthetic::{self, PieceGrid};
use image::{Rgb, RgbImage};
use std::time::Duration;

pub(super) const WHITE_HEAD: [u8; 3] = [245, 245, 240];

/// The head of a piece that leans over `square`: a disc of `colour` on the middle of it, `strength` (0 to 1) opaque.
pub(super) fn lean_over(image: &mut RgbImage, square: usize, colour: [u8; 3], strength: f32) {
    let Some(projection) = synthetic::corners().projection() else { return };
    let to_camera = projection.invert();
    let (file, rank) = ((square % 8) as f32, (square / 8) as f32);
    let x = MARGIN_PX as f32 + (file + 0.5) * SQUARE_PX as f32;
    let y = MARGIN_PX as f32 + (7.0 - rank + 0.5) * SQUARE_PX as f32;
    let (cx, cy) = to_camera * (x, y);
    let radius = 14.0f32;
    for py in 0..image.height() {
        for px in 0..image.width() {
            let (dx, dy) = (px as f32 - cx, py as f32 - cy);
            if dx * dx + dy * dy <= radius * radius {
                let old = image.get_pixel(px, py).0;
                let mix = |c: usize| (strength * colour[c] as f32 + (1.0 - strength) * old[c] as f32) as u8;
                image.put_pixel(px, py, Rgb([mix(0), mix(1), mix(2)]));
            }
        }
    }
}

/// A square of rank 3 or 4 that is empty at the start, with a piece on a square that may lean over it.
pub(super) fn leaned_square() -> usize {
    let sampler = Sampler::new(&synthetic::corners()).expect("a sampler");
    let sources = sampler.lean_sources();
    let start = synthetic::start_grid();
    (16..32)
        .find(|&square| sources[square].iter().any(|&n| start[usize::from(n)].is_some()))
        .expect("an empty square with a piece below it in the picture")
}

struct Rig {
    tracker: Tracker,
    now: Instant,
    frames: u64,
}

impl Rig {
    fn new() -> Rig {
        let mut tracker = Tracker::new(TrackerConfig::default());
        tracker.set_calibration(Some(&synthetic::corners()));
        tracker.start(Chess::default());
        Rig { tracker, now: Instant::now(), frames: 0 }
    }

    /// Shows a board for `seconds` at 10 pictures a second, with heads leaning over some squares (square, colour,
    /// strength); returns what the tracker said except "nothing".
    fn show(&mut self, grid: &PieceGrid, leans: &[(usize, [u8; 3], f32)], seconds: f32) -> Vec<Detection> {
        let mut out = Vec::new();
        for _ in 0..(seconds * 10.0).round() as usize {
            self.frames += 1;
            self.now += Duration::from_millis(100);
            let mut frame = synthetic::frame_from_grid(grid);
            for &(square, colour, strength) in leans {
                lean_over(&mut frame, square, colour, strength);
            }
            synthetic::degrade(&mut frame, -15, 10, self.frames);
            match self.tracker.feed(&frame, self.now) {
                Detection::Nothing => {}
                other => out.push(other),
            }
        }
        out
    }
}

fn moves_of(detections: &[Detection]) -> Vec<String> {
    detections
        .iter()
        .flat_map(|d| match d {
            Detection::Move { mv, .. } => vec![uci_of(*mv)],
            Detection::Sequence { moves, .. } => moves.iter().map(|m| uci_of(*m)).collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn the_squares_a_piece_may_lean_over_lie_beside_it_and_above_it_in_the_picture() {
    let sampler = Sampler::new(&synthetic::corners()).expect("a sampler");
    let sources = sampler.lean_sources();
    assert_eq!(sources.len(), 64);
    let mut count = 0;
    for square in 0..64usize {
        for &n in &sources[square] {
            let n = usize::from(n);
            count += 1;
            let (df, dr) = ((n % 8) as i32 - (square % 8) as i32, (n / 8) as i32 - (square / 8) as i32);
            assert!(df.abs() <= 1 && dr.abs() <= 1 && (df, dr) != (0, 0), "{n} is not beside {square}");
            // If a piece on n may lean over the square, a piece on the square does not lean over n.
            assert!(!sources[n].contains(&(square as u8)), "{square} and {n} lean over each other");
        }
    }
    // The synthetic camera looks from the first rank: the squares below a square in the picture are on the rank before
    // it (three of them, or two at the edge), and no square on the first rank has any.
    assert!(sources[..8].iter().all(Vec::is_empty), "{:?}", &sources[..8]);
    assert!(sources[8..].iter().all(|s| !s.is_empty()));
    assert!(count >= 7 * 8 * 2, "{count}");
}

#[test]
fn a_square_that_shows_the_head_of_a_piece_beside_it_does_not_stop_a_move_being_taken() {
    let square = leaned_square();
    let steps = synthetic::game_steps(&["e4"]);
    let mut rig = Rig::new();
    assert!(rig.show(&synthetic::start_grid(), &[], 3.0).is_empty());
    let mut lifted = steps[0].before;
    lifted[usize::from(steps[0].mv.from().unwrap())] = None;
    rig.show(&lifted, &[], 0.15);
    // The move, and a white head over an empty square nowhere near it. (Strong enough that without the lean the
    // square is more than the tracker lets a single square be off, weak enough that the move still ranks first: a
    // head as white as a piece can only be told from one by the height picture.)
    let found = rig.show(&steps[0].after, &[(square, WHITE_HEAD, 0.7)], 3.0);
    assert_eq!(moves_of(&found), ["e2e4"], "{found:?}");
    assert_eq!(rig.tracker.status(), Status::Watching);
}

#[test]
fn a_piece_that_really_comes_to_a_square_beside_a_piece_of_its_colour_is_not_taken_for_a_lean() {
    // Nf3 puts a white piece on a square with white pieces beside it; a head over e3 is no explanation of that.
    let steps = synthetic::game_steps(&["Nf3"]);
    let mut rig = Rig::new();
    assert!(rig.show(&synthetic::start_grid(), &[], 3.0).is_empty());
    let found = rig.show(&steps[0].after, &[], 3.0);
    assert_eq!(moves_of(&found), ["g1f3"], "{found:?}");
}

/// What a lean costs, from the numbers the pictures would give: a3 (index 16) that is empty, with a white piece on a2.
fn evidence_with_a_lean(occupancy: f32, has_depth: bool) -> (Evidence, Grid) {
    let mut grid = [Cell::Empty; 64];
    grid[8] = Cell::White;
    let mut costs = vec![[17.0f32, 60.0, 60.0]; 64];
    // The picture of a3 shows something white: it costs 94 as empty, 38 as white, 124 as black.
    costs[16] = [94.0, 38.0, 124.0];
    let mut lean = vec![Vec::new(); 64];
    lean[16] = vec![8];
    let mut evidence = Evidence {
        costs,
        typical: vec![[17.0, 20.0, 20.0]; 64],
        base: grid,
        change: None,
        ignored: false,
        covered: vec![false; 64],
        unknown: vec![false; 64],
        lean,
        occupancy: vec![0.0; 64],
        has_depth,
        allow: vec![None; 64],
    };
    evidence.occupancy[16] = occupancy;
    (evidence, grid)
}

#[test]
fn a_lean_costs_less_where_the_height_picture_sees_the_square_empty_and_is_not_believed_where_it_does_not() {
    // Colour alone: the lean is believed, at a price. Beyond what the square usually costs: 38 - 20 + 17 + 24 - 17 - 4 = 38.
    let (evidence, grid) = evidence_with_a_lean(0.0, false);
    let (cost, beyond, leaned) = evidence.square(16, &grid);
    assert!(leaned);
    assert_eq!(cost, 94.0, "which explanation is likeliest is not changed by a lean colour alone suggests");
    assert!((beyond - 38.0).abs() < 0.01, "{beyond}");
    assert!(beyond < SQUARE_LIMIT);

    // The height picture is sure the square is empty: the lean costs hardly anything, and counts for what is likeliest.
    let (evidence, grid) = evidence_with_a_lean(-0.9, true);
    let (cost, beyond, leaned) = evidence.square(16, &grid);
    assert!(leaned);
    assert!(beyond < 20.0 && cost < 60.0, "{cost} {beyond}");

    // The height picture says little, or that the square is occupied: it is no lean, and the misfit is all there.
    for occupancy in [0.0, 0.1, 0.9] {
        let (evidence, grid) = evidence_with_a_lean(occupancy, true);
        let (cost, beyond, leaned) = evidence.square(16, &grid);
        assert!(!leaned, "occupancy {occupancy}");
        assert_eq!(cost, 94.0);
        assert!((beyond - 73.0).abs() < 0.01, "{beyond}");
        assert!(beyond > SQUARE_LIMIT);
    }

    // A black head over a white piece's neighbour is no lean either: the picture does not look like the neighbour.
    let (mut evidence, grid) = evidence_with_a_lean(0.0, false);
    evidence.costs[16] = [94.0, 124.0, 38.0];
    assert!(!evidence.square(16, &grid).2);
}

#[test]
fn what_was_allowed_is_not_held_against_a_square_and_goes_when_the_square_is_normal_again() {
    let (mut evidence, grid) = evidence_with_a_lean(0.0, true);
    // No lean here (the height picture does not see it empty): 73 beyond. Allowed up to that: nothing is left.
    assert!((evidence.square(16, &grid).1 - 73.0).abs() < 0.01);
    evidence.allow[16] = Some((Cell::Empty.index(), 73.0));
    let (cost, beyond, _) = evidence.square(16, &grid);
    assert!(beyond < 0.01, "{beyond}");
    assert!((cost - 21.0).abs() < 0.01, "{cost}");
    // The same square holding something else is not excused.
    let mut white = grid;
    white[16] = Cell::White;
    evidence.costs[16] = [94.0, 90.0, 124.0];
    assert!(evidence.square(16, &white).1 > 40.0);
}
