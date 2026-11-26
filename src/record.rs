//! Recording of a game for later investigation (`--record`), read back by [`crate::replay`].
//!
//! Every time play begins (a new game, a resumed one, a game reopened by undo) a folder is made, named
//! after the start (`2026-11-16_11-30-05`: local time on Windows, UTC elsewhere):
//!
//! - `session.json`: how the program was set up: calibration, picture settings (of the second camera too, when
//!   there is one), tracker settings, the position the tracker started from.
//! - `frames/000001.jpg`, ...: the pictures the detection was given (turned and adjusted as it saw them).
//!   One every `--record-every` seconds, and every picture while something happens: the tracker is learning,
//!   a hand is moving, it is unsure, a move was found. With two cameras each time brings two pictures.
//!   When an `ffmpeg` is found the pictures go instead into one video for each camera (`cam0.mkv`, `cam2.mkv`
//!   for the depth view: see [`crate::video`]), a fifth to a twentieth of the size; a camera whose video cannot be
//!   written goes on in JPEG files.
//! - `events.jsonl`: one JSON object per line, in order, each with `t` (seconds since the recording began)
//!   and a `type`: `frame` (the file, or the video and the number of the picture in it, the tracker's status, the
//!   camera `cam` and the number `feed` that the pictures given together share), `video` (the first time a camera's
//!   video is used: its file and the size of its pictures), `video_lost` (its encoder failed), `detect` (moves found,
//!   and the position after), `unsure` (the candidates offered), `manual`, `undo`, `promotion`, `relearn`,
//!   `calibration`, `end`, and `closed` last. Positions are FEN strings.
//!
//! Nothing here may disturb the game: the capture thread only hands pictures and lines to the writer threads
//! (`try_send`, never waiting for the disk or the encoder); when they cannot keep up, pictures are dropped and
//! counted. JPEG pictures are encoded and written by two to four threads at once (one keeps up with about 20 a second,
//! and two cameras at 25 feeds a second give 50), the lines of `events.jsonl` are still appended in the order things
//! happened. A failure to write is reported once on the console and the game goes on.

use crate::calibration::Calibration;
use crate::engine::encode_jpeg;
use crate::fsutil::write_atomic;
use crate::picture::Picture;
use crate::tracker::TrackerConfig;
use crate::video::{self, Push, VideoWriter};
use anyhow::{Context, Result};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The format of the files below: a replay refuses a recording of a newer format. Format 2 added the second camera
/// (`cam` and `feed` on a `frame` line, `calibration2` and `picture2` in `session.json`); a recording of format 1 is
/// read as one made with one camera. Format 3 added the videos (`video` in `session.json`, the `video`, `video_lost`
/// and `frame` lines that tell the video and the number of a picture instead of a file); a recording of format 2
/// is read as before.
pub const FORMAT: u32 = 3;

/// Pictures that may wait for the writer: more than this and the next ones are dropped (a picture is about a
/// megabyte in memory), so that a slow disk costs pictures, not memory.
pub const DEFAULT_BACKLOG: usize = 12;
/// The same when every picture is kept: about a hundred megabytes at most, to ride out a pause of the disk.
pub const ALL_BACKLOG: usize = 32;
/// Lines and pictures waiting in all.
const QUEUE: usize = 512;
/// Quality of the saved pictures: the camera's own pictures are already JPEG, so a high quality adds little noise.
const JPEG_QUALITY: u8 = 92;

#[derive(Clone, Debug)]
pub struct RecordOptions {
    /// The folder in which one sub-folder is made for each recording.
    pub dir: PathBuf,
    /// Time between two pictures kept while nothing happens; zero keeps every picture the detection looks at.
    pub every: Duration,
    /// How many pictures may wait for the disk before the next ones are dropped.
    pub backlog: usize,
    /// The least time between two pictures kept while something is going on (at 25 pictures a second a recording
    /// would be huge, and a few a second tell the story); zero keeps every picture the detection looks at.
    pub busy_every: Duration,
    /// The `ffmpeg` that writes the pictures as video; `None` keeps them as JPEG files (the program looks for one,
    /// see [`crate::video::find_ffmpeg`], and puts it here; tests leave it out).
    pub ffmpeg: Option<PathBuf>,
}

impl RecordOptions {
    pub fn new(dir: PathBuf, every: Duration) -> RecordOptions {
        RecordOptions { dir, every, backlog: DEFAULT_BACKLOG, busy_every: Duration::from_millis(150), ffmpeg: None }
    }

    /// The same, with the pictures going into video files through this `ffmpeg` (or JPEG files when it is `None`).
    pub fn with_ffmpeg(mut self, ffmpeg: Option<PathBuf>) -> RecordOptions {
        self.ffmpeg = ffmpeg;
        self
    }

    /// Every picture the detection looks at is kept, quiet or not (`--record-all`). At 25 pictures a second that is
    /// several megabytes a second, so more of them may wait for the disk than usual before some are dropped (and
    /// counted: the program says so when the game ends).
    pub fn keeping_all(dir: PathBuf) -> RecordOptions {
        RecordOptions { dir, every: Duration::ZERO, backlog: ALL_BACKLOG, busy_every: Duration::ZERO, ffmpeg: None }
    }

    /// True when no picture is left out on purpose.
    pub fn keeps_all(&self) -> bool {
        self.every.is_zero() && self.busy_every.is_zero()
    }
}

/// What a replay needs to know about how the game was followed.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionMeta {
    pub format: u32,
    /// The program and its commit.
    pub program: String,
    pub started_utc: String,
    /// The same moment in the time of the computer, when the program can tell it (Windows).
    pub started_local: Option<String>,
    /// What the pictures came from: the camera, the demo, ...
    pub source: String,
    pub game_id: String,
    /// The position the tracker starts from (the board shows it).
    pub start_fen: String,
    /// Seconds of the game that were already played when the recording began (a resumed game).
    pub resumed_at: f64,
    pub record_every: f64,
    pub tracker: TrackerConfig,
    pub calibration: Option<Calibration>,
    pub picture: Picture,
    /// The second camera, when the game was followed with two: its calibration and picture settings.
    #[serde(default)]
    pub calibration2: Option<Calibration>,
    #[serde(default)]
    pub picture2: Option<Picture>,
    /// Set when the pictures go into video files (see the `video` lines of `events.jsonl`): the `ffmpeg` used and how.
    /// Absent when they are JPEG files.
    #[serde(default)]
    pub video: Option<VideoMeta>,
}

/// How the pictures were encoded.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VideoMeta {
    pub ffmpeg: String,
    pub codec: String,
    pub camera_crf: u32,
    pub depth_crf: u32,
}

enum Msg {
    Session(Vec<u8>),
    Frame { t: f64, camera: usize, feed: u64, image: Arc<RgbImage>, status: &'static str },
    Line(String),
}

/// One recording in progress. Methods only queue work: they are safe to call with the engine's lock held.
pub struct Recorder {
    tx: Option<SyncSender<Msg>>,
    begun: Instant,
    every: Duration,
    backlog: usize,
    last_saved: Option<Instant>,
    /// How many times pictures were given to the tracker (and offered here): the pictures of one such time, from the
    /// two cameras, carry the same number, so that a replay gives them to the tracker together.
    feeds: u64,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    folder: PathBuf,
    thread: Option<JoinHandle<()>>,
    /// The videos, when the pictures go into video files.
    video: Option<Arc<Videos>>,
}

/// The cameras that can have a video: the cameras and the height picture of the depth camera, numbered as the engine
/// numbers its views.
const VIEWS: usize = 3;

/// The videos of a recording. The capture side hands pictures to them through `state`; the writer thread ends them
/// (waits for each `ffmpeg` to finish its file) before it writes the last line, which tells how many pictures were kept.
struct Videos {
    ffmpeg: PathBuf,
    folder: PathBuf,
    state: Mutex<VideoState>,
}

struct VideoState {
    /// The video of each view, started at its first picture.
    streams: [Option<VideoWriter>; VIEWS],
    /// Views that go in JPEG files from now on: the encoder could not start or died.
    jpeg: [bool; VIEWS],
    /// The recording is over: no video takes a picture any more.
    finished: bool,
}

/// What the videos kept in all.
struct VideoTotals {
    written: u64,
    lost: u64,
    /// What went wrong with an encoder, if anything did (a picture counted as written may still have been lost with it).
    failures: Vec<String>,
}

impl Videos {
    /// Ends every video and waits for the files to be complete.
    fn finish(&self) -> VideoTotals {
        let streams: Vec<VideoWriter> = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.finished = true;
            state.streams.iter_mut().filter_map(Option::take).collect()
        };
        let mut totals = VideoTotals { written: 0, lost: 0, failures: Vec::new() };
        for stream in streams {
            let done = stream.finish();
            totals.written += done.written;
            totals.lost += done.lost;
            if let Some(reason) = done.failure {
                eprintln!("record: a video ended badly ({reason}); the pictures after that point are lost or in JPEG files");
                totals.failures.push(reason);
            }
        }
        totals
    }
}

impl Recorder {
    /// Makes the folder and starts the writer. `meta` gets the date and time of now.
    pub fn begin(options: &RecordOptions, mut meta: SessionMeta, now: Instant) -> Result<Recorder> {
        fs::create_dir_all(&options.dir).with_context(|| format!("cannot create {}", options.dir.display()))?;
        let (utc, local) = (utc_now(), local_now());
        meta.started_utc = utc.iso();
        meta.started_local = local.as_ref().map(Civil::iso);
        let name = local.as_ref().unwrap_or(&utc).folder_name();
        let folder = unique_folder(&options.dir, &name)?;
        fs::create_dir_all(folder.join("frames")).with_context(|| format!("cannot create {}", folder.display()))?;

        // Video only when the program that writes it is there; otherwise JPEG files, and the session says so.
        let video = match &options.ffmpeg {
            Some(ffmpeg) if video::usable(ffmpeg) => {
                meta.video = Some(video_meta(ffmpeg));
                Some(Arc::new(Videos {
                    ffmpeg: ffmpeg.clone(),
                    folder: folder.clone(),
                    state: Mutex::new(VideoState { streams: [None, None, None], jpeg: [false; VIEWS], finished: false }),
                }))
            }
            Some(ffmpeg) => {
                eprintln!("record: {} is not there: the pictures are saved as JPEG files", ffmpeg.display());
                meta.video = None;
                None
            }
            None => {
                meta.video = None;
                None
            }
        };

        let (tx, rx) = sync_channel(QUEUE + options.backlog);
        let queued = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_folder = folder.clone();
        let (writer_queued, writer_dropped) = (Arc::clone(&queued), Arc::clone(&dropped));
        let writer_video = video.clone();
        let thread = thread::Builder::new()
            .name("record".into())
            .spawn(move || writer(writer_folder, rx, now, writer_queued, writer_dropped, writer_video))
            .context("cannot start the recording thread")?;
        let recorder = Recorder {
            tx: Some(tx),
            begun: now,
            every: options.every,
            backlog: options.backlog,
            last_saved: None,
            feeds: 0,
            queued,
            dropped,
            folder,
            thread: Some(thread),
            video,
        };
        // Written by the writer, so that no file is written under the engine's lock.
        let bytes = serde_json::to_vec_pretty(&meta)?;
        recorder.send(Msg::Session(bytes));
        Ok(recorder)
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// How many pictures were dropped because the disk could not keep up.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn send(&self, msg: Msg) -> bool {
        let Some(tx) = &self.tx else { return false };
        match tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    fn seconds(&self, at: Instant) -> f64 {
        (at.saturating_duration_since(self.begun).as_secs_f64() * 1000.0).round() / 1000.0
    }

    /// Keeps the pictures the detection was given this time (each with the number of its camera), if it is time to
    /// (or `force`: something is happening). Call it before the events that these pictures led to, so that the file
    /// lists them in order.
    pub fn frames(&mut self, at: Instant, images: &[(usize, Arc<RgbImage>)], status: &'static str, force: bool) {
        self.feeds += 1;
        let due = force || self.last_saved.is_none_or(|last| at.saturating_duration_since(last) >= self.every);
        if !due {
            return;
        }
        let t = self.seconds(at);
        for (camera, image) in images {
            if self.keep_in_video(t, *camera, image, status) {
                self.last_saved = Some(at);
                continue;
            }
            // Too many waiting: the pictures that would be kept are lost, never the lines, which are small.
            if self.queued.load(Ordering::SeqCst) >= self.backlog {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            self.queued.fetch_add(1, Ordering::SeqCst);
            if self.send(Msg::Frame { t, camera: *camera, feed: self.feeds, image: Arc::clone(image), status }) {
                self.last_saved = Some(at);
            } else {
                self.queued.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Hands a picture to the video of its camera, which is started at the first picture. Returns false when the picture
    /// must go in a JPEG file instead (there is no video, or the encoder failed, or the picture is of another size
    /// than the video); true when it was taken, or lost because the encoder was too slow (and counted).
    fn keep_in_video(&self, t: f64, camera: usize, image: &Arc<RgbImage>, status: &'static str) -> bool {
        let Some(videos) = &self.video else { return false };
        if camera >= VIEWS {
            return false;
        }
        let mut state = videos.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.finished || state.jpeg[camera] {
            return false;
        }
        let name = format!("cam{camera}.{}", video::FILE_EXTENSION);
        if state.streams[camera].is_none() {
            let crf = if camera == VIEWS - 1 { video::DEPTH_CRF } else { video::CAMERA_CRF };
            let started = VideoWriter::start(
                &videos.ffmpeg,
                videos.folder.join(&name),
                videos.folder.join(format!("ffmpeg-cam{camera}.log")),
                image.width(),
                image.height(),
                crf,
                self.backlog.max(video::MIN_QUEUE),
            );
            match started {
                Ok(writer) => {
                    state.streams[camera] = Some(writer);
                    self.send(Msg::Line(
                        json!({"t": t, "type": "video", "cam": camera, "file": name, "width": image.width(), "height": image.height(),
                               "codec": video::CODEC, "crf": crf})
                        .to_string(),
                    ));
                }
                Err(e) => {
                    state.jpeg[camera] = true;
                    eprintln!("record: no video for camera {camera} ({e:#}): its pictures are saved as JPEG files");
                    return false;
                }
            }
        }
        let Some(stream) = state.streams[camera].as_mut() else { return false };
        match stream.push(image) {
            Push::Queued(index) => {
                self.send(Msg::Line(
                    json!({"t": t, "type": "frame", "video": name, "index": index, "status": status, "cam": camera, "feed": self.feeds})
                        .to_string(),
                ));
                true
            }
            Push::Full => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Push::Unusable => match stream.failure() {
                Some(reason) => {
                    // The encoder is gone: this camera goes on in JPEG files, and the recording says where it changed.
                    state.jpeg[camera] = true;
                    eprintln!("record: the video of camera {camera} failed ({reason}): its pictures are saved as JPEG files from now on");
                    self.send(Msg::Line(json!({"t": t, "type": "video_lost", "cam": camera, "file": name, "reason": reason}).to_string()));
                    false
                }
                // Only this picture does not fit (the camera changed its size): it goes in a file.
                None => false,
            },
        }
    }

    /// Keeps the picture of the first camera: `frames` for one camera.
    pub fn frame(&mut self, at: Instant, image: &Arc<RgbImage>, status: &'static str, force: bool) {
        self.frames(at, &[(0, Arc::clone(image))], status, force);
    }

    /// Notes something that happened. `data` is an object: its fields go next to `t` and `type`.
    pub fn event(&self, at: Instant, kind: &str, data: Value) {
        let mut line = Map::new();
        line.insert("t".into(), json!(self.seconds(at)));
        line.insert("type".into(), json!(kind));
        if let Value::Object(fields) = data {
            line.extend(fields);
        }
        self.send(Msg::Line(Value::Object(line).to_string()));
    }

    /// Waits until the pictures handed over so far are written (tests: the camera of a test is faster than any disk).
    #[cfg(test)]
    pub fn wait_idle(&self) {
        let started = Instant::now();
        while self.queued.load(Ordering::SeqCst) > 0 && started.elapsed() < Duration::from_secs(30) {
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Ends the recording and waits until everything is written (the engine does not wait: it drops the recorder).
    pub fn close(mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How many threads encode and write pictures at once. One thread keeps up with about 20 pictures a second at best
/// (encoding a 1280x720 picture and flushing the file take 20 to 45 ms), and with two cameras every picture the tracker
/// looks at is two pictures: 0.11.1 lost half of the pictures of `--record-all` that way.
fn writer_threads() -> usize {
    thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 4)
}

/// What the writer threads share. A message is taken together with its ticket, so that tickets follow the order in
/// which the messages were queued.
struct Inbox {
    rx: Receiver<Msg>,
    /// Tickets given so far, and pictures numbered so far.
    tickets: u64,
    pictures: u64,
}

/// The events file, and whose turn it is to append to it.
struct Events {
    /// The ticket that appends next.
    turn: u64,
    file: Option<std::fs::File>,
}

/// The writer keeps what it is given in the folder, in order. It ends when the recorder is dropped.
///
/// Several threads encode and write the pictures at the same time, which is the slow part, but a line is appended to
/// `events.jsonl` only when every message before it has been: the file reads in the order things happened, and a
/// `frame` line is only there once its picture is on the disk.
fn writer(
    folder: PathBuf,
    rx: Receiver<Msg>,
    begun: Instant,
    queued: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    videos: Option<Arc<Videos>>,
) {
    let file = match OpenOptions::new().create(true).append(true).open(folder.join("events.jsonl")) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("record: cannot write {}: {e}; this recording will be incomplete", folder.join("events.jsonl").display());
            None
        }
    };
    let reported = AtomicBool::new(false);
    let report = |what: String| {
        // Once: a full disk would otherwise fill the console as fast as the pictures come.
        if !reported.swap(true, Ordering::Relaxed) {
            eprintln!("record: {what}; the game goes on, the recording will be incomplete");
        }
    };
    let inbox = Mutex::new(Inbox { rx, tickets: 0, pictures: 0 });
    let events = Mutex::new(Events { turn: 0, file });
    let turn_changed = Condvar::new();
    // Appending is the one write that is not atomic: a line cut short by a power loss is skipped by the replay.
    let append = |events: &mut Events, line: &str| {
        if let Some(file) = events.file.as_mut() {
            let mut bytes = Vec::with_capacity(line.len() + 1);
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            if let Err(e) = file.write_all(&bytes) {
                report(format!("cannot write the events: {e}"));
            }
        }
    };
    let work = || {
        loop {
            let (msg, ticket, number) = {
                let mut inbox = inbox.lock().unwrap_or_else(PoisonError::into_inner);
                let Ok(msg) = inbox.rx.recv() else { break };
                let ticket = inbox.tickets;
                inbox.tickets += 1;
                let number = if matches!(msg, Msg::Frame { .. }) {
                    inbox.pictures += 1;
                    inbox.pictures
                } else {
                    0
                };
                (msg, ticket, number)
            };
            let is_frame = matches!(msg, Msg::Frame { .. });
            // The slow part, with the other threads doing theirs. A panic here must not stop the others from taking
            // their turn, which would hold up the whole recording for ever.
            let line = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
                Msg::Session(bytes) => {
                    if let Err(e) = write_atomic(&folder.join("session.json"), &bytes) {
                        report(format!("{e:#}"));
                    }
                    None
                }
                Msg::Frame { t, camera, feed, image, status } => {
                    let name = format!("frames/{number:06}.jpg");
                    let saved = encode_jpeg(&image, JPEG_QUALITY).and_then(|bytes| write_atomic(&folder.join(&name), &bytes));
                    match saved {
                        Ok(()) => {
                            Some(json!({"t": t, "type": "frame", "file": name, "status": status, "cam": camera, "feed": feed}).to_string())
                        }
                        Err(e) => {
                            report(format!("{e:#}"));
                            None
                        }
                    }
                }
                Msg::Line(line) => Some(line),
            }))
            .unwrap_or_else(|_| {
                report("a picture could not be written".into());
                None
            });
            let mut events = events.lock().unwrap_or_else(PoisonError::into_inner);
            while events.turn != ticket {
                events = turn_changed.wait(events).unwrap_or_else(PoisonError::into_inner);
            }
            if let Some(line) = line {
                append(&mut events, &line);
            }
            events.turn += 1;
            turn_changed.notify_all();
            drop(events);
            if is_frame {
                // The picture counts against the backlog until it is written: it is in memory until then.
                queued.fetch_sub(1, Ordering::SeqCst);
            }
        }
    };
    thread::scope(|scope| {
        for _ in 0..writer_threads() {
            scope.spawn(work);
        }
    });
    // The videos are ended after the last line before this one: each `ffmpeg` finishes its file, and what it kept and
    // lost is counted with the rest.
    let video = videos.as_ref().map(|v| v.finish());
    if let Some(video) = &video {
        dropped.fetch_add(video.lost, Ordering::Relaxed);
    }
    let t = (begun.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    let jpegs = inbox.lock().unwrap_or_else(PoisonError::into_inner).pictures;
    let mut events = events.lock().unwrap_or_else(PoisonError::into_inner);
    let mut closed = json!({"t": t, "type": "closed", "pictures": jpegs + video.as_ref().map_or(0, |v| v.written), "dropped": dropped.load(Ordering::Relaxed)});
    if let Some(video) = &video {
        closed["video_pictures"] = json!(video.written);
        if !video.failures.is_empty() {
            closed["video_errors"] = json!(video.failures);
        }
    }
    append(&mut events, &closed.to_string());
}

fn video_meta(ffmpeg: &Path) -> VideoMeta {
    VideoMeta {
        ffmpeg: ffmpeg.display().to_string(),
        codec: video::CODEC.to_string(),
        camera_crf: video::CAMERA_CRF,
        depth_crf: video::DEPTH_CRF,
    }
}

/// `base`, or `base-2`, `base-3`, ... when two recordings begin in the same second.
fn unique_folder(dir: &Path, base: &str) -> Result<PathBuf> {
    let mut candidate = dir.join(base);
    let mut n = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{base}-{n}"));
        n += 1;
        if n > 1000 {
            anyhow::bail!("too many recordings begun at {base}");
        }
    }
    Ok(candidate)
}

// ---- Date and time, without a library ------------------------------------------------------------------------

/// A date and a time of day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Civil {
    /// 2026-11-16T11:30:05
    pub fn iso(&self) -> String {
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// 2026-11-16_11-30-05: sorts by date and is a legal file name everywhere.
    pub fn folder_name(&self) -> String {
        format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", self.year, self.month, self.day, self.hour, self.minute, self.second)
    }

    /// The date and time of a moment given in seconds since 1970-01-01 00:00:00 UTC.
    pub fn from_unix(seconds: i64) -> Civil {
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        // Days since 0000-03-01 in 400-year cycles (the calendar repeats every 146097 days).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let day_of_era = z.rem_euclid(146_097);
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let m = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * m + 2) / 5 + 1) as u32;
        let month = if m < 10 { m + 3 } else { m - 9 } as u32;
        let year = (year_of_era + era * 400 + i64::from(month <= 2)) as i32;
        Civil { year, month, day, hour: (rest / 3600) as u32, minute: (rest % 3600 / 60) as u32, second: (rest % 60) as u32 }
    }
}

pub fn utc_now() -> Civil {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    Civil::from_unix(seconds)
}

/// The time on the clock of this computer. Only Windows can tell without a library; elsewhere `None`
/// and the UTC time is used.
#[cfg(windows)]
pub fn local_now() -> Option<Civil> {
    // The SYSTEMTIME of the Windows API.
    #[repr(C)]
    #[derive(Default)]
    struct SystemTimeFields {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    unsafe extern "system" {
        fn GetLocalTime(time: *mut SystemTimeFields);
    }
    let mut time = SystemTimeFields::default();
    // SAFETY: GetLocalTime only writes the structure it is given, which has the layout of SYSTEMTIME.
    unsafe { GetLocalTime(&mut time) };
    (time.year >= 1970 && time.month >= 1 && time.day >= 1).then_some(Civil {
        year: i32::from(time.year),
        month: u32::from(time.month),
        day: u32::from(time.day),
        hour: u32::from(time.hour),
        minute: u32::from(time.minute),
        second: u32::from(time.second),
    })
}

#[cfg(not(windows))]
pub fn local_now() -> Option<Civil> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_worked_out_from_seconds_since_1970() {
        assert_eq!(Civil::from_unix(0).iso(), "1970-01-01T00:00:00");
        assert_eq!(Civil::from_unix(1_700_000_000).iso(), "2023-11-14T22:13:20");
        // A leap day, and the day after the end of February in a year that is not one.
        assert_eq!(Civil::from_unix(1_709_164_800).iso(), "2024-02-29T00:00:00");
        assert_eq!(Civil::from_unix(1_740_787_200).iso(), "2025-03-01T00:00:00");
        assert_eq!(Civil::from_unix(1_700_000_000).folder_name(), "2023-11-14_22-13-20");
    }

    #[test]
    fn the_time_of_this_computer_is_a_plausible_one() {
        let now = local_now().unwrap_or_else(utc_now);
        assert!(now.year >= 2026, "{now:?}");
        assert!((1..=12).contains(&now.month) && (1..=31).contains(&now.day) && now.hour < 24);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicochess-record-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            format: FORMAT,
            program: "test".into(),
            started_utc: String::new(),
            started_local: None,
            source: "test".into(),
            game_id: "g".into(),
            start_fen: crate::game::fen_of(&shakmaty::Chess::default()),
            resumed_at: 0.0,
            record_every: 2.0,
            tracker: TrackerConfig::default(),
            calibration: None,
            picture: Picture::default(),
            calibration2: None,
            picture2: None,
            video: None,
        }
    }

    #[test]
    fn pictures_are_kept_every_so_often_or_when_forced_and_lines_follow_in_order() {
        let dir = temp_dir("frames");
        let options = RecordOptions::new(dir.clone(), Duration::from_secs(2));
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        let image = Arc::new(RgbImage::from_pixel(32, 24, image::Rgb([200, 100, 50])));
        recorder.frame(begun, &image, "learning", false); // the first one is always kept
        recorder.frame(begun + Duration::from_millis(500), &image, "watching", false); // too soon
        recorder.event(begun + Duration::from_millis(600), "manual", json!({"text": "e4"}));
        recorder.frame(begun + Duration::from_millis(700), &image, "moving", true); // forced
        recorder.frame(begun + Duration::from_millis(2800), &image, "watching", false); // 2 s after the last kept
        let folder = recorder.folder().to_path_buf();
        recorder.close();

        let lines: Vec<Value> =
            fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let kinds: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["frame", "manual", "frame", "frame", "closed"], "{lines:?}");
        assert_eq!(lines[0]["file"], "frames/000001.jpg");
        assert_eq!(lines[2]["status"], "moving");
        assert_eq!(lines[1]["text"], "e4");
        assert_eq!(lines[1]["t"], 0.6);
        assert_eq!(lines[4]["pictures"], 3);
        for n in 1..=3 {
            let picture = image::open(folder.join(format!("frames/{n:06}.jpg"))).unwrap();
            assert_eq!((picture.width(), picture.height()), (32, 24));
        }
        let session: SessionMeta = serde_json::from_slice(&fs::read(folder.join("session.json")).unwrap()).unwrap();
        assert_eq!(session.game_id, "g");
        assert!(session.started_utc.starts_with("20"), "{}", session.started_utc);
        // The folder is named after the start.
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 19, "{name}");
        assert!(name.as_bytes()[4] == b'-' && name.as_bytes()[10] == b'_', "{name}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeping_every_picture_leaves_none_out_whatever_the_status() {
        let dir = temp_dir("all");
        let options = RecordOptions::keeping_all(dir.clone());
        assert!(options.keeps_all() && !RecordOptions::new(dir.clone(), Duration::ZERO).keeps_all());
        assert!(options.backlog > DEFAULT_BACKLOG);
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        let image = Arc::new(RgbImage::from_pixel(16, 12, image::Rgb([10, 20, 30])));
        // Quiet pictures, 40 ms apart, none of them forced: all are kept.
        for i in 0..10u64 {
            recorder.frame(begun + Duration::from_millis(40 * i), &image, "watching", false);
            recorder.wait_idle();
        }
        let folder = recorder.folder().to_path_buf();
        recorder.close();
        let lines: Vec<Value> =
            fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.iter().filter(|l| l["type"] == "frame").count(), 10, "{lines:?}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pictures_written_by_several_threads_still_read_in_the_order_they_were_given() {
        let dir = temp_dir("order");
        let options = RecordOptions { backlog: 100_000, ..RecordOptions::keeping_all(dir.clone()) };
        assert!(writer_threads() >= 2);
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        // Pictures of different sizes take different times to encode, so that the threads finish out of order if
        // nothing holds them back; a line comes after every third feed, and each must land between its pictures.
        let big = Arc::new(RgbImage::from_fn(640, 480, |x, y| image::Rgb([(x * 7 + y * 3) as u8, (x ^ y) as u8, (x * y) as u8])));
        let small = Arc::new(RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3])));
        let feeds = 60u64;
        for i in 0..feeds {
            let at = begun + Duration::from_millis(40 * i);
            recorder.frames(at, &[(0, Arc::clone(&big)), (2, Arc::clone(&small))], "watching", false);
            if i % 3 == 2 {
                recorder.event(at, "manual", json!({"after": i}));
            }
        }
        let folder = recorder.folder().to_path_buf();
        assert_eq!(recorder.dropped(), 0);
        recorder.close();

        let lines: Vec<Value> =
            fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let mut expected = Vec::new();
        for i in 0..feeds {
            expected.push(format!("frame {} 0", i + 1));
            expected.push(format!("frame {} 2", i + 1));
            if i % 3 == 2 {
                expected.push(format!("manual {i}"));
            }
        }
        expected.push("closed".into());
        let seen: Vec<String> = lines
            .iter()
            .map(|l| match l["type"].as_str().unwrap() {
                "frame" => format!("frame {} {}", l["feed"], l["cam"]),
                "manual" => format!("manual {}", l["after"]),
                other => other.to_string(),
            })
            .collect();
        assert_eq!(seen, expected);
        // The files are numbered in the order of the lines, and all are there.
        let files: Vec<&str> = lines.iter().filter(|l| l["type"] == "frame").map(|l| l["file"].as_str().unwrap()).collect();
        for (i, file) in files.iter().enumerate() {
            assert_eq!(*file, format!("frames/{:06}.jpg", i + 1));
            assert!(folder.join(file).exists(), "{file}");
        }
        assert_eq!(lines.last().unwrap()["pictures"], 2 * feeds);
        fs::remove_dir_all(&dir).ok();
    }

    fn read_lines(folder: &Path) -> Vec<Value> {
        fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn with_ffmpeg_the_pictures_go_into_a_video_for_each_camera_and_the_lines_say_which_picture() {
        use crate::video::{PictureRef, Pictures};
        let dir = temp_dir("video");
        let ffmpeg = crate::video::tests::fake_ffmpeg();
        let options = RecordOptions { backlog: 100_000, ..RecordOptions::keeping_all(dir.clone()) }.with_ffmpeg(Some(ffmpeg.clone()));
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        let camera = Arc::new(RgbImage::from_pixel(32, 24, image::Rgb([200, 100, 50])));
        let depth = Arc::new(RgbImage::from_fn(16, 12, |x, y| image::Rgb([(x * 9) as u8, (y * 9) as u8, 7])));
        for i in 0..6u64 {
            let at = begun + Duration::from_millis(40 * i);
            recorder.frames(at, &[(0, Arc::clone(&camera)), (2, Arc::clone(&depth))], "watching", false);
            if i == 2 {
                recorder.event(at, "manual", json!({"text": "e4"}));
            }
        }
        let folder = recorder.folder().to_path_buf();
        assert_eq!(recorder.dropped(), 0);
        recorder.close();

        let lines = read_lines(&folder);
        let seen: Vec<String> = lines
            .iter()
            .map(|l| match l["type"].as_str().unwrap() {
                "video" => format!("video {} {}x{}", l["file"].as_str().unwrap(), l["width"], l["height"]),
                "frame" => format!("frame cam{} feed{} {}#{}", l["cam"], l["feed"], l["video"].as_str().unwrap(), l["index"]),
                other => other.to_string(),
            })
            .collect();
        let mut expected = Vec::new();
        for i in 0..6u64 {
            if i == 0 {
                expected.push("video cam0.mkv 32x24".to_string());
            }
            expected.push(format!("frame cam0 feed{} cam0.mkv#{i}", i + 1));
            if i == 0 {
                expected.push("video cam2.mkv 16x12".to_string());
            }
            expected.push(format!("frame cam2 feed{} cam2.mkv#{i}", i + 1));
            if i == 2 {
                expected.push("manual".to_string());
            }
        }
        expected.push("closed".to_string());
        assert_eq!(seen, expected);
        assert!(lines.iter().filter(|l| l["type"] == "frame").all(|l| l.get("file").is_none()));
        let closed = lines.last().unwrap();
        assert_eq!(
            (closed["pictures"].as_u64(), closed["video_pictures"].as_u64(), closed["dropped"].as_u64()),
            (Some(12), Some(12), Some(0))
        );
        assert!(closed.get("video_errors").is_none(), "{closed}");
        assert_eq!(fs::read_dir(folder.join("frames")).unwrap().count(), 0);
        let session: SessionMeta = serde_json::from_slice(&fs::read(folder.join("session.json")).unwrap()).unwrap();
        let video = session.video.expect("the session tells that the pictures are in video");
        assert_eq!(video.ffmpeg, ffmpeg.display().to_string());
        assert_eq!(video.codec, crate::video::CODEC);

        let mut pictures = Pictures::with_ffmpeg(&folder, ffmpeg.clone());
        for line in lines.iter().filter(|l| l["type"] == "video") {
            pictures.note_video(line);
        }
        let at = |name: &str, index: u64| PictureRef { file: None, video: Some(name.into()), index };
        assert_eq!(&pictures.load(&at("cam0.mkv", 4)).unwrap(), camera.as_ref());
        assert_eq!(&pictures.load(&at("cam2.mkv", 5)).unwrap(), depth.as_ref());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_video_that_dies_leaves_the_rest_of_the_pictures_in_files_and_the_recording_says_so() {
        // The stand-in ends at once when the path of its output has `fakedie` in it.
        let dir = temp_dir("fakedie");
        let options = RecordOptions { backlog: 100_000, ..RecordOptions::keeping_all(dir.clone()) }
            .with_ffmpeg(Some(crate::video::tests::fake_ffmpeg()));
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        let image = Arc::new(RgbImage::from_pixel(32, 24, image::Rgb([9, 9, 9])));
        let started = Instant::now();
        let mut n = 0u64;
        let died = |recorder: &Recorder| recorder.video.as_ref().is_some_and(|v| v.state.lock().unwrap().jpeg[0]);
        while !died(&recorder) && started.elapsed() < Duration::from_secs(20) {
            recorder.frame(begun + Duration::from_millis(40 * n), &image, "watching", false);
            n += 1;
            thread::sleep(Duration::from_millis(10));
        }
        assert!(died(&recorder), "the failure of the encoder was not noticed");
        for _ in 0..3 {
            recorder.frame(begun + Duration::from_millis(40 * n), &image, "watching", false);
            n += 1;
            recorder.wait_idle();
        }
        let folder = recorder.folder().to_path_buf();
        recorder.close();

        let lines = read_lines(&folder);
        let lost_at = lines.iter().position(|l| l["type"] == "video_lost").expect("a video_lost line");
        assert_eq!(lines[lost_at]["cam"], 0);
        let after: Vec<&Value> = lines[lost_at..].iter().filter(|l| l["type"] == "frame").collect();
        // The picture that found out about the failure goes in a file too, with the three that come after it.
        assert_eq!(after.len(), 4, "the pictures after the failure are in files");
        for line in &after {
            assert!(folder.join(line["file"].as_str().unwrap()).is_file());
        }
        let closed = lines.last().unwrap();
        assert!(
            closed["video_errors"].as_array().is_some_and(|e| !e.is_empty()),
            "the end of the recording tells that an encoder failed: {closed}"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_picture_that_a_video_cannot_hold_goes_into_a_file() {
        // A video needs even sizes: 15x9 is kept as a JPEG file, and no video is started for it.
        let dir = temp_dir("odd");
        let options = RecordOptions { backlog: 100_000, ..RecordOptions::keeping_all(dir.clone()) }
            .with_ffmpeg(Some(crate::video::tests::fake_ffmpeg()));
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        let odd = Arc::new(RgbImage::from_pixel(15, 9, image::Rgb([1, 2, 3])));
        recorder.frame(begun, &odd, "watching", false);
        recorder.wait_idle();
        let folder = recorder.folder().to_path_buf();
        recorder.close();
        let lines = read_lines(&folder);
        assert!(lines.iter().all(|l| l["type"] != "video"), "{lines:?}");
        assert_eq!(lines[0]["file"], "frames/000001.jpg");
        assert!(folder.join("frames/000001.jpg").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_program_that_is_not_there_means_files_and_the_session_does_not_claim_video() {
        let dir = temp_dir("missing");
        let options = RecordOptions::keeping_all(dir.clone()).with_ffmpeg(Some(dir.join("no-such-ffmpeg")));
        let begun = Instant::now();
        let mut recorder = Recorder::begin(&options, meta(), begun).unwrap();
        recorder.frame(begun, &Arc::new(RgbImage::from_pixel(32, 24, image::Rgb([1, 2, 3]))), "watching", false);
        recorder.wait_idle();
        let folder = recorder.folder().to_path_buf();
        recorder.close();
        let session: SessionMeta = serde_json::from_slice(&fs::read(folder.join("session.json")).unwrap()).unwrap();
        assert!(session.video.is_none());
        assert!(folder.join("frames/000001.jpg").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_recordings_in_the_same_second_get_two_folders() {
        let dir = temp_dir("twice");
        let options = RecordOptions::new(dir.clone(), Duration::ZERO);
        let a = Recorder::begin(&options, meta(), Instant::now()).unwrap();
        let b = Recorder::begin(&options, meta(), Instant::now()).unwrap();
        assert_ne!(a.folder(), b.folder());
        a.close();
        b.close();
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(&dir).ok();
    }
}
