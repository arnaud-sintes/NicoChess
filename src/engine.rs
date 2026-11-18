//! The heart of the program: ties the camera, the tracker, the game record, the
//! store and the roles together, and tells the pages when something changed.
//!
//! One mutex guards the state that matters (`Inner`). The capture thread takes it
//! once per frame, for a few milliseconds; commands from the pages take it briefly.

use crate::boarddetect;
use crate::calibration::{self, Calibration};
use crate::camera::{FrameRate, SourceFactory};
use crate::choice::CameraSelection;
use crate::depthlink::{AlignOutcome, AlignRequest, DepthLink};
use crate::game::{self, GameRecord};
use crate::picture::{self, Picture};
use crate::record::{RecordOptions, Recorder, SessionMeta};
use crate::roles::{self, Role, Roles};
use crate::store::{Store, Summary};
use crate::tracker::{Candidate, Detection, Doubt, Status, Tracker, TrackerConfig};
use anyhow::Result;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
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
const SIZE_NOTICE_SECOND: &str = "The picture of the second camera is not the size it had when the board was calibrated on it (another camera or resolution): calibrate it again.";

/// How many cameras there can be: the main one, and a second one that is optional.
pub const CAMERAS: usize = 2;
/// A picture to look for the board in: the camera it came from, the picture, and when.
type FollowJob = (usize, Arc<Latest>, Instant);

/// What the tracker looks at: the cameras, and after them the height picture of the depth camera (see `depth`).
const VIEWS: usize = CAMERAS + 1;
const DEPTH_VIEW: usize = CAMERAS;
/// A camera whose last picture is older than this is not working: the detection goes on without it. (Until then
/// the other cameras wait for it to be still: a game that goes on meanwhile may get too far for the tracker, which
/// takes up to three moves at once. People are not that quick; a test with a demo game is, and sets it shorter.)
const FRESH: Duration = Duration::from_secs(2);
/// A camera that has not given a picture yet is waited for this long after a game begins, so that the squares are
/// learned from all the cameras at once (opening a USB camera takes a moment).
const CAMERA_WAIT: Duration = Duration::from_secs(10);

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
    /// Keep a recording of every game for later investigation (see `record`).
    pub record: Option<RecordOptions>,
    /// The camera in use, when the page may choose among the cameras of the computer (see `choice`).
    pub camera: Option<Arc<CameraSelection>>,
    /// The second camera, when this program can have one: the one chosen (empty: none yet).
    pub camera2: Option<Arc<CameraSelection>>,
    /// A second camera that is always there (the demo and the synthetic board, which show the board from the other
    /// side), when `camera2` is not used.
    pub second_fixed: bool,
    /// The depth camera that goes with the main camera, when this program can read one (see `depthlink`).
    pub depth: Option<Arc<DepthLink>>,
    /// How long a camera may be silent before the detection goes on without it.
    pub fresh: Duration,
    /// How many pictures a second the main camera keeps, when the page may change it (a real camera).
    pub rate: Option<Arc<FrameRate>>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            idle_timeout: Duration::from_secs(60),
            game_timeout: Duration::from_secs(30 * 60),
            tracker: TrackerConfig::default(),
            track_interval: Duration::ZERO,
            record: None,
            camera: None,
            camera2: None,
            second_fixed: false,
            depth: None,
            fresh: FRESH,
            rate: None,
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

/// The last moves were taken although the tracker was not sure (see `Doubt`): the page says so, and the person can
/// put it right while those are still the last moves of the game.
#[derive(Clone)]
struct DoubtNote {
    /// How many moves the game had once they were taken, and how many of those they are.
    after: usize,
    count: usize,
    certainty: f32,
    alternatives: Vec<Candidate>,
}

/// Keeping the calibration of one camera on the board when the camera or the board moves a little.
struct Follow {
    /// When the board was last looked for, how many times the calibration was moved, and by how much (in squares)
    /// the last time.
    last: Option<Instant>,
    count: u32,
    shift: f32,
    /// How often the board is looked for: slower on a machine where looking takes long.
    every: Duration,
    /// The calibration when following began: the board may not drift further than a square from it.
    anchor: Option<Calibration>,
    last_save: Option<Instant>,
}

/// Everything the engine keeps about one camera.
struct Cam {
    calibration: Option<Calibration>,
    picture: Picture,
    /// What the camera is, as the source says it ("" while it is off).
    source: String,
    camera_error: Option<String>,
    /// When a picture last came, and the number of the last one given to the tracker.
    last_frame: Option<Instant>,
    last_fed: u64,
    /// The pictures are the size the calibration was made for (or there is no calibration).
    size_ok: bool,
    follow: Follow,
}

impl Cam {
    fn new(calibration: Option<Calibration>, picture: Picture) -> Cam {
        Cam {
            calibration,
            picture,
            source: String::new(),
            camera_error: None,
            last_frame: None,
            last_fed: 0,
            size_ok: true,
            follow: Follow { last: None, count: 0, shift: 0.0, every: FOLLOW_EVERY, anchor: None, last_save: None },
        }
    }
}

struct Inner {
    roles: Roles,
    mode: Mode,
    calibrating_since: Option<Instant>,
    /// The game being recorded, or the last one that finished.
    game: Option<GameRecord>,
    game_started: Option<Instant>,
    /// When play began (a game started or resumed): the cameras that have not given a picture are waited for a while.
    begun_at: Option<Instant>,
    tracker: Tracker,
    candidates: Vec<Candidate>,
    notice: Option<String>,
    last_status: Status,
    /// The main camera and the second one.
    cams: [Cam; CAMERAS],
    last_feed: Option<Instant>,
    /// When a picture was last handed to the recording.
    last_recorded: Option<Instant>,
    /// Which cameras (and the depth) the tracker was told were giving pictures, the last time (for the recording).
    last_active: Option<[bool; VIEWS]>,
    /// The game to write to the disk: set under the lock, written by `flush_save` once the lock is
    /// released (writing and flushing a file can take long on a small machine).
    pending_save: Option<GameRecord>,
    /// The recording of the game being played, when recording is on. Last in the lock order: it only queues
    /// work for its own thread, so it can be used with `inner` held.
    recorder: Option<Recorder>,
    doubt: Option<DoubtNote>,
}

/// Speed figures of the camera pipeline, smoothed.
#[derive(Default)]
struct Stats {
    last_frame: Option<Instant>,
    interval_ms: f32,
    process_ms: f32,
}

/// What belongs to one camera and is not behind the engine's lock: its latest picture, the ways to tell its capture
/// thread what to do, and where its files are.
struct Slot {
    frame: RwLock<Option<Arc<Latest>>>,
    /// A game starts: a scripted source begins again, a real camera does nothing.
    restart: AtomicBool,
    /// Set when another camera was chosen: the capture loop closes the camera and opens the new one at once.
    reopen: AtomicBool,
    /// The way to choose this camera among those of the computer, when this program offers a choice.
    selection: Option<Arc<CameraSelection>>,
    calibration_path: PathBuf,
    picture_path: PathBuf,
    stats: Mutex<Stats>,
}

impl Slot {
    fn new(selection: Option<Arc<CameraSelection>>, calibration_path: PathBuf, picture_path: PathBuf) -> Slot {
        Slot {
            frame: RwLock::new(None),
            restart: AtomicBool::new(false),
            reopen: AtomicBool::new(false),
            selection,
            calibration_path,
            picture_path,
            stats: Mutex::new(Stats::default()),
        }
    }
}

pub struct Engine {
    inner: Mutex<Inner>,
    /// The main camera and the second one.
    slots: [Slot; CAMERAS],
    /// The second camera is always there (see `EngineConfig::second_fixed`).
    second_fixed: bool,
    /// The depth camera, when this program can read one.
    depth: Option<Arc<DepthLink>>,
    camera_wanted: Mutex<bool>,
    camera_cv: Condvar,
    hub: Hub,
    clients: AtomicUsize,
    store: Store,
    /// The least time between two pictures given to the tracker, in microseconds (it follows the rate of the camera).
    track_interval: AtomicU64,
    /// How long a camera may be silent before the detection goes on without it.
    fresh: Duration,
    /// The pictures a second of the main camera, and where the page's choice is kept.
    rate: Option<Arc<FrameRate>>,
    rate_path: PathBuf,
    /// Set when the game has to be written: the saver thread wakes up and writes it, so that neither the camera
    /// thread nor the pages wait for the disk (see `request_save`).
    save_signal: (Mutex<bool>, Condvar),
    saver_started: AtomicBool,
    /// The way to the thread that keeps the calibration on the board when it moves (see `follow_later`).
    follow_tx: Mutex<Option<SyncSender<FollowJob>>>,
    /// Where games are recorded, and the settings of the tracker (kept in the recording).
    record: Option<RecordOptions>,
    tracker_config: TrackerConfig,
    /// Counts the pictures received; the picture streams wait on it.
    frame_count: Mutex<u64>,
    frame_cv: Condvar,
    frame_streams: AtomicUsize,
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

/// The pictures a second that the page set last time (`rate.json` next to the calibration), if it was.
pub fn remembered_fps(calibration_path: &Path) -> Option<f32> {
    let bytes = std::fs::read(calibration_path.with_file_name("rate.json")).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let fps = value["fps"].as_f64()? as f32;
    (1.0..=30.0).contains(&fps).then_some(fps)
}

impl Engine {
    pub fn new(config: EngineConfig, store: Store, calibration_path: PathBuf, calibration: Option<Calibration>) -> Arc<Engine> {
        let tracker_config = config.tracker.clone();
        let mut tracker = Tracker::new(config.tracker);
        tracker.set_view_calibration(0, calibration.as_ref());
        let picture_path = calibration_path.with_file_name("picture.json");
        let rate_path = calibration_path.with_file_name("rate.json");
        // The second camera has its own files next to those of the first.
        let second_calibration_path = calibration_path.with_file_name("calibration2.json");
        let second_picture_path = calibration_path.with_file_name("picture2.json");
        let second_calibration = Calibration::load(&second_calibration_path).unwrap_or_else(|e| {
            eprintln!("calibration (second camera): {e:#}; using none, calibrate again");
            None
        });
        tracker.set_view_calibration(1, second_calibration.as_ref());
        let second_picture = Picture::load(&second_picture_path).unwrap_or_else(|e| {
            eprintln!("picture (second camera): {e:#}; using the defaults");
            Picture::default()
        });
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
                begun_at: None,
                tracker,
                candidates: Vec::new(),
                notice: None,
                last_status: Status::NoCalibration,
                cams: [Cam::new(calibration, picture), Cam::new(second_calibration, second_picture)],
                last_feed: None,
                last_recorded: None,
                last_active: None,
                pending_save: None,
                recorder: None,
                doubt: None,
            }),
            slots: [
                Slot::new(config.camera, calibration_path, picture_path),
                Slot::new(config.camera2, second_calibration_path, second_picture_path),
            ],
            second_fixed: config.second_fixed,
            depth: config.depth,
            camera_wanted: Mutex::new(false),
            camera_cv: Condvar::new(),
            hub: Hub { subscribers: Mutex::new(Vec::new()) },
            clients: AtomicUsize::new(0),
            store,
            track_interval: AtomicU64::new(config.track_interval.as_micros() as u64),
            fresh: config.fresh,
            rate: config.rate,
            rate_path,
            save_signal: (Mutex::new(false), Condvar::new()),
            saver_started: AtomicBool::new(false),
            follow_tx: Mutex::new(None),
            record: config.record,
            tracker_config,
            frame_count: Mutex::new(0),
            frame_cv: Condvar::new(),
            frame_streams: AtomicUsize::new(0),
            save_lock: Mutex::new(()),
        })
    }

    // ---- The cameras ----------------------------------------------------------

    /// This program can have a second camera: one to choose, or the always present synthetic one.
    fn second_offered(&self) -> bool {
        self.second_fixed || self.slots[1].selection.is_some()
    }

    /// The second camera is in use: chosen, or always there.
    fn second_enabled(&self) -> bool {
        self.second_fixed || self.slots[1].selection.as_ref().is_some_and(|s| !s.device().is_empty())
    }

    /// This program has a second camera to talk about (one to choose, or the synthetic one).
    pub fn has_second_camera(&self) -> bool {
        self.second_offered()
    }

    /// The camera takes part: the first always does, the second when it is in use.
    fn cam_enabled(&self, cam: usize) -> bool {
        cam == 0 || self.second_enabled()
    }

    /// Wakes the capture threads: a camera was chosen or switched off. The lock is held while notifying, so that
    /// a thread that has just looked and is about to wait does not miss it.
    fn wake_cameras(&self) {
        let _wanted = self.camera_wanted.grab();
        self.camera_cv.notify_all();
    }

    // ---- Speed ---------------------------------------------------------------

    /// The least time between two pictures given to the tracker: a little less than the time between two pictures of
    /// the camera, so that jitter never skips one.
    fn track_interval(&self) -> Duration {
        Duration::from_micros(self.track_interval.load(Ordering::Relaxed))
    }

    /// Sets how many pictures a second are kept from the camera (the slider of the page), when this program can: a
    /// real camera. It is used at once, and kept for the next start.
    pub fn set_fps(&self, token: &str, fps: f32) -> ApiResult {
        let Some(rate) = &self.rate else { return Err(ApiError::Conflict("the pictures of this source come at a fixed pace".into())) };
        if !(1.0..=30.0).contains(&fps) {
            return Err(ApiError::BadRequest("pictures per second: from 1 to 30".into()));
        }
        drop(self.controlled(token)?);
        rate.set(fps);
        self.track_interval.store((0.8e6 / fps) as u64, Ordering::Relaxed);
        // Not being able to keep the choice is no reason to refuse it.
        if let Err(e) = crate::fsutil::write_atomic(&self.rate_path, json!({ "fps": fps }).to_string().as_bytes()) {
            eprintln!("camera: cannot keep the rate: {e:#}");
        }
        self.hub.notify();
        Ok(())
    }

    /// The game is marked to be written: wakes the saver thread, which writes it. Neither the camera thread nor the
    /// pages wait for the disk, which on a busy or synchronised folder can take a second.
    fn request_save(&self) {
        let (flag, wake) = &self.save_signal;
        *flag.grab() = true;
        wake.notify_one();
    }

    /// Starts the thread that looks for the board in a picture, to keep the calibration on it when it moves a little.
    /// That takes a while (hundreds of milliseconds on a small machine), and writes the calibration file; the thread
    /// that reads the camera must not wait for either, or the next pictures are late (and stale).
    fn spawn_follower(self: &Arc<Self>) {
        let mut slot = self.follow_tx.grab();
        if slot.is_some() {
            return;
        }
        // One picture waits at most: one that comes while the board is being looked for is not worth looking at later.
        let (tx, rx) = sync_channel::<FollowJob>(1);
        let engine = Arc::clone(self);
        let started = thread::Builder::new().name("follow".into()).spawn(move || {
            for (cam, latest, now) in rx {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.follow_board(cam, &latest, now))).is_err() {
                    eprintln!("follow: the board could not be looked for (internal error); going on");
                }
            }
        });
        if started.is_ok() {
            *slot = Some(tx);
        }
    }

    /// Hands a picture to the thread that follows the board. With no such thread (an engine that was never given a
    /// camera, as in some tests) the board is looked for here.
    fn follow_later(&self, cam: usize, latest: &Arc<Latest>, now: Instant) {
        let tx = self.follow_tx.grab().clone();
        match tx {
            Some(tx) => {
                let _ = tx.try_send((cam, Arc::clone(latest), now));
            }
            None => self.follow_board(cam, latest, now),
        }
    }

    fn spawn_saver(self: &Arc<Self>) {
        if self.saver_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let engine = Arc::clone(self);
        let _ = thread::Builder::new().name("saver".into()).spawn(move || {
            loop {
                {
                    let (flag, wake) = &engine.save_signal;
                    let mut wanted = flag.grab();
                    while !*wanted {
                        wanted = wake.wait(wanted).unwrap_or_else(PoisonError::into_inner);
                    }
                    *wanted = false;
                }
                // A failure is reported to the pages by `flush_save` itself.
                let _ = engine.flush_save();
            }
        });
    }

    // ---- Camera thread ------------------------------------------------------

    /// Runs the capture loop on its own thread: the camera is opened when someone
    /// needs it and closed when nobody does.
    pub fn spawn_capture(self: &Arc<Self>, factory: SourceFactory) -> std::io::Result<()> {
        self.spawn_capture_of(0, factory)
    }

    /// The same for the second camera. Its thread waits until the camera is in use (chosen, or always there).
    pub fn spawn_capture_second(self: &Arc<Self>, factory: SourceFactory) -> std::io::Result<()> {
        self.spawn_capture_of(1, factory)
    }

    fn spawn_capture_of(self: &Arc<Self>, cam: usize, factory: SourceFactory) -> std::io::Result<()> {
        self.spawn_saver();
        self.spawn_follower();
        let engine = Arc::clone(self);
        let name = if cam == 0 { "capture" } else { "capture2" };
        thread::Builder::new().name(name.into()).spawn(move || engine.capture_loop(cam, factory)).map(|_| ())
    }

    /// Runs the depth thread, when this program can read a depth camera: it reads the depth part of the main camera
    /// whenever the cameras are on and the box "use the depth" is ticked.
    pub fn spawn_depth(self: &Arc<Self>) -> std::io::Result<()> {
        let Some(link) = self.depth.clone() else { return Ok(()) };
        let engine = Arc::clone(self);
        thread::Builder::new().name("depth".into()).spawn(move || engine.depth_loop(&link)).map(|_| ())
    }

    /// Looks whether the main camera in use has a depth part, and tells the depth link. Looking for the cameras takes a
    /// moment: not under the lock.
    fn refresh_depth_offer(&self) {
        let Some(link) = &self.depth else { return };
        let Some(selection) = &self.slots[0].selection else { return };
        let offer = selection.list().ok().and_then(|cameras| crate::depthlink::offer_for(&cameras, &selection.device()));
        link.set_offer(offer);
    }

    fn depth_loop(&self, link: &DepthLink) {
        self.refresh_depth_offer();
        loop {
            {
                let mut wanted = self.camera_wanted.grab();
                while !(*wanted && link.wanted()) {
                    wanted = self.camera_cv.wait(wanted).unwrap_or_else(PoisonError::into_inner);
                }
            }
            let Some(name) = link.offer() else { continue };
            // Another mode asked for (a size, a speed) ends the reading, and the camera is opened again at once.
            let generation = link.generation();
            match link.open(&name) {
                Ok(mut source) => {
                    link.set_error(None);
                    link.set_source(source.describe());
                    link.set_modes(source.modes());
                    self.hub.notify();
                    while self.camera_is_wanted()
                        && link.wanted()
                        && link.offer().as_deref() == Some(name.as_str())
                        && link.generation() == generation
                    {
                        match source.next_frame() {
                            Ok(frame) => link.publish(&frame),
                            Err(e) => {
                                eprintln!("depth: {e:#}");
                                link.set_error(Some(format!("{e:#}")));
                                break;
                            }
                        }
                        // The squares are looked for once there is a picture to look in (it takes a moment).
                        if link.fresh().is_some()
                            && let Some(request) = link.take_request()
                        {
                            let outcome = link.align(&request);
                            link.publish_outcome(outcome);
                            self.hub.notify();
                        }
                    }
                }
                Err(e) => {
                    eprintln!("depth: {e:#}");
                    link.set_error(Some(format!("{e:#}")));
                }
            }
            link.set_source(String::new());
            self.hub.notify();
            if self.camera_is_wanted() && link.wanted() && link.generation() == generation {
                thread::sleep(CAMERA_RETRY);
            }
        }
    }

    /// The squares are to be looked for in the depth picture, with the board in `position` (what is on it tells the
    /// tall squares from the others). False when there is nothing to do: no depth, or no colour calibration yet.
    fn ask_depth_alignment(&self, inner: &Inner, position: &shakmaty::Chess) -> bool {
        let Some(link) = &self.depth else { return false };
        if !link.wanted() {
            return false;
        }
        let Some(colour) = inner.cams[0].calibration else { return false };
        let size =
            colour.size.or_else(|| self.slots[0].frame.read_ok().as_ref().map(|f| [f.image.width(), f.image.height()])).unwrap_or([0, 0]);
        if size[0] == 0 {
            return false;
        }
        let grid = crate::tracker::expected_grid(position);
        link.ask_to_align(AlignRequest {
            colour,
            colour_size: (size[0], size[1]),
            rotation: inner.cams[0].picture.rotation,
            occupied: grid.map(|cell| cell != crate::vision::Cell::Empty),
        });
        true
    }

    /// Puts the result of an alignment to use: the tracker looks at the squares where they were found.
    fn apply_depth_outcome(inner: &mut Inner, outcome: &AlignOutcome) {
        match outcome.calibration {
            Some(calibration) => {
                inner.tracker.set_view_advisory(DEPTH_VIEW, true);
                inner.tracker.set_view_calibration(DEPTH_VIEW, Some(&calibration));
                Self::record(inner, "calibration", json!({"cam": DEPTH_VIEW, "calibration": calibration, "how": "depth"}));
            }
            None => inner.tracker.set_view_calibration(DEPTH_VIEW, None),
        }
    }

    fn capture_loop(&self, cam: usize, factory: SourceFactory) {
        let slot = &self.slots[cam];
        loop {
            {
                let mut wanted = self.camera_wanted.grab();
                while !(*wanted && self.cam_enabled(cam)) {
                    wanted = self.camera_cv.wait(wanted).unwrap_or_else(PoisonError::into_inner);
                }
            }
            // A choice of camera made while it was off needs no reopening: it is opened now.
            slot.reopen.store(false, Ordering::SeqCst);
            let mut reopening = false;
            match factory() {
                Ok(mut source) => {
                    self.set_source(cam, source.describe());
                    while self.camera_is_wanted() && self.cam_enabled(cam) {
                        if slot.reopen.swap(false, Ordering::SeqCst) {
                            reopening = true;
                            break;
                        }
                        if slot.restart.swap(false, Ordering::SeqCst) {
                            source.restart();
                        }
                        match source.next_frame() {
                            Ok(image) => {
                                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.on_frame(cam, image))).is_err() {
                                    eprintln!("camera: a picture could not be processed (internal error); going on");
                                }
                            }
                            Err(e) => {
                                self.camera_failed(cam, format!("{e:#}"));
                                break;
                            }
                        }
                    }
                    // Dropping the source closes the camera.
                }
                Err(e) => self.camera_failed(cam, format!("{e:#}")),
            }
            *slot.frame.write_ok() = None;
            self.set_source(cam, String::new());
            if self.camera_is_wanted() && self.cam_enabled(cam) && !reopening {
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
        for cam in &mut inner.cams {
            cam.camera_error = None;
        }
        self.set_camera(false);
    }

    fn set_source(&self, cam: usize, description: String) {
        {
            let mut inner = self.inner.grab();
            // The pictures of a camera that was opened (or closed) are numbered from 1 again.
            inner.cams[cam].last_fed = 0;
            // A camera that is off has no pictures to be the wrong size.
            if description.is_empty() {
                inner.cams[cam].size_ok = true;
                Self::size_notice(&mut inner);
            }
            inner.cams[cam].source = description;
        }
        self.hub.notify();
    }

    fn camera_failed(&self, cam: usize, message: String) {
        if cam == 0 {
            eprintln!("camera: {message}");
        } else {
            eprintln!("camera 2: {message}");
        }
        self.inner.grab().cams[cam].camera_error = Some(message);
        self.hub.notify();
    }

    /// The notice about a camera whose pictures are not the size its calibration was made for: set while there is
    /// one, taken away when there is none. True when it changed.
    fn size_notice(inner: &mut Inner) -> bool {
        let wanted = if !inner.cams[0].size_ok {
            Some(SIZE_NOTICE)
        } else if !inner.cams[1].size_ok {
            Some(SIZE_NOTICE_SECOND)
        } else {
            None
        };
        match wanted {
            Some(text) if inner.notice.as_deref() != Some(text) => {
                inner.notice = Some(text.to_string());
                true
            }
            None if inner.notice.as_deref().is_some_and(|n| n == SIZE_NOTICE || n == SIZE_NOTICE_SECOND) => {
                inner.notice = None;
                true
            }
            _ => false,
        }
    }

    /// Handles one camera frame.
    fn on_frame(&self, cam: usize, image: RgbImage) {
        let started = Instant::now();
        let slot = &self.slots[cam];
        let seq = slot.frame.read_ok().as_ref().map_or(1, |l| l.seq + 1);
        let picture = self.inner.grab().cams[cam].picture;
        let raw = picture.rotate(image);
        let adjusted = picture.adjust(&raw);
        let latest = Arc::new(Latest::new(raw, adjusted, seq));
        *slot.frame.write_ok() = Some(Arc::clone(&latest));
        {
            *self.frame_count.grab() += 1;
            self.frame_cv.notify_all();
        }

        let now = Instant::now();
        // The height picture of the depth camera, for the tracker, made before the lock is taken (finding the plane of
        // the board takes a few milliseconds): only when the tracker is about to look.
        let depth_given: Option<(u64, RgbImage)> = self.depth.as_ref().and_then(|link| {
            if !link.wanted() || !link.aligned() {
                return None;
            }
            let (due, rotation) = {
                let guard = self.inner.grab();
                let due = guard.mode == Mode::Playing && guard.last_feed.is_none_or(|t| now.duration_since(t) >= self.track_interval());
                (due, guard.cams[0].picture.rotation)
            };
            if due { link.height_picture(rotation, true) } else { None }
        });
        let mut guard = self.inner.grab();
        let inner = &mut *guard;
        let mut changed = inner.cams[cam].camera_error.take().is_some();
        inner.cams[cam].last_frame = Some(now);
        // Time-based clean-up also runs with no page open (the camera must not stay on for ever).
        changed |= self.housekeeping(inner, now);
        // The depth camera found the squares: the tracker looks at them there from now on.
        if let Some(outcome) = self.depth.as_ref().and_then(|link| link.take_outcome()) {
            Self::apply_depth_outcome(inner, &outcome);
            changed = true;
        }

        // A calibration is only good for the picture size it was made for.
        let size = [latest.image.width(), latest.image.height()];
        let mut size_ok = true;
        if let Some(calibration) = inner.cams[cam].calibration.as_mut() {
            match calibration.size {
                None => calibration.size = Some(size),
                Some(known) if known != size => size_ok = false,
                Some(_) => {}
            }
        }
        inner.cams[cam].size_ok = size_ok;
        changed |= Self::size_notice(inner);

        let feed_due = inner.last_feed.is_none_or(|t| now.duration_since(t) >= self.track_interval());
        // The new pictures of the cameras that take part, for the tracker: one for each camera that has a calibration,
        // gives pictures of that size, and has not been looked at yet.
        let mut fed: Vec<(usize, Arc<Latest>)> = Vec::new();
        if inner.mode == Mode::Playing && feed_due {
            let mut active = [false; VIEWS];
            // Several arrays are indexed by the camera together.
            #[allow(clippy::needless_range_loop)]
            for c in 0..CAMERAS {
                let usable = inner.cams[c].calibration.is_some() && inner.cams[c].size_ok && self.cam_enabled(c);
                let fresh = inner.cams[c].last_frame.is_some_and(|t| now.duration_since(t) < self.fresh);
                // A camera that has gone quiet is left out, so that the others are not held up; one that has not
                // given a picture since play began is waited for a little, so that all learn the squares together.
                let waiting =
                    inner.begun_at.is_some_and(|b| now.duration_since(b) < CAMERA_WAIT && inner.cams[c].last_frame.is_none_or(|t| t < b));
                active[c] = usable && (fresh || waiting);
                inner.tracker.set_view_active(c, active[c]);
                if !usable || !fresh {
                    continue;
                }
                let frame = if c == cam { Some(Arc::clone(&latest)) } else { self.slots[c].frame.read_ok().clone() };
                if let Some(frame) = frame {
                    fed.push((c, frame));
                }
            }
            // The depth camera takes part when it is wanted, has found the squares, and gives pictures.
            if let Some(link) = &self.depth {
                active[DEPTH_VIEW] = link.wanted() && link.aligned() && link.fresh().is_some();
                inner.tracker.set_view_active(DEPTH_VIEW, active[DEPTH_VIEW]);
            }
            // A camera is not given the same picture twice: a repeat would count as one more still picture.
            fed.retain(|(c, frame)| frame.seq != inner.cams[*c].last_fed);
            // The recording says which cameras took part, so that a replay does the same (see `replay`).
            if inner.last_active != Some(active) && !fed.is_empty() {
                inner.last_active = Some(active);
                Self::record(inner, "views", json!({"active": active}));
            }
        }
        if !fed.is_empty() {
            inner.last_feed = Some(now);
            let mut images: Vec<Option<&RgbImage>> = vec![None; VIEWS];
            for (c, frame) in &fed {
                images[*c] = Some(frame.image.as_ref());
                inner.cams[*c].last_fed = frame.seq;
            }
            if let Some((seq, picture)) = &depth_given {
                images[DEPTH_VIEW] = Some(picture);
                if let Some(link) = &self.depth {
                    link.mark_fed(*seq);
                }
            }
            let detection = inner.tracker.feed_views(&images, now);
            // What the recording will say about this picture (see the module record).
            let mut noted: Option<(&'static str, Value)> = None;
            let found = matches!(detection, Detection::Move { .. } | Detection::Sequence { .. });
            match detection {
                Detection::Nothing => {
                    if inner.tracker.status() != Status::Unsure && !inner.candidates.is_empty() {
                        inner.candidates.clear();
                        changed = true;
                    }
                }
                Detection::Unsure { candidates } => {
                    // Tell the pages only when the list changed: this comes at every picture while unsure.
                    if !inner.candidates.iter().map(|c| &c.uci).eq(candidates.iter().map(|c| &c.uci)) {
                        noted = Some(("unsure", json!({"candidates": candidates_json(&candidates)})));
                        inner.candidates = candidates;
                        changed = true;
                    }
                }
                Detection::Sequence { moves, since } => {
                    inner.candidates.clear();
                    let doubt = inner.tracker.take_doubt();
                    let before = inner.game.as_ref().map_or(0, |g| g.moves.len());
                    self.apply_detected_moves(inner, &moves, since);
                    noted = Some(detection_event(inner, before, moves.len(), doubt.as_ref()));
                    remember_doubt(inner, doubt, before, moves.len());
                    changed = true;
                }
                Detection::Move { mv, since, promotion_guess } => {
                    inner.candidates.clear();
                    let doubt = inner.tracker.take_doubt();
                    let before = inner.game.as_ref().map_or(0, |g| g.moves.len());
                    self.apply_detected_move(inner, mv, since, promotion_guess);
                    noted = Some(detection_event(inner, before, 1, doubt.as_ref()));
                    remember_doubt(inner, doubt, before, 1);
                    changed = true;
                }
            }
            let status = inner.tracker.status();
            if status != inner.last_status {
                inner.last_status = status;
                changed = true;
            }
            // The picture comes before what it led to, so that the file reads in order. Every picture is kept
            // while something is going on; only now and then while the board is quiet.
            // At 25 pictures a second the recording keeps a few a second, not all (a replay takes the time from the file).
            let gap = self.record.as_ref().map_or(Duration::ZERO, |r| r.busy_every);
            let keep_picture = found || inner.last_recorded.is_none_or(|t| now.duration_since(t) >= gap);
            if let Some(recorder) = inner.recorder.as_mut() {
                if keep_picture {
                    let mut pictures: Vec<(usize, Arc<RgbImage>)> = fed.iter().map(|(c, frame)| (*c, Arc::clone(&frame.image))).collect();
                    if let Some((_, picture)) = &depth_given {
                        pictures.push((DEPTH_VIEW, Arc::new(picture.clone())));
                    }
                    recorder.frames(now, &pictures, status_name(status), found || status != Status::Watching);
                    inner.last_recorded = Some(now);
                }
                if let Some((kind, data)) = noted {
                    recorder.event(now, kind, data);
                }
            }
            self.close_recording_if_idle(inner);
        }
        let save_due = inner.pending_save.is_some();
        drop(guard);
        // The pages are told first; the game is written by the saver thread, whose failure becomes the notice of the pages.
        if changed {
            self.hub.notify();
        }
        if save_due {
            self.request_save();
        }
        self.follow_later(cam, &latest, now);
        self.note_speed(cam, started);
    }

    /// Keeps the figures shown under the camera picture: pictures per second, and the time
    /// one picture takes here.
    fn note_speed(&self, cam: usize, started: Instant) {
        let mut stats = self.slots[cam].stats.grab();
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
    pub fn camera_stats(&self, cam: usize) -> Value {
        let stats = self.slots[cam.min(CAMERAS - 1)].stats.grab();
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
    fn follow_board(&self, cam: usize, latest: &Latest, now: Instant) {
        let size = [latest.image.width(), latest.image.height()];
        let calibration = {
            let mut inner = self.inner.grab();
            // The switch is one for both cameras: it is kept with the first.
            if inner.mode != Mode::Playing || !inner.cams[0].picture.follow {
                return;
            }
            if inner.cams[cam].follow.last.is_some_and(|t| now.duration_since(t) < inner.cams[cam].follow.every) {
                return;
            }
            // Only while the board is quiet: no hand, no move being made.
            if !matches!(inner.tracker.status(), Status::Watching | Status::Learning) {
                return;
            }
            inner.cams[cam].follow.last = Some(now);
            match inner.cams[cam].calibration {
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
        inner.cams[cam].follow.every = FOLLOW_EVERY.max(spent * 20);
        let Ok(found) = found else { return };
        let moved = Calibration::for_frame(found.corners, size[0], size[1]);
        let shift = boarddetect::shift_in_squares(&calibration, &moved);
        if !(FOLLOW_MIN_SHIFT..=FOLLOW_MAX_SHIFT).contains(&shift) || moved.validate(size[0], size[1]).is_err() {
            return;
        }
        // Things may have changed while looking.
        if inner.mode != Mode::Playing || inner.cams[cam].calibration != Some(calibration) {
            return;
        }
        let anchor = *inner.cams[cam].follow.anchor.get_or_insert(calibration);
        if boarddetect::shift_in_squares(&anchor, &moved) > FOLLOW_MAX_DRIFT {
            eprintln!("follow: the board is more than a square from where the game began; not following (calibrate again)");
            return;
        }
        match inner.tracker.status() {
            // Nothing learned yet: learn from the better grid.
            Status::Learning => inner.tracker.set_view_calibration(cam, Some(&moved)),
            Status::Watching => inner.tracker.move_view_calibration(cam, &moved),
            _ => return,
        }
        inner.cams[cam].calibration = Some(moved);
        Self::record(&inner, "calibration", json!({"how": "follow", "cam": cam, "calibration": moved}));
        let save_due = inner.cams[cam].follow.last_save.is_none_or(|t| now.duration_since(t) >= FOLLOW_SAVE_EVERY);
        if save_due {
            inner.cams[cam].follow.last_save = Some(now);
        }
        inner.cams[cam].follow.count += 1;
        inner.cams[cam].follow.shift = shift;
        eprintln!("follow: the board moved by {shift:.2} squares in camera {}, calibration moved along", cam + 1);
        drop(inner);
        if save_due && let Err(e) = moved.save(&self.slots[cam].calibration_path) {
            eprintln!("calibration: cannot save the moved board: {e:#}");
        }
        self.hub.notify();
    }

    /// The time of a move that was seen at `since`: seconds since the first move of the game. The clock starts with the
    /// first move, not when the game was started on the page (the players may take a while to begin), so the first move
    /// is at 0 and starts the clock.
    fn move_time(inner: &mut Inner, since: Instant) -> f64 {
        if inner.game.as_ref().is_some_and(|g| g.moves.is_empty()) {
            inner.game_started = Some(since);
        }
        since.saturating_duration_since(inner.game_started.unwrap_or(since)).as_secs_f64()
    }

    fn apply_detected_move(&self, inner: &mut Inner, mv: shakmaty::Move, since: Instant, guess: bool) {
        let t = Self::move_time(inner, since);
        let Some(game) = inner.game.as_mut() else { return };
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

    /// Two or three moves were recognised at once: all are recorded, at the time the board became still.
    fn apply_detected_moves(&self, inner: &mut Inner, moves: &[shakmaty::Move], since: Instant) {
        let t = Self::move_time(inner, since);
        let Some(game) = inner.game.as_mut() else { return };
        let before = game.moves.len();
        for (i, mv) in moves.iter().copied().enumerate() {
            match game.push_move(mv, t, mv.promotion().is_some()) {
                Ok(record) => eprintln!("game: {} {} ({} moves at once)", record.san, record.uci, moves.len()),
                Err(e) => {
                    // The tracker and the game disagree: trust the game, and keep neither move.
                    while game.moves.len() > before {
                        game.undo();
                    }
                    inner.notice = Some(format!("the moves detected at once were rejected ({e:#}); position kept"));
                    if let Ok(pos) = game.position() {
                        inner.tracker.set_position(pos);
                    }
                    return;
                }
            }
            // The moves after the first were seen in the same picture: the page shows them together.
            if i > 0
                && let Some(last) = game.moves.last_mut()
            {
                last.together = true;
            }
        }
        self.after_game_change(inner);
    }

    /// Saves the game and ends it if the rules say so.
    fn after_game_change(&self, inner: &mut Inner) {
        // Whatever changed the game, the doubt was about moves that are no longer the last ones.
        inner.doubt = None;
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

    /// The latest picture of a camera (0 is the main one).
    pub fn latest_frame(&self, cam: usize) -> Option<Arc<Latest>> {
        self.slots.get(cam)?.frame.read_ok().clone()
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
    pub fn wait_for_frame(&self, cam: usize, last: Option<&Arc<Latest>>, timeout: Duration) -> Option<Arc<Latest>> {
        let deadline = Instant::now() + timeout;
        let mut count = self.frame_count.grab();
        loop {
            if let Some(frame) = self.latest_frame(cam)
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
    pub fn warped_jpeg(&self, cam: usize, with_grid: bool) -> ApiResult<Vec<u8>> {
        let latest = self.latest_frame(cam).ok_or_else(|| ApiError::Conflict("the camera is off".into()))?;
        self.warped_jpeg_of(cam, &latest, with_grid)
    }

    /// The top-down board from one given camera frame.
    pub fn warped_jpeg_of(&self, cam: usize, latest: &Latest, with_grid: bool) -> ApiResult<Vec<u8>> {
        let calibration =
            self.inner.grab().cams.get(cam).and_then(|c| c.calibration).ok_or_else(|| ApiError::Conflict("not calibrated yet".into()))?;
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
        let frame = self.slots[0].frame.read_ok().clone();

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
                "moves": g.moves.iter().map(|m| json!({"san": m.san, "uci": m.uci, "t": m.t, "guess": m.promotion_guess, "together": m.together})).collect::<Vec<_>>(),
                // Seconds since the first move while the game is going on: the page runs the clock of the player to move.
                "elapsed": inner.game_started.filter(|_| playing && !g.moves.is_empty()).map(|s| s.elapsed().as_secs_f64()),
                "result": g.result,
                "termination": g.termination,
                "over": g.result.is_some(),
                "draw_claim": g.claimable_draw().ok().flatten(),
            })
        });
        let promotion_pending = inner.game.as_ref().and_then(|g| g.moves.last()).is_some_and(|m| m.promotion_guess);
        // The cameras the tracker uses (the depth view is told apart, after them).
        let views = inner.tracker.views_in_use();
        let depth = self.depth.as_ref().map(|link| {
            let status = link.status();
            json!({
                "offered": link.offer().is_some(),
                "name": link.offer(),
                "on": link.on(),
                "source": status.source,
                "error": status.error,
                "aligned": link.aligned(),
                "score": (status.score * 100.0).round() / 100.0,
                "message": status.message,
                "in_use": views.get(DEPTH_VIEW).copied().unwrap_or(false),
                "ignored": inner.tracker.depth_ignored(),
                "settings": link.settings(),
                "modes": link.modes(),
                "valid": (status.valid * 100.0).round() / 100.0,
            })
        });

        json!({
            "version": crate::version_text(),
            "role": if role == Role::Controller { "controller" } else { "viewer" },
            "can_claim": inner.roles.is_free(now, playing),
            "mode": inner.mode.name(),
            "camera": {
                "on": frame.is_some() || inner.mode != Mode::Idle,
                "source": inner.cams[0].source,
                "choice": self.slots[0].selection.is_some(),
                "error": inner.cams[0].camera_error,
                "width": frame.as_ref().map_or(0, |f| f.image.width()),
                "height": frame.as_ref().map_or(0, |f| f.image.height()),
            },
            "calibration": inner.cams[0].calibration.map(|c| c.corners),
            "picture": picture_json(&inner.cams[0].picture),
            // Pictures a second asked of the camera, when the page may change it.
            "rate": self.rate.as_ref().map(|r| json!({"fps": r.fps()})),
            // The second camera, when this program can have one: whether it is in use, and what the first camera has
            // above (picture, calibration) for it.
            "second": if self.second_offered() {
                let second = self.slots[1].frame.read_ok().clone();
                json!({
                    "enabled": self.second_enabled(),
                    "on": self.second_enabled() && (second.is_some() || inner.mode != Mode::Idle),
                    "source": inner.cams[1].source,
                    "choice": self.slots[1].selection.is_some(),
                    "error": inner.cams[1].camera_error,
                    "width": second.as_ref().map_or(0, |f| f.image.width()),
                    "height": second.as_ref().map_or(0, |f| f.image.height()),
                    "calibration": inner.cams[1].calibration.map(|c| c.corners),
                    "picture": picture_json(&inner.cams[1].picture),
                })
            } else {
                Value::Null
            },
            "depth": depth,
            "stabilize": {
                "on": inner.cams[0].picture.follow,
                "adjusted": inner.cams.iter().map(|c| c.follow.count).sum::<u32>(),
                "last_shift": (inner.cams.iter().map(|c| c.follow.shift).fold(0.0f32, f32::max) * 100.0).round() / 100.0,
            },
            "tracker": {
                "status": status_name(inner.tracker.status()),
                "views": views.iter().take(CAMERAS).collect::<Vec<_>>(),
                "candidates": candidates_json(&inner.candidates),
                "doubt": doubt_json(inner),
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

    /// A camera number from a request: 0, or 1 when there is a second camera to talk about.
    fn check_cam(&self, cam: usize) -> ApiResult {
        if cam >= CAMERAS || (cam == 1 && !self.second_offered()) {
            return Err(ApiError::BadRequest("there is no such camera".into()));
        }
        Ok(())
    }

    pub fn set_calibration(&self, token: &str, cam: usize, corners: [[f32; 2]; 4]) -> ApiResult {
        self.check_cam(cam)?;
        let mut inner = self.controlled(token)?;
        let frame = self.latest_frame(cam).ok_or_else(|| ApiError::Conflict("the camera is off: open the calibration first".into()))?;
        let calibration = Calibration::for_frame(corners, frame.image.width(), frame.image.height());
        calibration.validate(frame.image.width(), frame.image.height())?;
        calibration.save(&self.slots[cam].calibration_path).map_err(ApiError::internal)?;
        inner.cams[cam].calibration = Some(calibration);
        inner.cams[cam].follow.anchor = None;
        inner.cams[cam].size_ok = true;
        Self::record(&inner, "calibration", json!({"how": "set", "cam": cam, "calibration": calibration}));
        // During a game the squares are learned again from the position reached.
        inner.tracker.set_view_calibration(cam, Some(&calibration));
        // Any notice is old news now; a camera that is still the wrong size says so again.
        inner.notice = None;
        Self::size_notice(&mut inner);
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Finds the board in the picture by itself and calibrates with it.
    /// Returns a sentence for the page: what was found, and what to check.
    pub fn detect_calibration(&self, token: &str, cam: usize) -> ApiResult<String> {
        self.check_cam(cam)?;
        drop(self.controlled(token)?);
        let frame = self.latest_frame(cam).ok_or_else(|| ApiError::Conflict("the camera is off: open the calibration first".into()))?;
        // Without the lock: searching takes a fraction of a second.
        let found = boarddetect::find_board(&frame.image).map_err(|e| ApiError::Conflict(format!("{e:#}")))?;
        self.set_calibration(token, cam, found.corners)?;
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
        // One switch for both cameras, kept with the first.
        let settings = Picture { follow: on, ..inner.cams[0].picture };
        settings.save(&self.slots[0].picture_path).map_err(ApiError::internal)?;
        inner.cams[0].picture = settings;
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
    fn store_picture(&self, inner: &mut Inner, cam: usize, picture: Picture, turns: i32) -> ApiResult {
        picture.validate()?;
        picture.save(&self.slots[cam].picture_path).map_err(ApiError::internal)?;
        inner.cams[cam].picture = picture;
        // The next picture will have the new settings. Redo the one on show too, so the
        // page does not have to wait for it.
        if let Some(latest) = self.latest_frame(cam) {
            let raw = Picture::default().turned(turns).rotate((*latest.raw).clone());
            let adjusted = picture.adjust(&raw);
            *self.slots[cam].frame.write_ok() = Some(Arc::new(Latest::new(raw, adjusted, latest.seq)));
            *self.frame_count.grab() += 1;
            self.frame_cv.notify_all();
        }
        Ok(())
    }

    /// Turns the picture of a camera by `turns` quarter turns clockwise (negative: counter-clockwise).
    /// The calibration is turned with it, so it stays valid.
    pub fn rotate_picture(&self, token: &str, cam: usize, turns: i32) -> ApiResult {
        self.check_cam(cam)?;
        let mut inner = self.picture_guard(token)?;
        let turns = turns.rem_euclid(4);
        if turns == 0 {
            return Ok(());
        }
        let mut turned_calibration = None;
        if let Some(calibration) = inner.cams[cam].calibration {
            let frame = self.latest_frame(cam).ok_or_else(|| {
                ApiError::Conflict("the camera is off: open it first, so the calibration can be turned with the picture".into())
            })?;
            let (corners, width, height) = picture::turn_points(&calibration.corners, frame.raw.width(), frame.raw.height(), turns);
            let turned = Calibration::for_frame(corners, width, height);
            turned.validate(width, height)?;
            turned_calibration = Some(turned);
        }
        let settings = inner.cams[cam].picture.turned(turns);
        if let Some(turned) = turned_calibration {
            turned.save(&self.slots[cam].calibration_path).map_err(ApiError::internal)?;
            inner.cams[cam].calibration = Some(turned);
            inner.cams[cam].follow.anchor = None;
            inner.tracker.set_view_calibration(cam, Some(&turned));
        }
        self.store_picture(&mut inner, cam, settings, turns)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Sets brightness and contrast of a camera.
    pub fn set_picture_adjust(&self, token: &str, cam: usize, brightness: f32, contrast: f32) -> ApiResult {
        self.check_cam(cam)?;
        let mut inner = self.picture_guard(token)?;
        let settings = Picture { brightness, contrast, ..inner.cams[cam].picture };
        self.store_picture(&mut inner, cam, settings, 0)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Stretches the picture so that the board (or the whole picture, if the board is
    /// not calibrated yet) uses most of the range from dark to light.
    pub fn auto_contrast(&self, token: &str, cam: usize) -> ApiResult {
        self.check_cam(cam)?;
        let mut inner = self.picture_guard(token)?;
        let frame = self.latest_frame(cam).ok_or_else(|| ApiError::Conflict("the camera is off: open it first".into()))?;
        let measured = match inner.cams[cam].calibration {
            Some(calibration) => calibration::warp_board(&frame.raw, &calibration)?,
            None => (*frame.raw).clone(),
        };
        let (contrast, brightness) = Picture::auto_contrast(&measured)?;
        let settings = Picture { brightness, contrast, ..inner.cams[cam].picture };
        self.store_picture(&mut inner, cam, settings, 0)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Back to the picture as the camera gives it (the rotation stays).
    pub fn reset_picture_adjust(&self, token: &str, cam: usize) -> ApiResult {
        self.set_picture_adjust(token, cam, 0.0, 1.0)
    }

    pub fn clear_calibration(&self, token: &str, cam: usize) -> ApiResult {
        self.check_cam(cam)?;
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is running".into()));
        }
        self.forget_calibration(&mut inner, cam)?;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    fn forget_calibration(&self, inner: &mut Inner, cam: usize) -> ApiResult {
        match std::fs::remove_file(&self.slots[cam].calibration_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ApiError::Internal(format!("cannot delete the calibration file: {e}"))),
        }
        inner.cams[cam].calibration = None;
        inner.cams[cam].follow.anchor = None;
        inner.cams[cam].size_ok = true;
        inner.tracker.set_view_calibration(cam, None);
        Self::size_notice(inner);
        Ok(())
    }

    // ---- Choice of camera --------------------------------------------------------

    /// The cameras of the computer, and the one in use, for the page. `selectable` is false when this program
    /// has no choice to offer (another system, another source).
    pub fn cameras(&self) -> Value {
        // The second camera: whether there is one to have, which one is chosen ("" is none), and whether the page
        // may choose (a synthetic second camera is always there).
        let second = if self.second_offered() {
            json!({
                "choice": self.slots[1].selection.is_some(),
                "selected": self.slots[1].selection.as_ref().map_or_else(String::new, |s| s.device()),
                "fixed": self.second_fixed,
            })
        } else {
            Value::Null
        };
        let Some(selection) = &self.slots[0].selection else {
            return json!({"selectable": false, "cameras": [], "selected": null, "second": second});
        };
        match selection.list() {
            Ok(all) => {
                // A camera with a depth part is one camera for the page, with a box for the depth.
                let cameras = if let Some(link) = &self.depth {
                    link.set_offer(crate::depthlink::offer_for(&all, &selection.device()));
                    crate::depthlink::without_depth_parts(all)
                } else {
                    all
                };
                json!({"selectable": true, "cameras": cameras, "selected": selection.device(), "second": second})
            }
            Err(e) => {
                json!({"selectable": true, "cameras": [], "selected": selection.device(), "second": second, "error": format!("{e:#}")})
            }
        }
    }

    /// Uses another camera as the main one (`cam` 0) or as the second one (`cam` 1; an empty `id` switches the second
    /// camera off). It is opened at once if the cameras are on, and chosen again at the next start. The calibration
    /// was made on the picture of the other camera, so it is forgotten; a second camera that is only switched off
    /// keeps its calibration, for when it is switched on again.
    pub fn select_camera(&self, token: &str, cam: usize, id: &str) -> ApiResult {
        self.check_cam(cam)?;
        let Some(selection) = &self.slots[cam].selection else {
            return Err(ApiError::Conflict("there is no choice of camera here".into()));
        };
        let refuse_when_playing = |inner: &Inner| {
            if inner.mode == Mode::Playing { Err(ApiError::Conflict("a game is running: end it first".into())) } else { Ok(()) }
        };
        refuse_when_playing(&*self.controlled(token)?)?;
        if cam == 1 && id.is_empty() {
            let mut inner = self.controlled(token)?;
            refuse_when_playing(&inner)?;
            if selection.device().is_empty() {
                return Ok(());
            }
            selection.clear().map_err(ApiError::internal)?;
            inner.cams[1].camera_error = None;
            // Its thread closes the camera at once.
            drop(inner);
            self.wake_cameras();
            self.hub.notify();
            return Ok(());
        }
        // Looking for the cameras takes a moment: not under the lock.
        let camera = selection.find(id).map_err(|e| ApiError::BadRequest(format!("{e:#}")))?;
        let mut inner = self.controlled(token)?;
        refuse_when_playing(&inner)?;
        if selection.device() == camera.id {
            return Ok(());
        }
        // Two cameras cannot be one: the second opening the camera that the first has would fail.
        let other = &self.slots[1 - cam].selection;
        if other.as_ref().is_some_and(|o| o.device() == camera.id) {
            return Err(ApiError::Conflict(format!(
                "camera {} is in use as camera {}: choose another one",
                camera.name,
                if cam == 0 { 2 } else { 1 }
            )));
        }
        selection.store(&camera).map_err(ApiError::internal)?;
        if inner.cams[cam].calibration.is_some() {
            self.forget_calibration(&mut inner, cam)?;
        }
        inner.cams[cam].camera_error = None;
        self.slots[cam].reopen.store(true, Ordering::SeqCst);
        drop(inner);
        // The other camera may have a depth part, or none.
        if cam == 0 {
            self.refresh_depth_offer();
        }
        // A second camera that was off starts now.
        self.wake_cameras();
        self.hub.notify();
        Ok(())
    }

    // ---- The depth camera ----------------------------------------------------------

    /// Ticks or unticks "use the depth" (kept for the next start). While a game is going on, the squares are looked for
    /// in the depth picture at once, from the position the game is in.
    pub fn set_depth(&self, token: &str, on: bool) -> ApiResult {
        let Some(link) = &self.depth else { return Err(ApiError::Conflict("this program cannot read a depth camera".into())) };
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if on && link.offer().is_none() {
            return Err(ApiError::Conflict("the camera in use has no depth part".into()));
        }
        link.set_on(on).map_err(ApiError::internal)?;
        if on {
            if inner.mode == Mode::Playing {
                let position = inner.tracker.position().clone();
                self.ask_depth_alignment(inner, &position);
            }
        } else {
            inner.tracker.set_view_active(DEPTH_VIEW, false);
            inner.tracker.set_view_calibration(DEPTH_VIEW, None);
        }
        drop(guard);
        // The depth thread starts or stops reading.
        self.wake_cameras();
        self.hub.notify();
        Ok(())
    }

    /// Changes what the depth camera is asked for and how its readings are used (the Calibrate tab): the distances
    /// that count, the size of the picture, its speed. Not during a game. The distances work at once; another size or
    /// speed opens the camera again, and the board has to be found in the new picture again.
    pub fn set_depth_settings(&self, token: &str, patch: crate::depthlink::DepthPatch) -> ApiResult {
        let Some(link) = &self.depth else { return Err(ApiError::Conflict("this program cannot read a depth camera".into())) };
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is running: end it first".into()));
        }
        let current = link.settings();
        let next = patch.applied_to(current);
        next.validate().map_err(|e| ApiError::BadRequest(format!("{e:#}")))?;
        // A picture the camera does not have is refused when it has said which it has.
        let modes = link.modes();
        if next.mode() != current.mode() && !modes.is_empty() && !modes.contains(&next.mode()) {
            return Err(ApiError::BadRequest("the depth camera has no such picture size and speed".into()));
        }
        let reopen = link.set_settings(next).map_err(ApiError::internal)?;
        if reopen {
            // What was found about the board belonged to the other picture.
            inner.tracker.set_view_active(DEPTH_VIEW, false);
            inner.tracker.set_view_calibration(DEPTH_VIEW, None);
        }
        drop(guard);
        // The depth thread opens the camera again.
        self.wake_cameras();
        self.hub.notify();
        Ok(())
    }

    /// Looks for the squares in the depth picture now, with the board in the position of the game (or the start
    /// position when there is no game: the page asks for it with the pieces set up).
    pub fn align_depth(&self, token: &str) -> ApiResult {
        let Some(link) = &self.depth else { return Err(ApiError::Conflict("this program cannot read a depth camera".into())) };
        let guard = self.controlled(token)?;
        if !link.wanted() {
            return Err(ApiError::Conflict("tick \"use the depth\" first".into()));
        }
        if guard.cams[0].calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board on the colour picture first".into()));
        }
        let position = if guard.mode == Mode::Playing { guard.tracker.position().clone() } else { shakmaty::Chess::default() };
        link.set_message("looking for the board in the depth picture...");
        self.ask_depth_alignment(&guard, &position);
        drop(guard);
        self.hub.notify();
        Ok(())
    }

    /// The height picture of the depth camera as it is now, as JPEG (and warped to the board when it was found, with
    /// `warp`): the page shows it so that a person can see what the depth camera sees.
    pub fn depth_jpeg(&self, warp: bool) -> ApiResult<Vec<u8>> {
        let Some(link) = &self.depth else { return Err(ApiError::Conflict("this program cannot read a depth camera".into())) };
        let rotation = self.inner.grab().cams[0].picture.rotation;
        let stamp = link.fresh().ok_or_else(|| ApiError::Conflict("the depth camera gives no picture".into()))?;
        let Some((_, picture)) = link.height_picture(rotation, false) else {
            // No flat surface to measure heights from: show the plain depth, so that it can still be seen.
            if warp {
                return Err(ApiError::Conflict("no flat surface in the depth picture".into()));
            }
            return encode_jpeg(&crate::depth::preview(&stamp.frame.rotated(rotation)), 80).map_err(ApiError::internal);
        };
        if !warp {
            return encode_jpeg(&picture, 80).map_err(ApiError::internal);
        }
        let calibration = link.calibration().ok_or_else(|| ApiError::Conflict("the board was not found in the depth picture".into()))?;
        let mut warped = calibration::warp_board(&picture, &calibration).map_err(ApiError::internal)?;
        calibration::draw_grid(&mut warped);
        encode_jpeg(&warped, 85).map_err(ApiError::internal)
    }

    // ---- Games -------------------------------------------------------------------

    pub fn start_game(&self, token: &str, request: StartRequest) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is already running".into()));
        }
        if inner.cams[0].calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board first".into()));
        }
        let id = self.store.new_id(request.started_at.as_deref());
        let game = GameRecord::new(id, request.started_at, &request.white, &request.black, request.fen)?;
        let position = game.start_position()?;
        inner.game = Some(game);
        self.begin_playing(&mut inner, position, 0.0);
        // A scripted source (the demo) begins its game again; a real camera does nothing.
        for slot in &self.slots {
            slot.restart.store(true, Ordering::SeqCst);
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Starts the recording of the game that begins now, if recording is on. A failure is reported and the game
    /// goes on without it.
    fn begin_recording(&self, inner: &mut Inner, position: &shakmaty::Chess, resume_at: f64) {
        let Some(options) = &self.record else { return };
        let meta = SessionMeta {
            format: crate::record::FORMAT,
            program: crate::version_text(),
            started_utc: String::new(),
            started_local: None,
            source: if self.second_enabled() && !inner.cams[1].source.is_empty() {
                format!("{} + {}", inner.cams[0].source, inner.cams[1].source)
            } else {
                inner.cams[0].source.clone()
            },
            game_id: inner.game.as_ref().map(|g| g.id.clone()).unwrap_or_default(),
            start_fen: game::fen_of(position),
            resumed_at: resume_at,
            record_every: options.every.as_secs_f64(),
            tracker: self.tracker_config.clone(),
            calibration: inner.cams[0].calibration,
            picture: inner.cams[0].picture,
            calibration2: if self.second_enabled() { inner.cams[1].calibration } else { None },
            picture2: self.second_enabled().then_some(inner.cams[1].picture),
        };
        inner.recorder = match Recorder::begin(options, meta, Instant::now()) {
            Ok(recorder) => {
                eprintln!("record: this game is recorded in {}", recorder.folder().display());
                Some(recorder)
            }
            Err(e) => {
                eprintln!("record: {e:#}; this game is not recorded");
                None
            }
        };
    }

    /// Notes something in the recording of the game, if there is one.
    fn record(inner: &Inner, kind: &str, data: Value) {
        if let Some(recorder) = &inner.recorder {
            recorder.event(Instant::now(), kind, data);
        }
    }

    /// The game is over or was ended: its recording ends. Call it once the last thing that happened in the game
    /// has been noted.
    fn close_recording_if_idle(&self, inner: &mut Inner) {
        if inner.mode == Mode::Playing {
            return;
        }
        let Some(recorder) = inner.recorder.take() else { return };
        let (result, reason) = inner.game.as_ref().map_or((None, None), |g| (g.result.clone(), g.termination.clone()));
        recorder.event(Instant::now(), "end", json!({"result": result, "reason": reason, "fen": game::fen_of(inner.tracker.position())}));
        if recorder.dropped() > 0 {
            eprintln!("record: {} pictures could not be kept, the disk was too slow", recorder.dropped());
        }
        eprintln!("record: saved in {}", recorder.folder().display());
    }

    /// The board shows `position`: follow the game from there. `resume_at` is how many seconds
    /// of the game were already played (0 for a new game), so the timestamps carry on.
    fn begin_playing(&self, inner: &mut Inner, position: shakmaty::Chess, resume_at: f64) {
        inner.tracker.start(position.clone());
        inner.doubt = None;
        // The depth camera looks for the squares in its picture, from what the board shows now.
        self.ask_depth_alignment(inner, &position);
        self.begin_recording(inner, &position, resume_at);
        // The time of the last move comes from a file: do not trust it to be a sane number.
        let played = Duration::try_from_secs_f64(resume_at.max(0.0)).unwrap_or_default();
        inner.game_started = Some(Instant::now().checked_sub(played).unwrap_or_else(Instant::now));
        inner.mode = Mode::Playing;
        inner.calibrating_since = None;
        inner.candidates.clear();
        inner.notice = None;
        inner.last_status = Status::Learning;
        for cam in &mut inner.cams {
            cam.follow.anchor = None;
        }
        inner.begun_at = Some(Instant::now());
        inner.last_active = None;
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
        if inner.cams[0].calibration.is_none() {
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
        self.close_recording_if_idle(&mut inner);
        drop(inner);
        if let Some(id) = forget
            && let Err(e) = self.store.remove(&id)
        {
            eprintln!("store: {e:#}");
        }
        self.hub.notify();
        self.flush_save()
    }

    /// The legal moves of the game's position, in UCI (promotions with the piece), for the page to show where a piece
    /// may go when it is dragged. Empty when no game is running.
    pub fn legal_moves(&self) -> Vec<String> {
        let inner = self.inner.grab();
        if inner.mode != Mode::Playing {
            return Vec::new();
        }
        let Some(position) = inner.game.as_ref().and_then(|g| g.position().ok()) else { return Vec::new() };
        shakmaty::Position::legal_moves(&position).into_iter().map(game::uci_of).collect()
    }

    /// Records a move entered by hand (UCI or SAN), or one picked from the candidates.
    pub fn manual_move(&self, token: &str, text: &str) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        let t = Self::move_time(inner, Instant::now());
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        // One move, or several ("e4 e5") when replies were played before the first move was seen: all or nothing.
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.is_empty() || parts.len() > 3 {
            return Err(ApiError::BadRequest("give one move, or up to three separated by spaces".into()));
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
        Self::record(inner, "manual", json!({"text": text.trim(), "fen": game::fen_of(&position)}));
        inner.tracker.set_position(position);
        inner.candidates.clear();
        self.after_game_change(inner);
        self.close_recording_if_idle(inner);
        drop(guard);
        self.hub.notify();
        self.flush_save()
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
        if reopenable && inner.cams[0].calibration.is_none() {
            return Err(ApiError::Conflict("calibrate the board first".into()));
        }
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        let undone = game.undo().ok_or_else(|| ApiError::Conflict("no move to take back".into()))?;
        let position = game.position().map_err(ApiError::internal)?;
        let resume_at = game.moves.last().map_or(0.0, |m| m.t);
        if reopenable {
            // The board must show the position again before learning resumes.
            self.begin_playing(inner, position, resume_at);
        } else {
            Self::record(inner, "undo", json!({"fen": game::fen_of(&position)}));
            // The board still shows the move that was taken back: nothing is decided, and nothing is learned from it,
            // until it shows the position again (the pieces put back) or the move that was played is entered.
            inner.tracker.undo_to(position, Some(undone.uci));
        }
        inner.doubt = None;
        inner.candidates.clear();
        self.save(inner);
        drop(guard);
        self.hub.notify();
        self.flush_save()
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
        Self::record(inner, "promotion", json!({"piece": piece.to_string(), "fen": game::fen_of(&position)}));
        inner.tracker.set_position(position);
        self.after_game_change(inner);
        self.close_recording_if_idle(inner);
        drop(guard);
        self.hub.notify();
        self.flush_save()
    }

    /// The person's answer to a move the tracker took although it was not sure. `Some(uci)` replaces it by one of
    /// the other likely explanations. `None` takes it back and leaves the person to say what was played: the
    /// tracker offers the explanations again and decides nothing until a move is entered.
    pub fn resolve_doubt(&self, token: &str, choice: Option<&str>) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        let note = inner.doubt.clone().ok_or_else(|| ApiError::Conflict("the last move is not in doubt (any more)".into()))?;
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        if game.moves.len() != note.after {
            inner.doubt = None;
            return Err(ApiError::Conflict("the game has moved on".into()));
        }
        let replacement = match choice {
            Some(uci) => Some(
                note.alternatives
                    .iter()
                    .find(|c| c.uci == uci)
                    .ok_or_else(|| ApiError::BadRequest("that is not one of the other likely moves".into()))?
                    .clone(),
            ),
            None => None,
        };
        // What was taken, when it was seen, and what it was.
        let first = note.after - note.count;
        let t = game.moves[first].t;
        let taken: Vec<(String, String)> = game.moves[first..].iter().map(|m| (m.uci.clone(), m.san.clone())).collect();
        for _ in 0..note.count {
            game.undo();
        }
        if let Some(chosen) = &replacement {
            for part in chosen.uci.split_whitespace() {
                if let Err(e) = game.push_text(part, t) {
                    // The alternatives are legal moves of this position, so this should not happen: put it back.
                    while game.moves.len() > first {
                        game.undo();
                    }
                    for (uci, _) in &taken {
                        let _ = game.push_text(uci, t);
                    }
                    return Err(ApiError::internal(e));
                }
            }
        }
        let position = game.position().map_err(ApiError::internal)?;
        Self::record(inner, "doubt", json!({"choice": choice, "fen": game::fen_of(&position)}));
        inner.tracker.set_position(position);
        inner.candidates.clear();
        if replacement.is_none() {
            // Ask again: the move that was taken and the other likely ones.
            let guessed = Candidate {
                uci: taken.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>().join(" "),
                san: taken.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(" "),
                cost: 0.0,
                chance: note.certainty,
            };
            let mut offered = vec![guessed];
            offered.extend(note.alternatives.iter().cloned());
            inner.tracker.hold(offered.clone());
            inner.candidates = offered;
        }
        self.after_game_change(inner);
        self.close_recording_if_idle(inner);
        drop(guard);
        self.hub.notify();
        self.flush_save()
    }

    /// Learns the look of the squares again, from the position the game is in.
    pub fn relearn(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        Self::record(&inner, "relearn", json!({}));
        inner.tracker.relearn();
        inner.last_status = Status::Learning;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    // ---- Past games ----------------------------------------------------------------

    pub fn list_games(&self) -> Result<Vec<Summary>> {
        // A game that was just played may not be on the disk yet.
        let _ = self.flush_save();
        self.store.list()
    }

    /// A stored game with the position after each move, ready to replay.
    pub fn game_detail(&self, id: &str) -> Result<Option<Value>> {
        let _ = self.flush_save();
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
            "moves": game.moves.iter().map(|m| json!({"san": m.san, "uci": m.uci, "t": m.t, "guess": m.promotion_guess, "together": m.together})).collect::<Vec<_>>(),
            "fens": fens,
        })))
    }

    pub fn game_pgn(&self, id: &str) -> Result<Option<String>> {
        let _ = self.flush_save();
        match self.store.load(id)? {
            Some(game) => Ok(Some(game.to_pgn()?)),
            None => Ok(None),
        }
    }
}

/// How a camera's picture is turned and adjusted, for the page.
fn picture_json(picture: &Picture) -> Value {
    json!({"rotation": picture.rotation, "brightness": picture.brightness, "contrast": picture.contrast})
}

/// The candidates of an unsure tracker, for the page and the recording.
fn candidates_json(candidates: &[Candidate]) -> Value {
    Value::Array(
        candidates
            .iter()
            .map(|c| json!({"uci": c.uci, "san": c.san, "cost": (c.cost * 10.0).round() / 10.0, "chance": (c.chance * 100.0).round() / 100.0}))
            .collect(),
    )
}

/// A doubtful move, for the page: how sure the tracker is, what it took, and what else it may have been. Only while
/// the doubtful moves are the last of the game.
fn doubt_json(inner: &Inner) -> Value {
    let (Some(note), Some(game)) = (&inner.doubt, &inner.game) else { return Value::Null };
    if inner.mode != Mode::Playing || game.moves.len() != note.after {
        return Value::Null;
    }
    let played: Vec<&str> = game.moves[note.after - note.count..].iter().map(|m| m.san.as_str()).collect();
    json!({"certainty": (note.certainty * 100.0).round() / 100.0, "played": played.join(" "), "alternatives": candidates_json(&note.alternatives)})
}

/// Keeps the doubt about moves that were just taken (if the game accepted them), for the page to show.
fn remember_doubt(inner: &mut Inner, doubt: Option<Doubt>, moves_before: usize, count: usize) {
    inner.doubt = None;
    let Some(doubt) = doubt else { return };
    let Some(game) = inner.game.as_ref() else { return };
    // The game may have refused the moves, or ended with them.
    if inner.mode != Mode::Playing || game.moves.len() != moves_before + count {
        return;
    }
    inner.doubt = Some(DoubtNote { after: game.moves.len(), count, certainty: doubt.certainty, alternatives: doubt.alternatives });
}

/// What the recording says of moves that were just found: the moves and the position after them, or that
/// the game refused them.
fn detection_event(inner: &Inner, moves_before: usize, count: usize, doubt: Option<&Doubt>) -> (&'static str, Value) {
    let fen = game::fen_of(inner.tracker.position());
    match inner.game.as_ref() {
        Some(game) if game.moves.len() == moves_before + count => {
            let found = &game.moves[moves_before..];
            (
                "detect",
                json!({"moves": found.iter().map(|m| &m.uci).collect::<Vec<_>>(), "san": found.iter().map(|m| &m.san).collect::<Vec<_>>(), "fen": fen, "doubt": doubt.map(|d| json!({"certainty": (d.certainty * 100.0).round() / 100.0, "alternatives": candidates_json(&d.alternatives)}))}),
            )
        }
        _ => ("rejected", json!({"text": inner.notice, "fen": fen})),
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::NoCalibration => "no_calibration",
        Status::Learning => "learning",
        Status::Watching => "watching",
        Status::Moving => "moving",
        Status::Unsure => "unsure",
        Status::Undone => "undone",
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
    fn detected_moves_are_all_recorded_and_an_illegal_sequence_is_not() {
        let (engine, dir) = engine_playing("detected-pair");
        use shakmaty::Position;
        let start = shakmaty::Chess::default();
        let e4 = crate::game::parse_move(&start, "e2e4").unwrap();
        let mut after = start.clone();
        after.play_unchecked(e4);
        let e5 = crate::game::parse_move(&after, "e7e5").unwrap();
        {
            let mut inner = engine.inner.grab();
            engine.apply_detected_moves(&mut inner, &[e4, e5], Instant::now());
            let game = inner.game.as_ref().unwrap();
            assert_eq!(game.moves.len(), 2);
            assert_eq!((game.moves[0].uci.as_str(), game.moves[1].uci.as_str()), ("e2e4", "e7e5"));
            // Seen in one picture: the page shows both as the last move.
            assert_eq!((game.moves[0].together, game.moves[1].together), (false, true));
            assert!(inner.pending_save.is_some(), "the game is marked to be written");
        }
        engine.flush_save().unwrap();
        assert_eq!(engine.list_games().unwrap()[0].moves, 2);

        // A pair that does not follow from the position (the tracker and the game disagree): neither is kept.
        {
            let mut inner = engine.inner.grab();
            engine.apply_detected_moves(&mut inner, &[e4, e5], Instant::now());
            assert_eq!(inner.game.as_ref().unwrap().moves.len(), 2);
            assert!(inner.notice.as_deref().is_some_and(|n| n.contains("the moves detected at once were rejected")));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_clock_starts_with_the_first_move_not_with_the_start_of_the_game() {
        let (engine, dir) = engine_playing("first-move-clock");
        let start = shakmaty::Chess::default();
        let e4 = crate::game::parse_move(&start, "e2e4").unwrap();
        let mut after = start.clone();
        shakmaty::Position::play_unchecked(&mut after, e4);
        let e5 = crate::game::parse_move(&after, "e7e5").unwrap();
        {
            let mut inner = engine.inner.grab();
            // The game was started on the page a minute ago, and the players only now play.
            inner.game_started = Instant::now().checked_sub(Duration::from_secs(60));
            let first = Instant::now();
            engine.apply_detected_move(&mut inner, e4, first, false);
            assert_eq!(inner.game.as_ref().unwrap().moves[0].t, 0.0, "the first move is at 0");
            // Black thinks for four seconds.
            engine.apply_detected_move(&mut inner, e5, first + Duration::from_secs(4), false);
            assert_eq!(inner.game.as_ref().unwrap().moves[1].t, 4.0);
            // Taking the first moves back and playing again starts the clock again.
            inner.game.as_mut().unwrap().undo();
            inner.game.as_mut().unwrap().undo();
            let later = first + Duration::from_secs(30);
            engine.apply_detected_move(&mut inner, e4, later, false);
            assert_eq!(inner.game.as_ref().unwrap().moves[0].t, 0.0);
        }
        // In the PGN, the first move took no time.
        let pgn = engine.game_pgn("pair").unwrap_or(None);
        assert!(pgn.is_none() || pgn.unwrap().contains("[%emt 0:00:00]"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_rate_of_the_camera_is_set_used_and_kept() {
        let dir = std::env::temp_dir().join(format!("nicochess-rate-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let rate = crate::camera::FrameRate::new(25.0);
        let config = EngineConfig { rate: Some(Arc::clone(&rate)), ..EngineConfig::default() };
        let engine = Engine::new(config, Store::open(&dir.join("games")).unwrap(), dir.join("calibration.json"), None);
        let token = "a11ce000a11ce000";
        engine.claim(token).unwrap();
        assert_eq!(engine.snapshot(token)["rate"]["fps"], 25.0);
        // Out of range, or not the controller: refused.
        assert!(matches!(engine.set_fps(token, 0.0), Err(ApiError::BadRequest(_))));
        assert!(matches!(engine.set_fps(token, 60.0), Err(ApiError::BadRequest(_))));
        assert!(matches!(engine.set_fps("b0b00000b0b00000", 10.0), Err(ApiError::Forbidden)));
        engine.set_fps(token, 10.0).unwrap();
        assert!((rate.fps() - 10.0).abs() < 1e-6);
        assert_eq!(engine.snapshot(token)["rate"]["fps"], 10.0);
        // The tracker then looks at a picture a little less than 100 ms after the last.
        assert_eq!(engine.track_interval(), Duration::from_micros(80_000));
        assert_eq!(remembered_fps(&dir.join("calibration.json")), Some(10.0));
        // A source with no rate of its own to change says so.
        let fixed = Engine::new(EngineConfig::default(), Store::open(&dir.join("games2")).unwrap(), dir.join("calibration2.json"), None);
        fixed.claim(token).unwrap();
        assert!(fixed.snapshot(token)["rate"].is_null());
        assert!(matches!(fixed.set_fps(token, 10.0), Err(ApiError::Conflict(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_legal_moves_of_the_game_are_given_for_the_board_of_the_page() {
        let (engine, dir) = engine_playing("legal");
        let moves = engine.legal_moves();
        assert_eq!(moves.len(), 20, "{moves:?}");
        assert!(moves.contains(&"e2e4".to_string()) && moves.contains(&"g1f3".to_string()));
        // A game that is not running has none.
        engine.inner.grab().mode = Mode::Idle;
        assert!(engine.legal_moves().is_empty());
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
