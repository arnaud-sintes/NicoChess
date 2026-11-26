//! Cheap tags for the pictures the tracker is given, and the judgement of which of them need not be looked at.
//!
//! A tag is information about a picture, made from a rough look at it (a [`Glance`]: the colour of the middle of every
//! square, from a few points, a sixteenth of what the tracker reads): how much of the board changed since the picture before,
//! whether it is the same as that one (`duplicate`: no square differs by more than the sensor's noise), and whether a large part
//! of the board differs from how it looked when it was last still, in the way a hand or an arm changes it, not a piece
//! (`occluded`). Tags never change which pictures the tracker looks at.
//!
//! The same tags say when a look may be skipped while the analysis is far behind the cameras (see `analysis::Thin`): never
//! unless the board has been quiet for seconds, the tracker is sure of it (`Tracker::quiet`), and the pictures are the same as
//! the ones the tracker last looked at, square by square. The judgement is made from the pictures alone, so a replay of the
//! same pictures takes the same decisions; a recording also says which looks were skipped.

use crate::engine::VIEWS;
use crate::vision::Glance;
use std::time::{Duration, Instant};

/// A square differs from the same square in another picture when it differs by more than this many grey levels (the mean of
/// the three colours, after the change of light over the whole board is taken out). The sensor's noise on the mean of 81
/// pixels is about a grey level; a white piece on a light square, the smallest change of a move, is 10 to 15.
pub const SQUARE_LIMIT: f32 = 4.0;
/// A square is covered when it differs from how it looked when the board was last still by more than this: a hand over a
/// square, or an arm, changes it by far more than a piece standing on it (or leaving it) does not, and by more than a shadow.
pub const COVERED_LIMIT: f32 = 25.0;
/// The board is occluded when at least this many squares are covered: a move changes two to four squares (a capture of a piece
/// that stands next to another, castling), a hand holding a piece over the board covers more.
pub const OCCLUDED_SQUARES: usize = 6;
/// The board counts as still after this many pictures in a row without a changed square; its look is then the reference
/// that the covered squares are counted against.
const STILL_PICTURES: usize = 3;
/// A board that has looked the same, occluded, for this long is taken as the new reference: else it would be occluded for
/// ever after a change that is not a hand (the light changed, the board was moved, or several moves were played one after the
/// other: more than a few squares differ from the last still picture then, and the count cannot tell that from an arm until
/// the picture has stayed). A hand that rests on the board for this long is taken for the board too, and the picture of it
/// leaving then counts as an occlusion of its own: the tag is information, not a decision.
const ADOPT_AFTER: Duration = Duration::from_secs(2);
/// A look is skipped only when the board has had no changed square for this long (on the capture times of the pictures).
pub const QUIET_FOR: Duration = Duration::from_secs(3);
/// At most this many looks of a view are skipped one after the other: then one is looked at, whatever the pictures say, so
/// that a change too slow for the glance to see cannot hide for long.
pub const MAX_THIN_RUN: u32 = 8;

/// How a picture compares with another: the residual of each square (the difference of its colour from the other picture's,
/// less the change of the light over the whole board, as the mean of the three colours), and the change of the light.
pub struct Diff {
    pub residual: [f32; 64],
    /// The mean over the colours of the median change of the squares: the light of the whole picture.
    pub light: f32,
}

impl Diff {
    pub fn between(a: &Glance, b: &Glance) -> Diff {
        // The light changes all over the board at once (the lamp, a cloud): the median of the squares' changes is that, since
        // a move or a hand changes only some of the squares.
        let mut shift = [0.0f32; 3];
        for (c, shift) in shift.iter_mut().enumerate() {
            let mut d: Vec<f32> = (0..64).map(|s| a.cells[s][c] - b.cells[s][c]).collect();
            d.sort_by(f32::total_cmp);
            *shift = 0.5 * (d[31] + d[32]);
        }
        let mut residual = [0.0f32; 64];
        for (s, r) in residual.iter_mut().enumerate() {
            *r = (0..3).map(|c| (a.cells[s][c] - b.cells[s][c] - shift[c]).abs()).sum::<f32>() / 3.0;
        }
        Diff { residual, light: shift.iter().map(|x| x.abs()).sum::<f32>() / 3.0 }
    }

    /// How many squares differ by more than `limit`.
    pub fn over(&self, limit: f32) -> usize {
        self.residual.iter().filter(|&&r| r > limit).count()
    }
}

/// What is known about a picture from a glance.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tag {
    /// Squares that differ from the picture before (of the same view) by more than [`SQUARE_LIMIT`]: all 64 for the first.
    pub changed: usize,
    /// Squares that differ from the last still picture by more than [`COVERED_LIMIT`].
    pub covered: usize,
    /// The picture is the same as the one before it, as far as a glance can tell.
    pub duplicate: bool,
    /// A hand or an arm is over the board (see [`OCCLUDED_SQUARES`]).
    pub occluded: bool,
}

/// A stretch of pictures of one view that were occluded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Occlusion {
    pub view: usize,
    /// The capture times of the first and the last picture of the stretch.
    pub from: Instant,
    pub to: Instant,
    pub pictures: u32,
    /// The most squares covered in one picture.
    pub peak: usize,
}

#[derive(Clone)]
struct Open {
    from: Instant,
    last: Instant,
    pictures: u32,
    peak: usize,
}

#[derive(Default)]
struct ViewTags {
    /// The picture before.
    prev: Option<Glance>,
    /// The last picture that was still, and not occluded.
    reference: Option<Glance>,
    /// Pictures in a row without a changed square.
    still: usize,
    /// When a square last changed (capture time), and when the stretch of pictures without a change began.
    last_change: Option<Instant>,
    still_since: Option<Instant>,
    occlusion: Option<Open>,
    /// What the tracker last looked at.
    analysed: Option<Glance>,
    /// Looks skipped one after the other.
    run: u32,
}

/// Tags the pictures of every view, one after the other in the order of their capture, and judges which looks may be skipped.
pub struct Tagger {
    views: [ViewTags; VIEWS],
    closed: Vec<Occlusion>,
    /// How long the board must have had no changed square for a look to be skipped ([`QUIET_FOR`] unless a test shortens it).
    quiet_for: Duration,
}

impl Default for Tagger {
    fn default() -> Tagger {
        Tagger { views: Default::default(), closed: Vec::new(), quiet_for: QUIET_FOR }
    }
}

impl Tagger {
    pub fn new() -> Tagger {
        Tagger::default()
    }

    /// Shortens (or lengthens) the quiet time that a look needs to be skipped: for tests, whose games are quick.
    pub fn set_quiet_for(&mut self, quiet_for: Duration) {
        self.quiet_for = quiet_for;
    }

    /// Tags a picture (given as its glance) of `view`, captured at `at`, and keeps what the next one is compared with.
    pub fn tag(&mut self, view: usize, at: Instant, glance: &Glance) -> Tag {
        let Some(state) = self.views.get_mut(view) else { return Tag::default() };
        // The squares moved in the picture (the board was followed, calibrated again): what was compared with is of no use.
        if state.prev.as_ref().is_some_and(|p| p.sampler != glance.sampler) {
            if let Some(o) = state.occlusion.take() {
                self.closed.push(Occlusion { view, from: o.from, to: o.last, pictures: o.pictures, peak: o.peak });
            }
            *state = ViewTags::default();
        }
        let changed = match &state.prev {
            Some(prev) => Diff::between(glance, prev).over(SQUARE_LIMIT),
            None => 64,
        };
        if changed > 0 {
            state.last_change = Some(at);
            state.still = 0;
            state.still_since = None;
        } else {
            state.still += 1;
            state.still_since.get_or_insert(at);
        }
        let covered = state.reference.as_ref().map_or(0, |r| Diff::between(glance, r).over(COVERED_LIMIT));
        let mut occluded = covered >= OCCLUDED_SQUARES;
        if state.still >= STILL_PICTURES {
            // Still: a board that is not occluded is the reference now. One that has been the same, occluded, for long is
            // too (what is over it does not go away): it is not occluded any more.
            let long = state.still_since.is_some_and(|since| at.saturating_duration_since(since) >= ADOPT_AFTER);
            if !occluded || long {
                state.reference = Some(glance.clone());
                occluded = false;
            }
        } else if state.reference.is_none() {
            // Nothing is known of the board yet: the first pictures are as good a reference as any.
            state.reference = Some(glance.clone());
        }
        match (&mut state.occlusion, occluded) {
            (Some(open), true) => {
                open.last = at;
                open.pictures += 1;
                open.peak = open.peak.max(covered);
            }
            (None, true) => state.occlusion = Some(Open { from: at, last: at, pictures: 1, peak: covered }),
            (open @ Some(_), false) => {
                if let Some(o) = open.take() {
                    self.closed.push(Occlusion { view, from: o.from, to: o.last, pictures: o.pictures, peak: o.peak });
                }
            }
            (None, false) => {}
        }
        state.prev = Some(glance.clone());
        Tag { changed, covered, duplicate: changed == 0, occluded }
    }

    /// The stretches of occluded pictures that ended since the last call.
    pub fn take_occlusions(&mut self) -> Vec<Occlusion> {
        std::mem::take(&mut self.closed)
    }

    /// Whether looking at these pictures (`(view, captured, glance)`, already tagged) may be skipped: each is the same as the
    /// picture of its view that the tracker last looked at (no square differs by more than [`SQUARE_LIMIT`], by the light
    /// of the whole board taken out), no square of the view has changed for [`QUIET_FOR`], none is occluded, and not too
    /// many looks were skipped just before.
    pub fn may_skip(&self, pictures: &[(usize, Instant, &Glance)]) -> bool {
        !pictures.is_empty()
            && pictures.iter().all(|&(view, at, glance)| {
                let Some(state) = self.views.get(view) else { return false };
                let Some(analysed) = state.analysed.as_ref().filter(|a| a.sampler == glance.sampler) else { return false };
                let quiet = match state.last_change {
                    Some(changed) => at.saturating_duration_since(changed) >= self.quiet_for,
                    None => false,
                };
                quiet && state.occlusion.is_none() && state.run < MAX_THIN_RUN && Diff::between(glance, analysed).over(SQUARE_LIMIT) == 0
            })
    }

    /// The tracker looked at this picture: it is what the next ones are compared with.
    pub fn analysed(&mut self, view: usize, glance: &Glance) {
        if let Some(state) = self.views.get_mut(view) {
            state.analysed = Some(glance.clone());
            state.run = 0;
        }
    }

    /// The look at this view was skipped.
    pub fn skipped(&mut self, view: usize) {
        if let Some(state) = self.views.get_mut(view) {
            state.run += 1;
        }
    }

    /// Forgets what was compared with (the squares moved, a new game): the next pictures are looked at.
    pub fn forget(&mut self) {
        for (view, state) in self.views.iter_mut().enumerate() {
            state.analysed = None;
            state.reference = None;
            state.prev = None;
            state.still = 0;
            state.still_since = None;
            state.last_change = None;
            state.run = 0;
            if let Some(o) = state.occlusion.take() {
                self.closed.push(Occlusion { view, from: o.from, to: o.last, pictures: o.pictures, peak: o.peak });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(level: f32) -> Glance {
        Glance { cells: [[level; 3]; 64], sampler: 1 }
    }

    /// A board with a pattern, so that squares differ from one another.
    fn board() -> Glance {
        let mut g = flat(100.0);
        for s in 0..64 {
            let light = (s % 8 + s / 8) % 2 == 1;
            g.cells[s] = if light { [180.0; 3] } else { [60.0; 3] };
        }
        g
    }

    fn covered(mut g: Glance, squares: &[usize]) -> Glance {
        for &s in squares {
            g.cells[s] = [200.0, 150.0, 120.0];
        }
        g
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_same_board_again_is_a_duplicate_and_a_light_change_all_over_it_is_not_a_change() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        assert_eq!(tagger.tag(0, at(base, 0), &a).changed, 64, "the first picture is all new");
        assert!(tagger.tag(0, at(base, 40), &a).duplicate);
        let mut brighter = a.clone();
        for cell in &mut brighter.cells {
            for c in cell.iter_mut() {
                *c += 6.0;
            }
        }
        let tag = tagger.tag(0, at(base, 80), &brighter);
        assert!(tag.duplicate, "{tag:?}: the light of the whole board changed, no square did");
    }

    #[test]
    fn a_piece_on_a_light_square_is_a_change_and_not_an_occlusion() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for i in 0..5 {
            tagger.tag(0, at(base, i * 40), &a);
        }
        // A white piece on a light square: 12 grey levels.
        let mut moved = a.clone();
        moved.cells[1] = [168.0; 3];
        moved.cells[10] = [190.0; 3];
        let tag = tagger.tag(0, at(base, 300), &moved);
        assert_eq!(tag.changed, 2, "{tag:?}");
        assert!(!tag.occluded && !tag.duplicate);
    }

    #[test]
    fn a_hand_over_many_squares_is_an_occlusion_that_is_told_once_when_it_ends() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for i in 0..5 {
            tagger.tag(0, at(base, i * 40), &a);
        }
        let hand = covered(a.clone(), &[18, 19, 20, 26, 27, 28, 34, 35]);
        for i in 0..4 {
            let tag = tagger.tag(0, at(base, 300 + i * 40), &hand);
            assert!(tag.occluded, "{tag:?}");
        }
        assert!(tagger.take_occlusions().is_empty(), "it is not over");
        let tag = tagger.tag(0, at(base, 600), &a);
        assert!(!tag.occluded);
        let told = tagger.take_occlusions();
        assert_eq!(told.len(), 1);
        assert_eq!((told[0].view, told[0].pictures, told[0].peak), (0, 4, 8));
        assert!(tagger.take_occlusions().is_empty());
    }

    #[test]
    fn an_occlusion_that_does_not_go_away_becomes_the_board_after_a_while() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for i in 0..5 {
            tagger.tag(0, at(base, i * 40), &a);
        }
        let other = covered(a, &[0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let mut last = Tag::default();
        for i in 0..400 {
            last = tagger.tag(0, at(base, 300 + i * 40), &other);
        }
        assert!(!last.occluded, "{last:?}: 16 s of the same picture");
        assert_eq!(tagger.take_occlusions().len(), 1);
    }

    #[test]
    fn several_moves_played_one_after_the_other_are_the_board_after_a_moment_and_not_an_occlusion_for_ever() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for i in 0..5 {
            tagger.tag(0, at(base, i * 40), &a);
        }
        // Four plies: eight squares differ from the last still picture, and stay so.
        let played = covered(a, &[8, 9, 16, 17, 40, 41, 48, 49]);
        let first = tagger.tag(0, at(base, 300), &played);
        assert!(first.occluded, "{first:?}: a count of squares cannot tell this from an arm at once");
        let mut last = first;
        for i in 1..80 {
            last = tagger.tag(0, at(base, 300 + i * 40), &played);
        }
        assert!(!last.occluded && last.duplicate, "{last:?}: it has stayed for 3 s: it is the board");
        assert_eq!(tagger.take_occlusions().len(), 1);
        // The next move is judged against it: two squares are no occlusion.
        let mut next = played.clone();
        next.cells[27] = [30.0; 3];
        next.cells[28] = [210.0; 3];
        let tag = tagger.tag(0, at(base, 3600), &next);
        assert!(!tag.occluded && tag.changed == 2, "{tag:?}");
    }

    #[test]
    fn glances_of_another_sampler_are_not_compared() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for i in 0..5 {
            tagger.tag(0, at(base, i * 40), &a);
        }
        // The board was followed: the squares are read somewhere else, every number is different.
        let mut moved = covered(a.clone(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        moved.sampler = 2;
        let tag = tagger.tag(0, at(base, 300), &moved);
        assert_eq!(tag.changed, 64, "{tag:?}: all new");
        assert!(!tag.occluded, "nothing to compare with");
        assert!(!tagger.may_skip(&[(0, at(base, 9000), &moved)]), "nothing of this sampler was looked at");
        tagger.analysed(0, &moved);
        for ms in (340..4000).step_by(40) {
            tagger.tag(0, at(base, ms), &moved);
        }
        assert!(tagger.may_skip(&[(0, at(base, 4000), &moved)]));
    }

    #[test]
    fn a_look_is_skipped_only_when_the_board_has_been_quiet_and_is_the_one_last_looked_at() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        let tag_all = |tagger: &mut Tagger, from: u64, to: u64, g: &Glance| {
            for ms in (from..to).step_by(40) {
                tagger.tag(0, at(base, ms), g);
            }
        };
        tag_all(&mut tagger, 0, 400, &a);
        tagger.analysed(0, &a);
        assert!(!tagger.may_skip(&[(0, at(base, 400), &a)]), "quiet for only 0.4 s");
        tag_all(&mut tagger, 400, 3600, &a);
        assert!(tagger.may_skip(&[(0, at(base, 3600), &a)]));
        // A different board is looked at.
        let mut moved = a.clone();
        moved.cells[9] = [170.0; 3];
        assert!(!tagger.may_skip(&[(0, at(base, 3640), &moved)]));
        // Not for a view that was never looked at, nor with nothing to go by.
        assert!(!tagger.may_skip(&[(1, at(base, 3600), &a)]));
        assert!(!tagger.may_skip(&[]));
    }

    #[test]
    fn after_a_run_of_skipped_looks_one_is_looked_at() {
        let base = Instant::now();
        let mut tagger = Tagger::new();
        let a = board();
        for ms in (0..4000).step_by(40) {
            tagger.tag(0, at(base, ms), &a);
        }
        tagger.analysed(0, &a);
        for _ in 0..MAX_THIN_RUN {
            assert!(tagger.may_skip(&[(0, at(base, 4000), &a)]));
            tagger.skipped(0);
        }
        assert!(!tagger.may_skip(&[(0, at(base, 4000), &a)]));
        tagger.analysed(0, &a);
        assert!(tagger.may_skip(&[(0, at(base, 4040), &a)]));
    }
}
