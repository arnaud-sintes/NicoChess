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
use crate::vision::{ADAPT_WEIGHT, Cell, DIM, Features, Glance, Grid, Models, Sampler, difference, frame_change};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use shakmaty::{Chess, Color, Move, Position, Role, Square};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// The settings of the tracker. They are kept in a recording (see `record`), so that a replay can use the
/// ones the game was played with.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Pictures in a row that must agree before the board counts as still (at least this many, and for
    /// `stable_time`).
    pub stable_frames: usize,
    /// How long, in seconds, the board must have been still before it counts as still. Time, not a number of pictures,
    /// so that it means the same at 3 pictures a second and at 25.
    #[serde(default = "default_stable_time")]
    pub stable_time: f32,
    /// How far back, in seconds, the picture is that a new one is compared with to see whether something moved (0: the
    /// picture before). A hand that creeps in changes little from one picture to the next at 25 pictures a second.
    #[serde(default = "default_motion_span")]
    pub motion_span: f32,
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
    /// A move that leads the runner-up by at least this many nats, but by less than `margin`, is accepted all the
    /// same, with a note of doubt (see [`Doubt`]): the person is told and can put it right with one click. `margin`
    /// or more turns this off, and the tracker then asks as it always did.
    #[serde(default = "default_soft_margin")]
    pub soft_margin: f32,
}

impl Default for TrackerConfig {
    fn default() -> Self {
        TrackerConfig {
            stable_frames: 3,
            stable_time: STABLE_TIME,
            motion_span: MOTION_SPAN,
            motion_threshold: 4.0,
            max_excess: 60.0,
            margin: 6.0,
            max_plies: 3,
            soft_margin: SOFT_MARGIN,
        }
    }
}

/// How long the board must have been still before the tracker looks at it, in seconds. It was 0.35, about as long
/// as the three pictures in a row of the first versions took at 3 pictures a second. That, with `MOTION_SPAN`,
/// meant half a second without a hand over the board, and in a fast game (a move every second or so) the hands are
/// hardly ever off it for that long: moves ran together and the tracker was lost for good. A shorter time is safe now
/// that a square a hand covers is not believed (see `Evidence::hide_covered`) and a picture with one is not learned from.
const STABLE_TIME: f32 = 0.20;

/// The still time of recordings made before there was one (they were made at 3 pictures a second, with the first
/// versions' 0.35 s, and a replay must judge them as they were played).
fn default_stable_time() -> f32 {
    0.35
}

/// How far back the picture is that a new one is compared with, to see whether something moved, in seconds.
const MOTION_SPAN: f32 = 0.10;

/// The span of recordings made before it could be set (0.15 s, as it was when it was introduced).
fn default_motion_span() -> f32 {
    0.15
}

/// The soft margin of recordings made before there was one.
fn default_soft_margin() -> f32 {
    SOFT_MARGIN
}

/// One move is not taken when two moves explain the picture better than it by this many margins (see `decide`).
const PAIR_LEAD: f32 = 1.5;

/// A move that leads by at least this much (nats) is taken even though it is not clear, and flagged as doubtful.
const SOFT_MARGIN: f32 = 2.0;
/// How the lead of one explanation over the others becomes a chance. The likelihoods are overconfident (the
/// squares are not independent), so the nats are divided by this before they are turned into odds. Measured on
/// the synthetic stress test (few cases): moves taken on a lead of 2 to 6 nats were right about two times in three,
/// so a lead of 6 nats is shown as about 80%, not 95%.
const CERTAINTY_SCALE: f32 = 4.0;

/// What a square may misfit, in nats, before it counts as unexplained: the noise on top of the usual.
const SLACK: f32 = 4.0;
/// With two cameras: how fast a camera is believed less about a square as no content explains what it sees there
/// (the weight falls by a factor e for each this many nats of misfit), and the least it is ever believed.
const TRUST_SCALE: f32 = 10.0;
const TRUST_FLOOR: f32 = 0.02;
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
/// An empty square beside a piece that stands below it in the picture may show that piece's head (see
/// `Sampler::lean_sources`): for the limits on what may be left unexplained it then costs what it costs to be that
/// colour, plus a penalty (nats). With a height picture, which must see the square empty (`LEAN_DEPTH_NEEDED`, 0 to 1,
/// see `Evidence::occupancy`), the penalty is `LEAN_PENALTY` times how far from empty it sees it plus one: nothing where
/// it is sure. A lean changes which explanation is the likeliest only where the height picture is as sure as `LEAN_SURE`.
/// With no height picture the penalty is `LEAN_PENALTY_COLOUR`.
const LEAN_PENALTY: f32 = 12.0;
const LEAN_PENALTY_COLOUR: f32 = 24.0;
const LEAN_DEPTH_NEEDED: f32 = 0.3;
const LEAN_SURE: f32 = 0.5;
/// When the tracker has been unsure for this long, in seconds, and one explanation is clear and leaves nothing
/// unexplained but a single odd square (a lean the models have not seen, a stain, a reflection), the board is taken
/// as it is and the oddity is allowed from then on (see `Tracker::allow`). Without it, one square nobody can explain
/// blocks the game for good: every explanation has the same misfit, and refusing them all teaches nothing.
const ADOPT_AFTER: f32 = 1.0;
/// The most a single square may misfit, in nats, to be adopted: more is a piece knocked over or a hand left there.
const ADOPT_LIMIT: f32 = 150.0;
/// A square that none of empty, white and black explains by this many nats (beyond what it usually shows) is covered: a
/// hand, an arm or a sleeve is over it. What it shows says nothing about the game, so it counts the same for every
/// explanation; the likeliest one is no longer the one that moves a covered piece away because a hand looks more like
/// an empty square than like a white piece.
const COVERED: f32 = 12.0;
/// With more squares covered than this there is an arm over the board: nothing is decided until it has gone.
const MAX_COVERED: usize = 20;
/// When the tracker has been unsure for this long, in seconds, with the board still, it looks for a longer way the game
/// may have gone: more moves than the three that are always tried (the players were faster than the tracker, or it
/// missed some). Without it, a tracker that is more than three moves behind can never explain the board again.
const DEEP_AFTER: f32 = 0.5;
/// The most moves looked for, and how many explanations are kept at each step of the search.
const DEEP_PLIES: u8 = 8;
const DEEP_WIDTH: usize = 80;
/// A square whose picture misfits what the position says it holds by more than this is not learned from by an ordinary
/// confirmed move (a hand that rests on a piece, and looks somewhat like the other colour, would otherwise become the
/// look of that piece). When the models are known to be behind (a drifting light, see `ADAPT_FACTOR`) they learn all
/// that the move was accepted with.
const TEACH_LIMIT: f32 = 40.0;
/// A square is covered only if it also differs from the last confirmed picture by this much (half of `difference`, which
/// is about `DIM` / 2 when nothing changed): a square that is odd but has not changed (a glare, a shadow that stays, the
/// light that drifts) is the models being behind, and they learn it; it is not a hand.
const DISTURBED: f32 = 20.0;
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
    /// A move was taken back, and the board does not show the position yet (see [`Tracker::undo_to`]).
    Undone,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    /// One move as UCI, or several separated by spaces.
    pub uci: String,
    /// The same as SAN.
    pub san: String,
    /// How much less likely than the likeliest explanation, in nats.
    pub cost: f32,
    /// How likely it is among the explanations that move something, from 0 to 1.
    pub chance: f32,
}

/// A move was taken although it was not clear: how sure the tracker is, and what else it may have been.
#[derive(Clone, Debug)]
pub struct Doubt {
    /// The chance that the moves taken are the ones played, from 0 to 1.
    pub certainty: f32,
    /// The other likely explanations, likeliest first.
    pub alternatives: Vec<Candidate>,
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
/// changes a lot (a piece was lifted or put down) between a picture and the one taken about `motion_span` before
/// it. (The picture just before would do at 3 pictures a second, but at 25 a hand that creeps in changes very little
/// from one picture to the next.) It is still when neither happened for `stable_frames` pictures in a row and
/// `stable_time`.
struct Gate {
    stable_frames: usize,
    stable_time: Duration,
    motion_span: Duration,
    motion_threshold: f32,
    /// The latest pictures, to compare the new one with a picture of about `motion_span` ago.
    history: VecDeque<(Instant, Vec<Features>)>,
    run: usize,
    since: Option<Instant>,
    /// When the latest picture of this run came.
    last: Option<Instant>,
    /// The sum of the features of the pictures of this run.
    sum: Vec<[f32; DIM]>,
}

/// How much one square must change between two pictures (see [`frame_change`]) to count as a piece coming or going.
const SQUARE_CHANGE: f32 = 30.0;

impl Gate {
    fn new(cfg: &TrackerConfig) -> Self {
        Gate {
            stable_frames: cfg.stable_frames.max(1),
            // From the command line or a recording: not a value that `Duration` cannot hold (or that would never end).
            stable_time: Duration::from_secs_f32(if cfg.stable_time.is_finite() { cfg.stable_time.clamp(0.0, 60.0) } else { STABLE_TIME }),
            motion_span: Duration::from_secs_f32(if cfg.motion_span.is_finite() { cfg.motion_span.clamp(0.0, 10.0) } else { MOTION_SPAN }),
            motion_threshold: cfg.motion_threshold,
            history: VecDeque::new(),
            run: 0,
            since: None,
            last: None,
            sum: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.history.clear();
        self.run = 0;
        self.since = None;
        self.last = None;
        self.sum.clear();
    }

    /// Adds a frame. Returns when the board became still, once it has been for long enough.
    fn push(&mut self, features: &[Features], now: Instant) -> Option<Instant> {
        // The picture to compare with: the newest one that is at least MOTION_SPAN old, else the oldest there is.
        let reference =
            self.history.iter().rev().find(|(at, _)| now.saturating_duration_since(*at) >= self.motion_span).or(self.history.front());
        let moved = match reference {
            Some((_, prev)) => {
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
        self.last = Some(now);
        self.history.push_back((now, features.to_vec()));
        self.trim_history(now);
        self.ready()
    }

    /// Pictures older than needed are dropped (one older than the span stays: it is what the next is compared with).
    fn trim_history(&mut self, now: Instant) {
        while self.history.len() > 2 && self.history.get(1).is_some_and(|(at, _)| now.saturating_duration_since(*at) >= self.motion_span) {
            self.history.pop_front();
        }
    }

    /// Counts one more still picture of the run that is going on, one that is the same as the one before it: what
    /// [`push`](Self::push) does with a picture that did not move, with the numbers of the picture before instead of
    /// the numbers of this one (it is not looked at). False when there is no run to go on with.
    fn repeat(&mut self, now: Instant) -> bool {
        if self.run == 0 {
            return false;
        }
        let Some((_, features)) = self.history.back().cloned() else { return false };
        self.run += 1;
        for (total, f) in self.sum.iter_mut().zip(&features) {
            for d in 0..DIM {
                total[d] += f.v[d];
            }
        }
        self.last = Some(now);
        self.history.push_back((now, features));
        self.trim_history(now);
        true
    }

    /// When the board became still, once it has been for long enough.
    fn ready(&self) -> Option<Instant> {
        let long_enough = match (self.since, self.last) {
            (Some(since), Some(last)) => last.saturating_duration_since(since) >= self.stable_time,
            _ => false,
        };
        if self.run >= self.stable_frames && long_enough { self.since } else { None }
    }

    /// The features averaged over the pictures of this run: less noise than a single picture.
    fn average(&self) -> Vec<Features> {
        let n = self.run.max(1) as f32;
        self.sum.iter().map(|total| Features { v: total.map(|x| x / n) }).collect()
    }
}

/// What the tracker knows about one camera: where its squares are, what they look like, and when its picture has
/// stopped moving. With two cameras there are two of these, and what they say is put together (see [`Evidence`]).
struct View {
    sampler: Option<Sampler>,
    models: Option<Models>,
    gate: Gate,
    /// What the board looked like in this camera (light corrected) when the position was last confirmed. Absent
    /// after a person set the position by hand, until a picture of it is seen.
    reference: Option<Vec<Features>>,
    /// The squares of `reference` that showed a hand when it was taken: they say nothing about what the board looked
    /// like, so what changed there is not known until a picture without the hand has been seen.
    stale: [bool; 64],
    /// False while the camera gives no pictures (off, unplugged, too slow): the others do not wait for it.
    active: bool,
    /// The view only helps: it is the height picture of a depth camera (see `depth`). It says which squares hold a
    /// piece, not of which colour, and its picture is noisier than a colour one, so it never decides when the board
    /// is still (the colour cameras do) and what it says is added to what they say (see `Evidence`).
    advisory: bool,
    /// For each square, the squares whose pieces may lean over it in this camera's picture (see `Sampler::lean_sources`).
    lean: Vec<Vec<u8>>,
}

impl View {
    fn new(cfg: &TrackerConfig) -> View {
        View {
            sampler: None,
            models: None,
            gate: Gate::new(cfg),
            reference: None,
            stale: [false; 64],
            active: true,
            advisory: false,
            lean: Vec::new(),
        }
    }

    /// Where the squares are in this camera's picture.
    fn set_sampler(&mut self, sampler: Option<Sampler>) {
        self.lean = sampler.as_ref().map(Sampler::lean_sources).unwrap_or_default();
        self.sampler = sampler;
    }

    /// A picture of the board, all of it known, is the reference from now on.
    fn set_reference(&mut self, features: Vec<Features>) {
        self.reference = Some(features);
        self.stale = [false; 64];
    }

    /// An advisory view has seen the board when it has been given a picture; it is not waited for to be still.
    fn has_seen(&self) -> bool {
        if self.advisory { self.gate.run > 0 } else { self.gate.ready().is_some() }
    }

    /// The camera is calibrated and gives pictures.
    fn counts(&self) -> bool {
        self.sampler.is_some() && self.active
    }

    /// It also knows what the squares look like, so its pictures can be read.
    fn judges(&self) -> bool {
        self.counts() && self.models.is_some()
    }
}

/// What the cameras that judge showed, averaged over the pictures since the board stopped moving: the number of the
/// camera, and its squares.
type Seen = Vec<(usize, Vec<Features>)>;

pub struct Tracker {
    cfg: TrackerConfig,
    /// One per camera. There is always one; the others are made when they are first mentioned.
    views: Vec<View>,
    /// The cameras that judged the last still picture.
    judged: Vec<usize>,
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
    /// Set when the last moves taken were doubtful; the engine takes it (see `take_doubt`).
    doubt: Option<Doubt>,
    /// The person took back a doubtful move and has to say what was played: nothing is decided until then.
    held: bool,
    /// The last height picture looked at was left out because it contradicted the colour cameras.
    depth_ignored: bool,
    /// A move was taken back (see `undo_to`): the board has not shown the position since, so nothing is decided.
    undone: bool,
    /// The move that was taken back, which is not offered again.
    undone_move: Option<String>,
    /// The squares that no content explained in the picture last judged (see [`COVERED`]).
    covered: Vec<bool>,
    /// When the tracker became unsure (valid while `unsure`), and the picture it last looked for a longer way for.
    unsure_since: Option<Instant>,
    deep_tried: Option<Grid>,
    /// The picture that `adopt_again` last looked at.
    adopt_tried: Option<Grid>,
    /// The empty squares that the last position decided was explained by a piece leaning over them.
    leaned: Vec<bool>,
    /// Squares whose picture has been odd for a good while with nothing else wrong (see [`ADOPT_AFTER`]): the content they
    /// were adopted with, and how much beyond the usual they misfit. That much is not held against any explanation, as
    /// long as the square holds the same content. It goes when the picture of the square is normal again.
    allow: Vec<Option<(usize, f32)>>,
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
    /// A height picture was given but contradicts the colour cameras too much, and was left out.
    ignored: bool,
    /// The squares that no content explains (see [`COVERED`]): they count the same for every explanation.
    covered: Vec<bool>,
    /// The squares whose reference picture showed a hand (see `View::stale`), where nothing is known about a change.
    unknown: Vec<bool>,
    /// For each square, the squares whose pieces may lean over it (see `Sampler::lean_sources`), from the colour cameras.
    lean: Vec<Vec<u8>>,
    /// What the height picture believes about each square, from -1 (empty, for sure) to 1 (occupied, for sure); 0 where
    /// it says nothing or there is no height picture.
    occupancy: Vec<f32>,
    /// A height picture was used (see `occupancy`).
    has_depth: bool,
    /// Squares whose misfit is allowed (see `Tracker::allow`): the content, and how much.
    allow: Vec<Option<(usize, f32)>>,
}

/// One camera's picture of the still board, with what is known about its squares.
struct Look<'a> {
    models: &'a Models,
    /// What may lean over each square in this camera's picture.
    lean: &'a [Vec<u8>],
    features: &'a [Features],
    reference: Option<&'a [Features]>,
    /// The squares of the reference that show a hand, not the board: what changed there is not known.
    stale: &'a [bool],
    /// From an advisory view: it says whether a square is occupied, and nothing else.
    advisory: bool,
}

/// How much what the height picture says about a square counts, compared with what the colour cameras say: the
/// chance that a square is occupied is shifted by this fraction of what the height picture believes. Below one:
/// a reading that went wrong must not outvote the colour cameras.
const DEPTH_WEIGHT: f32 = 0.7;
/// The most the height picture may add to the cost of a content of one square, in nats.
const DEPTH_CAP: f32 = 40.0;
/// The height picture and the colour cameras may differ about whether this many squares are occupied (where both
/// can be believed); more than that and the height picture is not used.
const DEPTH_MAX_DISAGREE: usize = 4;

/// What one camera's picture says: the numbers [`Evidence`] is made of.
struct Part {
    costs: Vec<[f32; 3]>,
    typical: Vec<[f32; 3]>,
    change: Option<(Vec<f32>, Vec<f32>)>,
    /// The squares of the reference that showed a hand: nothing is known about whether they changed.
    unknown: Vec<bool>,
}

impl Part {
    fn of(look: &Look) -> Part {
        let change = look.reference.map(|before| {
            let mut unchanged = Vec::with_capacity(64);
            let mut changed = Vec::with_capacity(64);
            for (square, (now, then)) in look.features.iter().zip(before).take(64).enumerate() {
                if look.stale.get(square).copied().unwrap_or(false) {
                    // The reference showed a hand here: nothing is known about whether the square changed.
                    unchanged.push(0.0);
                    changed.push(0.0);
                    continue;
                }
                let d = difference(now, then);
                // Unchanged: how strange the difference is. Changed: how strange it is that there is none.
                unchanged.push(0.5 * d);
                changed.push(NO_CHANGE_PENALTY * (-(d / DIM as f32) / 3.0).exp());
            }
            (unchanged, changed)
        });
        let unknown = (0..64).map(|square| look.stale.get(square).copied().unwrap_or(false)).collect();
        Part { costs: look.models.costs(look.features), typical: look.models.typical().to_vec(), change, unknown }
    }

    /// How far a camera can be believed about a square: it can when one of the three contents explains what it
    /// sees. When none does (a hand or an arm in front of the square, a glare, a piece hiding it) it says little,
    /// and the other camera is heard instead.
    fn trust(&self, square: usize) -> f32 {
        let misfit = (0..3).map(|c| (self.costs[square][c] - self.typical[square][c] - SLACK).max(0.0)).fold(f32::MAX, f32::min);
        (-misfit / TRUST_SCALE).exp().max(TRUST_FLOOR)
    }
}

impl Evidence {
    /// What the pictures say. With one camera this is that camera's numbers. With more, each square takes the
    /// numbers of the cameras that can be believed about it, weighted by how far they can: a square that one
    /// camera cannot see well is read from the other, and a square both see badly stays badly seen. The result is
    /// an average, so it is on the scale of one camera and the thresholds mean the same thing.
    fn new(looks: &[Look], base: Grid) -> Evidence {
        // The cameras that see colour are put together; a height picture then adds what it says about occupied squares.
        let colour: Vec<&Look> = looks.iter().filter(|l| !l.advisory).collect();
        let main: Vec<&Look> = if colour.is_empty() { looks.iter().collect() } else { colour };
        let (mut evidence, colour_trust) = Evidence::of_colour(&main, base);
        // What may lean over a square in any of the colour cameras' pictures.
        evidence.lean = (0..64)
            .map(|square| {
                let mut sources: Vec<u8> = main.iter().flat_map(|l| l.lean.get(square).into_iter().flatten().copied()).collect();
                sources.sort_unstable();
                sources.dedup();
                sources
            })
            .collect();
        for look in looks.iter().filter(|l| l.advisory && !main.iter().any(|m| std::ptr::eq(*m, *l))) {
            evidence.add_occupancy(&Part::of(look), &colour_trust);
        }
        evidence.hide_covered();
        evidence
    }

    /// Squares that no content explains are made neutral: every content costs the same there, and so does a change.
    /// (A hand that rests over a white piece fits "empty" better than "white", so without this the likeliest
    /// explanation is whichever one moves that piece away, and nothing the hand does not cover is believed.)
    fn hide_covered(&mut self) {
        for square in 0..64 {
            let misfit = (0..3).map(|c| (self.costs[square][c] - self.typical[square][c] - SLACK).max(0.0)).fold(f32::MAX, f32::min);
            if misfit <= COVERED {
                continue;
            }
            // Odd, but the same as when the position was last confirmed: the models are behind (or a glare, a shadow),
            // not a hand that came. (Where the reference showed a hand, or there is none, nothing says it did not.)
            let disturbed = self.unknown[square] || self.change.as_ref().is_none_or(|(unchanged, _)| unchanged[square] > DISTURBED);
            if !disturbed {
                continue;
            }
            self.covered[square] = true;
            let flat = self.typical[square].iter().copied().fold(f32::MAX, f32::min);
            self.costs[square] = [flat; 3];
            if let Some((unchanged, changed)) = self.change.as_mut() {
                unchanged[square] = 0.0;
                changed[square] = 0.0;
            }
        }
    }

    /// What a height picture says: for each square, how much more each content costs than the one the height picture
    /// likes best, where white and black are one content ("occupied"), for it cannot tell them apart.
    ///
    /// Two things are done with it. It is added to the costs, not averaged with them (averaging would halve the
    /// difference between white and black that the colour cameras see). And where the colour cameras cannot be
    /// believed about a square (a hand over it, a glare: no content explains what they see) and the height picture
    /// can, it stands in for them there: the square costs what it usually costs, plus what the height picture
    /// holds against each content. A square that neither explains (no reading, a hand in both) is left as it is.
    fn add_occupancy(&mut self, part: &Part, colour_trust: &[f32]) {
        // A height picture that contradicts the colour cameras where both can be believed is not looking at this
        // board (the camera was knocked, or its squares are not where they were thought to be): it is left out
        // rather than allowed to hold every explanation against the picture.
        let colour_read = Models::read(&self.costs);
        let disagree = (0..64)
            .filter(|&s| {
                if colour_trust[s] < 0.5 || part.trust(s) < 0.5 {
                    return false;
                }
                let depth_occupied = part.costs[s][1].min(part.costs[s][2]) < part.costs[s][0];
                if depth_occupied == (colour_read[s] != Cell::Empty) {
                    return false;
                }
                // An empty square that shows the colour of the piece beside it is a lean, not a disagreement.
                depth_occupied || !self.leans_over(s, colour_read[s], &colour_read)
            })
            .count();
        if disagree > DEPTH_MAX_DISAGREE {
            self.ignored = true;
            return;
        }
        self.has_depth = true;
        for square in 0..64 {
            let trust = part.trust(square);
            let colour = colour_trust[square];
            let occupied = part.costs[square][1].min(part.costs[square][2]);
            let own = [part.costs[square][0], occupied, occupied];
            let lowest = own[0].min(occupied);
            let stand_in = (1.0 - colour) * trust;
            self.occupancy[square] = ((own[0] - occupied).clamp(-DEPTH_CAP, DEPTH_CAP) / DEPTH_CAP) * trust;
            // What the colour cameras say about a change on this square is worth what they are worth about the square.
            if let Some((unchanged, changed)) = self.change.as_mut() {
                unchanged[square] *= 1.0 - stand_in;
                changed[square] *= 1.0 - stand_in;
            }
            for content in 0..3 {
                let against = (own[content] - lowest).min(DEPTH_CAP);
                let replaced = self.typical[square][content] + against;
                self.costs[square][content] = (1.0 - stand_in) * self.costs[square][content] + stand_in * replaced;
                self.costs[square][content] += DEPTH_WEIGHT * trust * colour * against;
            }
        }
    }

    /// The colour cameras put together, and how far they can be believed about each square (the best of them).
    fn of_colour(looks: &[&Look], base: Grid) -> (Evidence, Vec<f32>) {
        let mut parts: Vec<Part> = looks.iter().map(|l| Part::of(l)).collect();
        if parts.len() == 1 {
            let only = parts.remove(0);
            let trust = (0..64).map(|s| only.trust(s)).collect();
            return (
                Evidence {
                    costs: only.costs,
                    typical: only.typical,
                    base,
                    change: only.change,
                    ignored: false,
                    covered: vec![false; 64],
                    unknown: only.unknown,
                    lean: Vec::new(),
                    occupancy: vec![0.0; 64],
                    has_depth: false,
                    allow: vec![None; 64],
                },
                trust,
            );
        }
        let trust: Vec<Vec<f32>> = parts.iter().map(|p| (0..64).map(|s| p.trust(s)).collect()).collect();
        let mix = |value: &dyn Fn(&Part, usize) -> Option<f32>, square: usize| -> f32 {
            let (mut total, mut weight) = (0.0f32, 0.0f32);
            for (part, t) in parts.iter().zip(&trust) {
                if let Some(v) = value(part, square) {
                    total += t[square] * v;
                    weight += t[square];
                }
            }
            if weight > 0.0 { total / weight } else { 0.0 }
        };
        let mut costs = vec![[0.0f32; 3]; 64];
        let mut typical = vec![[0.0f32; 3]; 64];
        for square in 0..64 {
            for c in 0..3 {
                costs[square][c] = mix(&|p, s| Some(p.costs[s][c]), square);
                typical[square][c] = mix(&|p, s| Some(p.typical[s][c]), square);
            }
        }
        // What changed since the last confirmed picture: from the cameras that have one.
        let change = parts.iter().any(|p| p.change.is_some()).then(|| {
            let unchanged = (0..64).map(|s| mix(&|p, q| p.change.as_ref().map(|c| c.0[q]), s)).collect();
            let changed = (0..64).map(|s| mix(&|p, q| p.change.as_ref().map(|c| c.1[q]), s)).collect();
            (unchanged, changed)
        });
        let best_trust = (0..64).map(|s| trust.iter().map(|t| t[s]).fold(0.0f32, f32::max)).collect();
        let unknown = (0..64).map(|s| parts.iter().all(|p| p.unknown[s])).collect();
        let evidence = Evidence {
            costs,
            typical,
            base,
            change,
            ignored: false,
            covered: vec![false; 64],
            unknown,
            lean: Vec::new(),
            occupancy: vec![0.0; 64],
            has_depth: false,
            allow: vec![None; 64],
        };
        (evidence, best_trust)
    }

    /// How the picture fits a board: the misfit including what changed since the last confirmed picture, how
    /// much of the misfit is beyond what each square usually shows, and the worst square of those.
    fn fit(&self, grid: &Grid) -> (f32, f32, f32) {
        let (mut nll, mut excess, mut worst) = (0.0f32, 0.0f32, 0.0f32);
        for square in 0..64 {
            let (cost, beyond, _) = self.square(square, grid);
            nll += cost;
            excess += beyond;
            worst = worst.max(beyond);
            if let Some((unchanged, changed)) = &self.change {
                nll += CHANGE_WEIGHT * if grid[square] == self.base[square] { unchanged[square] } else { changed[square] };
            }
        }
        (nll, excess, worst)
    }

    /// Whether a piece of this colour stands on a square that may lean over `square` (see `Sampler::lean_sources`).
    fn leans_over(&self, square: usize, cell: Cell, grid: &Grid) -> bool {
        cell != Cell::Empty && self.lean.get(square).is_some_and(|sources| sources.iter().any(|&n| grid[usize::from(n)] == cell))
    }

    /// Whether a lean may explain the square. With a height picture it must see the square empty (it sees the foot of
    /// things, which a head that leans over does not hide): colour alone cannot tell a lean from a piece of the same
    /// colour that has come to the square, and a lean taken for one lets an explanation that misses a move through.
    fn lean_allowed(&self, square: usize) -> bool {
        !self.covered[square] && (!self.has_depth || self.occupancy[square] <= -LEAN_DEPTH_NEEDED)
    }

    /// What a lean costs on top of the colour of the piece (see `LEAN_PENALTY`).
    fn lean_penalty(&self, square: usize) -> f32 {
        if self.has_depth { LEAN_PENALTY * (1.0 + self.occupancy[square]).max(0.0) } else { LEAN_PENALTY_COLOUR }
    }

    /// Whether the picture of a square can be a piece beside it that leans over it: the square is empty, a piece stands on
    /// a square that may lean over it, and the picture looks more like that piece's colour than like an empty square. The
    /// height picture, where it is sure that the square is occupied, says there is no lean.
    fn lean_explains(&self, square: usize, grid: &Grid) -> bool {
        grid[square] == Cell::Empty
            && self.lean_allowed(square)
            && self.lean.get(square).into_iter().flatten().any(|&n| {
                let other = grid[usize::from(n)];
                other != Cell::Empty && self.costs[square][other.index()] < self.costs[square][Cell::Empty.index()]
            })
    }

    /// What a square costs with the content `grid` gives it, how much of that is beyond what it usually costs (what the
    /// limits judge), and whether a lean explains the misfit.
    ///
    /// An empty square may show the head of a piece that leans over it (a tall piece seen from the side by a low
    /// camera). The limits then take it as costing what the colour of that piece costs, plus a little. Which explanation
    /// is the likeliest is not changed by it: that stays as the pictures say, only a square that a lean explains no
    /// longer makes a good explanation unfit to be taken. The height picture, which sees the foot of things, confirms
    /// the lean where it sees the square empty and turns it down where it sees it occupied. A square whose misfit was
    /// allowed (see `Tracker::allow`) is not held against the explanation up to that much.
    fn square(&self, square: usize, grid: &Grid) -> (f32, f32, bool) {
        let c = grid[square].index();
        let (mut cost, mut judged, leaned) = self.square_raw(square, grid);
        let typical = self.typical[square][c];
        if let Some((content, level)) = self.allow[square]
            && content == c
        {
            cost -= level.min((cost - typical - SLACK).max(0.0));
            judged -= level.min((judged - typical - SLACK).max(0.0));
        }
        (cost, (judged - typical - SLACK).max(0.0), leaned)
    }

    /// The same without the allowance: what the square costs, what it costs for the limits (less when a lean explains
    /// it), and whether a lean does.
    fn square_raw(&self, square: usize, grid: &Grid) -> (f32, f32, bool) {
        let c = grid[square].index();
        let mut cost = self.costs[square][c];
        let mut judged = cost;
        let mut leaned = false;
        if c == Cell::Empty.index() && self.lean_allowed(square) {
            let penalty = self.lean_penalty(square);
            // Colour alone cannot tell a lean from a piece that has just come to the square, so what the pictures say
            // about which explanation is likeliest is only changed where the height picture is sure the square is empty.
            let sure = self.has_depth && self.occupancy[square] <= -LEAN_SURE;
            for &n in self.lean.get(square).into_iter().flatten() {
                let other = grid[usize::from(n)];
                if other == Cell::Empty {
                    continue;
                }
                let o = other.index();
                let via_lean = self.costs[square][o] - self.typical[square][o] + self.typical[square][c] + penalty;
                if via_lean < judged {
                    judged = via_lean;
                    leaned = true;
                }
            }
            if sure {
                cost = cost.min(judged);
            }
        }
        (cost, judged, leaned)
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
        self.expand_with(depth, evidence, usize::MAX, depth < 3);
    }

    /// The same, from the `width` likeliest explanations of `depth - 1` moves only (a beam: for a long search, where
    /// all the ways to play eight moves would be far too many), and with the positions of the new ones kept or not
    /// (they are not kept for the last depth looked at: nothing is played from them).
    fn expand_with(&mut self, depth: u8, evidence: &Evidence, width: usize, keep: bool) {
        let mut parents: Vec<usize> =
            (0..self.hyps.len()).filter(|&i| self.hyps[i].plies + 1 == depth && self.positions[i].is_some()).collect();
        if parents.len() > width {
            parents.sort_by(|&a, &b| self.hyps[a].score.total_cmp(&self.hyps[b].score));
            parents.truncate(width);
        }
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

    /// How likely each of these explanations is among all the ones that move something: the odds of their
    /// likelihoods, softened by `CERTAINTY_SCALE`, shared out so that they add up to 1.
    fn chances(&self, picks: &[usize]) -> Vec<f32> {
        let best = self.hyps.iter().filter(|h| h.plies >= 1).map(|h| h.score).fold(f32::INFINITY, f32::min);
        if !best.is_finite() {
            return vec![0.0; picks.len()];
        }
        let weight = |score: f32| ((best - score) / CERTAINTY_SCALE).exp();
        let total: f32 = self.hyps.iter().filter(|h| h.plies >= 1).map(|h| weight(h.score)).sum();
        picks.iter().map(|&i| weight(self.hyps[i].score) / total).collect()
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
        let views = vec![View::new(&cfg)];
        Tracker {
            cfg,
            views,
            judged: Vec::new(),
            position: Chess::default(),
            learn_pending: true,
            resync_pending: false,
            last_eval: None,
            still: false,
            unsure: false,
            candidates: Vec::new(),
            doubt: None,
            held: false,
            depth_ignored: false,
            undone: false,
            undone_move: None,
            covered: vec![false; 64],
            unsure_since: None,
            deep_tried: None,
            adopt_tried: None,
            leaned: vec![false; 64],
            allow: vec![None; 64],
        }
    }

    /// The camera `view` (0 is the first), made when it is first mentioned.
    fn view_mut(&mut self, view: usize) -> &mut View {
        while self.views.len() <= view {
            self.views.push(View::new(&self.cfg));
        }
        &mut self.views[view]
    }

    /// Which cameras the detection uses now: calibrated, giving pictures, and knowing what their squares look like.
    pub fn views_in_use(&self) -> Vec<bool> {
        self.views.iter().map(View::judges).collect()
    }

    /// Says whether a camera gives pictures. One that does not is left out until it does: the others neither wait
    /// for it nor are held back by it.
    pub fn set_view_active(&mut self, view: usize, active: bool) {
        let view = self.view_mut(view);
        if view.active != active {
            view.active = active;
            view.gate.reset();
        }
    }

    /// The height picture was left out of the last judgement because it contradicted the colour cameras.
    pub fn depth_ignored(&self) -> bool {
        self.depth_ignored
    }

    /// Says that a view is the height picture of a depth camera: it only helps (see `View::advisory`).
    pub fn set_view_advisory(&mut self, view: usize, advisory: bool) {
        self.view_mut(view).advisory = advisory;
    }

    /// The doubt about the moves that were just taken, if there is one. Taken once.
    pub fn take_doubt(&mut self) -> Option<Doubt> {
        self.doubt.take()
    }

    /// The person took back a move the tracker was doubtful about: the tracker offers these moves and decides
    /// nothing, whatever the pictures show, until the person says what was played (`set_position`).
    pub fn hold(&mut self, candidates: Vec<Candidate>) {
        self.undone = false;
        self.held = true;
        self.unsure = true;
        self.candidates = candidates;
    }

    pub fn status(&self) -> Status {
        if self.views.iter().all(|v| v.sampler.is_none()) {
            Status::NoCalibration
        } else if self.learn_pending {
            Status::Learning
        } else if !self.still {
            Status::Moving
        } else if self.undone {
            Status::Undone
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

    /// A rough look at a picture of a camera, where the tracker knows the squares of that camera (see `vision::Glance`).
    pub fn glance(&self, view: usize, image: &RgbImage) -> Option<Glance> {
        self.views.get(view)?.sampler.as_ref().map(|sampler| sampler.glance(image))
    }

    /// The tracker is sure of the board and has nothing to wait for: it has seen it still, every camera that decides has
    /// its squares learned and has been still for long enough, nothing is doubtful or held, no hand covers a square.
    /// Looking at one more picture that is the same as the ones before then changes nothing but the count of still
    /// pictures (see [`repeat_still`](Self::repeat_still)).
    pub fn quiet(&self) -> bool {
        if self.status() != Status::Watching || self.resync_pending || self.last_eval.is_none() || self.covered.iter().any(|&c| c) {
            return false;
        }
        // A camera that counts and has not learned its squares learns them at the next confirmed position (a look does it).
        if self.views.iter().any(|v| v.counts() && v.models.is_none()) {
            return false;
        }
        let mut deciding = self.views.iter().filter(|v| v.judges() && !v.advisory).peekable();
        deciding.peek().is_some() && deciding.all(|v| v.gate.ready().is_some())
    }

    /// Counts a picture of each of `views` as one more still picture, the same as the one the view had before, without
    /// looking at it (no sampling, no scoring). Only when the tracker is [`quiet`](Self::quiet): then a look at such a picture
    /// finds nothing, and the run of still pictures goes on as it does. False (and nothing done) when it is not quiet, or a
    /// view has no run to go on with: the picture must then be looked at.
    pub fn repeat_still(&mut self, views: &[usize], now: Instant) -> bool {
        if !self.quiet() {
            return false;
        }
        let counted = |v: &View| v.counts() && v.gate.run > 0;
        if views.iter().any(|&i| self.views.get(i).is_none_or(|v| !counted(v))) {
            return false;
        }
        for &i in views {
            self.views[i].gate.repeat(now);
        }
        true
    }

    /// Sets (or clears) the calibration of the first camera. The look of the squares is learned again.
    pub fn set_calibration(&mut self, calibration: Option<&Calibration>) {
        self.set_view_calibration(0, calibration);
    }

    /// Sets (or clears) the calibration of a camera. What its squares look like is forgotten. When no other camera
    /// has learned the squares, everything is learned again; otherwise the game goes on with the others, and this
    /// camera learns its squares at the next position that is confirmed (see `learn_late_views`).
    pub fn set_view_calibration(&mut self, view: usize, calibration: Option<&Calibration>) {
        let sampler = calibration.and_then(Sampler::new);
        let view = self.view_mut(view);
        view.set_sampler(sampler);
        view.models = None;
        view.reference = None;
        view.gate.reset();
        // While the squares are being learned, the other cameras go on with it: only this one starts again.
        if !self.learn_pending && self.views.iter().all(|v| v.models.is_none()) {
            self.relearn();
        }
    }

    /// The board moved a little in the picture: look at the squares where they are now, but
    /// keep what was learned about them (the position is still the same).
    pub fn move_calibration(&mut self, calibration: &Calibration) {
        self.move_view_calibration(0, calibration);
    }

    /// The same for one camera.
    pub fn move_view_calibration(&mut self, view: usize, calibration: &Calibration) {
        if let Some(sampler) = Sampler::new(calibration) {
            let view = self.view_mut(view);
            view.set_sampler(Some(sampler));
            view.gate.reset();
        }
    }

    /// Starts following a game from `position`, which the board must show.
    pub fn start(&mut self, position: Chess) {
        self.position = position;
        self.relearn();
    }

    /// A move was taken back: the game is in `position` again, but the board still shows the move, until the person puts
    /// the pieces back, or enters the move that was played. Meanwhile nothing is decided and nothing is learned (the
    /// move would come back at once, or the squares would learn a look that is not theirs); the moves that the picture
    /// does suggest, without the one that was taken back, are offered. The tracker goes on by itself as soon as the
    /// board shows `position`.
    pub fn undo_to(&mut self, position: Chess, undone: Option<String>) {
        self.set_position(position);
        self.undone = true;
        self.undone_move = undone;
        self.resync_pending = false;
    }

    /// The position changed outside the tracker (manual move or undo): the person says what the board
    /// shows. The next still picture that fits teaches the models, so that tracking goes on by itself.
    pub fn set_position(&mut self, position: Chess) {
        self.undone = false;
        self.position = position;
        self.last_eval = None;
        self.unsure = false;
        self.held = false;
        self.resync_pending = true;
        self.allow = vec![None; 64];
        // The references show another position: they say nothing about what changed.
        for view in &mut self.views {
            view.reference = None;
        }
    }

    /// Learns the look of the squares again from the current position.
    pub fn relearn(&mut self) {
        self.undone = false;
        for view in &mut self.views {
            view.models = None;
            view.reference = None;
            view.gate.reset();
        }
        self.learn_pending = true;
        self.resync_pending = false;
        self.last_eval = None;
        self.unsure = false;
        self.held = false;
        self.still = false;
        self.allow = vec![None; 64];
    }

    /// Looks at a picture of the first (or only) camera.
    pub fn feed(&mut self, image: &RgbImage, now: Instant) -> Detection {
        self.feed_views(&[Some(image)], now)
    }

    /// Looks at the pictures that came since the last call: `images[i]` is the new picture of camera `i`, or `None`
    /// when it has none (a camera that is slower than the others, or off).
    pub fn feed_views(&mut self, images: &[Option<&RgbImage>], now: Instant) -> Detection {
        // What each camera that sent a picture shows.
        let mut shown: Vec<Option<Vec<Features>>> = Vec::with_capacity(self.views.len());
        for (i, view) in self.views.iter().enumerate() {
            shown.push(match (&view.sampler, images.get(i).copied().flatten()) {
                (Some(sampler), Some(image)) if view.active => Some(sampler.features(image)),
                _ => None,
            });
        }
        if shown.iter().all(Option::is_none) {
            return Detection::Nothing;
        }

        if self.learn_pending || self.views.iter().all(|v| v.models.is_none()) {
            for (view, features) in self.views.iter_mut().zip(&shown) {
                if let Some(features) = features {
                    view.gate.push(features, now);
                }
            }
            // The squares are learned when every camera that gives pictures has seen the board still (a height
            // picture is not waited for: it learns from what it has seen by then).
            let waiting: Vec<usize> = (0..self.views.len()).filter(|&i| self.views[i].counts() && !self.views[i].advisory).collect();
            if !waiting.is_empty() && waiting.iter().all(|&i| self.views[i].gate.ready().is_some()) {
                let labels = expected_grid(&self.position);
                let mut learned_any = false;
                let learners: Vec<usize> = (0..self.views.len()).filter(|&i| self.views[i].counts() && self.views[i].has_seen()).collect();
                for i in learners {
                    let learned = self.views[i].gate.average();
                    if let Ok(models) = Models::learn(&learned, &labels) {
                        let view = &mut self.views[i];
                        view.models = Some(models);
                        view.set_reference(learned);
                        learned_any = true;
                    }
                }
                if learned_any {
                    self.learn_pending = false;
                    self.last_eval = Some(labels);
                    self.judged = (0..self.views.len()).filter(|&i| self.views[i].judges()).collect();
                    self.still = true;
                    for view in &mut self.views {
                        view.gate.reset();
                    }
                }
            }
            return Detection::Nothing;
        }

        let expected = expected_grid(&self.position);
        for (view, features) in self.views.iter_mut().zip(shown.iter_mut()) {
            let Some(features) = features else { continue };
            // A change of the light all over the board is taken out first.
            if let Some(models) = &view.models {
                models.compensate_light(features, &expected);
            }
            view.gate.push(features, now);
        }
        // The board is still when every camera that can judge has seen it still; it became still when the last of
        // them did.
        let judging: Vec<usize> = (0..self.views.len()).filter(|&i| self.views[i].judges()).collect();
        // Another set of cameras sees the board than the last time: what the pictures said then is no longer what
        // they say (a camera that dropped out, or came back, changes how clear a move is), so they are judged afresh.
        if self.judged != judging {
            self.judged.clone_from(&judging);
            self.last_eval = None;
        }
        let deciding: Vec<usize> = judging.iter().copied().filter(|&i| !self.views[i].advisory).collect();
        let mut since: Option<Instant> = None;
        let mut settled = !deciding.is_empty();
        for &i in &deciding {
            match self.views[i].gate.ready() {
                Some(at) => since = Some(since.map_or(at, |t| t.max(at))),
                None => settled = false,
            }
        }
        let Some(since) = since.filter(|_| settled) else {
            self.still = false;
            return Detection::Nothing;
        };
        self.still = true;
        if self.held {
            return Detection::Unsure { candidates: self.candidates.clone() };
        }
        // What the board shows is read from the pictures averaged since it stopped moving: steadier than one.
        // (A height picture that has had no picture since the squares were learned has nothing to say yet.)
        let seen: Seen = judging
            .iter()
            .filter(|&&i| !self.views[i].advisory || self.views[i].gate.run > 0)
            .map(|&i| (i, self.views[i].gate.average()))
            .collect();
        let evidence = {
            let looks: Vec<Look> = seen
                .iter()
                .filter_map(|(i, features)| {
                    let view = &self.views[*i];
                    Some(Look {
                        models: view.models.as_ref()?,
                        lean: &view.lean,
                        features,
                        reference: view.reference.as_deref(),
                        stale: &view.stale,
                        advisory: view.advisory,
                    })
                })
                .collect();
            let mut evidence = Evidence::new(&looks, expected);
            evidence.allow.clone_from(&self.allow);
            evidence
        };
        self.depth_ignored = evidence.ignored;
        self.covered.clone_from(&evidence.covered);
        if self.covered.iter().filter(|&&c| c).count() > MAX_COVERED {
            // An arm over the board, not a hand over a square: wait until it has gone.
            self.still = false;
            return Detection::Nothing;
        }
        let grid = Models::read(&evidence.costs);
        if self.undone {
            // Is the board showing the position again? It is when "nothing happened" is the best of the explanations,
            // as it is for any picture (the misfit alone cannot say: two changed squares can stay under any limit).
            if self.last_eval != Some(grid) {
                self.last_eval = Some(grid);
                let mut pool = Pool::new(&self.position, &evidence);
                pool.expand(1, &evidence);
                let (best, _) = pool.best_within(1);
                if pool.hyps[best].plies == 0 {
                    // The pieces are back: tracking goes on, and what the board shows now is the position as it was.
                    self.undone = false;
                    self.resync_pending = true;
                    self.last_eval = None;
                } else {
                    // Not the position: the moves the picture suggests are offered, except the one taken back.
                    let mut candidates = self.likeliest(&pool, 6, None, pool.hyps[best].score);
                    candidates.retain(|c| self.undone_move.as_deref() != Some(c.uci.as_str()));
                    candidates.truncate(4);
                    self.candidates = candidates;
                }
            }
            if self.undone {
                return Detection::Unsure { candidates: self.candidates.clone() };
            }
        }
        let detection = if self.last_eval == Some(grid) {
            if self.unsure {
                // The same picture again: it may be time to take it with one odd square (see `ADOPT_AFTER`), or to
                // look for a longer way (see `DEEP_AFTER`).
                self.adopt_again(&evidence, &seen, since, now)
                    .or_else(|| self.try_deeper(grid, &evidence, &seen, since, now))
                    .unwrap_or_else(|| Detection::Unsure { candidates: self.candidates.clone() })
            } else {
                Detection::Nothing
            }
        } else {
            self.last_eval = Some(grid);
            self.evaluate(&evidence, &seen, since, now, grid)
        };
        // The position is confirmed: a camera that came late, with nothing learned, learns it now.
        if !self.unsure && !matches!(detection, Detection::Unsure { .. }) {
            self.learn_late_views();
        }
        detection
    }

    /// A camera that gave no pictures when the squares were learned (it opened later, or was calibrated during the
    /// game) learns what its squares look like from the position the game is in, once the board is still.
    fn learn_late_views(&mut self) {
        let labels = expected_grid(&self.position);
        for view in &mut self.views {
            if !view.counts() || view.models.is_some() || !view.has_seen() {
                continue;
            }
            let learned = view.gate.average();
            if let Ok(models) = Models::learn(&learned, &labels) {
                view.models = Some(models);
                view.set_reference(learned);
                view.gate.reset();
            }
        }
    }

    /// Which explanation of the still picture is the likeliest, and whether it is clear enough to believe.
    fn evaluate(&mut self, evidence: &Evidence, seen: &Seen, since: Instant, now: Instant, grid: Grid) -> Detection {
        self.doubt = None;

        // Nothing, one move, two moves: always looked at together (a reply played before the picture was
        // taken must not be mistaken for one move). Three only when those are not clear.
        let max_plies = self.cfg.max_plies.clamp(1, 3) as u8;
        let mut pool = Pool::new(&self.position, evidence);
        for depth in 1..=max_plies.min(2) {
            pool.expand(depth, evidence);
        }
        // Two moves are only believed once three have been looked at too: a pair that leaves a low-contrast
        // change unexplained (a white pawn on a light square) can look best only because the third move is missing.
        if let Some(detection) = self.decide(&pool, evidence, seen, since, max_plies <= 2) {
            return detection;
        }
        if max_plies >= 3 {
            pool.expand(3, evidence);
            if let Some(detection) = self.decide(&pool, evidence, seen, since, true) {
                return detection;
            }
        }

        // Nothing is clear, because a hand hides a square that one of the likeliest explanations changes (the piece it
        // holds, or the square it was put on): that is not a board nobody can explain, it is a board that cannot be
        // seen yet. The tracker waits for the picture to change when the hand goes, and says nothing.
        if !self.unsure && self.hides_a_change(&pool) {
            return Detection::Nothing;
        }
        // If the tracker has been unsure for a while, a longer way is looked for.
        if let Some(detection) = self.try_deeper(grid, evidence, seen, since, now) {
            return detection;
        }
        // Else the likeliest explanations are offered.
        let (best, _) = pool.best_within(3);
        let best_score = pool.hyps[best].score;
        let candidates = self.likeliest(&pool, 4, None, best_score);
        if !self.unsure {
            self.unsure_since = Some(now);
        }
        self.unsure = true;
        self.candidates = candidates.clone();
        Detection::Unsure { candidates }
    }

    /// Whether, among the explanations that are about as likely as the likeliest one, there is one that changes a square
    /// that a hand covers (see [`COVERED`]): then what is unclear is what the hand hides.
    fn hides_a_change(&self, pool: &Pool) -> bool {
        if !self.covered.iter().any(|&c| c) {
            return false;
        }
        let mut order: Vec<usize> = (0..pool.hyps.len()).filter(|&i| pool.hyps[i].plies <= 3).collect();
        order.sort_by(|&a, &b| pool.hyps[a].score.total_cmp(&pool.hyps[b].score));
        let Some(&first) = order.first() else { return false };
        let best = pool.hyps[first].score;
        let before = expected_grid(&self.position);
        order.iter().take(6).filter(|&&i| pool.hyps[i].score - best < self.cfg.margin).any(|&i| {
            let mut position = self.position.clone();
            for mv in pool.moves(i) {
                position.play_unchecked(mv);
            }
            let after = expected_grid(&position);
            (0..64).any(|square| before[square] != after[square] && self.covered[square])
        })
    }

    /// When the tracker has been unsure for `DEEP_AFTER` and has not looked at this picture yet: looks for four to
    /// `DEEP_PLIES` moves that explain it, and takes them when they are much clearer than every other explanation
    /// (more clearly the more moves there are), with a note of doubt, so that the person is told. A beam search: only
    /// the likeliest explanations at each step are played on.
    fn try_deeper(&mut self, grid: Grid, evidence: &Evidence, seen: &Seen, since: Instant, now: Instant) -> Option<Detection> {
        let waited = self.unsure_since.is_some_and(|t| now.saturating_duration_since(t).as_secs_f32() >= DEEP_AFTER);
        if !self.unsure || self.held || self.undone || !waited || self.deep_tried == Some(grid) {
            return None;
        }
        self.deep_tried = Some(grid);
        let mut pool = Pool::new(&self.position, evidence);
        for depth in 1..=DEEP_PLIES {
            pool.expand_with(depth, evidence, DEEP_WIDTH, depth < DEEP_PLIES);
        }
        let (best, runner_up) = pool.best_within(DEEP_PLIES);
        let plies = pool.hyps[best].plies;
        // Fewer moves were judged already (and found unclear).
        if plies < 4 || pool.hyps[best].excess > SIMPLE_FRACTION * self.cfg.max_excess {
            return None;
        }
        let scale = 1.0 + 0.5 * f32::from(plies - 1);
        self.accept_doubtful(&pool, evidence, best, runner_up, self.cfg.margin * scale, seen, since)
    }

    /// The same picture is still on the board and the tracker is still unsure: the explanations are looked at again,
    /// once, now that it has waited long enough (see `try_adopt`).
    fn adopt_again(&mut self, evidence: &Evidence, seen: &Seen, since: Instant, now: Instant) -> Option<Detection> {
        if !self.can_adopt(since, now) || self.adopt_tried == self.last_eval {
            return None;
        }
        self.adopt_tried = self.last_eval;
        let mut pool = Pool::new(&self.position, evidence);
        for depth in 1..=(self.cfg.max_plies.clamp(1, 3) as u8) {
            pool.expand(depth, evidence);
        }
        self.try_adopt(&pool, evidence, seen, since, now)
    }

    /// The tracker has been unsure for long enough, the board has been still for as long, and the tracker is free to decide.
    /// (A board that is still changing says nothing about an odd square: the tracker may simply be more than three moves
    /// behind, and then the best of its explanations is wrong, and clear only by the odd square it leaves.)
    fn can_adopt(&self, since: Instant, now: Instant) -> bool {
        self.unsure
            && !self.held
            && !self.undone
            && now.saturating_duration_since(since).as_secs_f32() >= ADOPT_AFTER
            && self.unsure_since.is_some_and(|t| now.saturating_duration_since(t).as_secs_f32() >= ADOPT_AFTER)
    }

    /// When the tracker has been unsure for `ADOPT_AFTER` and one explanation (nothing, or up to three moves) is as clear as
    /// it must be for any other, and leaves nothing unexplained but one square (or a few) that is odd by less than
    /// `ADOPT_LIMIT`: the explanation is taken, and the odd squares are allowed from now on (see `Tracker::allow`). The
    /// models do not learn what the odd squares show.
    fn try_adopt(&mut self, pool: &Pool, evidence: &Evidence, seen: &Seen, since: Instant, now: Instant) -> Option<Detection> {
        if !self.can_adopt(since, now) {
            return None;
        }
        let (best, runner_up) = pool.best_within(3);
        let plies = pool.hyps[best].plies;
        let scale = 1.0 + 0.5 * f32::from(plies.saturating_sub(1));
        let (excess, worst) = (pool.hyps[best].excess, pool.hyps[best].worst);
        // Only odd squares are the trouble: what is left when the worst one is taken out is as small as for any other
        // simple explanation.
        if worst > ADOPT_LIMIT
            || excess - worst > SIMPLE_FRACTION * self.cfg.max_excess
            || runner_up - pool.hyps[best].score < self.cfg.margin * scale
        {
            return None;
        }
        // The odd squares are all ones that a leaning piece explains: another oddity may be a move nobody has seen.
        let mut position = self.position.clone();
        for mv in pool.moves(best) {
            position.play_unchecked(mv);
        }
        let grid = expected_grid(&position);
        if (0..64).any(|square| evidence.square(square, &grid).1 > 0.5 * SQUARE_LIMIT && !evidence.lean_explains(square, &grid)) {
            return None;
        }
        // Written out before `accept_with` moves the position on.
        let chance = pool.chances(&[best])[0];
        let alternatives = self.likeliest(pool, 3, Some(best), pool.hyps[best].score);
        let detection = self.accept_with(pool, evidence, best, runner_up, seen, since, ADOPT_LIMIT, true)?;
        if plies >= 1 {
            // Taken on a lead that is clear, but with a square nobody can explain: the person is told.
            self.doubt = Some(Doubt { certainty: chance, alternatives });
        }
        Some(detection)
    }

    /// Accepts the likeliest explanation if it is clear, or learns the picture if the board has not
    /// changed but looks different. `None` when nothing is clear.
    ///
    /// Nothing or one move are tried first: when one of them explains the picture there is no reason to
    /// invent more moves. Several are only looked at when the simple explanations leave too much unexplained.
    fn decide(&mut self, pool: &Pool, evidence: &Evidence, seen: &Seen, since: Instant, last_stage: bool) -> Option<Detection> {
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
            return self.accept(pool, evidence, 0, simple_runner_up, seen, since);
        }
        // Two moves that explain the picture clearly better than the best of nothing and one move are not "one move that
        // fits well enough": the reply was played too, and the one move only looks right because it explains most of what
        // changed. (Taking it leaves the reply as a change nobody explained, and the next move is then judged from a wrong
        // position.)
        let (pair, _) = pool.best_within(2);
        let pair_is_clearly_better = pool.hyps[pair].plies == 2
            && pool.hyps[simple].plies >= 1
            && pool.hyps[simple].score - pool.hyps[pair].score >= PAIR_LEAD * self.cfg.margin;
        if pool.hyps[simple].excess <= limit * self.cfg.max_excess && !pair_is_clearly_better {
            if simple_runner_up - pool.hyps[simple].score < self.cfg.margin {
                // Two simple explanations fit nearly alike: more moves would not help. If one leads a little,
                // it is taken and flagged; else the person is asked.
                return self.accept_doubtful(pool, evidence, simple, simple_runner_up, self.cfg.soft_margin, seen, since);
            }
            return self.accept(pool, evidence, simple, simple_runner_up, seen, since);
        }
        // The simple explanations leave too much unexplained: several moves may have been played.
        let (best, runner_up) = pool.best_within(3);
        if pool.hyps[best].plies >= 2 && !last_stage {
            return None; // wait until one more move has been looked at (see `evaluate`)
        }
        // Several moves at once must be clearer than one: there are many more ways to look alike.
        let scale = 1.0 + 0.5 * f32::from(pool.hyps[best].plies.saturating_sub(1));
        if runner_up - pool.hyps[best].score < self.cfg.margin * scale {
            return self.accept_doubtful(pool, evidence, best, runner_up, self.cfg.soft_margin * scale, seen, since);
        }
        self.accept(pool, evidence, best, runner_up, seen, since)
    }

    /// The explanations that move something, likeliest first (at most `n`, without `skip`), each with its chance.
    /// `best_score` is what `cost` is counted from. Call it before the tracker's position changes: the moves are
    /// written out from the position they were found for.
    fn likeliest(&self, pool: &Pool, n: usize, skip: Option<usize>, best_score: f32) -> Vec<Candidate> {
        let mut order: Vec<usize> = (0..pool.hyps.len()).filter(|&i| pool.hyps[i].plies >= 1 && Some(i) != skip).collect();
        order.sort_by(|&a, &b| pool.hyps[a].score.total_cmp(&pool.hyps[b].score));
        order.truncate(n);
        let chances = pool.chances(&order);
        order
            .into_iter()
            .zip(chances)
            .map(|(i, chance)| {
                let (uci, san) = self.describe(&pool.moves(i));
                Candidate { uci, san, cost: pool.hyps[i].score - best_score, chance }
            })
            .collect()
    }

    /// The likeliest explanation leads, but not by the margin: it is taken if it leads by the soft margin, and
    /// the doubt goes with it (see `take_doubt`) so that the person can be told and can correct it. Not right
    /// after a position was set by hand, where nothing is taken on thin evidence.
    #[allow(clippy::too_many_arguments)]
    fn accept_doubtful(
        &mut self,
        pool: &Pool,
        evidence: &Evidence,
        index: usize,
        runner_up: f32,
        soft_needed: f32,
        seen: &Seen,
        since: Instant,
    ) -> Option<Detection> {
        let lead = runner_up - pool.hyps[index].score;
        if self.resync_pending || pool.hyps[index].plies == 0 || !(lead >= soft_needed && soft_needed < f32::INFINITY) {
            return None;
        }
        // Written out before `accept` moves the position on.
        let chance = pool.chances(&[index])[0];
        let alternatives = self.likeliest(pool, 3, Some(index), pool.hyps[index].score);
        let detection = self.accept(pool, evidence, index, runner_up, seen, since)?;
        self.doubt = Some(Doubt { certainty: chance, alternatives });
        Some(detection)
    }

    fn accept(&mut self, pool: &Pool, evidence: &Evidence, index: usize, runner_up: f32, seen: &Seen, since: Instant) -> Option<Detection> {
        self.accept_with(pool, evidence, index, runner_up, seen, since, SQUARE_LIMIT, false)
    }

    /// Takes an explanation. No single square may misfit by more than `limit`. With `adopt`, the squares that do misfit
    /// are not learned from (the models would take a lean, a stain, a reflection for how the square looks) and are
    /// allowed from now on instead (see `update_allow`).
    #[allow(clippy::too_many_arguments)]
    fn accept_with(
        &mut self,
        pool: &Pool,
        evidence: &Evidence,
        index: usize,
        runner_up: f32,
        seen: &Seen,
        since: Instant,
        limit: f32,
        adopt: bool,
    ) -> Option<Detection> {
        let best = &pool.hyps[index];
        let gap = runner_up - best.score;

        if best.plies == 0 {
            // The board shows the position the game is in.
            let set_by_hand = self.resync_pending && best.excess <= RESYNC_FACTOR * self.cfg.max_excess;
            let drift = !set_by_hand && (best.excess > self.cfg.max_excess || best.worst > limit);
            if drift && ((best.excess > ADAPT_FACTOR * self.cfg.max_excess && !adopt) || best.worst > limit) {
                return None; // too much is unexplained, or one square is: not a drift of the light
            }
            self.update_allow(evidence, adopt);
            if set_by_hand {
                // A person set this position: whatever the picture looks like now is what it looks like.
                self.teach(seen, RESYNC_WEIGHT);
                self.remember(seen);
            } else if drift {
                self.teach(seen, if adopt { 1.0 } else { ADAPT_WEIGHT });
                self.remember(seen);
            }
            self.refresh_stale(seen);
            self.resync_pending = false;
            self.unsure = false;
            return Some(Detection::Nothing);
        }

        // A misfit spread thinly over many squares (no square far off) is the models being out of date, not a
        // hand or a knocked piece: it is allowed to be larger when the move wins clearly.
        let diffuse = best.excess > self.cfg.max_excess
            && best.excess <= ADAPT_FACTOR * self.cfg.max_excess
            && gap >= DIFFUSE_GAP * self.cfg.margin
            && !adopt;
        if (best.excess > self.cfg.max_excess && !diffuse && !adopt) || best.worst > limit {
            return None;
        }
        let moves = pool.moves(index);
        for mv in &moves {
            self.position.play_unchecked(*mv);
        }
        self.update_allow(evidence, adopt);
        // A move seen clearly teaches the models what the board looks like now, and more when they were behind.
        if gap >= 2.0 * self.cfg.margin {
            self.teach(seen, if diffuse { ADAPT_WEIGHT } else { 1.0 });
        }
        self.remember(seen);
        self.resync_pending = false;
        self.unsure = false;
        Some(match moves.as_slice() {
            [mv] => Detection::Move { mv: *mv, since, promotion_guess: mv.promotion().is_some() },
            _ => Detection::Sequence { moves, since },
        })
    }

    /// The position the game is in has just been decided: notes which empty squares show a piece that leans over them (their
    /// pictures are not learned), drops the allowances of squares that look normal again or hold something else, and with
    /// `adopt` allows what the odd squares misfit.
    fn update_allow(&mut self, evidence: &Evidence, adopt: bool) {
        let grid = expected_grid(&self.position);
        for square in 0..64 {
            let c = grid[square].index();
            let (_, judged, leaned) = evidence.square_raw(square, &grid);
            let beyond = (judged - evidence.typical[square][c] - SLACK).max(0.0);
            self.leaned[square] = leaned;
            if let Some((content, level)) = self.allow[square]
                && (content != c || beyond < 0.5 * level)
            {
                self.allow[square] = None;
            }
            if adopt && beyond > 0.5 * SQUARE_LIMIT {
                self.allow[square] = Some((c, beyond));
            }
        }
    }

    /// The models of every camera learn its picture as the look of the current position, except the squares a hand
    /// covers, or that do not look like what the position says they hold: those would teach a hand as a piece, and
    /// every later picture of the square would then misfit once the hand has gone. A picture that a person
    /// confirmed (the heavy weight) is learned whole.
    fn teach(&mut self, seen: &Seen, weight: f32) {
        let labels = expected_grid(&self.position);
        // What a person confirmed is learned whole. Else a hand's squares are skipped, and with an ordinary move the
        // squares that do not look like what they should hold, which a hand that looks like the other colour is.
        let confirmed = weight >= RESYNC_WEIGHT;
        let ordinary = weight < ADAPT_WEIGHT;
        for (i, features) in seen {
            let Some(models) = self.views[*i].models.as_mut() else { continue };
            let skip: Vec<bool> = if confirmed {
                Vec::new()
            } else {
                let (costs, typical) = (models.costs(features), models.typical());
                (0..64)
                    .map(|square| {
                        let c = labels[square].index();
                        // (An empty square that shows the head of a piece beside it would become the look of an empty square.)
                        self.covered[square]
                            || (self.leaned[square] && !confirmed)
                            || (ordinary && costs[square][c] - typical[square][c] - SLACK > TEACH_LIMIT)
                    })
                    .collect()
            };
            models.update_except(features, &labels, weight, &skip);
        }
    }

    /// What each camera showed is what the board looks like in the position the game is in now. A square that shows a
    /// hand is kept as it was and marked: the next picture without the hand says what changed there.
    fn remember(&mut self, seen: &Seen) {
        for (i, features) in seen {
            let covered = &self.covered;
            let view = &mut self.views[*i];
            match view.reference.as_mut() {
                Some(reference) if reference.len() == features.len() => {
                    for square in 0..64.min(features.len()) {
                        if covered[square] {
                            view.stale[square] = true;
                        } else {
                            reference[square] = features[square];
                            view.stale[square] = false;
                        }
                    }
                }
                _ => {
                    view.set_reference(features.clone());
                    let n = 64.min(features.len());
                    view.stale[..n].copy_from_slice(&covered[..n]);
                }
            }
        }
    }

    /// The board shows the position, and a square that showed a hand last time does not now: its reference is the
    /// picture of it that has no hand.
    fn refresh_stale(&mut self, seen: &Seen) {
        for (i, features) in seen {
            let view = &mut self.views[*i];
            let Some(reference) = view.reference.as_mut() else { continue };
            for square in 0..64.min(features.len()) {
                if view.stale[square] && !self.covered[square] {
                    reference[square] = features[square];
                    view.stale[square] = false;
                }
            }
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
mod depth_tests;

#[cfg(test)]
mod timing_tests;

#[cfg(test)]
mod blitz_tests;

#[cfg(test)]
mod lean_tests;

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
            Rig::with_config(position, TrackerConfig::default())
        }

        fn with_config(position: Chess, config: TrackerConfig) -> Rig {
            let mut tracker = Tracker::new(config);
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
    fn a_still_picture_that_is_the_same_can_be_counted_without_looking_at_it_and_the_game_is_followed_the_same() {
        let steps = synthetic::game_steps(&OPERA_GAME);
        let expected: Vec<String> = steps.iter().map(|s| uci_of(s.mv)).collect();
        // Once with every picture looked at, once with a picture that is the same as the one before counted without a look
        // whenever the tracker is quiet.
        let mut outcomes: Vec<(Vec<String>, usize)> = Vec::new();
        for thin in [false, true] {
            let mut rig = Rig::new(Chess::default());
            let mut skipped = 0usize;
            let mut seen = Vec::new();
            let mut previous: Option<PieceGrid> = None;
            let mut show = |rig: &mut Rig, grid: &PieceGrid, times: usize, seen: &mut Vec<String>, previous: &mut Option<PieceGrid>| {
                for _ in 0..times {
                    rig.frames += 1;
                    rig.now += Duration::from_millis(333);
                    if thin && previous.as_ref() == Some(grid) && rig.tracker.quiet() && rig.tracker.repeat_still(&[0], rig.now) {
                        skipped += 1;
                        continue;
                    }
                    *previous = Some(*grid);
                    let mut frame = synthetic::frame_from_grid(grid);
                    synthetic::degrade(&mut frame, -15, 10, rig.frames);
                    match rig.tracker.feed(&frame, rig.now) {
                        Detection::Nothing => {}
                        Detection::Move { mv, .. } => seen.push(uci_of(mv)),
                        other => panic!("unexpected {other:?}"),
                    }
                }
            };
            show(&mut rig, &synthetic::start_grid(), 7, &mut seen, &mut previous);
            for step in &steps {
                let mut lifted = step.before;
                lifted[usize::from(step.mv.from().unwrap())] = None;
                show(&mut rig, &lifted, 1, &mut seen, &mut previous);
                // A long still stretch after the move: this is where pictures are the same.
                show(&mut rig, &step.after, 12, &mut seen, &mut previous);
            }
            assert!(rig.tracker.position().is_checkmate());
            outcomes.push((seen, skipped));
        }
        assert_eq!(outcomes[0].0, expected);
        assert_eq!(outcomes[1].0, expected, "the same moves, found from the same pictures, with looks skipped");
        assert_eq!(outcomes[0].1, 0);
        assert!(outcomes[1].1 > steps.len() * 3, "{} looks were skipped", outcomes[1].1);
    }

    #[test]
    fn a_picture_is_not_counted_without_a_look_unless_the_tracker_is_quiet() {
        let mut rig = Rig::new(Chess::default());
        // Learning: nothing to repeat yet.
        assert!(!rig.tracker.quiet() && !rig.tracker.repeat_still(&[0], rig.now));
        rig.show(&synthetic::start_grid(), 7);
        assert_eq!(rig.tracker.status(), Status::Watching);
        assert!(rig.tracker.quiet());
        // A view the tracker has no picture of cannot be repeated.
        assert!(!rig.tracker.repeat_still(&[1], rig.now));
        assert!(rig.tracker.repeat_still(&[0], rig.now + Duration::from_millis(40)));
        // Pieces come and go: the board is moving, and it is looked at.
        let steps = synthetic::game_steps(&["e4"]);
        rig.show(&steps[0].after, 1);
        assert_eq!(rig.tracker.status(), Status::Moving);
        assert!(!rig.tracker.quiet() && !rig.tracker.repeat_still(&[0], rig.now));
        // A hand-set position: the models are stale until a picture of it is seen.
        rig.show(&steps[0].after, 5);
        rig.tracker.set_position(positions_after(&["e4", "e5"])[2].clone());
        assert!(!rig.tracker.quiet());
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

    /// The margin is so wide that no move is ever clear, so every move has to be taken on the soft margin.
    fn unclear_config(soft_margin: f32) -> TrackerConfig {
        TrackerConfig { margin: 1000.0, soft_margin, ..TrackerConfig::default() }
    }

    #[test]
    fn a_move_that_leads_by_the_soft_margin_only_is_taken_with_a_doubt() {
        let steps = synthetic::game_steps(&["e4"]);
        let mut rig = Rig::with_config(Chess::default(), unclear_config(0.5));
        assert!(rig.show(&synthetic::start_grid(), 7).is_empty());
        let detections = rig.show(&steps[0].after, 4);
        match detections.as_slice() {
            [Detection::Move { mv, .. }] => assert_eq!(uci_of(*mv), "e2e4"),
            other => panic!("expected the move to be taken, got {other:?}"),
        }
        let doubt = rig.tracker.take_doubt().expect("it was not clear: the move comes with a doubt");
        assert!(doubt.certainty > 0.5 && doubt.certainty <= 1.0, "{}", doubt.certainty);
        assert!(!doubt.alternatives.is_empty() && doubt.alternatives.len() <= 3);
        assert!(doubt.alternatives.iter().all(|c| c.uci != "e2e4" && c.chance >= 0.0 && c.chance < doubt.certainty));
        assert!(doubt.certainty + doubt.alternatives.iter().map(|c| c.chance).sum::<f32>() <= 1.0001, "chances share one whole");
        assert!(rig.tracker.take_doubt().is_none(), "it is taken once");
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    #[test]
    fn a_clear_move_comes_without_a_doubt_and_the_soft_margin_can_be_turned_off() {
        let steps = synthetic::game_steps(&["e4"]);
        // As usual: the picture is clear, the move is taken, no doubt.
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 7);
        assert!(matches!(rig.show(&steps[0].after, 4).as_slice(), [Detection::Move { .. }]));
        assert!(rig.tracker.take_doubt().is_none());
        // The soft margin as wide as the margin: nothing is taken on thin evidence, the person is asked.
        let mut strict = Rig::with_config(Chess::default(), unclear_config(1000.0));
        strict.show(&synthetic::start_grid(), 7);
        let detections = strict.show(&steps[0].after, 4);
        assert!(detections.iter().all(|d| matches!(d, Detection::Unsure { .. })), "{detections:?}");
        match detections.last() {
            Some(Detection::Unsure { candidates }) => {
                assert_eq!(candidates[0].uci, "e2e4");
                assert!(candidates[0].chance > 0.5, "{}", candidates[0].chance);
            }
            other => panic!("expected a list of candidates, got {other:?}"),
        }
        assert!(strict.tracker.take_doubt().is_none());
    }

    #[test]
    fn a_held_tracker_decides_nothing_until_a_move_is_entered() {
        let steps = synthetic::game_steps(&["e4", "e5"]);
        let mut rig = Rig::new(Chess::default());
        rig.show(&synthetic::start_grid(), 7);
        // The person took back a doubtful move: the picture shows e4, the position says the start.
        let offered = vec![Candidate { uci: "e2e4".into(), san: "e4".into(), cost: 0.0, chance: 0.6 }];
        rig.tracker.hold(offered);
        let detections = rig.show(&steps[0].after, 5);
        assert!(
            !detections.is_empty() && detections.iter().all(|d| matches!(d, Detection::Unsure { candidates } if candidates.len() == 1)),
            "{detections:?}"
        );
        assert_eq!(rig.tracker.status(), Status::Unsure);
        // The person says what was played: the hold ends and tracking goes on by itself.
        let mut after_e4 = Chess::default();
        after_e4.play_unchecked(crate::game::parse_move(&after_e4, "e2e4").unwrap());
        rig.tracker.set_position(after_e4);
        rig.show(&steps[0].after, 3);
        assert_eq!(rig.tracker.status(), Status::Watching);
        assert!(matches!(rig.show(&steps[1].after, 4).as_slice(), [Detection::Move { .. }]));
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

    /// Two cameras on opposite sides of the board (or only the first, to compare). `covered` says, for a picture,
    /// which squares a hand hides in which camera.
    struct TwoCameras {
        tracker: Tracker,
        now: Instant,
        frames: u64,
    }

    impl TwoCameras {
        fn new(position: Chess, second: bool) -> TwoCameras {
            let mut tracker = Tracker::new(TrackerConfig::default());
            tracker.set_view_calibration(0, Some(&synthetic::corners()));
            if second {
                tracker.set_view_calibration(1, Some(&synthetic::second_corners()));
            }
            tracker.start(position);
            TwoCameras { tracker, now: Instant::now(), frames: 0 }
        }

        fn show(&mut self, grid: &PieceGrid, times: usize, covered: &[(usize, usize)]) -> Vec<Detection> {
            self.show_each(grid, grid, times, covered)
        }

        /// The cameras show different boards (one of them is wrong, or late).
        fn show_each(
            &mut self,
            first_grid: &PieceGrid,
            second_grid: &PieceGrid,
            times: usize,
            covered: &[(usize, usize)],
        ) -> Vec<Detection> {
            let mut out = Vec::new();
            for _ in 0..times {
                self.frames += 1;
                self.now += Duration::from_millis(333);
                let mut first = synthetic::frame_from_grid(first_grid);
                synthetic::degrade(&mut first, -15, 10, self.frames);
                let mut second = synthetic::seen_from_the_other_side(&synthetic::frame_from_grid(second_grid));
                synthetic::degrade(&mut second, -10, 10, self.frames + 5000);
                for &(camera, square) in covered {
                    if camera == 0 {
                        synthetic::occlude(&mut first, &synthetic::corners(), square, 0.6);
                    } else {
                        synthetic::occlude(&mut second, &synthetic::second_corners(), square, 0.6);
                    }
                }
                match self.tracker.feed_views(&[Some(&first), Some(&second)], self.now) {
                    Detection::Nothing => {}
                    other => out.push(other),
                }
            }
            out
        }
    }

    #[test]
    fn two_cameras_follow_a_whole_game_as_one_does() {
        let steps = synthetic::game_steps(&OPERA_GAME);
        let mut rig = TwoCameras::new(Chess::default(), true);
        assert!(rig.show(&synthetic::start_grid(), 7, &[]).is_empty());
        assert_eq!(rig.tracker.status(), Status::Watching);
        assert_eq!(rig.tracker.views_in_use(), vec![true, true]);

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
        assert!(rig.tracker.position().is_checkmate());
    }

    /// An arm comes into one camera's picture, over the squares where a move is played, and stays there. One
    /// camera cannot see the move; the other one can.
    #[test]
    fn a_camera_that_cannot_see_the_squares_is_helped_by_the_other() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3", "Nc6"]);
        let covered = [(0, usize::from(Square::E2)), (0, usize::from(Square::E4)), (0, usize::from(Square::E3))];

        let mut results = Vec::new();
        for second in [false, true] {
            let mut rig = TwoCameras::new(Chess::default(), second);
            rig.show(&synthetic::start_grid(), 7, &[]);
            // The arm arrives before the move (a few pictures with it in the way and nothing else happening).
            rig.show(&synthetic::start_grid(), 4, &covered);
            let found = rig.show(&steps[0].after, 5, &covered);
            let right = found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e2e4"));
            let wrong = found
                .iter()
                .any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) != "e2e4") || matches!(d, Detection::Sequence { .. }));
            results.push((right, wrong));
        }
        assert!(!results[0].1 && !results[1].1, "no wrong move may be recorded: {results:?}");
        assert!(results[1].0, "with the other camera the move is found: {results:?}");
        assert!(!results[0].0, "the camera with the arm in the way cannot have found it on its own: {results:?}");
    }

    #[test]
    fn a_camera_that_gives_no_pictures_is_left_out_and_comes_back_by_itself() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3", "Nc6", "Bc4"]);
        let mut rig = TwoCameras::new(Chess::default(), true);
        rig.show(&synthetic::start_grid(), 7, &[]);

        // The second camera stops: the game goes on with the first.
        rig.tracker.set_view_active(1, false);
        assert_eq!(rig.tracker.views_in_use(), vec![true, false]);
        let found = rig.show(&steps[0].after, 5, &[]);
        assert!(matches!(found.as_slice(), [Detection::Move { .. }]), "{found:?}");
        let found = rig.show(&steps[1].after, 5, &[]);
        assert!(matches!(found.as_slice(), [Detection::Move { .. }]), "{found:?}");

        // It comes back: its squares were learned before, and it takes part again.
        rig.tracker.set_view_active(1, true);
        assert_eq!(rig.tracker.views_in_use(), vec![true, true]);
        let found = rig.show(&steps[2].after, 6, &[]);
        assert!(matches!(found.as_slice(), [Detection::Move { .. }]), "{found:?}");
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    /// A camera that shows the old board (stuck, or late) makes the tracker unsure; when it is left out, the other
    /// camera alone is clear, and the tracker decides with that one. (Judging again whenever the set of cameras changes
    /// also covers the case where the board read is the same as before, which only shows with real timing: see
    /// `a_second_camera_that_goes_away_during_a_game_does_not_stop_it` in `server`.)
    #[test]
    fn a_camera_that_drops_out_while_the_tracker_is_unsure_lets_it_decide_with_the_other() {
        let steps = synthetic::game_steps(&["e4"]);
        let mut rig = TwoCameras::new(Chess::default(), true);
        rig.show(&synthetic::start_grid(), 7, &[]);
        let found = rig.show_each(&steps[0].after, &synthetic::start_grid(), 6, &[]);
        assert!(found.iter().all(|d| matches!(d, Detection::Unsure { .. })), "the cameras disagree: nothing is taken: {found:?}");
        assert_eq!(rig.tracker.status(), Status::Unsure);

        rig.tracker.set_view_active(1, false);
        let found = rig.show_each(&steps[0].after, &synthetic::start_grid(), 6, &[]);
        assert!(found.iter().any(|d| matches!(d, Detection::Move { mv, .. } if uci_of(*mv) == "e2e4")), "{found:?}");
        assert_eq!(rig.tracker.status(), Status::Watching);
    }

    #[test]
    fn a_camera_calibrated_during_the_game_learns_its_squares_when_the_position_is_confirmed() {
        let steps = synthetic::game_steps(&["e4", "e5", "Nf3"]);
        let mut rig = TwoCameras::new(Chess::default(), false);
        rig.show(&synthetic::start_grid(), 7, &[]);
        let found = rig.show(&steps[0].after, 5, &[]);
        assert!(matches!(found.as_slice(), [Detection::Move { .. }]), "{found:?}");
        assert_eq!(rig.tracker.views_in_use(), vec![true]);

        // The second camera is switched on in the middle of the game: the first goes on, the second learns.
        rig.tracker.set_view_calibration(1, Some(&synthetic::second_corners()));
        assert_eq!(rig.tracker.status(), Status::Watching, "the game goes on with the first camera");
        let found = rig.show(&steps[0].after, 6, &[]);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(rig.tracker.views_in_use(), vec![true, true]);
        let found = rig.show(&steps[1].after, 5, &[]);
        assert!(matches!(found.as_slice(), [Detection::Move { .. }]), "{found:?}");
    }
}
