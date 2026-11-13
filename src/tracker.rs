//! Follows a game on the board: waits for the image to settle, then finds which
//! legal move explains what changed.
//!
//! Vision only says empty, white or black per square. The tracker keeps the real
//! position, so it already knows the piece types. For every legal move it computes
//! what the board should look like afterwards and picks the move whose picture is
//! the closest to what the camera sees. A square the camera is unsure about costs
//! little, so one misread square does not break the match.

use crate::calibration::Calibration;
use crate::game::{san_of, uci_of};
use crate::vision::{Cell, Classifier, Features, Grid, Sampler};
use image::RgbImage;
use shakmaty::{Chess, Color, Move, Position, Role, Square};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct TrackerConfig {
    /// Frames in a row that must agree before the board counts as still.
    pub stable_frames: usize,
    /// Mean change of the square colours between two frames above which something
    /// (a hand, a shadow) is moving over the board.
    pub motion_threshold: f32,
    /// A match is accepted when its cost is at most this.
    pub max_cost: f32,
    /// ... and the next best candidate costs at least this much more.
    pub margin: f32,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        TrackerConfig { stable_frames: 3, motion_threshold: 4.0, max_cost: 1.6, margin: 1.0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No calibration, so nothing can be read.
    NoCalibration,
    /// Learning how the squares look from the known position.
    Learning,
    /// The board is still and matches the game.
    Watching,
    /// Something is moving over the board, or the picture is changing.
    Moving,
    /// The board is still but no move explains it clearly.
    Unsure,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub uci: String,
    pub san: String,
    pub cost: f32,
}

#[derive(Debug)]
pub enum Detection {
    Nothing,
    /// A move was recognised. The tracker has already applied it to its position.
    Move {
        mv: Move,
        /// When the board became still after the move.
        since: Instant,
        /// The promotion piece is assumed to be a queen: vision cannot tell.
        promotion_guess: bool,
    },
    /// Two moves were recognised at once: the second player replied before the picture was taken
    /// (or the first move was never seen still). The tracker has applied both.
    Pair {
        first: Move,
        second: Move,
        /// When the board became still after the second move.
        since: Instant,
    },
    /// The board changed but no single move fits well enough.
    Unsure {
        candidates: Vec<Candidate>,
    },
}

/// Tells when the image has stopped changing.
struct Gate {
    stable_frames: usize,
    motion_threshold: f32,
    previous: Option<Vec<f32>>,
    grid: Option<Grid>,
    run: usize,
    since: Option<Instant>,
}

impl Gate {
    fn new(cfg: &TrackerConfig) -> Self {
        Gate {
            stable_frames: cfg.stable_frames.max(1),
            motion_threshold: cfg.motion_threshold,
            previous: None,
            grid: None,
            run: 0,
            since: None,
        }
    }

    fn reset(&mut self) {
        self.previous = None;
        self.grid = None;
        self.run = 0;
        self.since = None;
    }

    /// Adds a frame. Returns when the board became still, once it has been for long enough.
    fn push(&mut self, features: &[Features], grid: Option<Grid>, now: Instant) -> Option<Instant> {
        let flat: Vec<f32> = features.iter().flat_map(|f| f.mean).collect();
        let moved = match &self.previous {
            Some(prev) => {
                let total: f32 = flat.iter().zip(prev).map(|(a, b)| (a - b).abs()).sum();
                total / flat.len() as f32 > self.motion_threshold
            }
            None => true,
        };
        if moved || grid != self.grid {
            self.run = 1;
            self.grid = grid;
            self.since = Some(now);
        } else {
            self.run += 1;
        }
        self.previous = Some(flat);
        if self.run >= self.stable_frames { self.since } else { None }
    }
}

pub struct Tracker {
    cfg: TrackerConfig,
    sampler: Option<Sampler>,
    classifier: Option<Classifier>,
    gate: Gate,
    position: Chess,
    learn_pending: bool,
    /// The still picture that was last examined, so it is not examined twice.
    last_eval: Option<Grid>,
    still: bool,
    unsure: bool,
    /// The moves offered while unsure, kept so that they come back when the same picture is seen
    /// again after a hand passed over the board.
    candidates: Vec<Candidate>,
}

/// What the board should look like for a position.
pub fn expected_grid(pos: &Chess) -> Grid {
    let mut grid = [Cell::Empty; 64];
    for sq in Square::ALL {
        grid[usize::from(sq)] = match pos.board().piece_at(sq).map(|p| p.color) {
            None => Cell::Empty,
            Some(Color::White) => Cell::White,
            Some(Color::Black) => Cell::Black,
        };
    }
    grid
}

/// How badly a picture disagrees with an expected board. A square read with high
/// confidence that contradicts the expectation costs more than a doubtful one.
fn cost(seen: &Grid, confidence: &[f32; 64], expected: &Grid) -> f32 {
    (0..64).filter(|&i| seen[i] != expected[i]).map(|i| 0.5 + confidence[i]).sum()
}

struct Hypothesis {
    cost: f32,
    grid: Grid,
    /// `None` means "no move was played".
    mv: Option<Move>,
}

/// Two moves played one after the other, and what the board would look like after them.
struct PairHypothesis {
    cost: f32,
    grid: Grid,
    first: Move,
    second: Move,
    /// How many of the two moves are promotions to a queen (the one vision cannot tell apart from other pieces).
    queens: u8,
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        let gate = Gate::new(&cfg);
        Tracker {
            cfg,
            sampler: None,
            classifier: None,
            gate,
            position: Chess::default(),
            learn_pending: true,
            last_eval: None,
            still: false,
            unsure: false,
            candidates: Vec::new(),
        }
    }

    pub fn status(&self) -> Status {
        if self.sampler.is_none() {
            Status::NoCalibration
        } else if self.learn_pending {
            Status::Learning
        } else if !self.still {
            Status::Moving
        } else if self.unsure {
            Status::Unsure
        } else {
            Status::Watching
        }
    }

    #[cfg(test)]
    pub fn position(&self) -> &Chess {
        &self.position
    }

    /// Sets (or clears) the calibration. The look of the squares is learned again.
    pub fn set_calibration(&mut self, calibration: Option<&Calibration>) {
        self.sampler = calibration.and_then(Sampler::new);
        self.relearn();
    }

    /// The board moved a little in the picture: look at the squares where they are now, but
    /// keep what was learned about them (the position is still the same).
    pub fn move_calibration(&mut self, calibration: &Calibration) {
        if let Some(sampler) = Sampler::new(calibration) {
            self.sampler = Some(sampler);
            self.gate.reset();
        }
    }

    /// Starts following a game from `position`, which the board must show.
    pub fn start(&mut self, position: Chess) {
        self.position = position;
        self.relearn();
    }

    /// The position changed outside the tracker (manual move or undo).
    pub fn set_position(&mut self, position: Chess) {
        self.position = position;
        self.last_eval = None;
        self.unsure = false;
    }

    /// Learns the look of the squares again from the current position.
    pub fn relearn(&mut self) {
        self.classifier = None;
        self.learn_pending = true;
        self.last_eval = None;
        self.unsure = false;
        self.still = false;
        self.gate.reset();
    }

    pub fn feed(&mut self, image: &RgbImage, now: Instant) -> Detection {
        let Some(sampler) = &self.sampler else { return Detection::Nothing };
        let features = sampler.features(image);

        if self.learn_pending || self.classifier.is_none() {
            if self.gate.push(&features, None, now).is_some() {
                let labels = expected_grid(&self.position);
                if let Ok(classifier) = Classifier::learn(&features, &labels) {
                    self.classifier = Some(classifier);
                    self.learn_pending = false;
                    self.last_eval = Some(labels);
                    self.still = true;
                    self.gate.reset();
                }
            }
            return Detection::Nothing;
        }

        let classifier = self.classifier.as_ref().expect("checked above");
        let (grid, confidence) = classifier.classify_all(&features);
        let Some(since) = self.gate.push(&features, Some(grid), now) else {
            self.still = false;
            return Detection::Nothing;
        };
        self.still = true;
        if self.last_eval == Some(grid) {
            return if self.unsure { Detection::Unsure { candidates: self.candidates.clone() } } else { Detection::Nothing };
        }
        self.last_eval = Some(grid);
        self.evaluate(&grid, &confidence, since)
    }

    fn evaluate(&mut self, seen: &Grid, confidence: &[f32; 64], since: Instant) -> Detection {
        let mut options = vec![Hypothesis {
            cost: cost(seen, confidence, &expected_grid(&self.position)),
            grid: expected_grid(&self.position),
            mv: None,
        }];
        for mv in self.position.legal_moves() {
            let mut after = self.position.clone();
            after.play_unchecked(mv);
            let grid = expected_grid(&after);
            // Promotions to different pieces look the same: keep the queen.
            if let Some(existing) = options.iter_mut().find(|o| o.grid == grid) {
                if mv.promotion() == Some(Role::Queen) {
                    existing.mv = Some(mv);
                }
                continue;
            }
            options.push(Hypothesis { cost: cost(seen, confidence, &grid), grid, mv: Some(mv) });
        }
        options.sort_by(|a, b| a.cost.total_cmp(&b.cost));

        let best = &options[0];
        let runner_up = options.get(1).map_or(f32::INFINITY, |o| o.cost);
        if best.cost <= self.cfg.max_cost && runner_up - best.cost >= self.cfg.margin {
            self.unsure = false;
            return match best.mv {
                None => Detection::Nothing,
                Some(mv) => {
                    self.position.play_unchecked(mv);
                    // The picture now matches the new position.
                    self.last_eval = Some(*seen);
                    Detection::Move { mv, since, promotion_guess: mv.promotion().is_some() }
                }
            };
        }

        // No single move explains the board (or two do equally well). Perhaps two moves were played
        // between two still pictures: look at pairs. One is believed on the same terms as a single move
        // (a good fit, and at least `margin` better than the runner-up) and must also beat the best single
        // move by that margin.
        let best_single = best.cost;
        let pairs = if best_single > self.cfg.max_cost { self.pair_hypotheses(seen, confidence, best_single) } else { Vec::new() };
        if let Some(found) = pairs.first() {
            let next = pairs.get(1).map_or(f32::INFINITY, |p| p.cost);
            if found.cost <= self.cfg.max_cost && next - found.cost >= self.cfg.margin && best_single - found.cost >= self.cfg.margin {
                let (first, second) = (found.first, found.second);
                self.position.play_unchecked(first);
                self.position.play_unchecked(second);
                self.unsure = false;
                self.last_eval = Some(*seen);
                return Detection::Pair { first, second, since };
            }
        }

        self.unsure = true;
        // The moves to choose from: the likeliest singles, and the pairs that fit better than any single.
        let mut offered: Vec<Candidate> = pairs.iter().take(2).map(|p| self.pair_candidate(p)).collect();
        offered.extend(
            options.iter().filter_map(|o| o.mv.map(|mv| Candidate { uci: uci_of(mv), san: san_of(&self.position, mv), cost: o.cost })),
        );
        offered.sort_by(|a, b| a.cost.total_cmp(&b.cost));
        offered.truncate(4);
        self.candidates = offered.clone();
        Detection::Unsure { candidates: offered }
    }

    /// The pairs of moves (one by each side) after which the board would look the most like the picture,
    /// best first, at most four. Only those that fit better than `must_beat` are kept: a pair that does
    /// not beat the best single move is of no use.
    fn pair_hypotheses(&self, seen: &Grid, confidence: &[f32; 64], must_beat: f32) -> Vec<PairHypothesis> {
        let mut found: Vec<PairHypothesis> = Vec::new();
        for first in self.position.legal_moves() {
            let mut after_first = self.position.clone();
            after_first.play_unchecked(first);
            for second in after_first.legal_moves() {
                let mut after_second = after_first.clone();
                after_second.play_unchecked(second);
                let grid = expected_grid(&after_second);
                let cost = cost(seen, confidence, &grid);
                if cost >= must_beat {
                    continue;
                }
                let queens = u8::from(first.promotion() == Some(Role::Queen)) + u8::from(second.promotion() == Some(Role::Queen));
                // Promotions to different pieces look the same: keep the queens.
                if let Some(existing) = found.iter_mut().find(|p| p.grid == grid) {
                    if queens > existing.queens {
                        *existing = PairHypothesis { cost, grid, first, second, queens };
                    }
                    continue;
                }
                found.push(PairHypothesis { cost, grid, first, second, queens });
            }
        }
        found.sort_by(|a, b| a.cost.total_cmp(&b.cost));
        found.truncate(4);
        found
    }

    /// A pair of moves as something the page can offer: "e2e4 e7e5" to send, "e4 e5" to show.
    fn pair_candidate(&self, pair: &PairHypothesis) -> Candidate {
        let mut after_first = self.position.clone();
        let first_san = san_of(&after_first, pair.first);
        after_first.play_unchecked(pair.first);
        let second_san = san_of(&after_first, pair.second);
        Candidate {
            uci: format!("{} {}", uci_of(pair.first), uci_of(pair.second)),
            san: format!("{first_san} {second_san}"),
            cost: pair.cost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::{GameRecord, parse_fen};
    use crate::synthetic::{self, OPERA_GAME, PieceGrid};
    use std::time::Duration;

    /// Feeds frames with a poor camera: darker, noisy, different noise each time.
    struct Rig {
        tracker: Tracker,
        now: Instant,
        frames: u64,
    }

    impl Rig {
        fn new(position: Chess) -> Rig {
            let mut tracker = Tracker::new(TrackerConfig::default());
            tracker.set_calibration(Some(&synthetic::corners()));
            tracker.start(position);
            Rig { tracker, now: Instant::now(), frames: 0 }
        }

        fn show(&mut self, grid: &PieceGrid, times: usize) -> Vec<Detection> {
            let mut out = Vec::new();
            for _ in 0..times {
                self.frames += 1;
                self.now += Duration::from_millis(333);
                let mut frame = synthetic::frame_from_grid(grid);
                synthetic::degrade(&mut frame, -15, 10, self.frames);
                match self.tracker.feed(&frame, self.now) {
                    Detection::Nothing => {}
                    other => out.push(other),
                }
            }
            out
        }
    }

    #[test]
    fn follows_the_whole_opera_game() {
        let steps = synthetic::game_steps(&OPERA_GAME);
        let mut rig = Rig::new(Chess::default());
        // Three still frames to learn the squares, three more to settle.
        assert!(rig.show(&synthetic::start_grid(), 7).is_empty());
        assert_eq!(rig.tracker.status(), Status::Watching);

        let mut seen = Vec::new();
        for step in &steps {
            // The piece is lifted off its square for one frame (a hand is over the board).
            let mut lifted = step.before;
            lifted[usize::from(step.mv.from().unwrap())] = None;
            assert!(rig.show(&lifted, 1).is_empty());
            for detection in rig.show(&step.after, 4) {
                match detection {
                    Detection::Move { mv, .. } => seen.push(uci_of(mv)),
                    other => panic!("unexpected {other:?} after {}", uci_of(step.mv)),
                }
            }
        }
        let expected: Vec<String> = steps.iter().map(|s| uci_of(s.mv)).collect();
        assert_eq!(seen, expected);
        assert!(rig.tracker.position().is_checkmate());
    }

    #[test]
    fn ignores_a_hand_and_a_still_board() {
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 4);

        // A long time with nothing happening: no move.
        assert!(rig.show(&synthetic::start_grid(), 10).is_empty());

        // Pieces come and go for two frames only (a hand): still no move.
        let steps = synthetic::game_steps(&["e4"]);
        assert!(rig.show(&steps[0].after, 2).is_empty());
        assert_eq!(rig.tracker.status(), Status::Moving);
        assert!(rig.show(&synthetic::start_grid(), 4).is_empty());
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    #[test]
    fn two_moves_played_between_two_pictures_are_recognised() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3"]);
        let mut rig = Rig::new(Chess::default());
        assert!(rig.show(&synthetic::start_grid(), 7).is_empty());

        // White moves and Black answers before the picture is taken: the board shows both.
        let detections = rig.show(&steps[1].after, 4);
        match detections.as_slice() {
            [Detection::Pair { first, second, .. }] => {
                assert_eq!((uci_of(*first), uci_of(*second)), ("e2e4".to_string(), "e7e5".to_string()))
            }
            other => panic!("expected one pair, got {other:?}"),
        }
        assert_eq!(rig.tracker.status(), Status::Watching);
        // The tracker is in step with the game: the next ordinary move is a single move again.
        let detections = rig.show(&steps[2].after, 4);
        assert!(matches!(detections.as_slice(), [Detection::Move { mv, .. }] if uci_of(*mv) == "g1f3"), "{detections:?}");
    }

    #[test]
    fn a_pair_with_captures_is_recognised_too() {
        // 1.e4 e5 2.Nf3 d6 3.d4 Bg4 are seen one move at a time; then dxe5 Bxf3 at once.
        let steps = synthetic::game_steps(&OPERA_GAME);
        let mut rig = Rig::new(Chess::default());
        assert!(rig.show(&synthetic::start_grid(), 7).is_empty());
        for step in &steps[..6] {
            assert!(matches!(rig.show(&step.after, 4).as_slice(), [Detection::Move { .. }]));
        }
        let detections = rig.show(&steps[7].after, 4);
        match detections.as_slice() {
            [Detection::Pair { first, second, .. }] => {
                assert_eq!((uci_of(*first), uci_of(*second)), ("d4e5".to_string(), "g4f3".to_string()))
            }
            other => panic!("expected one pair, got {other:?}"),
        }
    }

    #[test]
    fn unsure_offers_the_pair_that_fits_best_but_is_not_believed_alone() {
        // A board that matches no move and no pair well: three pieces vanish. Nothing is guessed.
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 4);
        let mut broken = synthetic::start_grid();
        for sq in [Square::A2, Square::C2, Square::G7, Square::H8] {
            broken[usize::from(sq)] = None;
        }
        let detections = rig.show(&broken, 4);
        assert!(detections.iter().all(|d| matches!(d, Detection::Unsure { .. })), "{detections:?}");
        assert_eq!(rig.tracker.status(), Status::Unsure);
    }

    #[test]
    fn reports_when_no_move_explains_the_board() {
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 4);

        // Three pieces vanish at once: no legal move does that.
        let mut broken = synthetic::start_grid();
        for sq in [Square::A2, Square::C2, Square::G7] {
            broken[usize::from(sq)] = None;
        }
        let detections = rig.show(&broken, 4);
        assert!(!detections.is_empty());
        assert!(detections.iter().all(|d| matches!(d, Detection::Unsure { candidates } if !candidates.is_empty())));
        assert_eq!(rig.tracker.status(), Status::Unsure);

        // A hand passes over the board and goes away: the same picture is seen again, and the
        // moves to choose from come back (they must not vanish while the page asks for a choice).
        rig.show(&synthetic::start_grid(), 1);
        let again = rig.show(&broken, 4);
        assert!(again.iter().any(|d| matches!(d, Detection::Unsure { candidates } if !candidates.is_empty())));
        assert_eq!(rig.tracker.status(), Status::Unsure);

        // Putting the pieces back and playing a real move recovers.
        rig.show(&synthetic::start_grid(), 4);
        let steps = synthetic::game_steps(&["e4"]);
        let detections = rig.show(&steps[0].after, 4);
        assert!(matches!(detections.as_slice(), [Detection::Move { .. }]));
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    #[test]
    fn a_manual_correction_resynchronises() {
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 4);
        let steps = synthetic::game_steps(&["e4"]);

        // The user enters 1.e4 by hand before the camera sees it.
        let mut pos = Chess::default();
        pos.play_unchecked(steps[0].mv);
        rig.tracker.set_position(pos);
        assert!(rig.show(&steps[0].after, 4).is_empty(), "the board already matches");
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    #[test]
    fn promotion_defaults_to_a_queen_and_is_flagged() {
        let pos = parse_fen("4k3/P7/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let mut rig = Rig::new(pos.clone());
        rig.show(&synthetic::grid_from_position(&pos), 4);

        let mut after = synthetic::grid_from_position(&pos);
        after[usize::from(Square::A7)] = None;
        after[usize::from(Square::A8)] = Some(Color::White);
        let detections = rig.show(&after, 4);
        match detections.as_slice() {
            [Detection::Move { mv, promotion_guess, .. }] => {
                assert_eq!(uci_of(*mv), "a7a8q");
                assert!(promotion_guess);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_game_record_and_the_tracker_agree() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3"]);
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 4);
        let mut game = GameRecord::new("t".into(), None, "A", "B", None).unwrap();
        for step in &steps {
            for d in rig.show(&step.after, 4) {
                if let Detection::Move { mv, .. } = d {
                    game.push_move(mv, 1.0, false).unwrap();
                }
            }
        }
        assert_eq!(game.moves.iter().map(|m| m.san.as_str()).collect::<Vec<_>>(), ["e4", "e5", "Nf3"]);
        assert_eq!(game.position().unwrap(), *rig.tracker.position());
    }
}
