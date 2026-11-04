//! The heart of the program: ties the camera, the tracker, the game record, the
//! store and the roles together, and tells the pages when something changed.
//!
//! One mutex guards the state that matters (`Inner`). The capture thread takes it
//! once per frame, for a few milliseconds; commands from the pages take it briefly.

use crate::calibration::{self, Calibration};
use crate::camera::SourceFactory;
use crate::game::{self, GameRecord};
use crate::roles::{self, Role, Roles};
use crate::store::{Store, Summary};
use crate::tracker::{Candidate, Detection, Status, Tracker, TrackerConfig};
use anyhow::Result;
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

/// A calibration page left open for longer than this closes itself.
const CALIBRATION_LIMIT: Duration = Duration::from_secs(10 * 60);
/// How long to wait before trying the camera again after a failure.
const CAMERA_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub idle_timeout: Duration,
    pub game_timeout: Duration,
    pub tracker: TrackerConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            idle_timeout: Duration::from_secs(60),
            game_timeout: Duration::from_secs(30 * 60),
            tracker: TrackerConfig::default(),
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
}

impl ApiError {
    pub fn status(&self) -> u16 {
        match self {
            ApiError::Forbidden => 403,
            ApiError::BadRequest(_) => 400,
            ApiError::Conflict(_) => 409,
        }
    }

    pub fn message(&self) -> String {
        match self {
            ApiError::Forbidden => "only the controller can do that".to_string(),
            ApiError::BadRequest(m) | ApiError::Conflict(m) => m.clone(),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::BadRequest(format!("{e:#}"))
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
    pub image: Arc<RgbImage>,
    pub seq: u64,
    jpeg: OnceLock<Arc<Vec<u8>>>,
}

impl Latest {
    fn new(image: RgbImage, seq: u64) -> Latest {
        Latest { image: Arc::new(image), seq, jpeg: OnceLock::new() }
    }

    pub fn jpeg(&self) -> Arc<Vec<u8>> {
        self.jpeg
            .get_or_init(|| Arc::new(encode_jpeg(&self.image, 80).unwrap_or_default()))
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
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    fn notify(&self) {
        // A page that has gone away has dropped its receiver: forget it.
        self.subscribers.lock().unwrap().retain(|tx| tx.send(()).is_ok());
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
}

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
        Arc::new(Engine {
            inner: Mutex::new(Inner {
                roles: Roles::new(config.idle_timeout, config.game_timeout),
                mode: Mode::Idle,
                calibrating_since: None,
                game: None,
                game_started: None,
                tracker,
                calibration,
                candidates: Vec::new(),
                notice: None,
                camera_error: None,
                source: String::new(),
                last_status: Status::NoCalibration,
            }),
            frame: RwLock::new(None),
            camera_wanted: Mutex::new(false),
            camera_cv: Condvar::new(),
            hub: Hub { subscribers: Mutex::new(Vec::new()) },
            clients: AtomicUsize::new(0),
            restart_source: AtomicBool::new(false),
            store,
            calibration_path,
        })
    }

    // ---- Camera thread ------------------------------------------------------

    /// Runs the capture loop on its own thread: the camera is opened when someone
    /// needs it and closed when nobody does.
    pub fn spawn_capture(self: &Arc<Self>, factory: SourceFactory) -> std::io::Result<()> {
        let engine = Arc::clone(self);
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || engine.capture_loop(factory))
            .map(|_| ())
    }

    fn capture_loop(&self, factory: SourceFactory) {
        loop {
            {
                let mut wanted = self.camera_wanted.lock().unwrap();
                while !*wanted {
                    wanted = self.camera_cv.wait(wanted).unwrap();
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
                            Ok(image) => self.on_frame(image),
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
            *self.frame.write().unwrap() = None;
            self.set_source(String::new());
            if self.camera_is_wanted() {
                thread::sleep(CAMERA_RETRY);
            }
        }
    }

    fn camera_is_wanted(&self) -> bool {
        *self.camera_wanted.lock().unwrap()
    }

    fn set_camera(&self, wanted: bool) {
        *self.camera_wanted.lock().unwrap() = wanted;
        self.camera_cv.notify_all();
    }

    fn set_source(&self, description: String) {
        self.inner.lock().unwrap().source = description;
        self.hub.notify();
    }

    fn camera_failed(&self, message: String) {
        eprintln!("camera: {message}");
        self.inner.lock().unwrap().camera_error = Some(message);
        self.hub.notify();
    }

    /// Handles one camera frame.
    fn on_frame(&self, image: RgbImage) {
        let seq = self.frame.read().unwrap().as_ref().map_or(1, |l| l.seq + 1);
        let latest = Arc::new(Latest::new(image, seq));
        *self.frame.write().unwrap() = Some(Arc::clone(&latest));

        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        let mut changed = inner.camera_error.take().is_some();

        if inner.mode == Mode::Playing {
            match inner.tracker.feed(&latest.image, now) {
                Detection::Nothing => {
                    if inner.tracker.status() != Status::Unsure && !inner.candidates.is_empty() {
                        inner.candidates.clear();
                        changed = true;
                    }
                }
                Detection::Unsure { candidates } => {
                    inner.candidates = candidates;
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
        if changed {
            self.hub.notify();
        }
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

    /// Saves the game and ends it if the rules say so.
    fn after_game_change(&self, inner: &mut Inner) {
        let Some(game) = inner.game.as_mut() else { return };
        match game.rules_outcome() {
            Ok(Some((result, reason))) => {
                game.finish(&result, &reason);
                eprintln!("game: over, {result} ({reason})");
                inner.mode = Mode::Idle;
                inner.calibrating_since = None;
                self.set_camera(false);
            }
            Ok(None) => {}
            Err(e) => inner.notice = Some(format!("{e:#}")),
        }
        self.save(inner);
    }

    fn save(&self, inner: &mut Inner) {
        if let Some(game) = &inner.game {
            match self.store.save(game) {
                Ok(()) => inner.notice = None,
                Err(e) => {
                    eprintln!("store: {e:#}");
                    inner.notice = Some(format!("cannot save the game: {e:#}"));
                }
            }
        }
    }

    // ---- Frames -------------------------------------------------------------

    pub fn latest_frame(&self) -> Option<Arc<Latest>> {
        self.frame.read().unwrap().clone()
    }

    /// The top-down board for the current calibration, as JPEG.
    pub fn warped_jpeg(&self, with_grid: bool) -> ApiResult<Vec<u8>> {
        let calibration = self
            .inner
            .lock()
            .unwrap()
            .calibration
            .ok_or_else(|| ApiError::Conflict("not calibrated yet".into()))?;
        let latest = self.latest_frame().ok_or_else(|| ApiError::Conflict("the camera is off".into()))?;
        let mut warped = calibration::warp_board(&latest.image, &calibration)?;
        if with_grid {
            calibration::draw_grid(&mut warped);
        }
        Ok(encode_jpeg(&warped, 85)?)
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
        let mut guard = self.inner.lock().unwrap();
        let inner = &mut *guard;
        self.housekeeping(inner, now);
        let playing = inner.mode == Mode::Playing;
        let role = if roles::valid_token(token) { inner.roles.touch(token, now, playing) } else { Role::Viewer };
        let frame = self.frame.read().unwrap().clone();

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
            })
        });
        let promotion_pending = inner
            .game
            .as_ref()
            .and_then(|g| g.moves.last())
            .is_some_and(|m| m.promotion_guess);

        json!({
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
            "corner_names": calibration::CORNER_NAMES,
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
    fn housekeeping(&self, inner: &mut Inner, now: Instant) {
        let playing = inner.mode == Mode::Playing;
        let lost_controller = inner.roles.expire(now, playing);
        let calibration_too_long = inner.calibrating_since.is_some_and(|s| now.duration_since(s) > CALIBRATION_LIMIT);
        if inner.mode == Mode::Calibrating && (lost_controller || calibration_too_long) {
            inner.mode = Mode::Idle;
            inner.calibrating_since = None;
            self.set_camera(false);
        }
    }

    fn controlled(&self, token: &str) -> ApiResult<MutexGuard<'_, Inner>> {
        let now = Instant::now();
        let mut guard = self.inner.lock().unwrap();
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
        let mut inner = self.inner.lock().unwrap();
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
            inner.mode = Mode::Idle;
            inner.calibrating_since = None;
            self.set_camera(false);
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
            inner.mode = Mode::Idle;
            inner.calibrating_since = None;
            self.set_camera(false);
        }
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    pub fn set_calibration(&self, token: &str, corners: [[f32; 2]; 4]) -> ApiResult {
        let mut inner = self.controlled(token)?;
        let frame = self
            .latest_frame()
            .ok_or_else(|| ApiError::Conflict("the camera is off: open the calibration first".into()))?;
        let calibration = Calibration { corners };
        calibration.validate(frame.image.width(), frame.image.height())?;
        calibration.save(&self.calibration_path)?;
        inner.calibration = Some(calibration);
        // During a game the squares are learned again from the position reached.
        inner.tracker.set_calibration(Some(&calibration));
        inner.notice = None;
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    pub fn clear_calibration(&self, token: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode == Mode::Playing {
            return Err(ApiError::Conflict("a game is running".into()));
        }
        match std::fs::remove_file(&self.calibration_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ApiError::BadRequest(format!("cannot delete the calibration file: {e}"))),
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
        inner.tracker.start(position);
        inner.game = Some(game);
        inner.game_started = Some(Instant::now());
        inner.mode = Mode::Playing;
        inner.calibrating_since = None;
        inner.candidates.clear();
        inner.notice = None;
        inner.last_status = Status::Learning;
        self.restart_source.store(true, Ordering::SeqCst);
        self.set_camera(true);
        drop(inner);
        self.hub.notify();
        Ok(())
    }

    /// Ends the game with a result decided by the players.
    pub fn end_game(&self, token: &str, result: &str, reason: &str) -> ApiResult {
        let mut inner = self.controlled(token)?;
        if inner.mode != Mode::Playing {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        if !["1-0", "0-1", "1/2-1/2", "*"].contains(&result) {
            return Err(ApiError::BadRequest("unknown result".into()));
        }
        if !["resignation", "agreement", "abandoned", "other"].contains(&reason) {
            return Err(ApiError::BadRequest("unknown reason".into()));
        }
        let no_moves = inner.game.as_ref().is_none_or(|g| g.moves.is_empty());
        if reason == "abandoned" && no_moves {
            // Nothing was played: do not keep an empty game.
            inner.game = None;
        } else if let Some(game) = inner.game.as_mut() {
            game.finish(result, reason);
            self.save(&mut inner);
        }
        inner.mode = Mode::Idle;
        inner.candidates.clear();
        self.set_camera(false);
        drop(inner);
        self.hub.notify();
        Ok(())
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
        game.push_text(text, t)?;
        let position = game.position()?;
        inner.tracker.set_position(position);
        inner.candidates.clear();
        self.after_game_change(inner);
        drop(guard);
        self.hub.notify();
        Ok(())
    }

    /// Takes back the last move. Also reopens a game that ended by the rules,
    /// for example when a misread move led to a false checkmate.
    pub fn undo(&self, token: &str) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        let reopenable = inner.mode == Mode::Idle
            && inner.game.as_ref().is_some_and(|g| {
                matches!(g.termination.as_deref(), Some("checkmate" | "stalemate" | "insufficient material"))
            });
        if inner.mode != Mode::Playing && !reopenable {
            return Err(ApiError::Conflict("no game is running".into()));
        }
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        game.undo().ok_or_else(|| ApiError::Conflict("no move to take back".into()))?;
        let position = game.position()?;
        let resume_at = game.moves.last().map_or(0.0, |m| m.t);
        if reopenable {
            // The board must show the position again before learning resumes.
            inner.mode = Mode::Playing;
            inner.game_started = Some(
                Instant::now()
                    .checked_sub(Duration::from_secs_f64(resume_at))
                    .unwrap_or_else(Instant::now),
            );
            inner.tracker.start(position);
            self.set_camera(true);
        } else {
            inner.tracker.set_position(position);
        }
        inner.candidates.clear();
        self.save(inner);
        drop(guard);
        self.hub.notify();
        Ok(())
    }

    /// Changes the piece chosen for the last promotion.
    pub fn set_promotion(&self, token: &str, piece: char) -> ApiResult {
        let mut guard = self.controlled(token)?;
        let inner = &mut *guard;
        let game = inner.game.as_mut().ok_or_else(|| ApiError::Conflict("no game".into()))?;
        game.set_last_promotion(piece)?;
        let position = game.position()?;
        inner.tracker.set_position(position);
        self.after_game_change(inner);
        drop(guard);
        self.hub.notify();
        Ok(())
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
