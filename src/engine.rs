//! The heart of the program: ties the camera, the tracker, the game record, the
//! store and the roles together, and tells the pages when something changed.
//!
//! One mutex guards the state that matters (`Inner`). The capture thread takes it
//! once per frame, for a few milliseconds; commands from the pages take it briefly.

use crate::boarddetect;
use crate::calibration::{self, Calibration};
use crate::camera::SourceFactory;
use crate::game::{self, GameRecord};
use crate::picture::{self, Picture};
use crate::roles::{self, Role, Roles};
use crate::store::{Store, Summary};
use crate::tracker::{Candidate, Detection, Status, Tracker, TrackerConfig};
use anyhow::Result;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Locks that go on working after a thread panicked while holding them: the state they guard is
/// plain data, and one failed picture must not make every later request fail too.
trait Recover<T> {
    fn grab(&self) -> MutexGuard<'_, T>;
}

impl<T> Recover<T> for Mutex<T> {
    fn grab(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

trait RecoverRw<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T>;
    fn write_ok(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> RecoverRw<T> for RwLock<T> {
    fn read_ok(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }
    fn write_ok(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A calibration page left open for longer than this closes itself.
const CALIBRATION_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long to wait before trying the camera again after a failure.
const CAMERA_RETRY: Duration = Duration::from_secs(2);
/// During a game, how often the board is looked for again, to follow a camera that moved.
const FOLLOW_EVERY: Duration = Duration::from_millis(1500);
/// A shift smaller than this (in squares) is noise; one larger than this is not the same board.
const FOLLOW_MIN_SHIFT: f32 = 0.04;
const FOLLOW_MAX_SHIFT: f32 = 0.45;
/// Over a whole game the calibration may not move further than this (in squares) from where it began.
const FOLLOW_MAX_DRIFT: f32 = 1.0;
/// The moved calibration is written to disk at most this often.
const FOLLOW_SAVE_EVERY: Duration = Duration::from_secs(10);

/// Start of the notice shown when a game could not be written.
const SAVE_FAILED: &str = "cannot save the game";

/// Shown when the picture has another size than the one the board was calibrated on.
const SIZE_NOTICE: &str =
    "The camera picture is not the size it had when the board was calibrated (another camera or resolution): calibrate again.";

/// Microseconds the last preview JPEG took to make (for the speed figures on the page).
static ENCODE_US: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub idle_timeout: Duration,
    pub game_timeout: Duration,
    pub tracker: TrackerConfig,
    /// The tracker looks at a picture at most this often, however fast the camera
    /// delivers (the preview can be faster than the detection needs to be).
    pub track_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            idle_timeout: Duration::from_secs(60),
            game_timeout: Duration::from_secs(30 * 60),
            tracker: TrackerConfig::default(),
            track_interval: Duration::ZERO,
        }
    }
}

/// Why a command was refused.
#[derive(Debug)]
pub enum ApiError {
    /// The caller is not the controller.
    Forbidden,
    /// The request itself is wrong.
    BadRequest(String),
    /// The request is fine but not possible right now.
    Conflict(String),
    /// Something failed on this side (a file could not be written, for example).
    Internal(String),
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            ApiError::Forbidden => 403,
            ApiError::BadRequest(_) => 400,
            ApiError::Conflict(_) => 409,
            ApiError::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ApiError::Forbidden => "only the controller can do that".to_string(),
            ApiError::BadRequest(m) | ApiError::Conflict(m) | ApiError::Internal(m) => m.clone(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::BadRequest(format!("{e:#}"))
    }
}

impl ApiError {
    /// For a failure that is not the caller's fault: files, disk.
    pub fn internal(e: anyhow::Error) -> Self {
        ApiError::Internal(format!("{e:#}"))
    }
}

pub type ApiResult<T = ()> = Result<T, ApiError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Camera off.
    Idle,
    /// Camera on for the calibration page.
    Calibrating,
    /// A game is being recorded.
    Playing,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Idle => "idle",
            Mode::Calibrating => "calibrating",
            Mode::Playing => "playing",
        }
    }
}

/// The latest camera frame. The JPEG for the preview is made only if someone asks.
pub struct Latest {
    /// What everyone sees: turned and adjusted (brightness, contrast).
    pub image: Arc<RgbImage>,
    /// Turned, but not adjusted: the same picture as `image` when nothing is adjusted.
    pub raw: Arc<RgbImage>,
    pub seq: u64,
    jpeg: OnceLock<Arc<Vec<u8>>>,
}

impl Latest {
    fn new(raw: RgbImage, adjusted: Option<RgbImage>, seq: u64) -> Latest {
        let raw = Arc::new(raw);
        let image = adjusted.map_or_else(|| Arc::clone(&raw), Arc::new);
        Latest { image, raw, seq, jpeg: OnceLock::new() }
    }

    pub fn jpeg(&self) -> Arc<Vec<u8>> {
        self.jpeg
            .get_or_init(|| {
                let started = Instant::now();
                let bytes = Arc::new(encode_jpeg(&self.image, 80).unwrap_or_default());
                ENCODE_US.store(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32, Ordering::Relaxed);
                bytes
            })
            .clone()
    }
}

pub fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgb8,
    )?;
    Ok(bytes)
}

/// Wakes the pages that listen for events.
struct Hub {
    subscribers: Mutex<Vec<Sender<()>>>,
}

impl Hub {
    fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = channel();
        self.subscribers.grab().push(tx);
        rx
    }

    fn notify(&self) {
        // A page that has gone away has dropped its receiver: forget it.
        self.subscribers.grab().retain(|tx| tx.send(()).is_ok());
    }
}

/// Counts open picture streams while it lives.
pub struct FrameStreamGuard {
    engine: Arc<Engine>,
}

impl Drop for FrameStreamGuard {
    fn drop(&mut self) {
        self.engine.frame_streams.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Counts open event streams while it lives.
pub struct ClientGuard {
    engine: Arc<Engine>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.engine.clients.fetch_sub(1, Ordering::SeqCst);
        self.engine.hub.notify();
    }
}

struct Inner {
    roles: Roles,
    mode: Mode,
    calibrating_since: Option<Instant>,
    /// The game being recorded, or the last one that finished.
    game: Option<GameRecord>,
    game_started: Option<Instant>,
    tracker: Tracker,
    calibration: Option<Calibration>,
    candidates: Vec<Candidate>,
    notice: Option<String>,
    camera_error: Option<String>,
    source: String,
    last_status: Status,
    picture: Picture,
    last_feed: Option<Instant>,
    /// When the board was last looked for (to follow it), how many times the calibration was
    /// moved, and by how much (in squares) the last time.
    last_follow: Option<Instant>,
    follow_count: u32,
    follow_shift: f32,
    /// How often the board is looked for: slower on a machine where looking takes long.
    follow_every: Duration,
    /// The calibration when following began: the board may not drift further than a square from it.
    follow_anchor: Option<Calibration>,
    last_follow_save: Option<Instant>,
    /// The game to write to the disk: set under the lock, written by `flush_save` once the lock is
    /// released (writing and flushing a file can take long on a small machine).
    pending_save: Option<GameRecord>,
}

/// Speed figures of the camera pipeline, smoothed.
#[derive(Default)]
struct Stats {
    last_frame: Option<Instant>,
    interval_ms: f32,
    process_ms: f32,
}

pub struct Engine {
    inner: Mutex<Inner>,
    frame: RwLock<Option<Arc<Latest>>>,
    camera_wanted: Mutex<bool>,
    camera_cv: Condvar,
    hub: Hub,
    clients: AtomicUsize,
    restart_source: AtomicBool,
    store: Store,
    calibration_path: PathBuf,
    picture_path: PathBuf,
    track_interval: Duration,
    /// Counts the pictures received; the picture streams wait on it.
    frame_count: Mutex<u64>,
    frame_cv: Condvar,
    frame_streams: AtomicUsize,
    stats: Mutex<Stats>,
    /// Held while a game is written: one writer at a time, so that an older version cannot land
    /// after a newer one. Taken before `inner`, never while holding it.
    save_lock: Mutex<()>,
}

/// Picture streams allowed at once (two per page: the camera and the top-down board).
const MAX_FRAME_STREAMS: usize = 16;

#[derive(Debug, Default)]
pub struct StartRequest {
    pub white: String,
    pub black: String,
    pub fen: Option<String>,
    pub started_at: Option<String>,
}

impl Engine {
    pub fn new(config: EngineConfig, store: Store, calibration_path: PathBuf, calibration: Option<Calibration>) -> Arc<Engine> {
        let mut tracker = Tracker::new(config.tracker);
        tracker.set_calibration(calibration.as_ref());
        let picture_path = calibration_path.with_file_name("picture.json");
        let unfinished = store.latest_unfinished().unwrap_or_else(|e| {
            eprintln!("store: {e:#}");
            None
        });
        if let Some(game) = &unfinished {
            eprintln!("game: {} was not finished; it can be resumed or closed from the page", game.id);
        }
        let picture = Picture::load(&picture_path).unwrap_or_else(|e| {
            eprintln!("picture: {e:#}; using the defaults");
            Picture::default()
        });
        Arc::new(Engine {
            inner: Mutex::new(Inner {
                roles: Roles::new(config.idle_timeout, config.game_timeout),
                mode: Mode::Idle,
                calibrating_since: None,
                game: unfinished,
                game_started: None,
                tracker,
                calibration,
                candidates: Vec::new(),
                notice: None,
                camera_error: None,
                source: String::new(),
                last_status: Status::NoCalibration,
                picture,
                last_feed: None,
                last_follow: None,
                follow_count: 0,
                follow_shift: 0.0,
                follow_every: FOLLOW_EVERY,
                follow_anchor: None,
                last_follow_save: None,
                pending_save: None,
            }),
            frame: RwLock::new(None),
            camera_wanted: Mutex::new(false),
            camera_cv: Condvar::new(),
            hub: Hub { subscribers: Mutex::new(Vec::new()) },
            clients: AtomicUsize::new(0),
            restart_source: AtomicBool::new(false),
            store,
            calibration_path,
            picture_path,
            track_interval: config.track_interval,
            frame_count: Mutex::new(0),
            frame_cv: Condvar::new(),
            frame_streams: AtomicUsize::new(0),
            stats: Mutex::new(Stats::default()),
            save_lock: Mutex::new(()),
        })
    }

    // ---- Camera thread ------------------------------------------------------

    /// Runs the capture loop on its own thread: the camera is opened when someone
    /// needs it and closed when nobody does.
    pub fn spawn_capture(self: &Arc<Self>, factory: SourceFactory) -> std::io::Result<()> {
        let engine = Arc::clone(self);
        thread::Builder::new().name("capture".into()).spawn(move || engine.capture_loop(factory)).map(|_| ())
    }

    fn capture_loop(&self, factory: SourceFactory) {
        loop {
            {
                let mut wanted = self.camera_wanted.grab();
                while !*wanted {
                    wanted = self.camera_cv.wait(wanted).unwrap_or_else(PoisonError::into_inner);
                }
            }
            match factory() {
                Ok(mut source) => {
                    self.set_source(source.describe());
                    while self.camera_is_wanted() {
                        if self.restart_source.swap(false, Ordering::SeqCst) {
                            source.restart();
                        }
                        match source.next_frame() {
                            Ok(image) => {
                                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.on_frame(image))).is_err() {
                                    eprintln!("camera: a picture could not be processed (internal error); going on");
                                }
                            }
                            Err(e) => {
                                self.camera_failed(format!("{e:#}"));
                                break;
                            }
                        }
                    }
                    // Dropping the source closes the camera.
                }
                Err(e) => self.camera_failed(format!("{e:#}")),
            }
            *self.frame.write_ok() = None;
            self.set_source(String::new());
            if self.camera_is_wanted() {
                thread::sleep(CAMERA_RETRY);
            }
        }
    }

    fn camera_is_wanted(&self) -> bool {
        *self.camera_wanted.grab()
    }

    fn set_camera(&self, wanted: bool) {
        *self.camera_wanted.grab() = wanted;
        self.camera_cv.notify_all();
    }

    /// Back to idle: nothing is recorded or calibrated, and the camera is released.
    fn go_idle(&self, inner: &mut Inner) {
        inner.mode = Mode::Idle;
        inner.calibrating_since = None;
        // An error of a camera that is now off would only confuse.
        inner.camera_error = None;
        self.set_camera(false);
    }

    fn set_source(&self, description: String) {
        self.inner.grab().source = description;
        self.hub.notify();
    }

    fn camera_failed(&self, message: String) {
        eprintln!("camera: {message}");
        self.inner.grab().camera_error = Some(message);
        self.hub.notify();
    }

    /// Handles one camera frame.
    fn on_frame(&self, image: RgbImage) {
        let started = Instant::now();
        let seq = self.frame.read_ok().as_ref().map_or(1, |l| l.seq + 1);
        let picture = self.inner.grab().picture;
        let raw = picture.rotate(image);
        let adjusted = picture.adjust(&raw);
        let latest = Arc::new(Latest::new(raw, adjusted, seq));
        *self.frame.write_ok() = Some(Arc::clone(&latest));
        {
            *self.frame_count.grab() += 1;
            self.frame_cv.notify_all();
        }

        let now = Instant::now();
        let mut guard = self.inner.grab();
        let inner = &mut *guard;
        let mut changed = inner.camera_error.take().is_some();
        // Time-based clean-up also runs with no page open (the camera must not stay on for ever).
        changed |= self.housekeeping(inner, now);

        // A calibration is only good for the picture size it was made for.
        let size = [latest.image.width(), latest.image.height()];
        let mut size_ok = true;
        if let Some(calibration) = inner.calibration.as_mut() {
            match calibration.size {
                None => calibration.size = Some(size),
                Some(known) if known != size => size_ok = false,
                Some(_) => {}
            }
        }
        if !size_ok {
            if inner.notice.as_deref() != Some(SIZE_NOTICE) {
                inner.notice = Some(SIZE_NOTICE.to_string());
                changed = true;
            }
        } else if inner.notice.as_deref() == Some(SIZE_NOTICE) {
            inner.notice = None;
            changed = true;
        }

        let feed_due = inner.last_feed.is_none_or(|t| now.duration_since(t) >= self.track_interval);
        if inner.mode == Mode::Playing && feed_due && size_ok {
            inner.last_feed = Some(now);
            match inner.tracker.feed(&latest.image, now) {
                Detection::Nothing => {
                    if inner.tracker.status() != Status::Unsure && !inner.candidates.is_empty() {
                        inner.candidates.clear();
                        changed = true;
                    }
                }
                Detection::Unsure { candidates } => {
                    // Tell the pages only when the list changed: this comes at every picture while unsure.
                    if !inner.candidates.iter().map(|c| &c.uci).eq(candidates.iter().map(|c| &c.uci)) {
                        inner.candidates = candidates;
                        changed = true;
                    }
                }
                Detection::Pair { first, second, since } => {
                    inner.candidates.clear();
                    self.apply_detected_pair(inner, first, second, since);
                    changed = true;
                }
                Detection::Move { mv, since, promotion_guess } => {
                    inner.candidates.clear();
                    self.apply_detected_move(inner, mv, since, promotion_guess);
                    changed = true;
                }
            }
            let status = inner.tracker.status();
            if status != inner.last_status {
                inner.last_status = status;
                changed = true;
            }
        }
        drop(guard);
        // A failure to save becomes the notice of the pages; nothing else to do here.
        let _ = self.flush_save();
        if changed {
            self.hub.notify();
        }
        self.follow_board(&latest, now);
        self.note_speed(started);
    }

    /// Keeps the figures shown under the camera picture: pictures per second, and the time
    /// one picture takes here.
    fn note_speed(&self, started: Instant) {
        let mut stats = self.stats.grab();
        let smooth = |old: f32, new: f32| if old == 0.0 { new } else { 0.8 * old + 0.2 * new };
        if let Some(last) = stats.last_frame {
            stats.interval_ms = smooth(stats.interval_ms, started.duration_since(last).as_secs_f32() * 1000.0);
        }
        stats.last_frame = Some(started);
        stats.process_ms = smooth(stats.process_ms, started.elapsed().as_secs_f32() * 1000.0);
    }

    /// True while a game is being recorded (the Android app refuses camera changes then).
    pub fn is_playing(&self) -> bool {
        self.inner.grab().mode == Mode::Playing
    }

    /// Pictures per second and milliseconds per picture, for the page.
    pub fn camera_stats(&self) -> Value {
        let stats = self.stats.grab();
        let fps = match stats.last_frame {
            Some(last) if last.elapsed() < Duration::from_secs(3) && stats.interval_ms > 0.0 => 1000.0 / stats.interval_ms,
            _ => 0.0,
        };
        json!({
            "fps": (fps * 10.0).round() / 10.0,
            "process_ms": (stats.process_ms * 10.0).round() / 10.0,
            "encode_ms": (ENCODE_US.load(Ordering::Relaxed) as f32 / 100.0).round() / 10.0,
            "streams": self.frame_streams.load(Ordering::SeqCst),
        })
    }

    /// Stabilization: during a game, look for the board again now and then. If the camera or
    /// the board has shifted a little (a knock, a phone slipping on its stand), the calibration
    /// moves with it, so the squares stay where the detection looks for them.
    fn follow_board(&self, latest: &Latest, now: Instant) {
        let size = [latest.image.width(), latest.image.height()];
        let calibration = {
            let mut inner = self.inner.grab();
            if inner.mode != Mode::Playing || !inner.picture.follow {
                return;
            }
            if inner.last_follow.is_some_and(|t| now.duration_since(t) < inner.follow_every) {
                return;
            }
            // Only while the board is quiet: no hand, no move being made.
            if !matches!(inner.tracker.status(), Status::Watching | Status::Learning) {
                return;
            }
            inner.last_follow = Some(now);
            match inner.calibration {
                Some(c) if c.size.is_none_or(|known| known == size) => c,
                _ => return,
            }
        };
        // The search runs without the lock: it takes a few tens of milliseconds on a PC, more on
        // a small machine, which then looks less often (never more than 1/20th of the time).
        let started = Instant::now();
        let found = boarddetect::refine(&latest.image, &calibration);
        let spent = started.elapsed();
        let mut inner = self.inner.grab();
        inner.follow_every = FOLLOW_EVERY.max(spent * 20);
        let Ok(found) = found else { return };
        let moved = Calibration::for_frame(found.corners, size[0], size[1]);
        let shift = boarddetect::shift_in_squares(&calibration, &moved);
        if !(FOLLOW_MIN_SHIFT..=FOLLOW_MAX_SHIFT).contains(&shift) || moved.validate(size[0], size[1]).is_err() {
            return;
        }
        // Things may have changed while looking.
        if inner.mode != Mode::Playing || inner.calibration != Some(calibration) {
            return;
        }
        let anchor = *inner.follow_anchor.get_or_insert(calibration);
        if boarddetect::shift_in_squares(&anchor, &moved) > FOLLOW_MAX_DRIFT {
            eprintln!("follow: the board is more than a square from where the game began; not following (calibrate again)");
            return;
        }
        match inner.tracker.status() {
            // Nothing learned yet: learn from the better grid.
            Status::Learning => inner.tracker.set_calibration(Some(&moved)),
            Status::Watching => inner.tracker.move_calibration(&moved),
            _ => return,
        }
        inner.calibration = Some(moved);
        let save_due = inner.last_follow_save.is_none_or(|t| now.duration_since(t) >= FOLLOW_SAVE_EVERY);
        if save_due {
            inner.last_follow_save = Some(now);
        }
        inner.follow_count += 1;
        inner.follow_shift = shift;
        eprintln!("follow: the board moved by {shift:.2} squares, calibration moved along");
        drop(inner);
        if save_due && let Err(e) = moved.save(&self.calibration_path) {
            eprintln!("calibration: cannot save the moved board: {e:#}");
        }
        self.hub.notify();
    }

    fn apply_detected_move(&self, inner: &mut Inner, mv: shakmaty::Move, since: Instant, guess: bool) {
        let Some(game) = inner.game.as_mut() else { return };
        let t = since.saturating_duration_since(inner.game_started.unwrap_or(since)).as_secs_f64();
        match game.push_move(mv, t, guess) {
            Ok(record) => eprintln!("game: {} {}", record.san, record.uci),
            Err(e) => {
                // The tracker and the game disagree: trust the game.
                inner.notice = Some(format!("a detected move was rejected ({e:#}); position kept"));
                if let Ok(pos) = game.position() {
                    inner.tracker.set_position(pos);
                }
                return;
            }
        }
        self.after_game_change(inner);
    }

    /// Two moves were recognised at once: both are recorded, at the time the board became still.
    fn apply_detected_pair(&self, inner: &mut Inner, first: shakmaty::Move, second: shakmaty::Move, since: Instant) {
        let Some(game) = inner.game.as_mut() else { return };
        let t = since.saturating_duration_since(inner.game_started.unwrap_or(since)).as_secs_f64();
        let before = game.moves.len();
        for mv in [first, second] {
            match game.push_move(mv, t, mv.promotion().is_some()) {
                Ok(record) => eprintln!("game: {} {} (two moves at once)", record.san, record.uci),
                Err(e) => {
                    // The tracker and the game disagree: trust the game, and keep neither move.
                    while game.moves.len() > before {
                        game.undo();
                    }
                    inner.notice = Some(format!("two detected moves were rejected ({e:#}); position kept"));
                    if let Ok(pos) = game.position() {
                        inner.tracker.set_position(pos);
                    }
                    return;
                }
            }
        }
        self.after_game_change(inner);
    }

    /// Saves the game and ends it if the rules say so.
    fn after_game_change(&self, inner: &mut Inner) {
        let Some(game) = inner.game.as_mut() else { return };
        let mut warning = None;
        match game.rules_outcome() {
            Ok(Some((result, reason))) => {
                game.finish(&result, &reason);
                eprintln!("game: over, {result} ({reason})");
                self.go_idle(inner);
            }
            Ok(None) => {}
            Err(e) => warning = Some(format!("{e:#}")),
        }
        self.save(inner);
        if warning.is_some() {
            inner.notice = warning;
        }
    }

    /// Marks the game to be written. The caller calls `flush_save` after releasing the lock.
    fn save(&self, inner: &mut Inner) {
        inner.pending_save = inner.game.clone();
    }

    /// Writes the game marked by `save`. Call it with no lock held. Whoever gets here writes the
    /// newest version. A failure becomes the notice of the pages and an error for the caller.
    fn flush_save(&self) -> ApiResult {
        let _writing = self.save_lock.grab();
        let Some(game) = self.inner.grab().pending_save.take() else { return Ok(()) };
        match self.store.save(&game) {
            Ok(()) => {
                let mut inner = self.inner.grab();
                if inner.notice.as_deref().is_some_and(|n| n.starts_with(SAVE_FAILED)) {
                    inner.notice = None;
                }
                Ok(())
            }
            Err(e) => {
                eprintln!("store: {e:#}");
                let message = format!("{SAVE_FAILED}: {e:#}");
                self.inner.grab().notice = Some(message.clone());
                self.hub.notify();
                Err(ApiError::Internal(message))
            }
        }
    }

    // ---- Frames -------------------------------------------------------------

    pub fn latest_frame(&self) -> Option<Arc<Latest>> {
        self.frame.read_ok().clone()
    }

    /// Registers an open picture stream. Returns `None` if there are too many.
    pub fn open_frame_stream(self: &Arc<Self>) -> Option<FrameStreamGuard> {
        if self.frame_streams.fetch_add(1, Ordering::SeqCst) >= MAX_FRAME_STREAMS {
            self.frame_streams.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(FrameStreamGuard { engine: Arc::clone(self) })
    }

    /// Waits for a picture other than `last`. Returns `None` if none comes in time.
    pub fn wait_for_frame(&self, last: Option<&Arc<Latest>>, timeout: Duration) -> Option<Arc<Latest>> {
        let deadline = Instant::now() + timeout;
        let mut count = self.frame_count.grab();
        loop {
            if let Some(frame) = self.latest_frame()
                && last.is_none_or(|l| !Arc::ptr_eq(l, &frame))
            {
                return Some(frame);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            count = self.frame_cv.wait_timeout(count, deadline - now).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// True when the camera is off or about to be: a picture stream can end.
    pub fn camera_is_idle(&self) -> bool {
        self.inner.grab().mode == Mode::Idle
    }

    /// The top-down board for the current calibration, as JPEG.
    pub fn warped_jpeg(&self, with_grid: bool) -> ApiResult<Vec<u8>> {
        let latest = self.latest_frame().ok_or_else(|| ApiError::Conflict("the camera is off".into()))?;
        self.warped_jpeg_of(&latest, with_grid)
    }

    /// The top-down board from one given camera frame.
    pub fn warped_jpeg_of(&self, latest: &Latest, with_grid: bool) -> ApiResult<Vec<u8>> {
        let calibration = self.inner.grab().calibration.ok_or_else(|| ApiError::Conflict("not calibrated yet".into()))?;
        let mut warped = calibration::warp_board(&latest.image, &calibration).map_err(ApiError::internal)?;
        if with_grid {
            calibration::draw_grid(&mut warped);
        }
        encode_jpeg(&warped, 85).map_err(ApiError::internal)
    }

    // ---- Pages: state and events --------------------------------------------

    /// Registers an open event stream. Returns `None` if there are too many.
    pub fn open_stream(self: &Arc<Self>, limit: usize) -> Option<(ClientGuard, Receiver<()>)> {
        if self.clients.fetch_add(1, Ordering::SeqCst) >= limit {
            self.clients.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        let rx = self.hub.subscribe();
        self.hub.notify();
        Some((ClientGuard { engine: Arc::clone(self) }, rx))
    }

    /// Everything a page needs to draw itself, for one user.
    pub fn snapshot(&self, token: &str) -> Value {
        let now = Instant::now();
        let mut guard = self.inner.grab();
        let inner = &mut *guard;
        self.housekeeping(inner, now);
        let playing = inner.mode == Mode::Playing;
        let role = if roles::valid_token(token) { inner.roles.touch(token, now, playing) } else { Role::Viewer };
        let frame = self.frame.read_ok().clone();

        let game = inner.game.as_ref().map(|g| {
            let position = g.position().ok();
            json!({
                "id": g.id,
                "started_at": g.started_at,
                "white": g.white,
                "black": g.black,
                "start_fen": g.start_fen,
                "fen": position.as_ref().map(game::fen_of),
                "turn": position.as_ref().map(|p| if shakmaty::Position::turn(p) == shakmaty::Color::White { "white" } else { "black" }),
                "moves": g.moves.iter().map(|m| json!({"san": m.san, "uci": m.uci, "t": m.t, "guess": m.promotion_guess})).collect::<Vec<_>>(),
                "result": g.result,
                "termination": g.termination,
                "over": g.result.is_some(),
                "draw_claim": g.claimable_draw().ok().flatten(),
            })
        });
        let promotion_pending = inner.game.as_ref().and_then(|g| g.moves.last()).is_some_and(|m| m.promotion_guess);

        json!({
            "version": crate::version_text(),
            "role": if role == Role::Controller { "controller" } else { "viewer" },
            "can_claim": inner.roles.is_free(now, playing),
            "mode": inner.mode.name(),
            "camera": {
                "on": frame.is_some() || inner.mode != Mode::Idle,
                "source": inner.source,
                "error": inner.camera_error,
                "width": frame.as_ref().map_or(0, |f| f.image.width()),
                "height": frame.as_ref().map_or(0, |f| f.image.height()),
            },
            "calibration": inner.calibration.map(|c| c.corners),
            "picture": {
                "rotation": inner.picture.rotation,
                "brightness": inner.picture.brightness,
                "contrast": inner.picture.contrast,
            },
            "stabilize": {
                "on": inner.picture.follow,
                "adjusted": inner.follow_count,
                "last_shift": (inner.follow_shift * 100.0).round() / 100.0,
            },
            "tracker": {
                "status": status_name(inner.tracker.status()),
                "candidates": inner.candidates.iter().map(|c| json!({"uci": c.uci, "san": c.san, "cost": (c.cost * 10.0).round() / 10.0})).collect::<Vec<_>>(),
                "promotion_pending": promotion_pending,
            },
            "game": game,
            "notice": inner.notice,
            "clients": self.clients.load(Ordering::SeqCst),
        })
    }

    /// Time-based clean-up: a controller who has gone silent loses the role.
    /// Returns true when the engine went idle (the pages must be told).
    fn housekeeping(&self, inner: &mut Inner, now: Instant) -> bool {
        let playing = inner.mode == Mode::Playing;
        let lost_controller = inner.roles.expire(now, playing);
        let calibration_too_long = inner.calibrating_since.is_some_and(|s| now.duration_since(s) > CALIBRATION_LIMIT);
        if inner.mode == Mode::Calibrating && (lost_controller || calibration_too_long) {
            self.go_idle(inner);
            return true;
        }
        false
    }

    fn controlled(&self, token: &str) -> ApiResult<MutexGuard<'_, Inner>> {
        let now = Instant::now();
        let mut guard = self.inner.grab();
        self.housekeeping(&mut guard, now);
        let playing = guard.mode == Mode::Playing;
        if !roles::valid_token(token) || !guard.roles.is_controller(token, now, playing) {
            return Err(ApiError::Forbidden);
        }
        guard.roles.touch(token, now, playing);
        Ok(guard)
    }

    // ---- Roles ----------------------------------------------------------------

    pub fn claim(&self, token: &str) -> ApiResult {
        if !roles::valid_token(token) {
            return Err(ApiError::BadRequest("missing client token".into()));
        }
        let now = Instant::now();
        let mut inner = self.inner.grab();
        let playing = inner.mode == Mode::Playing;
        if !inner.roles.claim(token, now, playing) {
            return Err(ApiError::Conflict("someone else is controlling".into()));
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    pub fn release(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        inner.roles.release(token);
        if inner.mode == Mode::Calibrating {
            self.go_idle(&mut inner);
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    // ---- Calibration -----------------------------------------------------------

    pub fn open_calibration(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Idle {
            inner.mode = Mode::Calibrating;
            inner.calibrating_since = Some(Instant::now());
            self.set_camera(true);
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    pub fn close_calibration(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Calibrating {
            self.go_idle(&mut inner);
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    pub fn set_calibration(&self, token: &str, corners: [[f32; 2]; 4]) -> ApiResult {
        let mut inner = self.controlled(token)?;
        let frame = self.latest_frame().ok_or_else(|| ApiError::Conflict("the camera is off: open the calibration first".into()))?;
        let calibration = Calibration::for_frame(corners, frame.image.width(), frame.image.height());
        calibration.validate(frame.image.width(), frame.image.height())?;
        calibration.save(&self.calibration_path).map_err(ApiError::internal)?;
        inner.calibration = Some(calibration);
        inner.follow_anchor = None;
        // During a game the squares are learned again from the position reached.
        inner.tracker.set_calibration(Some(&calibration));
        inner.notice = None;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Finds the board in the picture by itself and calibrates with it.
    /// Returns a sentence for the page: what was found, and what to check.
    pub fn detect_calibration(&self, token: &str) -> ApiResult<String> {
        drop(self.controlled(token)?);
        let frame = self.latest_frame().ok_or_else(|| ApiError::Conflict("the camera is off: open the calibration first".into()))?;
        // Without the lock: searching takes a fraction of a second.
        let found = boarddetect::find_board(&frame.image).map_err(|e| ApiError::Conflict(format!("{e:#}")))?;
        self.set_calibration(token, found.corners)?;
        let mut text = format!("Board found ({} of 49 inner corners seen). ", found.matched);
        if found.sides_from_pieces {
            text.push_str("White is on the side of the lighter pieces.");
        } else {
            text.push_str("I could not tell White's side, so a1 is at the lower left: use Swap sides if it is the other way.");
        }
        Ok(text)
    }

    /// Switches on or off the following of the board during a game.
    pub fn set_follow(&self, token: &str, on: bool) -> ApiResult {
        let mut inner = self.controlled(token)?;
        let settings = Picture { follow: on, ..inner.picture };
        settings.save(&self.picture_path).map_err(ApiError::internal)?;
        inner.picture = settings;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// The picture settings may not change during a game: the squares were learned
    /// from the picture as it was.
    fn picture_guard(&self, token: &str) -> ApiResult<MutexGuard<'_, Inner>> {
        let inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is running: end it first".into()));
        }
        Ok(inner)
    }

    /// Keeps new picture settings. `turns` is how many quarter turns clockwise the picture
    /// was just turned by (0 when only brightness or contrast changed).
    fn store_picture(&self, inner: &mut Inner, picture: Picture, turns: i32) -> ApiResult {
        picture.validate()?;
        picture.save(&self.picture_path).map_err(ApiError::internal)?;
        inner.picture = picture;
        // The next picture will have the new settings. Redo the one on show too, so the
        // page does not have to wait for it.
        if let Some(latest) = self.latest_frame() {
            let raw = Picture::default().turned(turns).rotate((*latest.raw).clone());
            let adjusted = picture.adjust(&raw);
            *self.frame.write_ok() = Some(Arc::new(Latest::new(raw, adjusted, latest.seq)));
            *self.frame_count.grab() += 1;
            self.frame_cv.notify_all();
        }
        Ok(())
    }

    /// Turns the picture by `turns` quarter turns clockwise (negative: counter-clockwise).
    /// The calibration is turned with it, so it stays valid.
    pub fn rotate_picture(&self, token: &str, turns: i32) -> ApiResult {
        let mut inner = self.picture_guard(token)?;
        let turns = turns.rem_euclid(4);
        if turns == 0 {
            return Ok(());
        }
        let mut turned_calibration = None;
        if let Some(calibration) = inner.calibration {
            let frame = self.latest_frame().ok_or_else(|| {
                ApiError::Conflict("the camera is off: open it first, so the calibration can be turned with the picture".into())
            })?;
            let (corners, width, height) = picture::turn_points(&calibration.corners, frame.raw.width(), frame.raw.height(), turns);
            let turned = Calibration::for_frame(corners, width, height);
            turned.validate(width, height)?;
            turned_calibration = Some(turned);
        }
        let settings = inner.picture.turned(turns);
        if let Some(turned) = turned_calibration {
            turned.save(&self.calibration_path).map_err(ApiError::internal)?;
            inner.calibration = Some(turned);
            inner.follow_anchor = None;
            inner.tracker.set_calibration(Some(&turned));
        }
        self.store_picture(&mut inner, settings, turns)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Sets brightness and contrast.
    pub fn set_picture_adjust(&self, token: &str, brightness: f32, contrast: f32) -> ApiResult {
        let mut inner = self.picture_guard(token)?;
        let settings = Picture { brightness, contrast, ..inner.picture };
        self.store_picture(&mut inner, settings, 0)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Stretches the picture so that the board (or the whole picture, if the board is
    /// not calibrated yet) uses most of the range from dark to light.
    pub fn auto_contrast(&self, token: &str) -> ApiResult {
        let mut inner = self.picture_guard(token)?;
        let frame = self.latest_frame().ok_or_else(|| ApiError::Conflict("the camera is off: open it first".into()))?;
        let measured = match inner.calibration {
            Some(calibration) => calibration::warp_board(&frame.raw, &calibration)?,
            None => (*frame.raw).clone(),
        };
        let (contrast, brightness) = Picture::auto_contrast(&measured)?;
        let settings = Picture { brightness, contrast, ..inner.picture };
        self.store_picture(&mut inner, settings, 0)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Back to the picture as the camera gives it (the rotation stays).
    pub fn reset_picture_adjust(&self, token: &str) -> ApiResult {
        self.set_picture_adjust(token, 0.0, 1.0)
    }

    pub fn clear_calibration(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is running".into()));
        }
        match std::fs::remove_file(&self.calibration_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ApiError::Internal(format!("cannot delete the calibration file: {e}"))),
        }
        inner.calibration = None;
        inner.tracker.set_calibration(None);
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    // ---- Games -------------------------------------------------------------------

    pub fn start_game(&self, token: &str, request: StartRequest) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is already running".into()));
        }
        if inner.calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board first".into()));
        }
        let id = self.store.new_id(request.started_at.as_deref());
        let game = GameRecord::new(id, request.started_at, &request.white, &request.black, request.fen)?;
        let position = game.start_position()?;
        inner.game = Some(game);
        self.begin_playing(&mut inner, position, 0.0);
        // A scripted source (the demo) begins its game again; a real camera does nothing.
        self.restart_source.store(true, Ordering::SeqCst);
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// The board shows `position`: follow the game from there. `resume_at` is how many seconds
    /// of the game were already played (0 for a new game), so the timestamps carry on.
    fn begin_playing(&self, inner: &mut Inner, position: shakmaty::Chess, resume_at: f64) {
        inner.tracker.start(position);
        // The time of the last move comes from a file: do not trust it to be a sane number.
        let played = Duration::try_from_secs_f64(resume_at.max(0.0)).unwrap_or_default();
        inner.game_started = Some(Instant::now().checked_sub(played).unwrap_or_else(Instant::now));
        inner.mode = Mode::Playing;
        inner.calibrating_since = None;
        inner.candidates.clear();
        inner.notice = None;
        inner.last_status = Status::Learning;
        inner.follow_anchor = None;
        self.set_camera(true);
    }

    /// Goes on with the unfinished game that was found at start (or left by a crash or a stop).
    /// The board must show the position the game had reached.
    pub fn resume_game(&self, token: &str) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is already running".into()));
        }
        if inner.calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board first".into()));
        }
        let game = inner
            .game
            .as_ref()
            .filter(|g| g.result.is_none())
            .ok_or_else(|| ApiError::Conflict("there is no unfinished game to resume".into()))?;
        let position = game.position().map_err(ApiError::internal)?;
        let resume_at = game.moves.last().map_or(0.0, |m| m.t);
        self.begin_playing(inner, position, resume_at);
        drop(guard);
        self.hub.notify();
        Ok(())
    }

    /// Ends the game with a result decided by the players.
    pub fn end_game(&self, token: &str, result: &str, reason: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        let unfinished_but_stopped = inner.mode != Mode::Playing && inner.game.as_ref().is_some_and(|g| g.result.is_none());
        if inner.mode != Mode::Playing && !unfinished_but_stopped {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        if !["1-0", "0-1", "1/2-1/2", "*"].contains(&result) {
            return Err(ApiError::BadRequest("unknown result".into()));
        }
        // A draw by a rule that a player must claim: only when the position allows it.
        const CLAIMS: [&str; 2] = ["threefold repetition", "50-move rule"];
        if !["resignation", "agreement", "abandoned", "other"].contains(&reason) && !CLAIMS.contains(&reason) {
            return Err(ApiError::BadRequest("unknown reason".into()));
        }
        if CLAIMS.contains(&reason) {
            if result != "1/2-1/2" {
                return Err(ApiError::BadRequest("a claimed draw is a draw".into()));
            }
            let claimable = inner.game.as_ref().and_then(|g| g.claimable_draw().ok().flatten());
            if claimable != Some(reason) {
                return Err(ApiError::Conflict("that draw cannot be claimed in this position".into()));
            }
        }
        let no_moves = inner.game.as_ref().is_none_or(|g| g.moves.is_empty());
        let mut forget = None;
        if reason == "abandoned" && no_moves {
            // Nothing was played: do not keep an empty game, in memory or on the disk.
            forget = inner.game.take().map(|g| g.id);
            inner.pending_save = None;
        } else if let Some(game) = inner.game.as_mut() {
            game.finish(result, reason);
            self.save(&mut inner);
        }
        self.go_idle(&mut inner);
        inner.candidates.clear();
        drop(inner);
        if let Some(id) = forget
            && let Err(e) = self.store.remove(&id)
        {
            eprintln!("store: {e:#}");
        }
        let saved = self.flush_save();
        self.hub.notify();
        saved
    }

    /// Records a move entered by hand (UCI or SAN), or one picked from the candidates.
    pub fn manual_move(&self, token: &str, text: &str) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        let t = inner.game_started.map_or(0.0, |s| s.elapsed().as_secs_f64());
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        // One move, or two ("e4 e5") when a reply was played before the first move was seen: all or nothing.
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.is_empty() || parts.len() > 2 {
            return Err(ApiError::BadRequest("give one move, or two separated by a space".into()));
        }
        let before = game.moves.len();
        for part in parts {
            if let Err(e) = game.push_text(part, t) {
                while game.moves.len() > before {
                    game.undo();
                }
                return Err(e.into());
            }
        }
        let position = game.position().map_err(ApiError::internal)?;
        inner.tracker.set_position(position);
        inner.candidates.clear();
        self.after_game_change(inner);
        drop(guard);
        let saved = self.flush_save();
        self.hub.notify();
        saved
    }

    /// Takes back the last move. Also reopens a game that ended by the rules,
    /// for example when a misread move led to a false checkmate.
    pub fn undo(&self, token: &str) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        let reopenable = inner.mode != Mode::Playing
            && inner.game.as_ref().is_some_and(|g| {
                matches!(
                    g.termination.as_deref(),
                    Some("checkmate" | "stalemate" | "insufficient material" | "fivefold repetition" | "75-move rule")
                )
            });
        if inner.mode != Mode::Playing && !reopenable {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        if reopenable && inner.calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board first".into()));
        }
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        game.undo().ok_or_else(|| ApiError::Conflict("no move to take back".into()))?;
        let position = game.position().map_err(ApiError::internal)?;
        let resume_at = game.moves.last().map_or(0.0, |m| m.t);
        if reopenable {
            // The board must show the position again before learning resumes.
            self.begin_playing(inner, position, resume_at);
        } else {
            inner.tracker.set_position(position);
        }
        inner.candidates.clear();
        self.save(inner);
        drop(guard);
        let saved = self.flush_save();
        self.hub.notify();
        saved
    }

    /// Changes the piece chosen for the last promotion.
    pub fn set_promotion(&self, token: &str, piece: char) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode != Mode::Playing {
            // A finished game keeps its result: take the last move back first (which reopens the
            // game when the rules had ended it), then change the piece.
            return Err(ApiError::Conflict("no game is running".into()));
        }
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        game.set_last_promotion(piece)?;
        let position = game.position().map_err(ApiError::internal)?;
        inner.tracker.set_position(position);
        self.after_game_change(inner);
        drop(guard);
        let saved = self.flush_save();
        self.hub.notify();
        saved
    }

    /// Learns the look of the squares again, from the position the game is in.
    pub fn relearn(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        inner.tracker.relearn();
        inner.last_status = Status::Learning;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    // ---- Past games ----------------------------------------------------------------

    pub fn list_games(&self) -> Result<Vec<Summary>> {
        self.store.list()
    }

    /// A stored game with the position after each move, ready to replay.
    pub fn game_detail(&self, id: &str) -> Result<Option<Value>> {
        let Some(game) = self.store.load(id)? else { return Ok(None) };
        let fens = game.fens()?;
        Ok(Some(json!({
            "id": game.id,
            "started_at": game.started_at,
            "white": game.white,
            "black": game.black,
            "start_fen": game.start_fen,
            "result": game.result,
            "termination": game.termination,
            "moves": game.moves.iter().map(|m| json!({"san": m.san, "uci": m.uci, "t": m.t, "guess": m.promotion_guess})).collect::<Vec<_>>(),
            "fens": fens,
        })))
    }

    pub fn game_pgn(&self, id: &str) -> Result<Option<String>> {
        match self.store.load(id)? {
            Some(game) => Ok(Some(game.to_pgn()?)),
            None => Ok(None),
        }
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::NoCalibration => "no_calibration",
        Status::Learning => "learning",
        Status::Watching => "watching",
        Status::Moving => "moving",
        Status::Unsure => "unsure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with a game that is being recorded, to feed the detections of the tracker by hand.
    fn engine_playing(name: &str) -> (Arc<Engine>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let engine = Engine::new(EngineConfig::default(), store, dir.join("calibration.json"), None);
        {
            let mut inner = engine.inner.grab();
            inner.game = Some(GameRecord::new("pair".into(), None, "A", "B", None).unwrap());
            inner.mode = Mode::Playing;
            inner.game_started = Some(Instant::now());
        }
        (engine, dir)
    }

    #[test]
    fn two_detected_moves_are_both_recorded_and_an_illegal_pair_is_not() {
        let (engine, dir) = engine_playing("detected-pair");
        use shakmaty::Position;
        let start = shakmaty::Chess::default();
        let e4 = crate::game::parse_move(&start, "e2e4").unwrap();
        let mut after = start.clone();
        after.play_unchecked(e4);
        let e5 = crate::game::parse_move(&after, "e7e5").unwrap();
        {
            let mut inner = engine.inner.grab();
            engine.apply_detected_pair(&mut inner, e4, e5, Instant::now());
            let game = inner.game.as_ref().unwrap();
            assert_eq!(game.moves.len(), 2);
            assert_eq!((game.moves[0].uci.as_str(), game.moves[1].uci.as_str()), ("e2e4", "e7e5"));
            assert!(inner.pending_save.is_some(), "the game is marked to be written");
        }
        engine.flush_save().unwrap();
        assert_eq!(engine.list_games().unwrap()[0].moves, 2);

        // A pair that does not follow from the position (the tracker and the game disagree): neither is kept.
        {
            let mut inner = engine.inner.grab();
            engine.apply_detected_pair(&mut inner, e4, e5, Instant::now());
            assert_eq!(inner.game.as_ref().unwrap().moves.len(), 2);
            assert!(inner.notice.as_deref().is_some_and(|n| n.contains("two detected moves were rejected")));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_lock_goes_on_working_after_a_thread_panicked_holding_it() {
        let shared = Arc::new(Mutex::new(5));
        let other = Arc::clone(&shared);
        let _ = thread::spawn(move || {
            let _guard = other.lock().unwrap();
            panic!("a picture could not be processed");
        })
        .join();
        assert!(shared.lock().is_err(), "the lock is poisoned");
        assert_eq!(*shared.grab(), 5);
    }
}
