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
//! - `events.jsonl`: one JSON object per line, in order, each with `t` (seconds since the recording began)
//!   and a `type`: `frame` (the file, the tracker's status, the camera `cam` and the number `feed` that the
//!   pictures given together share), `detect` (moves found, and the position after),
//!   `unsure` (the candidates offered), `manual`, `undo`, `promotion`, `relearn`, `calibration`, `end`, and
//!   `closed` last. Positions are FEN strings.
//!
//! Nothing here may disturb the game: the capture thread only hands pictures and lines to a writer thread
//! (`try_send`, never waiting for the disk); when the disk cannot keep up, pictures are dropped and counted.
//! A failure to write is reported once on the console and the game goes on.

use crate::calibration::Calibration;
use crate::engine::encode_jpeg;
use crate::fsutil::write_atomic;
use crate::picture::Picture;
use crate::tracker::TrackerConfig;
use anyhow::{Context, Result};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The format of the files below: a replay refuses a recording of a newer format. Format 2 added the second camera
/// (`cam` and `feed` on a `frame` line, `calibration2` and `picture2` in `session.json`); a recording of format 1 is
/// read as one made with one camera.
pub const FORMAT: u32 = 2;

/// Pictures that may wait for the writer: more than this and the next ones are dropped (a picture is about a
/// megabyte in memory), so that a slow disk costs pictures, not memory.
pub const DEFAULT_BACKLOG: usize = 12;
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
}

impl RecordOptions {
    pub fn new(dir: PathBuf, every: Duration) -> RecordOptions {
        RecordOptions { dir, every, backlog: DEFAULT_BACKLOG }
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

        let (tx, rx) = sync_channel(QUEUE + options.backlog);
        let queued = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_folder = folder.clone();
        let (writer_queued, writer_dropped) = (Arc::clone(&queued), Arc::clone(&dropped));
        let thread = thread::Builder::new()
            .name("record".into())
            .spawn(move || writer(writer_folder, rx, now, writer_queued, writer_dropped))
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

/// The writer thread keeps what it is given in the folder, in order. It ends when the recorder is dropped.
fn writer(folder: PathBuf, rx: Receiver<Msg>, begun: Instant, queued: Arc<AtomicUsize>, dropped: Arc<AtomicU64>) {
    let mut events = match OpenOptions::new().create(true).append(true).open(folder.join("events.jsonl")) {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("record: cannot write {}: {e}; this recording will be incomplete", folder.join("events.jsonl").display());
            None
        }
    };
    let mut reported = false;
    let mut report = |what: String| {
        // Once: a full disk would otherwise fill the console as fast as the pictures come.
        if !reported {
            reported = true;
            eprintln!("record: {what}; the game goes on, the recording will be incomplete");
        }
    };
    // Appending is the one write that is not atomic: a line cut short by a power loss is skipped by the replay.
    let mut append = |line: &str, report: &mut dyn FnMut(String)| {
        if let Some(file) = events.as_mut() {
            let mut bytes = Vec::with_capacity(line.len() + 1);
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            if let Err(e) = file.write_all(&bytes) {
                report(format!("cannot write the events: {e}"));
            }
        }
    };
    let mut n = 0u64;
    for msg in rx {
        match msg {
            Msg::Session(bytes) => {
                if let Err(e) = write_atomic(&folder.join("session.json"), &bytes) {
                    report(format!("{e:#}"));
                }
            }
            Msg::Frame { t, camera, feed, image, status } => {
                queued.fetch_sub(1, Ordering::SeqCst);
                n += 1;
                let name = format!("frames/{n:06}.jpg");
                let saved = encode_jpeg(&image, JPEG_QUALITY).and_then(|bytes| write_atomic(&folder.join(&name), &bytes));
                match saved {
                    Ok(()) => append(
                        &json!({"t": t, "type": "frame", "file": name, "status": status, "cam": camera, "feed": feed}).to_string(),
                        &mut report,
                    ),
                    Err(e) => report(format!("{e:#}")),
                }
            }
            Msg::Line(line) => append(&line, &mut report),
        }
    }
    let t = (begun.elapsed().as_secs_f64() * 1000.0).round() / 1000.0;
    append(&json!({"t": t, "type": "closed", "pictures": n, "dropped": dropped.load(Ordering::Relaxed)}).to_string(), &mut report);
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
