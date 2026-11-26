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
    /// Which views took part.
    pub active: [bool; VIEWS],
    /// The set of views that took part is not the one of the look before (a recording says so).
    pub views_changed: bool,
    pub detection: Detection,
}

/// Decides, picture by picture, when the tracker looks and at what, and owns the tracker.
pub struct Analyzer {
    pub tracker: Tracker,
    /// The newest picture taken from the inbox for each view.
    latest: [Option<Item>; VIEWS],
    /// The number of the last picture of each view that the tracker was given: it is never given the same one twice.
    last_fed: [u64; VIEWS],
    last_look: Option<Instant>,
    last_active: Option<[bool; VIEWS]>,
}

impl Analyzer {
    pub fn new(tracker: Tracker) -> Analyzer {
        Analyzer { tracker, latest: [None, None, None], last_fed: [0; VIEWS], last_look: None, last_active: None }
    }

    /// Play begins (or begins again): what the views were told is told again at the first look.
    pub fn begin(&mut self) {
        self.last_active = None;
    }

    /// Takes a picture, which is the newest of its view, and looks if it is time. `context` is asked only when it is.
    /// Pictures must come in the order of their capture, as the inbox gives them. Only a picture of a camera starts a
    /// look; the height picture of the depth camera is given to the looks that the cameras start.
    pub fn on_picture(&mut self, item: Item, context: impl FnOnce() -> Context, interval: Duration) -> Option<Look> {
        let at = item.captured;
        let view = item.view;
        if view >= VIEWS {
            return None;
        }
        self.latest[view] = Some(item);
        if view == DEPTH_VIEW {
            return None;
        }
        // Not the time yet: no need to ask what the engine is doing. (The test is made again with the engine's own
        // interval, which a page may change between the two.)
        if self.last_look.is_some_and(|last| at.saturating_duration_since(last) < interval) {
            return None;
        }
        let context = context();
        if !context.playing || self.last_look.is_some_and(|last| at.saturating_duration_since(last) < context.interval) {
            return None;
        }
        let mut active = [false; VIEWS];
        let mut fed: Vec<Item> = Vec::new();
        // The arrays of the cameras are indexed together.
        #[allow(clippy::needless_range_loop)]
        for c in 0..CAMERAS {
            let newest = self.latest[c].as_ref();
            let fresh = newest.is_some_and(|p| at.saturating_duration_since(p.captured) < context.fresh);
            // A camera that has gone quiet is left out, so that the others are not held up; one that has not given a
            // picture since play began is waited for a little, so that all learn the squares together.
            let waiting =
                context.begun_at.is_some_and(|b| at.saturating_duration_since(b) < CAMERA_WAIT && newest.is_none_or(|p| p.captured < b));
            active[c] = context.usable[c] && (fresh || waiting);
            self.tracker.set_view_active(c, active[c]);
            if !context.usable[c] || !fresh {
                continue;
            }
            if let Some(picture) = newest {
                fed.push(picture.clone());
            }
        }
        // The depth camera takes part when it is wanted, has found the squares, and gives pictures.
        let depth = self.latest[DEPTH_VIEW].as_ref();
        active[DEPTH_VIEW] = context.depth && depth.is_some_and(|p| at.saturating_duration_since(p.captured) < context.fresh);
        self.tracker.set_view_active(DEPTH_VIEW, active[DEPTH_VIEW]);
        if active[DEPTH_VIEW]
            && let Some(picture) = depth
        {
            fed.push(picture.clone());
        }
        // A view is not given the same picture twice: a repeat would count as one more still picture.
        fed.retain(|picture| picture.seq != self.last_fed[picture.view]);
        // The first look has no look before it to be told different from.
        let views_changed = self.last_active != Some(active) && !fed.is_empty();
        if views_changed {
            self.last_active = Some(active);
        }
        if fed.is_empty() {
            return None;
        }
        self.last_look = Some(at);
        for picture in &fed {
            self.last_fed[picture.view] = picture.seq;
        }
        let images: Vec<Option<&RgbImage>> = (0..VIEWS).map(|v| fed.iter().find(|p| p.view == v).map(|p| p.image.as_ref())).collect();
        let started = Instant::now();
        let detection = self.tracker.feed_views(&images, at);
        let step = started.elapsed();
        Some(Look { at, started, step, fed, active, views_changed, detection })
    }
}

/// How the pictures wait: for the console, the page and the recording.
#[derive(Clone, Copy, Debug, Default)]
pub struct Backlog {
    /// Pictures waiting now, the memory they take, and how old the oldest of them is (since its capture).
    pub waiting: usize,
    pub bytes: usize,
    pub oldest: Duration,
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
        Backlog { waiting, bytes: state.waiting_bytes, oldest: oldest.map_or(Duration::ZERO, |c| now.saturating_duration_since(c)) }
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
    pub fn next(&self) -> Next {
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
                state = self.wake.wait(state).unwrap_or_else(PoisonError::into_inner);
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
        }
    }

    fn analyzer() -> Analyzer {
        Analyzer::new(Tracker::new(TrackerConfig::default()))
    }

    fn take_picture(next: Next) -> (usize, u64) {
        match next {
            Next::Picture(item, _) => (item.view, item.seq),
            Next::Job(_) => panic!("a job"),
            Next::Stop => panic!("stopped"),
        }
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
        let order: Vec<(usize, u64)> = (0..4).map(|_| take_picture(inbox.next())).collect();
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
        assert_eq!(take_picture(inbox.next()), (0, 1));
        assert!(started.elapsed() < Duration::from_millis(100));
        // A view that sent a picture just now may send an older one: it is waited for, up to LAG_WAIT.
        assert!(inbox.push(picture(1, 1, base, 0)));
        assert!(inbox.push(picture(0, 2, base, 30)));
        assert_eq!(take_picture(inbox.next()), (1, 1));
        let started = Instant::now();
        assert_eq!(take_picture(inbox.next()), (0, 2), "the other view sent nothing newer, so the wait ends");
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
        assert_eq!(take_picture(inbox.next()), (0, 1));
        assert_eq!(take_picture(inbox.next()), (0, 2));
        assert_eq!(take_picture(inbox.next()), (0, 3));
        let Next::Job(job) = inbox.next() else { panic!("the job comes before the picture pushed after it") };
        assert!(job.begin());
        assert!(!handle.cancel(), "a job that is running cannot be given up");
        assert_eq!(take_picture(inbox.next()), (0, 4));
        assert!(ran.lock().unwrap().is_empty());
    }

    #[test]
    fn a_command_that_was_given_up_does_not_run() {
        let inbox = Inbox::new(1 << 30);
        let handle = inbox.submit(Box::new(|_, _| panic!("must not run"))).expect("not stopped");
        assert!(handle.cancel());
        let Next::Job(job) = inbox.next() else { panic!("a job") };
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
        assert_eq!(take_picture(inbox.next()), (0, 1));
        assert!(inbox.push(picture(0, 4, base, 120)));
        assert_eq!(inbox.backlog().waiting, 2);
    }

    #[test]
    fn a_stopped_inbox_ends_the_thread_and_takes_no_command() {
        let inbox = Inbox::new(1 << 30);
        let waiter = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || matches!(inbox.next(), Next::Stop))
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
            if let Some(look) = analyzer.on_picture(picture(0, i + 1, base, i * 40), || playing(100), Duration::from_millis(100)) {
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
        let idle = || Context { playing: false, ..playing(0) };
        for i in 0..5u64 {
            assert!(analyzer.on_picture(picture(0, i + 1, base, i * 40), idle, Duration::ZERO).is_none());
        }
        // The pictures were taken all the same: the first look once a game is played has the newest of them.
        let look = analyzer.on_picture(picture(0, 6, base, 240), || playing(0), Duration::ZERO).expect("a look");
        assert_eq!(look.fed[0].seq, 6);
    }

    #[test]
    fn both_cameras_are_given_together_and_a_quiet_one_is_left_out() {
        let mut analyzer = analyzer();
        let base = Instant::now();
        // A picture of the second camera comes while no game is played: taken, nothing looked at.
        let idle = || Context { playing: false, ..playing(0) };
        assert!(analyzer.on_picture(picture(1, 1, base, 0), idle, Duration::ZERO).is_none());
        let look = analyzer.on_picture(picture(0, 1, base, 10), || playing(0), Duration::ZERO).expect("a look");
        assert_eq!(look.fed.iter().map(|p| (p.view, p.seq)).collect::<Vec<_>>(), [(0, 1), (1, 1)]);
        assert_eq!(look.active, [true, true, false]);
        // A view is not given the same picture twice: the next look, begun by the first camera, has only its new picture.
        let look = analyzer.on_picture(picture(0, 2, base, 20), || playing(0), Duration::ZERO).expect("a look");
        assert_eq!(look.fed.iter().map(|p| (p.view, p.seq)).collect::<Vec<_>>(), [(0, 2)]);
        assert_eq!(look.active, [true, true, false]);
        // The second camera goes quiet for more than FRESH: the first one goes on alone.
        let look = analyzer.on_picture(picture(0, 3, base, 10 + 2000 + 100), || playing(0), Duration::ZERO).expect("a look");
        assert_eq!(look.fed.iter().map(|p| p.view).collect::<Vec<_>>(), [0]);
        assert_eq!(look.active, [true, false, false]);
        assert!(look.views_changed);
    }
}
