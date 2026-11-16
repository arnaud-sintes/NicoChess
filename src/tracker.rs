//! Follows a game on the board: waits for the image to settle, then finds which legal moves
//! explain what changed.
//!
//! Vision only says, square by square, how well the picture fits "empty", "white" or "black"
//! ([`Models`], which learn what every square looks like, and keep learning). The tracker keeps the
//! real position, so it already knows the piece types.
//!
//! For every way the game may have gone on (no move, any one legal move, any two, any three) it
//! works out what the board should look like and how likely the picture is then: the sum, over
//! the squares, of how badly each square fits what it should hold. The likeliest way is accepted
//! when it beats the next one by a clear margin, and explains the picture: no square is
//! left with a misfit that the last confirmed position did not already have.
//!
//! Judging by what changed, not by a fixed limit on the misfit, is what keeps the tracking when a
//! square is persistently odd (a glare, a shadow, a piece the models never saw there): that oddity is
//! the same for every explanation, and the models take it in by learning from every confirmed move.
//! The same learning follows a light that fades during the game.
// Numbers per feature are kept in parallel arrays: indexing them together reads best.
#![allow(clippy::needless_range_loop)]

use crate::calibration::Calibration;
use crate::game::{san_of, uci_of};
use crate::vision::{ADAPT_WEIGHT, Cell, DIM, Features, Grid, Models, Sampler, difference, frame_change};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use shakmaty::{Chess, Color, Move, Position, Role, Square};
use std::collections::HashMap;
use std::time::Instant;

/// The settings of the tracker. They are kept in a recording (see `record`), so that a replay can use the
/// ones the game was played with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Frames in a row that must agree before the board counts as still.
    pub stable_frames: usize,
    /// Mean change of the square colours between two frames above which something
    /// (a hand, a shadow) is moving over the board.
    pub motion_threshold: f32,
    /// How much misfit, in nats, a picture may have left over once the likeliest explanation is
    /// taken, beyond what the squares usually show (see [`Models::typical`]), added up over the squares.
    /// A single square may not be more than [`SQUARE_LIMIT`] off.
    pub max_excess: f32,
    /// How much better than the next explanation, in nats, the likeliest one must be.
    /// 6 nats is about 400 to 1.
    pub margin: f32,
    /// How many moves in a row may have been played between two pictures (1 to 3).
    pub max_plies: usize,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        TrackerConfig { stable_frames: 3, motion_threshold: 4.0, max_excess: 60.0, margin: 6.0, max_plies: 3 }
    }
}

/// What a square may misfit, in nats, before it counts as unexplained: the noise on top of the usual.
const SLACK: f32 = 4.0;
/// What each move beyond "nothing happened" costs: of two explanations that fit as well, the shorter wins.
const PLY_PENALTY: f32 = 1.5;
/// What each move beyond the first costs on top: a second move can always soak up a misfit somewhere else on
/// the board, so it must explain a lot to be believed.
const EXTRA_PLY_PENALTY: f32 = 6.0;
/// How much the change since the last confirmed picture counts against the squares of the likelihood.
const CHANGE_WEIGHT: f32 = 1.0;
/// What it costs when the explanation says a square changed and it looks just like before (at most).
const NO_CHANGE_PENALTY: f32 = 14.0;
/// How much a single square may be unexplained, in nats: a leaning piece or a glare spoils several squares a
/// little, a piece that was knocked over or a hand left on the board spoils one square a lot.
const SQUARE_LIMIT: f32 = 45.0;
/// Nothing or one move is believed on its own, without looking at several moves, when it leaves at most this
/// fraction of `max_excess` unexplained.
const SIMPLE_FRACTION: f32 = 0.5;
/// "Nothing happened" and a misfit up to this many times `max_excess`: the light or the models have
/// drifted, not the board: the models learn the picture.
const ADAPT_FACTOR: f32 = 3.0;
/// The same, right after a person has set the position by hand: the picture is believed more.
const RESYNC_FACTOR: f32 = 4.0;
/// How much a picture the person has confirmed counts in the models, against 1 for a move the tracker saw by itself.
const RESYNC_WEIGHT: f32 = 6.0;
/// A move may leave up to `ADAPT_FACTOR` times `max_excess` unexplained, if it wins by this many margins and
/// no single square is far off.
const DIFFUSE_GAP: f32 = 4.0;
/// Right after a hand-set position, a move must win by this many margins to be believed over "nothing happened".
const RESYNC_MOVE_MARGIN: f32 = 4.0;

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
    /// One move as UCI, or several separated by spaces.
    pub uci: String,
    /// The same as SAN.
    pub san: String,
    /// How much less likely than the likeliest explanation, in nats.
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
    /// Two or three moves were recognised at once: the reply was played before the picture was taken
    /// (or the pictures in between were not seen). The tracker has applied them all.
    Sequence {
        moves: Vec<Move>,
        /// When the board became still after the last move.
        since: Instant,
    },
    /// The board changed but no move fits well enough.
    Unsure {
        candidates: Vec<Candidate>,
    },
}

/// Tells when the image has stopped changing, and keeps the average of the pictures since.
///
/// The board is moving when the squares change on the whole (a hand, a shadow) or when any single square
/// changes a lot between two pictures (a piece was lifted or put down). It is still when neither happened
/// for `stable_frames` pictures in a row.
struct Gate {
    stable_frames: usize,
    motion_threshold: f32,
    previous: Option<Vec<Features>>,
    run: usize,
    since: Option<Instant>,
    /// The sum of the features of the pictures of this run.
    sum: Vec<[f32; DIM]>,
}

/// How much one square must change between two pictures (see [`frame_change`]) to count as a piece coming or going.
const SQUARE_CHANGE: f32 = 30.0;

impl Gate {
    fn new(cfg: &TrackerConfig) -> Self {
        Gate {
            stable_frames: cfg.stable_frames.max(1),
            motion_threshold: cfg.motion_threshold,
            previous: None,
            run: 0,
            since: None,
            sum: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.previous = None;
        self.run = 0;
        self.since = None;
        self.sum.clear();
    }

    /// Adds a frame. Returns when the board became still, once it has been for long enough.
    fn push(&mut self, features: &[Features], now: Instant) -> Option<Instant> {
        let moved = match &self.previous {
            Some(prev) => {
                let colours: f32 = features
                    .iter()
                    .zip(prev)
                    .map(|(a, b)| a.middle().iter().zip(b.middle()).map(|(x, y)| (x - y).abs()).sum::<f32>())
                    .sum();
                let on_the_whole = colours / (3.0 * features.len() as f32) > self.motion_threshold;
                on_the_whole || features.iter().zip(prev).any(|(a, b)| frame_change(a, b) > SQUARE_CHANGE)
            }
            None => true,
        };
        if moved {
            self.run = 1;
            self.since = Some(now);
            self.sum = features.iter().map(|f| f.v).collect();
        } else {
            self.run += 1;
            for (total, f) in self.sum.iter_mut().zip(features) {
                for d in 0..DIM {
                    total[d] += f.v[d];
                }
            }
        }
        self.previous = Some(features.to_vec());
        if self.run >= self.stable_frames { self.since } else { None }
    }

    /// The features averaged over the pictures of this run: less noise than a single picture.
    fn average(&self) -> Vec<Features> {
        let n = self.run.max(1) as f32;
        self.sum.iter().map(|total| Features { v: total.map(|x| x / n) }).collect()
    }
}

pub struct Tracker {
    cfg: TrackerConfig,
    sampler: Option<Sampler>,
    models: Option<Models>,
    gate: Gate,
    position: Chess,
    learn_pending: bool,
    /// The position was set by hand: the next still picture that fits it teaches the models.
    resync_pending: bool,
    /// The still picture that was last examined, so it is not examined twice.
    last_eval: Option<Grid>,
    still: bool,
    unsure: bool,
    /// The moves offered while unsure, kept so that they come back when the same picture is seen
    /// again after a hand passed over the board.
    candidates: Vec<Candidate>,
    /// What the board looked like (light corrected) when the position was last confirmed. Absent after a
    /// person set the position by hand, until a picture of it is seen.
    reference: Option<Vec<Features>>,
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

/// What the picture says, to be held against every explanation.
struct Evidence {
    /// What each square costs for each content, and what it usually costs.
    costs: Vec<[f32; 3]>,
    typical: Vec<[f32; 3]>,
    /// The board of the last confirmed position: the explanations are changes from it.
    base: Grid,
    /// Square by square, what the picture costs if the square did not change since the last confirmed
    /// picture, and if it did. Absent when there is no such picture.
    change: Option<(Vec<f32>, Vec<f32>)>,
}

impl Evidence {
    fn new(models: &Models, features: &[Features], base: Grid, reference: Option<&[Features]>) -> Evidence {
        let change = reference.map(|before| {
            let mut unchanged = Vec::with_capacity(64);
            let mut changed = Vec::with_capacity(64);
            for (now, then) in features.iter().zip(before).take(64) {
                let d = difference(now, then);
                // Unchanged: how strange the difference is. Changed: how strange it is that there is none.
                unchanged.push(0.5 * d);
                changed.push(NO_CHANGE_PENALTY * (-(d / DIM as f32) / 3.0).exp());
            }
            (unchanged, changed)
        });
        Evidence { costs: models.costs(features), typical: models.typical().to_vec(), base, change }
    }

    /// How the picture fits a board: the misfit including what changed since the last confirmed picture, how
    /// much of the misfit is beyond what each square usually shows, and the worst square of those.
    fn fit(&self, grid: &Grid) -> (f32, f32, f32) {
        let (mut nll, mut excess, mut worst) = (0.0f32, 0.0f32, 0.0f32);
        for square in 0..64 {
            let c = grid[square].index();
            let cost = self.costs[square][c];
            nll += cost;
            let beyond = (cost - self.typical[square][c] - SLACK).max(0.0);
            excess += beyond;
            worst = worst.max(beyond);
            if let Some((unchanged, changed)) = &self.change {
                nll += CHANGE_WEIGHT * if grid[square] == self.base[square] { unchanged[square] } else { changed[square] };
            }
        }
        (nll, excess, worst)
    }
}

fn grid_key(grid: &Grid) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for cell in grid {
        hash = (hash ^ cell.index() as u64).wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// One way the game may have gone on since the last confirmed position.
struct Hypothesis {
    /// The explanation this one extends (one move fewer), and the move that extends it.
    parent: Option<usize>,
    mv: Option<Move>,
    plies: u8,
    /// How much of the picture's misfit with this board nobody can explain, in all, and on the worst square.
    excess: f32,
    worst: f32,
    /// `nll` plus the price of the moves: what the explanations are ranked by.
    score: f32,
}

/// All the explanations looked at, each board once (two ways to the same board count as one).
struct Pool {
    hyps: Vec<Hypothesis>,
    positions: Vec<Option<Chess>>,
    seen: HashMap<u64, usize>,
}

impl Pool {
    fn new(position: &Chess, evidence: &Evidence) -> Pool {
        let grid = expected_grid(position);
        let (nll, excess, worst) = evidence.fit(&grid);
        let mut seen = HashMap::new();
        seen.insert(grid_key(&grid), 0);
        Pool {
            hyps: vec![Hypothesis { parent: None, mv: None, plies: 0, excess, worst, score: nll }],
            positions: vec![Some(position.clone())],
            seen,
        }
    }

    /// Adds every way to play one more move after the explanations of `depth - 1` moves.
    fn expand(&mut self, depth: u8, evidence: &Evidence) {
        let parents: Vec<usize> =
            (0..self.hyps.len()).filter(|&i| self.hyps[i].plies + 1 == depth && self.positions[i].is_some()).collect();
        for parent in parents {
            let Some(position) = self.positions[parent].clone() else { continue };
            for mv in position.legal_moves() {
                let mut child = position.clone();
                child.play_unchecked(mv);
                let grid = expected_grid(&child);
                let key = grid_key(&grid);
                let (nll, excess, worst) = evidence.fit(&grid);
                let score = nll + PLY_PENALTY * f32::from(depth) + EXTRA_PLY_PENALTY * f32::from(depth - 1);
                let hypothesis = Hypothesis { parent: Some(parent), mv: Some(mv), plies: depth, excess, worst, score };
                let keep = depth < 3;
                match self.seen.get(&key) {
                    Some(&i) => {
                        // Promotions to different pieces look the same: keep the queen. Else the shorter way wins.
                        let existing = &self.hyps[i];
                        let queen = mv.promotion() == Some(Role::Queen) && existing.mv.is_some_and(|m| m.promotion() != Some(Role::Queen));
                        if score < existing.score - 1e-3 || (existing.plies == depth && queen) {
                            self.hyps[i] = hypothesis;
                            self.positions[i] = keep.then_some(child);
                        }
                    }
                    None => {
                        self.seen.insert(key, self.hyps.len());
                        self.hyps.push(hypothesis);
                        self.positions.push(keep.then_some(child));
                    }
                }
            }
        }
    }

    /// The likeliest explanation of at most `max_plies` moves, and the score of the next one (infinite if there is none).
    fn best_within(&self, max_plies: u8) -> (usize, f32) {
        let mut best = 0;
        for i in 1..self.hyps.len() {
            if self.hyps[i].plies <= max_plies && self.hyps[i].score < self.hyps[best].score {
                best = i;
            }
        }
        let runner_up = (0..self.hyps.len())
            .filter(|&i| i != best && self.hyps[i].plies <= max_plies)
            .map(|i| self.hyps[i].score)
            .fold(f32::INFINITY, f32::min);
        (best, runner_up)
    }

    /// The moves of an explanation, in order.
    fn moves(&self, mut i: usize) -> Vec<Move> {
        let mut moves = Vec::new();
        while let Some(mv) = self.hyps[i].mv {
            moves.push(mv);
            match self.hyps[i].parent {
                Some(parent) => i = parent,
                None => break,
            }
        }
        moves.reverse();
        moves
    }
}

impl Tracker {
    pub fn new(cfg: TrackerConfig) -> Self {
        let gate = Gate::new(&cfg);
        Tracker {
            cfg,
            sampler: None,
            models: None,
            gate,
            position: Chess::default(),
            learn_pending: true,
            resync_pending: false,
            last_eval: None,
            still: false,
            unsure: false,
            candidates: Vec::new(),
            reference: None,
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

    /// The position the tracker believes the board shows.
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

    /// The position changed outside the tracker (manual move or undo): the person says what the board
    /// shows. The next still picture that fits teaches the models, so that tracking goes on by itself.
    pub fn set_position(&mut self, position: Chess) {
        self.position = position;
        self.last_eval = None;
        self.unsure = false;
        self.resync_pending = true;
        // The reference shows another position: it says nothing about what changed.
        self.reference = None;
    }

    /// Learns the look of the squares again from the current position.
    pub fn relearn(&mut self) {
        self.models = None;
        self.reference = None;
        self.learn_pending = true;
        self.resync_pending = false;
        self.last_eval = None;
        self.unsure = false;
        self.still = false;
        self.gate.reset();
    }

    pub fn feed(&mut self, image: &RgbImage, now: Instant) -> Detection {
        let Some(sampler) = &self.sampler else { return Detection::Nothing };
        let mut features = sampler.features(image);

        if self.learn_pending || self.models.is_none() {
            if self.gate.push(&features, now).is_some() {
                let labels = expected_grid(&self.position);
                let learned = self.gate.average();
                if let Ok(models) = Models::learn(&learned, &labels) {
                    self.models = Some(models);
                    self.reference = Some(learned);
                    self.learn_pending = false;
                    self.last_eval = Some(labels);
                    self.still = true;
                    self.gate.reset();
                }
            }
            return Detection::Nothing;
        }

        let models = self.models.as_ref().expect("checked above");
        // A change of the light all over the board is taken out first.
        models.compensate_light(&mut features, &expected_grid(&self.position));
        let Some(since) = self.gate.push(&features, now) else {
            self.still = false;
            return Detection::Nothing;
        };
        self.still = true;
        // What the board shows is read from the pictures averaged since it stopped moving: steadier than one.
        let averaged = self.gate.average();
        let grid = Models::read(&models.costs(&averaged));
        if self.last_eval == Some(grid) {
            return if self.unsure { Detection::Unsure { candidates: self.candidates.clone() } } else { Detection::Nothing };
        }
        self.last_eval = Some(grid);
        self.evaluate(&averaged, since)
    }

    /// Which explanation of the still picture is the likeliest, and whether it is clear enough to believe.
    fn evaluate(&mut self, features: &[Features], since: Instant) -> Detection {
        let Some(models) = self.models.as_ref() else { return Detection::Nothing };
        let evidence = Evidence::new(models, features, expected_grid(&self.position), self.reference.as_deref());

        // Nothing, one move, two moves: always looked at together (a reply played before the picture was
        // taken must not be mistaken for one move). Three only when those are not clear.
        let max_plies = self.cfg.max_plies.clamp(1, 3) as u8;
        let mut pool = Pool::new(&self.position, &evidence);
        for depth in 1..=max_plies.min(2) {
            pool.expand(depth, &evidence);
        }
        // Two moves are only believed once three have been looked at too: a pair that leaves a low-contrast
        // change unexplained (a white pawn on a light square) can look best only because the third move is missing.
        if let Some(detection) = self.decide(&pool, features, since, max_plies <= 2) {
            return detection;
        }
        if max_plies >= 3 {
            pool.expand(3, &evidence);
            if let Some(detection) = self.decide(&pool, features, since, true) {
                return detection;
            }
        }

        // Nothing is clear: offer the likeliest explanations.
        let (best, _) = pool.best_within(3);
        let best_score = pool.hyps[best].score;
        let mut order: Vec<usize> = (0..pool.hyps.len()).filter(|&i| pool.hyps[i].plies >= 1).collect();
        order.sort_by(|&a, &b| pool.hyps[a].score.total_cmp(&pool.hyps[b].score));
        let candidates: Vec<Candidate> = order
            .into_iter()
            .take(4)
            .map(|i| {
                let (uci, san) = self.describe(&pool.moves(i));
                Candidate { uci, san, cost: pool.hyps[i].score - best_score }
            })
            .collect();
        self.unsure = true;
        self.candidates = candidates.clone();
        Detection::Unsure { candidates }
    }

    /// Accepts the likeliest explanation if it is clear, or learns the picture if the board has not
    /// changed but looks different. `None` when nothing is clear.
    ///
    /// Nothing or one move are tried first: when one of them explains the picture there is no reason to
    /// invent more moves. Several are only looked at when the simple explanations leave too much unexplained.
    fn decide(&mut self, pool: &Pool, features: &[Features], since: Instant, last_stage: bool) -> Option<Detection> {
        let (simple, simple_runner_up) = pool.best_within(1);
        // A person has just said what the board shows, and the camera has had no time to see more than a reply:
        // a square that looks odd is the models being out of date, not two or three moves nobody saw. Without
        // this, one misread square can be "explained" by invented moves, and the tracker is off for good.
        let limit = if self.resync_pending { RESYNC_FACTOR } else { SIMPLE_FRACTION };
        if self.resync_pending
            && pool.hyps[simple].plies >= 1
            && simple_runner_up - pool.hyps[simple].score < RESYNC_MOVE_MARGIN * self.cfg.margin
            && pool.hyps[0].excess <= RESYNC_FACTOR * self.cfg.max_excess
        {
            // The models are stale after a hand-set position, so a move that only looks somewhat better than the
            // position the person typed (a queen leaning over its neighbour) is not believed: the typed position
            // is adopted and the models learn from it.
            return self.accept(pool, 0, simple_runner_up, features, since);
        }
        if pool.hyps[simple].excess <= limit * self.cfg.max_excess {
            if simple_runner_up - pool.hyps[simple].score < self.cfg.margin {
                return None; // two simple explanations fit alike: more moves would not help
            }
            return self.accept(pool, simple, simple_runner_up, features, since);
        }
        // The simple explanations leave too much unexplained: several moves may have been played.
        let (best, runner_up) = pool.best_within(3);
        if pool.hyps[best].plies >= 2 && !last_stage {
            return None; // wait until one more move has been looked at (see `evaluate`)
        }
        // Several moves at once must be clearer than one: there are many more ways to look alike.
        let needed = self.cfg.margin * (1.0 + 0.5 * f32::from(pool.hyps[best].plies.saturating_sub(1)));
        if runner_up - pool.hyps[best].score < needed {
            return None;
        }
        self.accept(pool, best, runner_up, features, since)
    }

    fn accept(&mut self, pool: &Pool, index: usize, runner_up: f32, features: &[Features], since: Instant) -> Option<Detection> {
        let best = &pool.hyps[index];
        let gap = runner_up - best.score;

        if best.plies == 0 {
            // The board shows the position the game is in.
            if self.resync_pending && best.excess <= RESYNC_FACTOR * self.cfg.max_excess {
                // A person set this position: whatever the picture looks like now is what it looks like.
                self.teach(features, RESYNC_WEIGHT);
                self.reference = Some(features.to_vec());
            } else if best.excess > self.cfg.max_excess || best.worst > SQUARE_LIMIT {
                if best.excess > ADAPT_FACTOR * self.cfg.max_excess || best.worst > SQUARE_LIMIT {
                    return None; // too much is unexplained, or one square is: not a drift of the light
                }
                self.teach(features, ADAPT_WEIGHT);
                self.reference = Some(features.to_vec());
            }
            self.resync_pending = false;
            self.unsure = false;
            return Some(Detection::Nothing);
        }

        // A misfit spread thinly over many squares (no square far off) is the models being out of date, not a
        // hand or a knocked piece: it is allowed to be larger when the move wins clearly.
        let diffuse =
            best.excess > self.cfg.max_excess && best.excess <= ADAPT_FACTOR * self.cfg.max_excess && gap >= DIFFUSE_GAP * self.cfg.margin;
        if (best.excess > self.cfg.max_excess && !diffuse) || best.worst > SQUARE_LIMIT {
            return None;
        }
        let moves = pool.moves(index);
        for mv in &moves {
            self.position.play_unchecked(*mv);
        }
        // A move seen clearly teaches the models what the board looks like now, and more when they were behind.
        if gap >= 2.0 * self.cfg.margin {
            self.teach(features, if diffuse { ADAPT_WEIGHT } else { 1.0 });
        }
        self.reference = Some(features.to_vec());
        self.resync_pending = false;
        self.unsure = false;
        Some(match moves.as_slice() {
            [mv] => Detection::Move { mv: *mv, since, promotion_guess: mv.promotion().is_some() },
            _ => Detection::Sequence { moves, since },
        })
    }

    /// The models learn the picture as the look of the current position.
    fn teach(&mut self, features: &[Features], weight: f32) {
        let labels = expected_grid(&self.position);
        if let Some(models) = self.models.as_mut() {
            models.update(features, &labels, weight);
        }
    }

    /// Moves played one after the other, as UCI and as SAN, each separated by spaces.
    fn describe(&self, moves: &[Move]) -> (String, String) {
        let mut position = self.position.clone();
        let (mut uci, mut san) = (Vec::new(), Vec::new());
        for mv in moves {
            san.push(san_of(&position, *mv));
            uci.push(uci_of(*mv));
            position.play_unchecked(*mv);
        }
        (uci.join(" "), san.join(" "))
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

    /// Plays SAN moves on a position and returns every position reached.
    fn positions_after(sans: &[&str]) -> Vec<Chess> {
        let mut pos = Chess::default();
        let mut out = vec![pos.clone()];
        for text in sans {
            let mv = text.parse::<shakmaty::san::San>().expect("valid SAN").to_move(&pos).expect("legal move");
            pos.play_unchecked(mv);
            out.push(pos.clone());
        }
        out
    }

    #[test]
    fn tracking_goes_on_by_itself_after_a_position_was_set_by_hand() {
        // Glare, a moving shadow, leaning pieces: the look of the squares has drifted a good way by the end.
        let sans = ["e4", "e5", "Nf3", "Nc6", "Bc4", "Nf6", "Ng5", "d5", "exd5", "Nxd5"];
        let positions = positions_after(&sans);
        let mut tracker = Tracker::new(TrackerConfig::default());
        tracker.set_calibration(Some(&synthetic::corners()));
        tracker.start(positions[0].clone());
        let (mut now, mut frames) = (Instant::now(), 0u64);
        let mut show = |tracker: &mut Tracker, index: usize, times: usize| -> Vec<Detection> {
            let progress = index as f32 / sans.len() as f32;
            let mut out = Vec::new();
            for _ in 0..times {
                frames += 1;
                now += Duration::from_millis(333);
                let frame = synthetic::frame_from_position(&positions[index], &synthetic::Conditions::HARD, progress, frames);
                match tracker.feed(&frame, now) {
                    Detection::Nothing => {}
                    other => out.push(other),
                }
            }
            out
        };

        assert!(show(&mut tracker, 0, 7).is_empty());
        for index in 1..=4 {
            let seen = show(&mut tracker, index, 4);
            assert!(matches!(seen.as_slice(), [Detection::Move { .. }]), "move {index}: {seen:?}");
        }

        // The person types the next move, as if the program had not seen it; the board shows it already.
        tracker.set_position(positions[5].clone());
        let after_typing = show(&mut tracker, 5, 5);
        assert!(after_typing.is_empty(), "no move may be invented for a picture that shows the typed position: {after_typing:?}");

        // The following moves are found by themselves; where the tracker is unsure the person types the move
        // again, as in real use. It may never be stuck, and it may never record a move that was not played.
        let mut typed = 0;
        for index in 6..=sans.len() {
            let seen = show(&mut tracker, index, 4);
            let mut found = false;
            for detection in &seen {
                match detection {
                    Detection::Move { mv, .. } => {
                        assert_eq!(san_of(&positions[index - 1], *mv), sans[index - 1], "wrong move recorded at {index}");
                        found = true;
                    }
                    Detection::Unsure { .. } => {}
                    other => panic!("move {index} ({}): unexpected {other:?}", sans[index - 1]),
                }
            }
            if !found {
                typed += 1;
                tracker.set_position(positions[index].clone());
                let late = show(&mut tracker, index, 3);
                assert!(late.iter().all(|d| matches!(d, Detection::Unsure { .. })), "after typing move {index}: {late:?}");
            }
        }
        assert!(typed <= 2, "{typed} of 6 moves had to be typed again after the position was set by hand");
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
            [Detection::Sequence { moves, .. }] => {
                assert_eq!(moves.iter().map(|m| uci_of(*m)).collect::<Vec<_>>(), ["e2e4", "e7e5"]);
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
            [Detection::Sequence { moves, .. }] => {
                assert_eq!(moves.iter().map(|m| uci_of(*m)).collect::<Vec<_>>(), ["d4e5", "g4f3"]);
            }
            other => panic!("expected one pair, got {other:?}"),
        }
    }

    #[test]
    fn three_moves_played_between_two_pictures_are_recognised() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3", "Nc6"]);
        let mut rig = Rig::new(Chess::default());
        assert!(rig.show(&synthetic::start_grid(), 7).is_empty());
        let detections = rig.show(&steps[2].after, 4);
        match detections.as_slice() {
            [Detection::Sequence { moves, .. }] => {
                assert_eq!(moves.iter().map(|m| uci_of(*m)).collect::<Vec<_>>(), ["e2e4", "e7e5", "g1f3"]);
            }
            other => panic!("expected three moves, got {other:?}"),
        }
        // And it is in step: the next move is a single move again.
        assert!(matches!(rig.show(&steps[3].after, 4).as_slice(), [Detection::Move { .. }]));
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
