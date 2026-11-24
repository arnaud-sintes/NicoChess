//! The tracker with a colour camera and a depth camera (the height picture is an advisory view, see `View`).

use super::*;
use crate::calibration::Calibration;
use crate::depth::{self, DepthFilter, Plane};
use crate::game::uci_of;
use crate::synthetic::{self, OPERA_GAME, PieceGrid};
use shakmaty::Color;
use std::collections::HashMap;
use std::time::Duration;

const DW: u32 = 424;
const DH: u32 = 240;

/// Where the board is in the picture of the depth camera: not where it is in the colour picture.
fn depth_corners() -> Calibration {
    Calibration::for_frame([[110.0, 215.0], [320.0, 212.0], [300.0, 40.0], [140.0, 42.0]], DW, DH)
}

/// The colour camera is view 0 and the depth camera view 2, as in the engine.
struct WithDepth {
    tracker: Tracker,
    now: Instant,
    frames: u64,
    filter: DepthFilter,
    depth: bool,
    /// The black pieces give no depth reading at all.
    absorbing: bool,
    /// Squares over which a white head leans in the colour picture (the depth camera is not fooled by it).
    ghosts: Vec<usize>,
    /// The depth pictures already drawn, by board and by which of a few noises they have (drawing takes long in a
    /// test build).
    drawn: HashMap<(PieceGrid, u32), depth::DepthFrame>,
}

impl WithDepth {
    fn new(position: Chess, depth: bool, absorbing: bool) -> WithDepth {
        let mut tracker = Tracker::new(TrackerConfig::default());
        tracker.set_view_calibration(0, Some(&synthetic::corners()));
        if depth {
            tracker.set_view_calibration(2, Some(&depth_corners()));
            tracker.set_view_advisory(2, true);
        }
        tracker.start(position);
        WithDepth {
            tracker,
            now: Instant::now(),
            frames: 0,
            filter: DepthFilter::default(),
            depth,
            absorbing,
            ghosts: Vec::new(),
            drawn: HashMap::new(),
        }
    }

    /// `covered` are squares a hand hides in the colour picture. The depth camera is not bothered by it.
    fn show(&mut self, grid: &PieceGrid, times: usize, covered: &[usize]) -> Vec<Detection> {
        let mut out = Vec::new();
        for _ in 0..times {
            self.frames += 1;
            self.now += Duration::from_millis(333);
            let mut colour = synthetic::frame_from_grid(grid);
            synthetic::degrade(&mut colour, -15, 10, self.frames);
            for &square in covered {
                synthetic::occlude(&mut colour, &synthetic::corners(), square, 0.6);
            }
            for &square in &self.ghosts {
                super::lean_tests::lean_over(&mut colour, square, super::lean_tests::WHITE_HEAD, 1.0);
            }
            let height = self.depth.then(|| {
                let mut occupied = [false; 64];
                let mut black = [false; 64];
                for (i, piece) in grid.iter().enumerate() {
                    occupied[i] = piece.is_some();
                    black[i] = *piece == Some(Color::Black);
                }
                let (absorbing, seed) = (self.absorbing, (self.frames % 3) as u32);
                let raw = self
                    .drawn
                    .entry((*grid, seed))
                    .or_insert_with(|| depth::fake::frame(DW, DH, &depth_corners().corners, &occupied, &black, absorbing, 0.03, seed))
                    .clone();
                let frame = self.filter.push(&raw);
                let plane = Plane::fit(&frame, Some(&depth_corners().corners)).expect("the plane of the board");
                depth::height_picture(&frame, &plane)
            });
            match self.tracker.feed_views(&[Some(&colour), None, height.as_ref()], self.now) {
                Detection::Nothing => {}
                other => out.push(other),
            }
        }
        out
    }
}

#[test]
fn a_game_with_captures_is_followed_with_a_depth_camera_beside_the_colour_one() {
    // The first dozen moves of the Opera game: pawns, a pinned piece, and captures that change the colour of a square.
    let steps = synthetic::game_steps(&OPERA_GAME[..12]);
    let mut rig = WithDepth::new(Chess::default(), true, true);
    assert!(rig.show(&synthetic::start_grid(), 7, &[]).is_empty());
    assert_eq!(rig.tracker.status(), Status::Watching);
    assert_eq!(rig.tracker.views_in_use(), vec![true, false, true]);

    let mut seen = Vec::new();
    for step in &steps {
        let mut lifted = step.before;
        lifted[usize::from(step.mv.from().unwrap())] = None;
        assert!(rig.show(&lifted, 1, &[]).is_empty());
        for detection in rig.show(&step.after, 4, &[]) {
            match detection {
                Detection::Move { mv, .. } => seen.push(uci_of(mv)),
                other => panic!("unexpected {other:?} after {}", uci_of(step.mv)),
            }
        }
    }
    let expected: Vec<String> = steps.iter().map(|s| uci_of(s.mv)).collect();
    assert_eq!(seen, expected);
}

/// A hand comes over the squares of a move in the colour picture and stays: the colour camera cannot see the move.
/// The depth camera sees it from its own spot.
#[test]
fn a_hand_that_hides_a_move_from_the_colour_camera_does_not_hide_it_from_the_depth_camera() {
    let steps = synthetic::game_steps(&["e4", "e5", "Nf3", "Nc6"]);
    let covered = [usize::from(Square::E2), usize::from(Square::E4), usize::from(Square::E3)];

    let mut results = Vec::new();
    for depth in [false, true] {
        let mut rig = WithDepth::new(Chess::default(), depth, false);
        rig.show(&synthetic::start_grid(), 7, &[]);
        rig.show(&synthetic::start_grid(), 4, &covered);
        let found = rig.show(&steps[0].after, 5, &covered);
        let right = found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e2e4"));
        let wrong = found
            .iter()
            .any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) != "e2e4") || matches!(d, Detection::Sequence { .. }));
        results.push((right, wrong));
    }
    assert!(!results[0].1 && !results[1].1, "no wrong move may be recorded: {results:?}");
    assert!(!results[0].0, "the colour camera alone cannot find it: {results:?}");
    assert!(results[1].0, "with the depth camera the move is found: {results:?}");
}

#[test]
fn the_depth_camera_is_not_waited_for_and_can_drop_out_and_come_back() {
    let steps = synthetic::game_steps(&["e4", "e5", "Nf3", "Nc6", "Bc4"]);
    let mut rig = WithDepth::new(Chess::default(), true, false);
    rig.show(&synthetic::start_grid(), 7, &[]);
    assert_eq!(rig.tracker.views_in_use(), vec![true, false, true]);

    rig.tracker.set_view_active(2, false);
    let found = rig.show(&steps[0].after, 5, &[]);
    assert!(found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e2e4")), "{found:?}");
    assert_eq!(rig.tracker.views_in_use(), vec![true, false, false]);

    rig.tracker.set_view_active(2, true);
    let found = rig.show(&steps[1].after, 6, &[]);
    assert!(found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e7e5")), "{found:?}");
    let found = rig.show(&steps[2].after, 6, &[]);
    assert!(found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "g1f3")), "{found:?}");
}

/// A depth camera that gives pictures nobody can read (no reading anywhere) must not stop the game, nor invent moves.
#[test]
fn a_depth_camera_that_sees_nothing_cannot_spoil_the_colour_camera() {
    let steps = synthetic::game_steps(&["e4", "e5", "Nf3"]);
    let mut rig = WithDepth::new(Chess::default(), true, false);
    rig.show(&synthetic::start_grid(), 7, &[]);
    // From now on the depth camera shows an empty board whatever happens (its picture is the one of the start of the game
    // with nothing standing out).
    let blank = {
        let flat = depth::DepthFrame::new(DW, DH, vec![600; (DW * DH) as usize]);
        let plane = Plane::fit(&flat, None).expect("a plane");
        depth::height_picture(&flat, &plane)
    };
    let mut seen = Vec::new();
    for step in &steps {
        for _ in 0..5 {
            rig.frames += 1;
            rig.now += Duration::from_millis(333);
            let mut colour = synthetic::frame_from_grid(&step.after);
            synthetic::degrade(&mut colour, -15, 10, rig.frames);
            if let Detection::Move { mv, .. } = rig.tracker.feed_views(&[Some(&colour), None, Some(&blank)], rig.now) {
                seen.push(uci_of(mv));
            }
        }
    }
    assert_eq!(seen, ["e2e4", "e7e5", "g1f3"]);
}

/// A head as white as a piece leans over an empty square. Colour alone takes it for a piece and cannot follow the game;
/// the depth camera, which sees the foot of things, sees nothing there, and the move is taken.
#[test]
fn a_head_that_leans_over_an_empty_square_stops_the_colour_camera_but_not_the_depth_camera() {
    let square = super::lean_tests::leaned_square();
    let steps = synthetic::game_steps(&["e4"]);
    let mut taken = Vec::new();
    for depth in [false, true] {
        let mut rig = WithDepth::new(Chess::default(), depth, false);
        rig.show(&synthetic::start_grid(), 7, &[]);
        rig.ghosts = vec![square];
        let found = rig.show(&steps[0].after, 8, &[]);
        let wrong = found
            .iter()
            .any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) != "e2e4") || matches!(d, Detection::Sequence { .. }));
        taken.push((found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e2e4")), wrong));
    }
    assert_eq!(taken[1], (true, false), "with the depth camera: {taken:?}");
    assert!(!taken[0].0, "colour alone cannot tell the head from a piece: {taken:?}");
}
