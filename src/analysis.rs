//! The analysis stage: one thread that owns the tracker and takes, in order, the pictures of every source and the
//! commands of the people.
//!
//! Capture threads stamp a picture and put it in the [`Inbox`], which never waits and never drops a picture while
//! it is within its memory budget; the analysis may run behind the cameras (the page shows a move a little late, and
//! says so), but nothing that may hold a move is thrown away. Everything that changes what the tracker or the game
//! believes (a game started, a move typed by hand, an undo, a calibration) is a [`Job`] in the same inbox: it runs after
//! the pictures that were queued before it was sent, so that a person who takes a move back while the analysis is two
//! seconds behind takes back the move that they saw, not one that the analysis finds a moment later.
//!
//! The [`Analyzer`] is the part that decides when the tracker looks and at which pictures. It knows nothing of threads or
//! of the engine: it is given pictures one by one (in the order of their capture) and the state of the engine that
//! a look depends on (a [`Context`]), so that a replay runs the very same rule as the live program.

use crate::engine::{CAMERAS, DEPTH_VIEW, Engine, VIEWS};
use crate::stage::{Budget, cost_of};
use crate::tags::{Occlusion, Tag, Tagger};
use crate::tracker::{Detection, Tracker};
use image::RgbImage;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long a source that is not the oldest in the queue is waited for, since its last picture, before a picture of
/// another source is taken without it: pictures are taken in the order of their capture, and a source that has gone
/// quiet (a camera that is off, a depth camera that is slow) must not hold the others up.
pub const LAG_WAIT: Duration = Duration::from_millis(150);

/// A camera that has not given a picture since play began is waited for this long, so that all learn the squares together.
pub const CAMERA_WAIT: Duration = Duration::from_secs(10);

/// The memory that pictures waiting for the analysis may take, by default. The pictures are shared with the preview and
/// the recorder, which count them in their own budgets: this one is a safety net for an analysis that cannot keep up for
/// minutes, not a way of keeping up.
pub const ANALYSIS_BUDGET: usize = 2 << 30;

/// The analysis is behind the cameras when this many pictures of a view wait for it, or the oldest has waited this long
/// (since its capture): only then may a look at a picture that is the same as the last one looked at be skipped (see [`Thin`]).
pub const THIN_WAITING: usize = 8;
pub const THIN_AGE: Duration = Duration::from_secs(1);

/// When the analysis may skip the look at a picture that is the same as the one it last looked at, on a board that has been
/// quiet for seconds and that the tracker is sure of (see `tags::Tagger::may_skip` and `Tracker::quiet`). The picture is
/// kept in the recording whatever is decided; the tracker is told that one more still picture went by (`Tracker::repeat_still`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Thin {
    /// Every look is made.
    Never = 0,
    /// Looks are skipped while the analysis is behind the cameras (see [`Context::behind`]), and only then: what the program
    /// does when it keeps up is what it always did. The default.
    #[default]
    Behind = 1,
    /// Looks are skipped whenever they may be, behind or not: for tests, and for finding out on real recordings whether
    /// skipping changes what the tracker finds (`--replay-thin`).
    Always = 2,
}

impl Thin {
    pub fn parse(text: &str) -> Option<Thin> {
        match text {
            "never" | "off" => Some(Thin::Never),
            "behind" => Some(Thin::Behind),
            "always" => Some(Thin::Always),
            _ => None,
        }
    }
}

/// What `--thin` said, for the engine that the program builds (the command line sets it before the engine is made).
static GLOBAL_THIN: AtomicU8 = AtomicU8::new(1);

pub fn set_global_thin(thin: Thin) {
    GLOBAL_THIN.store(thin as u8, Ordering::Relaxed);
}

pub fn global_thin() -> Thin {
    match GLOBAL_THIN.load(Ordering::Relaxed) {
        0 => Thin::Never,
        2 => Thin::Always,
        _ => Thin::Behind,
    }
}

/// A picture as the analysis takes it.
#[derive(Clone)]
pub struct Item {
    /// 0 and 1 are the cameras, 2 (`DEPTH_VIEW`) is the height picture of the depth camera.
    pub view: usize,
    /// The number of the picture among those of its view (a hole in the numbers is a picture that was lost).
    pub seq: u64,
    /// When the camera made the picture, as far as the source knows (else when the program got it).
    pub captured: Instant,
    /// How long the capture thread waited for it inside the camera (none for the depth camera): close to nothing means
    /// that the picture was already there, so the program was not keeping up with the camera.
    pub waited: Option<Duration>,
    /// What the tracker looks at: turned, and adjusted in brightness and contrast.
    pub image: Arc<RgbImage>,
    /// What a recording keeps: turned, not adjusted (the same picture when nothing is adjusted).
    pub raw: Arc<RgbImage>,
}

impl Item {
    /// What the picture takes in a queue.
    pub fn cost(&self) -> usize {
        cost_of(&self.image) + if Arc::ptr_eq(&self.image, &self.raw) { 0 } else { cost_of(&self.raw) }
    }
}

/// What a look of the tracker depends on besides the pictures: the state of the engine at that moment.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    /// A game is being played (the tracker looks only then).
    pub playing: bool,
    /// For each camera: it has a calibration, gives pictures of the size it was made for, and is in use.
    pub usable: [bool; CAMERAS],
    /// The depth camera is wanted and has found the squares.
    pub depth: bool,
    /// The least time between two looks.
    pub interval: Duration,
    /// A camera whose newest picture is older than this (when a look begins) is left out of it, and the others go on.
    pub fresh: Duration,
    /// When play began (see [`CAMERA_WAIT`]).
    pub begun_at: Option<Instant>,
    /// The pictures wait for the analysis in numbers (see [`THIN_WAITING`]): a look may be skipped (see [`Thin`]).
    pub behind: bool,
}

/// One look of the tracker: what it was given, and what it found.
pub struct Look {
    /// When the picture that started the look was captured: the time the tracker was given (a move is "since" a moment
    /// counted from capture times, which a lag of the analysis does not change).
    pub at: Instant,
    /// When the tracker began on it, and how long it took.
    pub started: Instant,
    pub step: Duration,
    /// The pictures it was given together (one per view at most).
    pub fed: Vec<Item>,
    /// When each of them was captured, counted from `at`, in microseconds: before it is negative. The picture that began
    /// the look has none; the others are how far from it in time the picture that was chosen for the view is.
    pub skews_us: Vec<i64>,
    /// The view of the picture that began the look.
    pub trigger: usize,
    /// The number of the newest picture of each view that had come to the analysis when the look was made (a look may wait
    /// for the next picture of another view, so it is made when a later picture comes than the ones it was given).
    pub released: [u64; VIEWS],
    /// The look was made because the analysis was asked to decide at once (a command, or no picture for a while), not
    /// because a picture came (see [`Analyzer::flush_look`]).
    pub flushed: bool,
    /// Views that took part and were given no picture: none was captured within the tolerance of the picture that began
    /// the look (see [`Pairing::Nearest`]).
    pub unpaired: [bool; VIEWS],
    /// Which views took part.
    pub active: [bool; VIEWS],
    /// The set of views that took part is not the one of the look before (a recording says so).
    pub views_changed: bool,
    /// The tracker did not look at these pictures: they were the same as the ones it last looked at, it was sure of the
    /// board, and the analysis was behind (see [`Thin`]); it was told that one more still picture went by.
    pub thinned: bool,
    /// What a glance says of each picture that was given (view, tag).
    pub tags: Vec<(usize, Tag)>,
    /// Stretches of occluded pictures that ended with this look.
    pub occlusions: Vec<Occlusion>,
    pub detection: Detection,
}

/// How the pictures that the tracker is given together are chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pairing {
    /// For each other view, the picture captured nearest in time to the one that began the look, if it is within the
    /// tolerance of that view (see [`Pace::tolerance`]); the earlier one when two are as near. When the view's next
    /// picture may still turn out to be the nearest, the look waits for it, for a little (see [`PAIR_WAIT_MAX`]).
    #[default]
    Nearest,
    /// For each other view, the newest picture that had come when the look began (what recordings of format 5 and before
    /// were made with: kept so that a replay of every picture of such a recording takes the pictures as it did).
    Newest,
}

/// The least and the greatest tolerance of a view, and the one used until its pace is known.
pub const PAIR_TOLERANCE_MIN: Duration = Duration::from_millis(40);
pub const PAIR_TOLERANCE_MAX: Duration = Duration::from_millis(400);
pub const PAIR_TOLERANCE_DEFAULT: Duration = Duration::from_millis(100);
/// How much of a view's own interval between pictures its tolerance is: one picture lost or late still pairs, two do not.
const TOLERANCE_INTERVALS: f32 = 1.5;
/// A view that has not given a picture for this many of its intervals (and at least [`LIVE_MIN`]) is not waited for.
const LIVE_INTERVALS: u32 = 3;
const LIVE_MIN: Duration = Duration::from_millis(150);
/// The longest a look waits, counted on capture times, for the next picture of a view that may be the nearest. A view that
/// is slower than this still pairs, with its newest picture before the look, up to one of its intervals away.
pub const PAIR_WAIT_MAX: Duration = Duration::from_millis(120);
/// How many gaps between pictures tell a view's pace (their median), and how many pictures of a view are kept to pair with,
/// and for how long: a look is decided within the greatest tolerance and the longest wait of its picture, and the pictures
/// are big (a 1280x720 picture takes 2.7 MB), so no more than that is kept.
const PACE_SAMPLES: usize = 9;
const HISTORY_MAX: usize = 32;
const HISTORY_AGE: Duration = Duration::from_millis(700);

/// The pace of one view, from the capture times of the pictures it gave: a view that is quiet, or slow, or lost a picture,
/// has other limits than one that gives a picture every 40 ms.
#[derive(Default)]
struct Pace {
    last: Option<Instant>,
    gaps: VecDeque<Duration>,
}

impl Pace {
    fn note(&mut self, at: Instant) {
        if let Some(last) = self.last {
            // A picture that comes after a newer one (late from its camera) says nothing about the pace.
            if at > last {
                self.gaps.push_back(at - last);
                if self.gaps.len() > PACE_SAMPLES {
                    self.gaps.pop_front();
                }
            }
        }
        self.last = Some(self.last.map_or(at, |last| last.max(at)));
    }

    /// The usual time between two pictures: the median of the last gaps, once there are three.
    fn interval(&self) -> Option<Duration> {
        if self.gaps.len() < 3 {
            return None;
        }
        let mut sorted: Vec<Duration> = self.gaps.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }

    /// How far in time a picture of this view may be from the one that began a look and still go with it: 1.5 of its
    /// intervals between pictures, between [`PAIR_TOLERANCE_MIN`] and [`PAIR_TOLERANCE_MAX`].
    fn tolerance(&self) -> Duration {
        self.interval().map_or(PAIR_TOLERANCE_DEFAULT, |i| i.mul_f32(TOLERANCE_INTERVALS).clamp(PAIR_TOLERANCE_MIN, PAIR_TOLERANCE_MAX))
    }

    /// A view whose newest picture is older than this when a look begins is not waited for (it may be gone).
    fn live_window(&self) -> Duration {
        self.interval().map_or(LIVE_MIN, |i| (i * LIVE_INTERVALS).max(LIVE_MIN))
    }
}

/// The signed time from `from` to `to`, in microseconds.
fn signed_us(from: Instant, to: Instant) -> i64 {
    if to >= from { to.duration_since(from).as_micros() as i64 } else { -(from.duration_since(to).as_micros() as i64) }
}

fn apart(a: Instant, b: Instant) -> Duration {
    if a >= b { a - b } else { b - a }
}

/// What a view has to pair with a look: the picture chosen for it, and when the newest picture near the look was captured
/// (for the question whether the view is still giving pictures).
struct Companion {
    chosen: Option<Item>,
    newest: Option<Instant>,
}

/// Decides, picture by picture, when the tracker looks and at what, and owns the tracker.
///
/// A look is begun by a picture of a camera (the height picture of the depth camera is only ever given along). The
/// pictures that go with it are chosen by [`Pairing`]. With the nearest one the look may have to wait for the next picture
/// of another view; the picture waits in a queue of its own meanwhile, and the wait is counted on capture times (the
/// capture time of the newest picture that has come: the clock of the stream), never on the clock of the machine, so that
/// a replay of the same pictures makes the same looks. What the machine's clock is used for is to end a wait when no
/// picture comes any more (see [`flush_look`](Self::flush_look)).
pub struct Analyzer {
    pub tracker: Tracker,
    pairing: Pairing,
    /// The newest picture delivered for each view (what [`Pairing::Newest`] pairs with).
    latest: [Option<Item>; VIEWS],
    /// The pictures delivered lately, in the order of their capture, for each view ([`Pairing::Nearest`] looks in them).
    history: [VecDeque<Item>; VIEWS],
    pace: [Pace; VIEWS],
    /// The capture time of the newest picture delivered, and the number of the newest of each view.
    stream: Option<Instant>,
    delivered: [u64; VIEWS],
    /// The pictures of the cameras that may begin a look, in the order of their capture, not decided yet.
    pending: VecDeque<Item>,
    /// The number of the last picture of each view that the tracker was given: it is never given the same one twice.
    last_fed: [u64; VIEWS],
    last_look: Option<Instant>,
    last_active: Option<[bool; VIEWS]>,
    /// When looks may be skipped, and what is known of the pictures that were given.
    thin: Thin,
    tagger: Tagger,
    /// What a recording says about the next look (it was skipped or not): taken instead of the rule, once.
    recorded_thin: Option<bool>,
}

impl Analyzer {
    pub fn new(tracker: Tracker) -> Analyzer {
        Analyzer::with_pairing(tracker, Pairing::default())
    }

    pub fn with_pairing(tracker: Tracker, pairing: Pairing) -> Analyzer {
        Analyzer {
            tracker,
            pairing,
            latest: [None, None, None],
            history: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            pace: [Pace::default(), Pace::default(), Pace::default()],
            stream: None,
            delivered: [0; VIEWS],
            pending: VecDeque::new(),
            last_fed: [0; VIEWS],
            last_look: None,
            last_active: None,
            thin: Thin::default(),
            tagger: Tagger::new(),
            recorded_thin: None,
        }
    }

    /// Play begins (or begins again): what the views were told is told again at the first look.
    pub fn begin(&mut self) {
        self.last_active = None;
        self.pending.clear();
        // Nothing of what was compared with before is the board of the game that begins.
        self.tagger.forget();
    }

    /// When looks may be skipped (see [`Thin`]).
    pub fn set_thin(&mut self, thin: Thin) {
        self.thin = thin;
    }

    pub fn thin(&self) -> Thin {
        self.thin
    }

    /// How long the board must have been quiet for a look to be skipped (a test's games are quick).
    pub fn set_thin_quiet(&mut self, quiet_for: Duration) {
        self.tagger.set_quiet_for(quiet_for);
    }

    /// A replay says what a recording says of the next look: it was skipped or not. The rule is not asked for it.
    pub fn expect_thin(&mut self, recorded: Option<bool>) {
        self.recorded_thin = recorded;
    }

    /// Looks at the pictures that the tracker is given together at `at`, or skips the look (see [`Thin`]): `pictures` are
    /// `(view, captured, picture)`. `behind` is whether the analysis is behind the cameras. Gives what the tracker found,
    /// whether the look was skipped, and what a glance says of each picture. Used by a look of the analysis, and by a replay
    /// that gives the pictures as the recording names them.
    pub fn feed_given(
        &mut self,
        pictures: &[(usize, Instant, &RgbImage)],
        at: Instant,
        behind: bool,
    ) -> (Detection, bool, Vec<(usize, Tag)>) {
        // The tags of all pictures are made, whatever is decided: they are what the next ones are compared with.
        let glances: Vec<Option<crate::vision::Glance>> =
            pictures.iter().map(|&(view, _, image)| self.tracker.glance(view, image)).collect();
        let mut tags = Vec::new();
        for (&(view, captured, _), glance) in pictures.iter().zip(&glances) {
            if let Some(glance) = glance {
                tags.push((view, self.tagger.tag(view, captured, glance)));
            }
        }
        let judged = |tagger: &Tagger| {
            let known: Option<Vec<(usize, Instant, &crate::vision::Glance)>> =
                pictures.iter().zip(&glances).map(|(&(view, captured, _), g)| g.as_ref().map(|g| (view, captured, g))).collect();
            known.is_some_and(|known| tagger.may_skip(&known))
        };
        let wanted = match self.recorded_thin.take() {
            Some(flag) => flag,
            None => match self.thin {
                Thin::Never => false,
                Thin::Behind => behind && judged(&self.tagger),
                Thin::Always => judged(&self.tagger),
            },
        };
        let views: Vec<usize> = pictures.iter().map(|&(view, _, _)| view).collect();
        if wanted && self.tracker.repeat_still(&views, at) {
            for &view in &views {
                self.tagger.skipped(view);
            }
            return (Detection::Nothing, true, tags);
        }
        let mut images: Vec<Option<&RgbImage>> = vec![None; VIEWS];
        for &(view, _, image) in pictures {
            if let Some(slot) = images.get_mut(view) {
                *slot = Some(image);
            }
        }
        let detection = self.tracker.feed_views(&images, at);
        for (&(view, _, _), glance) in pictures.iter().zip(&glances) {
            if let Some(glance) = glance {
                self.tagger.analysed(view, glance);
            }
        }
        (detection, false, tags)
    }

    /// The stretches of occluded pictures that ended since the last call (a replay has no use for them).
    pub fn take_occlusions(&mut self) -> Vec<Occlusion> {
        self.tagger.take_occlusions()
    }

    /// Pictures of cameras that have not been decided on yet: a look waits for the pictures that may go with it.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Takes a picture. Pictures must come in the order of their capture, as the inbox gives them. Then ask for the looks
    /// that are due with [`next_look`](Self::next_look), until it says there are none.
    pub fn deliver(&mut self, item: Item) {
        let view = item.view;
        if view >= VIEWS {
            return;
        }
        let at = item.captured;
        self.stream = Some(self.stream.map_or(at, |s| s.max(at)));
        self.delivered[view] = self.delivered[view].max(item.seq);
        self.pace[view].note(at);
        self.latest[view] = Some(item.clone());
        if self.pairing == Pairing::Nearest {
            // In the order of capture, even when a picture comes late from its camera.
            let history = &mut self.history[view];
            let mut place = history.len();
            while place > 0 && history[place - 1].captured > at {
                place -= 1;
            }
            history.insert(place, item.clone());
            let newest = self.stream.unwrap_or(at);
            while history.len() > HISTORY_MAX
                || (history.len() > 2 && history.front().is_some_and(|p| newest.saturating_duration_since(p.captured) > HISTORY_AGE))
            {
                history.pop_front();
            }
        }
        if view != DEPTH_VIEW {
            self.pending.push_back(item);
        }
    }

    /// The next look that is due, if there is one: it was made (the tracker looked) and its result is in it. `context` is
    /// asked, once, when something has to be decided. Call it again until it gives `None`, publishing each look before
    /// asking for the next (the tracker may be put back on the game's position by what a look led to).
    pub fn next_look(&mut self, context: &mut dyn FnMut() -> Context, interval: Duration) -> Option<Look> {
        self.decide(context, interval, false)
    }

    /// The same, deciding at once what is waiting for a picture that has not come: used when the commands of a person are to
    /// be carried out, and when no picture has come for a while (the cameras stopped, or the game is over).
    pub fn flush_look(&mut self, context: &mut dyn FnMut() -> Context, interval: Duration) -> Option<Look> {
        self.decide(context, interval, true)
    }

    fn too_soon(&self, at: Instant, interval: Duration) -> bool {
        self.last_look.is_some_and(|last| at.saturating_duration_since(last) < interval)
    }

    fn decide(&mut self, context: &mut dyn FnMut() -> Context, interval: Duration, force: bool) -> Option<Look> {
        let mut asked: Option<Context> = None;
        while let Some(trigger) = self.pending.front() {
            let at = trigger.captured;
            // Not the time yet: no need to ask what the engine is doing. (The test is made again with the engine's own
            // interval, which a page may change between the two.)
            if self.too_soon(at, interval) {
                self.pending.pop_front();
                continue;
            }
            let state = *asked.get_or_insert_with(&mut *context);
            if !state.playing || self.too_soon(at, state.interval) {
                self.pending.pop_front();
                continue;
            }
            if !force && self.pairing == Pairing::Nearest && self.must_wait(trigger, &state) {
                return None;
            }
            let trigger = self.pending.pop_front().expect("a picture was seen");
            if let Some(look) = self.look_at(trigger, &state, force) {
                return Some(look);
            }
            // Every picture of it had been given before: the next one.
        }
        None
    }

    /// The next picture of another view may be nearer to this picture than the one that view gave before it: wait for it,
    /// but only as long as it could be (and never more than [`PAIR_WAIT_MAX`]), and not for a view that has gone quiet.
    fn must_wait(&self, trigger: &Item, state: &Context) -> bool {
        let Some(stream) = self.stream else { return false };
        let at = trigger.captured;
        for view in (0..VIEWS).filter(|&v| v != trigger.view) {
            let wanted = if view == DEPTH_VIEW { state.depth } else { state.usable[view] };
            if !wanted {
                continue;
            }
            let Some(last) = self.history[view].back() else { continue };
            // A picture after this one is already there: it is known which is the nearest.
            if last.captured > at {
                continue;
            }
            let pace = &self.pace[view];
            let behind = at.saturating_duration_since(last.captured);
            if behind > pace.live_window() {
                continue;
            }
            // The next picture is nearer only if it is captured before `behind` after this one, and it is of use only
            // within the tolerance.
            let bound = behind.min(pace.tolerance()).min(PAIR_WAIT_MAX);
            if stream < at + bound {
                return true;
            }
        }
        false
    }

    /// What the view `v` has to go with a look begun at `at`.
    fn companion(&self, v: usize, at: Instant) -> Companion {
        if self.pairing == Pairing::Newest {
            let newest = self.latest[v].clone();
            return Companion { newest: newest.as_ref().map(|p| p.captured), chosen: newest };
        }
        let history = &self.history[v];
        let mut place = history.len();
        while place > 0 && history[place - 1].captured > at {
            place -= 1;
        }
        let before = place.checked_sub(1).map(|i| &history[i]);
        let after = history.get(place);
        let nearest = match (before, after) {
            (Some(b), Some(a)) => Some(if apart(a.captured, at) < apart(b.captured, at) { a } else { b }),
            (b, a) => b.or(a),
        };
        let tolerance = self.pace[v].tolerance();
        Companion { newest: before.or(after).map(|p| p.captured), chosen: nearest.filter(|p| apart(p.captured, at) <= tolerance).cloned() }
    }

    fn look_at(&mut self, trigger: Item, state: &Context, flushed: bool) -> Option<Look> {
        let at = trigger.captured;
        let trigger_view = trigger.view;
        let mut active = [false; VIEWS];
        let mut unpaired = [false; VIEWS];
        let mut fed: Vec<(Item, i64)> = Vec::new();
        // The arrays of the cameras are indexed together.
        #[allow(clippy::needless_range_loop)]
        for c in 0..CAMERAS {
            let companion =
                if c == trigger_view { Companion { chosen: Some(trigger.clone()), newest: Some(at) } } else { self.companion(c, at) };
            let fresh = companion.newest.is_some_and(|n| apart(n, at) < state.fresh);
            // A camera that has gone quiet is left out, so that the others are not held up; one that has not given a
            // picture since play began is waited for a little, so that all learn the squares together.
            let waiting =
                state.begun_at.is_some_and(|b| at.saturating_duration_since(b) < CAMERA_WAIT && companion.newest.is_none_or(|n| n < b));
            active[c] = state.usable[c] && (fresh || waiting);
            self.tracker.set_view_active(c, active[c]);
            if !state.usable[c] || !fresh {
                continue;
            }
            match companion.chosen {
                Some(picture) => {
                    let skew = if c == trigger_view { 0 } else { signed_us(at, picture.captured) };
                    fed.push((picture, skew));
                }
                None => unpaired[c] = true,
            }
        }
        // The depth camera takes part when it is wanted, has found the squares, and gives pictures.
        let depth = self.companion(DEPTH_VIEW, at);
        active[DEPTH_VIEW] = state.depth && depth.newest.is_some_and(|n| apart(n, at) < state.fresh);
        self.tracker.set_view_active(DEPTH_VIEW, active[DEPTH_VIEW]);
        if active[DEPTH_VIEW] {
            match depth.chosen {
                Some(picture) => fed.push((picture.clone(), signed_us(at, picture.captured))),
                None => unpaired[DEPTH_VIEW] = true,
            }
        }
        // A view is not given the same picture twice: a repeat would count as one more still picture.
        fed.retain(|(picture, _)| picture.seq != self.last_fed[picture.view]);
        // The first look has no look before it to be told different from.
        let views_changed = self.last_active != Some(active) && !fed.is_empty();
        if views_changed {
            self.last_active = Some(active);
        }
        if fed.is_empty() {
            return None;
        }
        self.last_look = Some(at);
        for (picture, _) in &fed {
            self.last_fed[picture.view] = picture.seq;
        }
        let pictures: Vec<(usize, Instant, &RgbImage)> = fed.iter().map(|(p, _)| (p.view, p.captured, p.image.as_ref())).collect();
        let started = Instant::now();
        let (detection, thinned, tags) = self.feed_given(&pictures, at, state.behind);
        let step = started.elapsed();
        let occlusions = self.tagger.take_occlusions();
        let (fed, skews_us): (Vec<Item>, Vec<i64>) = fed.into_iter().unzip();
        Some(Look {
            at,
            started,
            step,
            fed,
            skews_us,
            trigger: trigger_view,
            released: self.delivered,
            flushed,
            unpaired,
            active,
            views_changed,
            thinned,
            tags,
            occlusions,
            detection,
        })
    }
}

/// How the pictures wait: for the console, the page and the recording.
#[derive(Clone, Copy, Debug, Default)]
pub struct Backlog {
    /// Pictures waiting now, the memory they take, and how old the oldest of them is (since its capture).
    pub waiting: usize,
    pub bytes: usize,
    pub oldest: Duration,
    /// The most pictures waiting in the queue of one view.
    pub deepest: usize,
}

impl Backlog {
    /// The analysis is behind the cameras: many pictures of a view wait, or the oldest has waited long (see [`Thin`]).
    pub fn is_behind(&self) -> bool {
        self.deepest >= THIN_WAITING || self.oldest >= THIN_AGE
    }
}

/// What a command does in the analysis thread.
pub type Work = Box<dyn FnOnce(&Engine, &mut Analyzer) + Send>;

/// The state of a command, which a person may stop waiting for.
const PENDING: u8 = 0;
const RUNNING: u8 = 1;
const CANCELLED: u8 = 2;

/// Something to be done in the analysis thread, after the pictures that were in the inbox when it was sent.
pub struct Job {
    after: [u64; VIEWS],
    run: Work,
    state: Arc<AtomicU8>,
}

impl Job {
    /// How many pictures each view had pushed when the command was sent: the ones that came before it.
    pub fn after(&self) -> [u64; VIEWS] {
        self.after
    }

    /// The thread is about to run it: false when the person who sent it gave up waiting (it must not run then).
    pub fn begin(&self) -> bool {
        self.state.compare_exchange(PENDING, RUNNING, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }

    pub fn run(self, engine: &Engine, analyzer: &mut Analyzer) {
        (self.run)(engine, analyzer);
    }
}

/// The handle of the sender of a job: it can stop waiting, if the job has not begun.
pub struct JobHandle {
    state: Arc<AtomicU8>,
}

impl JobHandle {
    /// True when the job will not run (it had not begun). False: it is running, wait for its answer.
    pub fn cancel(&self) -> bool {
        self.state.compare_exchange(PENDING, CANCELLED, Ordering::SeqCst, Ordering::SeqCst).is_ok()
    }
}

/// What the analysis thread takes next.
pub enum Next {
    Picture(Item, Backlog),
    Job(Job),
    /// Nothing came within the time asked for.
    Idle,
    Stop,
}

struct State {
    queues: [VecDeque<Item>; VIEWS],
    pushed: [u64; VIEWS],
    popped: [u64; VIEWS],
    /// The capture time of the newest picture each view pushed, and when (by the clock) it did.
    watermark: [Option<Instant>; VIEWS],
    last_push: [Option<Instant>; VIEWS],
    jobs: VecDeque<Job>,
    stop: bool,
    /// Pictures that did not fit in the budget (see [`Inbox::push`]), not yet told.
    lost: [u64; VIEWS],
    waiting_bytes: usize,
}

/// The queues of the sources and the commands, taken in order by the analysis thread.
pub struct Inbox {
    state: Mutex<State>,
    wake: Condvar,
    budget: Arc<Budget>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Inbox {
    pub fn new(budget_bytes: usize) -> Arc<Inbox> {
        Arc::new(Inbox {
            state: Mutex::new(State {
                queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
                pushed: [0; VIEWS],
                popped: [0; VIEWS],
                watermark: [None; VIEWS],
                last_push: [None; VIEWS],
                jobs: VecDeque::new(),
                stop: false,
                lost: [0; VIEWS],
                waiting_bytes: 0,
            }),
            wake: Condvar::new(),
            budget: Budget::new(budget_bytes),
        })
    }

    /// Queues a picture. It never waits. False only when the memory budget is full (the picture is then left out and
    /// counted, see [`take_lost`](Self::take_lost)): the caller says so loudly.
    pub fn push(&self, item: Item) -> bool {
        let view = item.view;
        if view >= VIEWS {
            return false;
        }
        let cost = item.cost();
        if !self.budget.reserve(cost) {
            lock(&self.state).lost[view] += 1;
            return false;
        }
        let mut state = lock(&self.state);
        state.pushed[view] += 1;
        state.watermark[view] = Some(state.watermark[view].map_or(item.captured, |w| w.max(item.captured)));
        state.last_push[view] = Some(Instant::now());
        state.waiting_bytes += cost;
        state.queues[view].push_back(item);
        drop(state);
        self.wake.notify_all();
        true
    }

    /// Queues a command: it runs after every picture that was pushed before this call. `None` when the inbox is stopped.
    pub fn submit(&self, run: Work) -> Option<JobHandle> {
        let mut state = lock(&self.state);
        if state.stop {
            return None;
        }
        let handle = JobHandle { state: Arc::new(AtomicU8::new(PENDING)) };
        let after = state.pushed;
        state.jobs.push_back(Job { after, run, state: Arc::clone(&handle.state) });
        drop(state);
        self.wake.notify_all();
        Some(handle)
    }

    /// Stops the analysis thread (the engine is gone).
    pub fn stop(&self) {
        lock(&self.state).stop = true;
        self.wake.notify_all();
    }

    /// How many pictures of each view did not fit in the budget since the last call.
    pub fn take_lost(&self) -> [u64; VIEWS] {
        std::mem::take(&mut lock(&self.state).lost)
    }

    /// The pictures that wait now.
    pub fn backlog(&self) -> Backlog {
        let state = lock(&self.state);
        Self::backlog_of(&state, Instant::now())
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    fn backlog_of(state: &State, now: Instant) -> Backlog {
        let waiting = state.queues.iter().map(VecDeque::len).sum();
        let oldest = state.queues.iter().filter_map(|q| q.front()).map(|p| p.captured).min();
        Backlog {
            waiting,
            bytes: state.waiting_bytes,
            oldest: oldest.map_or(Duration::ZERO, |c| now.saturating_duration_since(c)),
            deepest: state.queues.iter().map(VecDeque::len).max().unwrap_or(0),
        }
    }

    /// The view whose first waiting picture was captured first, among those that `allowed` says.
    fn oldest_head(state: &State, allowed: impl Fn(usize) -> bool) -> Option<usize> {
        (0..VIEWS)
            .filter(|&v| allowed(v))
            .filter_map(|v| state.queues[v].front().map(|p| (v, p.captured)))
            .min_by_key(|&(_, c)| c)
            .map(|(v, _)| v)
    }

    fn pop(&self, state: &mut State, view: usize, now: Instant) -> Next {
        let backlog = Self::backlog_of(state, now);
        let item = state.queues[view].pop_front().expect("a head was seen");
        state.popped[view] += 1;
        let cost = item.cost();
        state.waiting_bytes = state.waiting_bytes.saturating_sub(cost);
        self.budget.release(cost);
        Next::Picture(item, backlog)
    }

    /// The next thing for the analysis thread, waiting for it. Pictures come in the order of their capture, across
    /// the views: a picture is taken when no other view can still send an older one, which is when each of them has
    /// a picture waiting that is not older, or has sent one that is newer, or has been quiet for [`LAG_WAIT`]. A command
    /// comes after the pictures that were pushed before it was sent, which are taken at once, in capture order.
    ///
    /// With `idle`, when nothing comes within that time the answer is [`Next::Idle`] (the thread has something waiting for
    /// a picture that may not come: see [`Analyzer::flush_look`]).
    pub fn next(&self, idle: Option<Duration>) -> Next {
        let mut state = lock(&self.state);
        loop {
            if state.stop {
                return Next::Stop;
            }
            let now = Instant::now();
            if let Some(job) = state.jobs.front() {
                let after = job.after;
                let owed = |v: usize| state.popped[v] < after[v];
                if !(0..VIEWS).any(owed) {
                    let job = state.jobs.pop_front().expect("a job was seen");
                    return Next::Job(job);
                }
                if let Some(view) = Self::oldest_head(&state, owed) {
                    return self.pop(&mut state, view, now);
                }
                // Pictures owed but none waiting: they were left out of the queue after the job was counted; do not
                // wait for them for ever.
                let job = state.jobs.pop_front().expect("a job was seen");
                return Next::Job(job);
            }
            let Some(view) = Self::oldest_head(&state, |_| true) else {
                match idle {
                    None => state = self.wake.wait(state).unwrap_or_else(PoisonError::into_inner),
                    Some(idle) => {
                        let (next, timeout) = self.wake.wait_timeout(state, idle).unwrap_or_else(PoisonError::into_inner);
                        state = next;
                        if timeout.timed_out() && !state.stop && state.jobs.is_empty() && state.queues.iter().all(VecDeque::is_empty) {
                            return Next::Idle;
                        }
                    }
                }
                continue;
            };
            let captured = state.queues[view].front().map(|p| p.captured).unwrap_or(now);
            // Another view that is empty and may still send an older picture: wait for it, but not for long.
            let mut until: Option<Instant> = None;
            for other in (0..VIEWS).filter(|&o| o != view && state.queues[o].is_empty()) {
                let newer_sent = state.watermark[other].is_some_and(|w| w >= captured);
                let deadline = state.last_push[other].map(|at| at + LAG_WAIT);
                if let Some(deadline) = deadline
                    && !newer_sent
                    && deadline > now
                {
                    until = Some(until.map_or(deadline, |u| u.min(deadline)));
                }
            }
            match until {
                None => return self.pop(&mut state, view, now),
                Some(deadline) => {
                    state =
                        self.wake.wait_timeout(state, deadline.saturating_duration_since(now)).unwrap_or_else(PoisonError::into_inner).0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::TrackerConfig;

    fn picture(view: usize, seq: u64, base: Instant, ms: u64) -> Item {
        let image = Arc::new(RgbImage::new(4, 4));
        Item { view, seq, captured: base + Duration::from_millis(ms), waited: None, image: Arc::clone(&image), raw: image }
    }

    fn playing(interval_ms: u64) -> Context {
        Context {
            playing: true,
            usable: [true, true],
            depth: false,
            interval: Duration::from_millis(interval_ms),
            fresh: Duration::from_secs(2),
            begun_at: None,
            behind: false,
        }
    }

    fn analyzer() -> Analyzer {
        Analyzer::new(Tracker::new(TrackerConfig::default()))
    }

    fn take_picture(next: Next) -> (usize, u64) {
        match next {
            Next::Picture(item, _) => (item.view, item.seq),
            Next::Job(_) => panic!("a job"),
            Next::Idle => panic!("idle"),
            Next::Stop => panic!("stopped"),
        }
    }

    /// Delivers a picture and collects the looks that are due, as the analysis thread does.
    fn feed(analyzer: &mut Analyzer, item: Item, context: Context) -> Vec<Look> {
        analyzer.deliver(item);
        let mut looks = Vec::new();
        while let Some(look) = analyzer.next_look(&mut || context, context.interval) {
            looks.push(look);
        }
        looks
    }

    fn flush(analyzer: &mut Analyzer, context: Context) -> Vec<Look> {
        let mut looks = Vec::new();
        while let Some(look) = analyzer.flush_look(&mut || context, context.interval) {
            looks.push(look);
        }
        looks
    }

    /// The views and numbers of the pictures a look was given, and how far from the look each was, in ms.
    fn given(look: &Look) -> Vec<(usize, u64, i64)> {
        look.fed.iter().zip(&look.skews_us).map(|(p, skew)| (p.view, p.seq, skew / 1000)).collect()
    }

    #[test]
    fn pictures_come_in_the_order_of_their_capture_across_the_views() {
        let inbox = Inbox::new(1 << 30);
        let base = Instant::now();
        // The second camera's pictures are pushed after the first's, but were captured between them.
        assert!(inbox.push(picture(0, 1, base, 0)));
        assert!(inbox.push(picture(0, 2, base, 40)));
        assert!(inbox.push(picture(1, 1, base, 10)));
        assert!(inbox.push(picture(1, 2, base, 50)));
        let order: Vec<(usize, u64)> = (0..4).map(|_| take_picture(inbox.next(None))).collect();
        assert_eq!(order, [(0, 1), (1, 1), (0, 2), (1, 2)]);
        assert_eq!(inbox.backlog().waiting, 0);
    }

    #[test]
    fn a_view_that_has_gone_quiet_does_not_hold_the_others_up_for_long() {
        let inbox = Inbox::new(1 << 30);
        let base = Instant::now();
        assert!(inbox.push(picture(0, 1, base, 0)));
        // Nothing was ever pushed by the other views: no wait at all.
        let started = Instant::now();
        assert_eq!(take_picture(inbox.next(None)), (0, 1));
        assert!(started.elapsed() < Duration::from_millis(100));
        // A view that sent a picture just now may send an older one: it is waited for, up to LAG_WAIT.
        assert!(inbox.push(picture(1, 1, base, 0)));
        assert!(inbox.push(picture(0, 2, base, 30)));
        assert_eq!(take_picture(inbox.next(None)), (1, 1));
        let started = Instant::now();
        assert_eq!(take_picture(inbox.next(None)), (0, 2), "the other view sent nothing newer, so the wait ends");
        assert!(started.elapsed() < LAG_WAIT + Duration::from_millis(400), "{:?}", started.elapsed());
    }

    #[test]
    fn a_command_comes_after_the_pictures_that_were_pushed_before_it() {
        let inbox = Inbox::new(1 << 30);
        let base = Instant::now();
        for seq in 1..=3 {
            assert!(inbox.push(picture(0, seq, base, seq * 40)));
        }
        let ran = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let log = Arc::clone(&ran);
        let handle = inbox.submit(Box::new(move |_, _| log.lock().unwrap().push("job"))).expect("not stopped");
        assert!(inbox.push(picture(0, 4, base, 200)));
        assert_eq!(take_picture(inbox.next(None)), (0, 1));
        assert_eq!(take_picture(inbox.next(None)), (0, 2));
        assert_eq!(take_picture(inbox.next(None)), (0, 3));
        let Next::Job(job) = inbox.next(None) else { panic!("the job comes before the picture pushed after it") };
        assert!(job.begin());
        assert!(!handle.cancel(), "a job that is running cannot be given up");
        assert_eq!(take_picture(inbox.next(None)), (0, 4));
        assert!(ran.lock().unwrap().is_empty());
    }

    #[test]
    fn a_command_that_was_given_up_does_not_run() {
        let inbox = Inbox::new(1 << 30);
        let handle = inbox.submit(Box::new(|_, _| panic!("must not run"))).expect("not stopped");
        assert!(handle.cancel());
        let Next::Job(job) = inbox.next(None) else { panic!("a job") };
        assert!(!job.begin());
    }

    #[test]
    fn pictures_past_the_budget_are_left_out_and_counted_never_silently() {
        let one = picture(0, 1, Instant::now(), 0).cost();
        let inbox = Inbox::new(one * 2);
        let base = Instant::now();
        assert!(inbox.push(picture(0, 1, base, 0)));
        assert!(inbox.push(picture(0, 2, base, 40)));
        assert!(!inbox.push(picture(0, 3, base, 80)));
        assert_eq!(inbox.take_lost(), [1, 0, 0]);
        assert_eq!(inbox.take_lost(), [0, 0, 0]);
        // Taking pictures gives the room back.
        assert_eq!(take_picture(inbox.next(None)), (0, 1));
        assert!(inbox.push(picture(0, 4, base, 120)));
        assert_eq!(inbox.backlog().waiting, 2);
    }

    #[test]
    fn when_nothing_comes_for_the_time_asked_the_answer_is_idle_and_a_picture_that_comes_is_given_first() {
        let inbox = Inbox::new(1 << 30);
        let base = Instant::now();
        let started = Instant::now();
        assert!(matches!(inbox.next(Some(Duration::from_millis(60))), Next::Idle));
        assert!(started.elapsed() >= Duration::from_millis(50) && started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        // A picture that is there is given at once, not after the time.
        assert!(inbox.push(picture(0, 1, base, 0)));
        let started = Instant::now();
        assert_eq!(take_picture(inbox.next(Some(Duration::from_secs(10)))), (0, 1));
        assert!(started.elapsed() < Duration::from_secs(5));
        // A stopped inbox says so, whatever the time asked for.
        inbox.stop();
        assert!(matches!(inbox.next(Some(Duration::from_secs(10))), Next::Stop));
    }

    #[test]
    fn a_stopped_inbox_ends_the_thread_and_takes_no_command() {
        let inbox = Inbox::new(1 << 30);
        let waiter = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || matches!(inbox.next(None), Next::Stop))
        };
        std::thread::sleep(Duration::from_millis(50));
        inbox.stop();
        assert!(waiter.join().unwrap());
        assert!(inbox.submit(Box::new(|_, _| {})).is_none());
    }

    #[test]
    fn the_tracker_looks_at_the_pace_of_the_capture_times_and_never_at_a_picture_twice() {
        let mut analyzer = analyzer();
        let base = Instant::now();
        let mut looks = Vec::new();
        // Pictures every 40 ms, a look at most every 100 ms: at 0, 120 and 240 ms.
        for i in 0..8u64 {
            for look in feed(&mut analyzer, picture(0, i + 1, base, i * 40), playing(100)) {
                looks.push(look.at.duration_since(base).as_millis());
                assert_eq!(look.fed.len(), 1);
            }
        }
        assert_eq!(looks, [0, 120, 240]);
    }

    #[test]
    fn nothing_is_looked_at_when_no_game_is_played() {
        let mut analyzer = analyzer();
        let base = Instant::now();
        let idle = Context { playing: false, ..playing(0) };
        for i in 0..5u64 {
            assert!(feed(&mut analyzer, picture(0, i + 1, base, i * 40), idle).is_empty());
        }
        assert!(!analyzer.has_pending(), "a picture that cannot begin a look is not kept waiting");
        // The pictures were taken all the same: the first look once a game is played has the newest of them.
        let looks = feed(&mut analyzer, picture(0, 6, base, 240), playing(0));
        assert_eq!(looks.len(), 1);
        assert_eq!(looks[0].fed[0].seq, 6);
    }

    /// Delivers pictures `(view, capture time in ms)` in the order of their capture, as the inbox gives them (the pictures of
    /// each view are numbered from 1), and returns the looks with the capture time of the picture whose arrival made each.
    fn run(analyzer: &mut Analyzer, base: Instant, schedule: &[(usize, u64)], context: Context) -> Vec<(Look, u64)> {
        let mut numbers = [0u64; VIEWS];
        let mut out = Vec::new();
        for &(view, ms) in schedule {
            numbers[view] += 1;
            for look in feed(analyzer, picture(view, numbers[view], base, ms), context) {
                out.push((look, ms));
            }
        }
        out
    }

    fn ms_of(look: &Look, base: Instant) -> u64 {
        look.at.duration_since(base).as_millis() as u64
    }

    #[test]
    fn a_look_goes_with_the_picture_of_the_other_camera_that_is_nearest_in_time_not_the_newest_before_it() {
        let base = Instant::now();
        // Both cameras every 40 ms, the second one 15 ms behind the first; only the first begins looks (a look at most every
        // 30 ms: the second camera's pictures come 15 ms after a look).
        let schedule: Vec<(usize, u64)> = (0..12u64).flat_map(|k| [(0, k * 40), (1, k * 40 + 15)]).collect();
        let mut nearest = analyzer();
        let looks = run(&mut nearest, base, &schedule, playing(30));
        // The first camera's sixth picture, at 200 ms, goes with the second camera's sixth, at 215 (15 ms after), not with
        // the fifth at 175 (25 ms before).
        let look = looks.iter().map(|(l, _)| l).find(|l| l.fed[0].view == 0 && l.fed[0].seq == 6).expect("a look at the sixth picture");
        assert_eq!(given(look), [(0, 6, 0), (1, 6, 15)]);
        assert!(look.unpaired.iter().all(|u| !u));
        // Each picture of the second camera goes with one look at most.
        let mut seen = std::collections::HashSet::new();
        for (look, _) in &looks {
            for p in look.fed.iter().filter(|p| p.view == 1) {
                assert!(seen.insert(p.seq), "picture {} of the second camera was given twice", p.seq);
            }
        }
        // The older rule takes the newest picture that had come: the one at 175, 25 ms before.
        let mut newest = Analyzer::with_pairing(Tracker::new(TrackerConfig::default()), Pairing::Newest);
        let looks = run(&mut newest, base, &schedule, playing(30));
        let look = looks.iter().map(|(l, _)| l).find(|l| l.fed[0].view == 0 && l.fed[0].seq == 6).expect("a look");
        assert_eq!(given(look), [(0, 6, 0), (1, 5, -25)]);
    }

    #[test]
    fn the_nearest_picture_is_chosen_the_earlier_one_on_a_tie_and_none_beyond_the_tolerance() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // A view with a picture every 40 ms: a tolerance of 60 ms.
        for k in 0..6u64 {
            analyzer.deliver(picture(1, k + 1, base, k * 40));
        }
        let at = |ms: u64| base + Duration::from_millis(ms);
        let chosen = |analyzer: &Analyzer, ms: u64| analyzer.companion(1, at(ms)).chosen.map(|p| p.seq);
        assert_eq!(chosen(&analyzer, 100), Some(3), "100 is 20 ms from 80 and from 120: the earlier");
        assert_eq!(chosen(&analyzer, 105), Some(4), "15 ms from 120, 25 from 80");
        assert_eq!(chosen(&analyzer, 95), Some(3));
        assert_eq!(chosen(&analyzer, 200), Some(6), "the last one, at 200");
        assert_eq!(chosen(&analyzer, 255), Some(6), "55 ms after the last one: within 60");
        assert_eq!(chosen(&analyzer, 262), None, "62 ms: beyond the tolerance");
        assert!(analyzer.companion(1, at(262)).newest.is_some(), "though the view is still known to give pictures");
        // Before the pace is known the tolerance is the default one.
        let mut fresh = analyzer_fresh();
        fresh.deliver(picture(1, 1, base, 0));
        assert_eq!(fresh.companion(1, at(90)).chosen.map(|p| p.seq), Some(1));
        assert!(fresh.companion(1, at(110)).chosen.is_none());
        // A view that gives a picture every 300 ms has the greatest tolerance, not 450.
        let mut slow = analyzer_fresh();
        for k in 0..5u64 {
            slow.deliver(picture(1, k + 1, base, k * 300));
        }
        assert_eq!(slow.pace[1].tolerance(), PAIR_TOLERANCE_MAX);
        assert_eq!(analyzer_fresh().pace[1].tolerance(), PAIR_TOLERANCE_DEFAULT);
    }

    fn analyzer_fresh() -> Analyzer {
        analyzer()
    }

    #[test]
    fn a_picture_that_is_late_is_waited_for_when_it_may_be_the_nearest() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // (Delivered while no game is played, so that they do not begin looks of their own.)
        let quiet = Context { playing: false, ..playing(30) };
        for (n, ms) in [0u64, 40, 80].iter().enumerate() {
            feed(&mut analyzer, picture(1, n as u64 + 1, base, *ms), quiet);
        }
        // The first camera's picture at 140 is 60 ms after the second camera's last: the next picture of that camera,
        // which should have come at 120, may still come and be nearer: wait.
        assert!(feed(&mut analyzer, picture(0, 1, base, 140), playing(30)).is_empty());
        assert!(analyzer.has_pending());
        // It comes, late, at 150: it is the nearest.
        let looks = feed(&mut analyzer, picture(1, 4, base, 150), playing(30));
        assert_eq!(looks.len(), 1);
        assert_eq!(given(&looks[0]), [(0, 1, 0), (1, 4, 10)]);
        assert!(!analyzer.has_pending());
    }

    #[test]
    fn the_wait_ends_without_the_picture_when_it_cannot_be_nearer_than_the_one_before() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // (Delivered while no game is played, so that they do not begin looks of their own.)
        let quiet = Context { playing: false, ..playing(30) };
        for (n, ms) in [0u64, 40, 80].iter().enumerate() {
            feed(&mut analyzer, picture(1, n as u64 + 1, base, *ms), quiet);
        }
        // A picture of the first camera at 120, 40 ms after the second camera's last: a picture of that camera nearer
        // than 40 ms must be captured before 160.
        assert!(feed(&mut analyzer, picture(0, 1, base, 120), playing(30)).is_empty());
        assert!(feed(&mut analyzer, picture(0, 2, base, 150), playing(30)).is_empty(), "the stream is at 150: it may still come");
        let looks = feed(&mut analyzer, picture(0, 3, base, 165), playing(30));
        assert_eq!(looks.len(), 1, "the stream is past 160: nothing can be nearer than the picture at 80");
        assert_eq!(given(&looks[0]), [(0, 1, 0), (1, 3, -40)]);
    }

    #[test]
    fn a_picture_beyond_the_tolerance_is_not_given_and_a_quiet_view_is_not_waited_for() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // The second camera every 40 ms to 120 ms (tolerance 60 ms), then nothing.
        for (n, ms) in [0u64, 40, 80, 120].iter().enumerate() {
            feed(&mut analyzer, picture(1, n as u64 + 1, base, *ms), playing(30));
        }
        // The first camera's picture at 300 is 180 ms after the last of the second: it is quiet (more than 150 ms), so the
        // look is made at once, without it, and the look says that the view was not paired.
        let looks = feed(&mut analyzer, picture(0, 1, base, 300), playing(30));
        assert_eq!(looks.len(), 1, "no wait for a view that has gone quiet");
        assert_eq!(given(&looks[0]), [(0, 1, 0)]);
        assert!(looks[0].unpaired[1] && !looks[0].unpaired[0]);
        assert!(looks[0].active[1], "the camera is still fresh (less than FRESH): only no picture of it goes with this look");
        assert_eq!(looks[0].trigger, 0);
    }

    #[test]
    fn what_is_waiting_is_decided_at_once_when_asked_to() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // (Delivered while no game is played, so that they do not begin looks of their own.)
        let quiet = Context { playing: false, ..playing(30) };
        for (n, ms) in [0u64, 40, 80].iter().enumerate() {
            feed(&mut analyzer, picture(1, n as u64 + 1, base, *ms), quiet);
        }
        assert!(feed(&mut analyzer, picture(0, 1, base, 115), playing(30)).is_empty());
        assert!(analyzer.has_pending());
        let looks = flush(&mut analyzer, playing(30));
        assert_eq!(looks.len(), 1);
        assert_eq!(given(&looks[0]), [(0, 1, 0), (1, 3, -35)], "decided with what had come");
        assert!(!analyzer.has_pending());
        assert!(flush(&mut analyzer, playing(30)).is_empty());
    }

    #[test]
    fn a_wait_is_never_longer_than_the_bound_whatever_the_pace_of_the_other_view() {
        let base = Instant::now();
        let mut analyzer = analyzer();
        // The second camera gives a picture every 300 ms (its tolerance is 400 ms), the first every 40 ms.
        let mut schedule: Vec<(usize, u64)> = (0..40u64).map(|k| (0, 3 + k * 40)).collect();
        schedule.extend((0..6u64).map(|k| (1, k * 300)));
        schedule.sort_by_key(|&(view, ms)| (ms, view));
        let looks = run(&mut analyzer, base, &schedule, playing(30));
        assert!(looks.len() > 20, "{} looks", looks.len());
        for (look, released) in &looks {
            let waited = released.saturating_sub(ms_of(look, base));
            // The release comes with the arrival of a picture: at most one interval of the first camera after the bound.
            assert!(waited <= PAIR_WAIT_MAX.as_millis() as u64 + 40, "a look at {} ms was released at {released} ms", ms_of(look, base));
        }
        // The slow camera still goes with looks, with the picture nearest within its tolerance.
        assert!(looks.iter().any(|(l, _)| l.fed.iter().any(|p| p.view == 1)));
    }

    #[test]
    fn with_jitter_every_look_gets_the_nearest_picture_of_the_other_camera_and_the_same_pictures_give_the_same_looks() {
        let base = Instant::now();
        // The second camera, every 40 ms with a jitter of up to 12 ms either way (a fixed pseudo-random series).
        let mut state = 12345u64;
        let mut jitter = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) % 25) as i64 - 12
        };
        let second: Vec<u64> = (0..60u64).map(|k| (100 + k as i64 * 40 + jitter()) as u64).collect();
        let first: Vec<u64> = (0..60u64).map(|k| 100 + k * 40).collect();
        let mut schedule: Vec<(usize, u64)> = first.iter().map(|&ms| (0, ms)).chain(second.iter().map(|&ms| (1, ms))).collect();
        schedule.sort_by_key(|&(view, ms)| (ms, view));
        let mut a = analyzer();
        let looks = run(&mut a, base, &schedule, playing(30));
        let mut checked = 0;
        for (look, _) in looks.iter().filter(|(l, _)| l.trigger == 0) {
            let Some(index) = look.fed.iter().position(|p| p.view == 1) else { continue };
            let at = ms_of(look, base) as i64;
            // The nearest of all the second camera's pictures (the earlier one on a tie).
            let best = second.iter().enumerate().min_by_key(|&(_, &ms)| ((ms as i64 - at).abs(), ms)).map(|(n, _)| n as u64 + 1).unwrap();
            assert_eq!(look.fed[index].seq, best, "the look at {at} ms");
            assert!(look.skews_us[index].abs() <= 40_000, "{}", look.skews_us[index]);
            checked += 1;
        }
        assert!(checked > 30, "{checked} looks with a picture of the second camera");
        // The same pictures in the same order give the same looks.
        let mut b = analyzer();
        let again = run(&mut b, base, &schedule, playing(30));
        let shape = |looks: &[(Look, u64)]| looks.iter().map(|(l, r)| (ms_of(l, base), given(l), *r)).collect::<Vec<_>>();
        assert_eq!(shape(&looks), shape(&again));
    }

    #[test]
    fn the_analysis_is_behind_when_many_pictures_of_a_view_wait_or_the_oldest_has_waited_long() {
        let backlog =
            |deepest: usize, oldest_ms: u64| Backlog { waiting: deepest * 3, bytes: 0, oldest: Duration::from_millis(oldest_ms), deepest };
        assert!(!backlog(0, 0).is_behind() && !backlog(THIN_WAITING - 1, 900).is_behind(), "a few pictures, a short wait: keeping up");
        assert!(backlog(THIN_WAITING, 100).is_behind());
        assert!(backlog(1, 1000).is_behind());
    }

    #[test]
    fn the_rule_for_skipping_looks_is_read_from_its_name_and_is_behind_by_default() {
        assert_eq!(Thin::default(), Thin::Behind);
        assert_eq!(Thin::parse("never"), Some(Thin::Never));
        assert_eq!(Thin::parse("behind"), Some(Thin::Behind));
        assert_eq!(Thin::parse("always"), Some(Thin::Always));
        assert_eq!(Thin::parse("sometimes"), None);
        assert_eq!(global_thin(), Thin::Behind, "the command line did not say otherwise");
    }

    #[test]
    fn a_look_that_the_recording_says_was_skipped_is_made_when_the_tracker_is_not_sure_of_the_board() {
        let base = Instant::now();
        let mut a = analyzer();
        // No game position learned: the tracker cannot count a picture it did not look at, so it looks.
        a.expect_thin(Some(true));
        let looks = feed(&mut a, picture(0, 1, base, 0), playing(0));
        assert_eq!(looks.len(), 1);
        assert!(!looks[0].thinned, "a tracker that is not quiet looks at the picture");
        // And what a recording says is used once, not for the next look.
        let looks = feed(&mut a, picture(0, 2, base, 40), playing(0));
        assert!(!looks[0].thinned);
    }

    #[test]
    fn the_pace_of_a_view_is_the_median_of_its_gaps_and_a_late_picture_does_not_count() {
        let base = Instant::now();
        let mut pace = Pace::default();
        assert_eq!((pace.interval(), pace.tolerance(), pace.live_window()), (None, PAIR_TOLERANCE_DEFAULT, LIVE_MIN));
        let at = |ms: u64| base + Duration::from_millis(ms);
        for ms in [0, 40, 80, 120, 300, 340] {
            pace.note(at(ms));
        }
        assert_eq!(pace.interval(), Some(Duration::from_millis(40)), "the one long gap does not change the median");
        assert_eq!(pace.tolerance(), Duration::from_millis(60));
        assert_eq!(pace.live_window(), LIVE_MIN, "3 intervals are 120 ms, at least LIVE_MIN");
        // A picture that comes after a newer one says nothing.
        pace.note(at(330));
        assert_eq!(pace.interval(), Some(Duration::from_millis(40)));
        assert_eq!(pace.last, Some(at(340)));
    }
}
