//! How fast a move is noticed, whatever the pictures per second, and what happens after an undo.

use super::*;
use crate::game::uci_of;
use crate::synthetic::{self, PieceGrid};
use std::time::Duration;

/// Feeds pictures a fixed time apart.
struct Timed {
    tracker: Tracker,
    now: Instant,
    frames: u64,
    spacing: Duration,
}

impl Timed {
    fn new(spacing: Duration) -> Timed {
        let mut tracker = Tracker::new(TrackerConfig::default());
        tracker.set_calibration(Some(&synthetic::corners()));
        tracker.start(Chess::default());
        Timed { tracker, now: Instant::now(), frames: 0, spacing }
    }

    /// Shows a board for `seconds`; returns what the tracker found, each with the time since the board was first shown.
    fn show(&mut self, grid: &PieceGrid, seconds: f32) -> Vec<(Detection, Duration)> {
        let started = self.now;
        let mut out = Vec::new();
        let count = (seconds / self.spacing.as_secs_f32()).round() as usize;
        for _ in 0..count {
            self.frames += 1;
            self.now += self.spacing;
            let mut frame = synthetic::frame_from_grid(grid);
            synthetic::degrade(&mut frame, -15, 10, self.frames);
            match self.tracker.feed(&frame, self.now) {
                Detection::Nothing => {}
                other => out.push((other, self.now - started)),
            }
        }
        out
    }
}

/// How long after the picture shows e4 played the move is found, with pictures this far apart.
fn time_to_find_e4(spacing: Duration) -> Duration {
    let steps = synthetic::game_steps(&["e4"]);
    let mut rig = Timed::new(spacing);
    assert!(rig.show(&synthetic::start_grid(), 3.0).is_empty());
    let mut lifted = steps[0].before;
    lifted[usize::from(steps[0].mv.from().unwrap())] = None;
    // A piece held in the air for less than the still time is not looked at.
    assert!(rig.show(&lifted, 0.15).is_empty());
    let found = rig.show(&steps[0].after, 3.0);
    match found.as_slice() {
        [(Detection::Move { mv, .. }, at)] => {
            assert_eq!(uci_of(*mv), "e2e4");
            *at
        }
        other => panic!("expected e2e4 once, got {other:?}"),
    }
}

#[test]
fn a_move_is_noticed_as_soon_after_it_is_played_at_25_pictures_a_second_as_it_took_at_3() {
    let slow = time_to_find_e4(Duration::from_millis(333));
    let fast = time_to_find_e4(Duration::from_millis(40));
    // The board must be still for a while before it is looked at, whatever the pictures a second.
    assert!(fast >= Duration::from_millis(300), "not looked at before it is still: {fast:?}");
    assert!(fast <= Duration::from_millis(800), "{fast:?}");
    assert!(slow <= Duration::from_millis(1400), "{slow:?}");
    assert!(fast < slow, "more pictures a second must not make it slower: {fast:?} against {slow:?}");
}

#[test]
fn an_undone_move_is_not_found_again_while_the_board_still_shows_it() {
    let steps = synthetic::game_steps(&["e4", "e5"]);
    let spacing = Duration::from_millis(333);
    let mut rig = Timed::new(spacing);
    rig.show(&synthetic::start_grid(), 3.0);
    let found = rig.show(&steps[0].after, 3.0);
    assert!(matches!(found.as_slice(), [(Detection::Move { .. }, _)]), "{found:?}");

    // The move is taken back (a person pressed Undo), and the pieces are left where they are.
    rig.tracker.undo_to(Chess::default(), Some("e2e4".into()));
    let seen = rig.show(&steps[0].after, 4.0);
    assert!(seen.iter().all(|(d, _)| matches!(d, Detection::Unsure { .. })), "nothing is decided: {seen:?}");
    assert_eq!(rig.tracker.status(), Status::Undone);
    // What the picture suggests is offered, but not the move that was taken back.
    for (detection, _) in &seen {
        if let Detection::Unsure { candidates } = detection {
            assert!(candidates.iter().all(|c| c.uci != "e2e4"), "{candidates:?}");
        }
    }

    // The pieces are put back: tracking goes on by itself, and the move is found when it is played again.
    let back = rig.show(&synthetic::start_grid(), 4.0);
    assert!(back.iter().all(|(d, _)| !matches!(d, Detection::Move { .. })), "{back:?}");
    assert_eq!(rig.tracker.status(), Status::Watching);
    let again = rig.show(&steps[0].after, 3.0);
    assert!(matches!(again.as_slice(), [(Detection::Move { mv, .. }, _)] if uci_of(*mv) == "e2e4"), "{again:?}");
}

#[test]
fn entering_the_move_that_was_played_ends_the_wait_after_an_undo() {
    let steps = synthetic::game_steps(&["e4", "e5"]);
    let mut rig = Timed::new(Duration::from_millis(333));
    rig.show(&synthetic::start_grid(), 3.0);
    rig.show(&steps[0].after, 3.0);
    rig.tracker.undo_to(Chess::default(), Some("e2e4".into()));
    rig.show(&steps[0].after, 2.0);
    assert_eq!(rig.tracker.status(), Status::Undone);
    // The person enters the move that really was played (here it is the same one).
    let mut position = Chess::default();
    position.play_unchecked(steps[0].mv);
    rig.tracker.set_position(position);
    let seen = rig.show(&steps[0].after, 3.0);
    assert!(seen.iter().all(|(d, _)| !matches!(d, Detection::Unsure { .. })), "{seen:?}");
    assert_eq!(rig.tracker.status(), Status::Watching);
    // And the game goes on: the reply is found.
    let reply = rig.show(&steps[1].after, 3.0);
    assert!(matches!(reply.as_slice(), [(Detection::Move { mv, .. }, _)] if uci_of(*mv) == "e7e5"), "{reply:?}");
}

#[test]
fn a_still_time_that_cannot_be_held_does_not_stop_the_program() {
    // From the command line or the file of a recording: infinite, not a number, negative, huge.
    for (stable, span) in [(f32::INFINITY, f32::NAN), (f32::NAN, f32::INFINITY), (-5.0, -1.0), (1.0e30, 1.0e30)] {
        let config = TrackerConfig { stable_time: stable, motion_span: span, ..TrackerConfig::default() };
        let mut tracker = Tracker::new(config);
        tracker.set_calibration(Some(&synthetic::corners()));
        tracker.start(Chess::default());
        let mut now = Instant::now();
        for i in 0..6 {
            now += Duration::from_millis(333);
            let mut frame = synthetic::frame_from_grid(&synthetic::start_grid());
            synthetic::degrade(&mut frame, -15, 10, i);
            let _ = tracker.feed(&frame, now);
        }
    }
}
