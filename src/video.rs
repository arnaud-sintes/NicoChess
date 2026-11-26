//! Pictures kept as video, through an external `ffmpeg` program (`--record` and `--record-all` when one is found).
//!
//! A JPEG file for every picture is big (about 250 KB at 1280x720 with the noise of a cheap camera, so `--record-all`
//! writes several megabytes a second) and costs a JPEG encoding per picture. A video of the same pictures is five to
//! twenty times smaller, because the board hardly changes, and one `ffmpeg` process per camera does the encoding on
//! cores of its own. Nothing is linked: the program only starts `ffmpeg` and pipes the raw pictures (`rgb24`) into
//! it, so the Android and the 32-bit Linux builds are not touched and the package does not grow. Without an `ffmpeg`
//! the recorder keeps writing JPEG files.
//!
//! The video is a plain Matroska file (it can be read even when the program was killed before it ended) with the
//! nominal rate of 25 pictures a second: the real time of each picture is in `events.jsonl`, next to the number of the
//! picture in the video. Reading it back (a replay, the tools) starts `ffmpeg` again and reads the raw pictures from
//! its output, so `ffmpeg` is needed to look at a recording made this way.
//!
//! Where `ffmpeg` is looked for, in this order: `--ffmpeg <path>`, the variable `NICOCHESS_FFMPEG`, next to the program,
//! then the folders of `PATH`.

use anyhow::{Context, Result, bail};
use image::RgbImage;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};

/// Quality of the pictures of a camera: x264 `-crf` (lower is better and bigger). On the recordings of 2026-11-24 the
/// tracker decided exactly as it did on the JPEG pictures at 18, 20 and 23 (one certainty differed by a point), and
/// not at 28 (two detections that lead to positions the game never had). 23 is the chosen quality: about 2.7 times
/// smaller than 20, and a crop of it still reads as well as one of the original (the keyframes of `tools/groundtruth`
/// number 28 instead of the 26 of the JPEG pictures on the clean game). docs/video-recording.md has the measurements.
pub const CAMERA_CRF: u32 = 23;
/// The same for the height picture of the depth camera, which has smooth areas where a hand or a piece shows as a
/// bright spot: the tracker decided the same with it at 18, 20 and 23, and 23 makes it about 1.5 times smaller.
pub const DEPTH_CRF: u32 = 23;
/// The extension of the video files.
pub const FILE_EXTENSION: &str = "mkv";
/// The fewest pictures that may wait for the encoder of one video.
pub const MIN_QUEUE: usize = 8;
/// The rate the video says it has. Only a label: the time of each picture comes from the events.
const NOMINAL_FPS: u32 = 25;
/// A keyframe about every two seconds of the video (the pictures are read in order, so this only bounds the cost of
/// jumping into the middle).
const KEYFRAME_EVERY: u32 = 50;
/// The codec as it is written in the session.
pub const CODEC: &str = "libx264 veryfast";

static EXPLICIT: OnceLock<PathBuf> = OnceLock::new();

/// Sets the `ffmpeg` to use (`--ffmpeg`): it beats every other place.
pub fn set_explicit(path: PathBuf) {
    let _ = EXPLICIT.set(path);
}

fn program_name() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

/// The `ffmpeg` to use, if there is one: the one named with `--ffmpeg`, the one in `NICOCHESS_FFMPEG`, the one next to
/// the program, or the first on `PATH`.
pub fn find_ffmpeg() -> Option<PathBuf> {
    if let Some(path) = EXPLICIT.get() {
        return Some(path.clone());
    }
    if let Some(path) = std::env::var_os("NICOCHESS_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(path);
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
        let beside = dir.join(program_name());
        if beside.is_file() {
            return Some(beside);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program_name())).find(|candidate| candidate.is_file())
}

/// A command for `ffmpeg` that keeps out of the way of the game: no console window and a lower priority on Windows (the
/// tracker must never wait for the encoder).
fn command(ffmpeg: &Path) -> Command {
    let mut command = Command::new(ffmpeg);
    command.arg("-hide_banner").args(["-loglevel", "error"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        command.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
    command
}

/// In the tests of this crate a path with this name stands for an `ffmpeg` that is run inside the test, not as a process:
/// it writes the raw pictures to the file as they are and reads them back as they are. So the plumbing (the queue, the
/// numbering, the order, the lines, the failure of an encoder) is tested without a real `ffmpeg`; one ignored test
/// uses a real one.
#[cfg(test)]
pub const FAKE_FFMPEG: &str = "fake-ffmpeg-for-tests";

#[cfg(test)]
fn is_fake(path: &Path) -> bool {
    path == Path::new(FAKE_FFMPEG)
}

#[cfg(not(test))]
fn is_fake(_: &Path) -> bool {
    false
}

/// True when this `ffmpeg` can be started (it is a file; or the stand-in of the tests).
pub fn usable(path: &Path) -> bool {
    is_fake(path) || path.is_file()
}

/// Where the encoder thread puts the raw pictures: the input of an `ffmpeg` process.
enum Sink {
    Process {
        child: Child,
        stdin: Option<ChildStdin>,
    },
    /// The stand-in of the tests: a file, or an encoder that dies at once (the path of its output has `fakedie` in it).
    #[cfg(test)]
    Fake {
        file: File,
        dies: bool,
    },
}

impl Sink {
    fn open(ffmpeg: &Path, file: &Path, log: &Path, (width, height): (u32, u32), crf: u32) -> Result<Sink> {
        #[cfg(test)]
        if is_fake(ffmpeg) {
            let dies = file.to_string_lossy().contains("fakedie");
            return Ok(Sink::Fake { file: File::create(file).with_context(|| format!("cannot write {}", file.display()))?, dies });
        }
        let log_file = File::create(log).with_context(|| format!("cannot write {}", log.display()))?;
        let mut command = command(ffmpeg);
        command
            .args(["-y", "-f", "rawvideo", "-pix_fmt", "rgb24", "-s"])
            .arg(format!("{width}x{height}"))
            .args(["-framerate", &NOMINAL_FPS.to_string(), "-i", "-", "-an"])
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", &crf.to_string(), "-g", &KEYFRAME_EVERY.to_string()])
            // Two threads are plenty at 25 pictures a second (about five times faster than real time) and leave the
            // other cores to the tracker.
            .args(["-threads", "2", "-pix_fmt", "yuv420p", "-f", "matroska"])
            .arg(file)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file));
        let mut child = command.spawn().with_context(|| format!("cannot start {}", ffmpeg.display()))?;
        let stdin = child.stdin.take();
        Ok(Sink::Process { child, stdin })
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Process { stdin, .. } => stdin.as_mut().ok_or(std::io::ErrorKind::BrokenPipe)?.write_all(bytes),
            #[cfg(test)]
            Sink::Fake { file, dies } => {
                if *dies {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                } else {
                    file.write_all(bytes)
                }
            }
        }
    }

    /// Closes the input and waits until the file is complete.
    fn finish(self) -> std::result::Result<(), String> {
        match self {
            Sink::Process { mut child, stdin } => {
                drop(stdin);
                match child.wait() {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(format!("ffmpeg ended with {status}")),
                    Err(e) => Err(format!("cannot wait for ffmpeg: {e}")),
                }
            }
            #[cfg(test)]
            Sink::Fake { dies, .. } => {
                if dies {
                    Err("the stand-in encoder died".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// What the encoder thread and the recorder tell each other about one video.
#[derive(Default)]
struct Shared {
    /// Pictures written to the encoder, and pictures that were accepted but could not be (the encoder died).
    written: AtomicU64,
    lost: AtomicU64,
    failed: AtomicBool,
    reason: Mutex<String>,
}

impl Shared {
    fn fail(&self, reason: String) {
        let mut text = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.failed.swap(true, Ordering::SeqCst) {
            *text = reason;
        }
    }
}

/// The answer of [`VideoWriter::push`].
#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    /// Taken: it is the picture with this number in the video.
    Queued(u64),
    /// The encoder is too slow: this picture is lost (the caller counts it).
    Full,
    /// The encoder is gone (or this picture is of another size than the video): this picture and the next ones must
    /// go elsewhere.
    Unusable,
}

/// One video being written: a thread feeds the pictures to an `ffmpeg` process.
pub struct VideoWriter {
    tx: Option<SyncSender<Arc<RgbImage>>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    width: u32,
    height: u32,
    /// Pictures accepted so far: the number the next one gets.
    queued: u64,
}

/// What a finished video holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Pictures that were written to the encoder.
    pub written: u64,
    /// Pictures that were accepted and then lost because the encoder ended early.
    pub lost: u64,
    /// Why the encoder failed, if it did.
    pub failure: Option<String>,
}

impl VideoWriter {
    /// Starts `ffmpeg` (in a thread of its own, so that the caller never waits for the process to start) to write the
    /// pictures of `width` x `height` into `file`. Its messages go to `log`, which is removed if there are none. At most
    /// `queue` pictures wait for the encoder: past that, [`push`](Self::push) says `Full`.
    pub fn start(ffmpeg: &Path, file: PathBuf, log: PathBuf, width: u32, height: u32, crf: u32, queue: usize) -> Result<VideoWriter> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("a video needs even sizes, not {width}x{height}");
        }
        let (tx, rx) = sync_channel::<Arc<RgbImage>>(queue.max(1));
        let shared = Arc::new(Shared::default());
        let thread = thread::Builder::new()
            .name("video".into())
            .spawn({
                let shared = Arc::clone(&shared);
                let ffmpeg = ffmpeg.to_path_buf();
                move || encode(&ffmpeg, &file, &log, (width, height), crf, rx, &shared)
            })
            .context("cannot start the video thread")?;
        Ok(VideoWriter { tx: Some(tx), shared, thread: Some(thread), width, height, queued: 0 })
    }

    /// Hands one picture to the encoder, without waiting.
    pub fn push(&mut self, image: &Arc<RgbImage>) -> Push {
        if self.shared.failed.load(Ordering::SeqCst) || (image.width(), image.height()) != (self.width, self.height) {
            return Push::Unusable;
        }
        let Some(tx) = &self.tx else { return Push::Unusable };
        match tx.try_send(Arc::clone(image)) {
            Ok(()) => {
                self.queued += 1;
                Push::Queued(self.queued - 1)
            }
            Err(TrySendError::Full(_)) => Push::Full,
            Err(TrySendError::Disconnected(_)) => Push::Unusable,
        }
    }

    /// True when the encoder has failed: what was written so far stays, nothing more can be.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::SeqCst)
    }

    /// Why the encoder failed.
    pub fn failure(&self) -> Option<String> {
        self.failed().then(|| self.shared.reason.lock().unwrap_or_else(PoisonError::into_inner).clone())
    }

    /// Ends the video: waits until the encoder has written the file.
    pub fn finish(mut self) -> Totals {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        Totals {
            written: self.shared.written.load(Ordering::SeqCst),
            lost: self.shared.lost.load(Ordering::SeqCst),
            failure: self.failure(),
        }
    }
}

/// The encoder thread: starts `ffmpeg`, feeds it the pictures it is given until the queue is closed, then waits for it
/// to finish the file. When anything fails the pictures that arrive are counted as lost, never held up.
fn encode(
    ffmpeg: &Path,
    file: &Path,
    log: &Path,
    (width, height): (u32, u32),
    crf: u32,
    rx: std::sync::mpsc::Receiver<Arc<RgbImage>>,
    shared: &Shared,
) {
    let mut sink = match Sink::open(ffmpeg, file, log, (width, height), crf) {
        Ok(sink) => Some(sink),
        Err(e) => {
            shared.fail(format!("{e:#}"));
            None
        }
    };
    for image in rx.iter() {
        match sink.as_mut() {
            Some(sink) if !shared.failed.load(Ordering::SeqCst) => match sink.write(image.as_raw()) {
                Ok(()) => {
                    shared.written.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => {
                    shared.fail(format!("cannot write to ffmpeg: {e}"));
                    shared.lost.fetch_add(1, Ordering::SeqCst);
                }
            },
            _ => {
                shared.lost.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
    if let Some(Err(reason)) = sink.map(Sink::finish) {
        shared.fail(reason);
    }
    if std::fs::metadata(log).is_ok_and(|m| m.len() == 0) {
        let _ = std::fs::remove_file(log);
    }
}

// ---- Reading pictures back --------------------------------------------------------------------------------------------

/// Where a recorded picture is: a JPEG file, or a picture of a video.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PictureRef {
    pub file: Option<String>,
    pub video: Option<String>,
    pub index: u64,
}

impl PictureRef {
    /// The picture a `frame` event or one of the entries of its `files` tells of.
    pub fn of_event(event: &Value) -> Option<PictureRef> {
        let file = event["file"].as_str().filter(|f| !f.is_empty()).map(str::to_string);
        let video = event["video"].as_str().filter(|f| !f.is_empty()).map(str::to_string);
        if file.is_none() && video.is_none() {
            return None;
        }
        Some(PictureRef { file, video, index: event["index"].as_u64().unwrap_or(0) })
    }

    /// A short name for messages: the file, or the video and the number.
    pub fn name(&self) -> String {
        match (&self.file, &self.video) {
            (Some(file), _) => file.clone(),
            (None, Some(video)) => format!("{video}#{}", self.index),
            (None, None) => String::new(),
        }
    }
}

/// `ffmpeg` giving the pictures of a video, one after the other.
struct VideoReader {
    /// The `ffmpeg` process (none for the stand-in of the tests, which reads the file as it is).
    child: Option<Child>,
    out: Box<dyn Read + Send>,
    /// The number of the picture that the next read gives.
    next: u64,
    width: u32,
    height: u32,
}

impl VideoReader {
    fn open(ffmpeg: &Path, file: &Path, width: u32, height: u32) -> Result<VideoReader> {
        #[cfg(test)]
        if is_fake(ffmpeg) {
            let out = BufReader::new(File::open(file).with_context(|| format!("cannot read {}", file.display()))?);
            return Ok(VideoReader { child: None, out: Box::new(out), next: 0, width, height });
        }
        let mut command = command(ffmpeg);
        command.arg("-i").arg(file).args(["-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"]);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", ffmpeg.display()))?;
        let out = BufReader::with_capacity(1 << 20, child.stdout.take().context("no output from ffmpeg")?);
        Ok(VideoReader { child: Some(child), out: Box::new(out), next: 0, width, height })
    }

    /// The picture with this number; pictures before it are read and thrown away.
    fn picture(&mut self, index: u64) -> Result<RgbImage> {
        let size = self.width as usize * self.height as usize * 3;
        while self.next < index {
            let skipped = std::io::copy(&mut (&mut self.out).take(size as u64), &mut std::io::sink())?;
            if skipped < size as u64 {
                bail!("the video ends before picture {index}");
            }
            self.next += 1;
        }
        let mut raw = vec![0u8; size];
        self.out.read_exact(&mut raw).with_context(|| format!("the video ends before picture {index}"))?;
        self.next += 1;
        RgbImage::from_raw(self.width, self.height, raw).context("a picture of the video has the wrong size")
    }
}

impl Drop for VideoReader {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn missing_ffmpeg_message(video: &str) -> String {
    format!(
        "this recording keeps its pictures in video files ({video}), which are read with ffmpeg: give it with --ffmpeg <path>, \
         or put ffmpeg next to the program or on PATH"
    )
}

/// The pictures of a recording: JPEG files, and videos read through `ffmpeg`.
pub struct Pictures {
    folder: PathBuf,
    /// The videos the recording tells of (`video` events): file name and size of the pictures.
    videos: HashMap<String, (u32, u32)>,
    readers: HashMap<String, VideoReader>,
    ffmpeg: Option<PathBuf>,
}

impl Pictures {
    pub fn new(folder: &Path) -> Pictures {
        Pictures { folder: folder.to_path_buf(), videos: HashMap::new(), readers: HashMap::new(), ffmpeg: None }
    }

    /// The same, reading the videos with this `ffmpeg` instead of looking for one.
    pub fn with_ffmpeg(folder: &Path, ffmpeg: PathBuf) -> Pictures {
        Pictures { ffmpeg: Some(ffmpeg), ..Pictures::new(folder) }
    }

    /// Takes note of a `video` event of the recording (the name and size of a video).
    pub fn note_video(&mut self, event: &Value) {
        if let (Some(file), Some(w), Some(h)) = (event["file"].as_str(), event["width"].as_u64(), event["height"].as_u64()) {
            self.videos.insert(file.to_string(), (w as u32, h as u32));
        }
    }

    /// True when the recording keeps pictures in videos.
    pub fn has_videos(&self) -> bool {
        !self.videos.is_empty()
    }

    /// Looks for `ffmpeg` now when the recording has videos, so that a missing one is told at once, plainly.
    pub fn require_ffmpeg_for_videos(&mut self) -> Result<()> {
        if self.has_videos() && self.ffmpeg.is_none() {
            let name = self.videos.keys().next().cloned().unwrap_or_default();
            self.ffmpeg = Some(find_ffmpeg().with_context(|| missing_ffmpeg_message(&name))?);
        }
        Ok(())
    }

    /// The picture, from its file or from its video.
    pub fn load(&mut self, picture: &PictureRef) -> Result<RgbImage> {
        if let Some(file) = &picture.file {
            return Ok(image::open(self.folder.join(file)).with_context(|| format!("cannot read the picture {file}"))?.to_rgb8());
        }
        let video = picture.video.as_deref().context("a picture without a file or a video")?;
        let &(width, height) = self.videos.get(video).with_context(|| format!("the recording does not describe the video {video}"))?;
        // Pictures are read in order: going back means reading the video again from its start.
        if self.readers.get(video).is_some_and(|r| r.next > picture.index) {
            self.readers.remove(video);
        }
        if !self.readers.contains_key(video) {
            let ffmpeg = match &self.ffmpeg {
                Some(path) => path.clone(),
                None => {
                    let found = find_ffmpeg().with_context(|| missing_ffmpeg_message(video))?;
                    self.ffmpeg = Some(found.clone());
                    found
                }
            };
            let reader = VideoReader::open(&ffmpeg, &self.folder.join(video), width, height)?;
            self.readers.insert(video.to_string(), reader);
        }
        let reader = self.readers.get_mut(video).context("video reader")?;
        reader.picture(picture.index).with_context(|| format!("cannot read picture {} of {video}", picture.index))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// The stand-in for `ffmpeg` (see [`FAKE_FFMPEG`]): it writes the raw pictures it is given to the file as they are, and
    /// reads them back as they are, so what goes in comes out unchanged.
    pub fn fake_ffmpeg() -> PathBuf {
        PathBuf::from(FAKE_FFMPEG)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicochess-video-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn picture(seed: u8) -> Arc<RgbImage> {
        Arc::new(RgbImage::from_fn(16, 8, |x, y| image::Rgb([seed.wrapping_add(x as u8 * 3), seed.wrapping_add(y as u8 * 5), seed])))
    }

    #[test]
    fn pictures_written_through_the_encoder_read_back_in_order_and_by_number() {
        let dir = scratch("roundtrip");
        let mut writer = VideoWriter::start(&fake_ffmpeg(), dir.join("cam0.mkv"), dir.join("cam0.log"), 16, 8, 20, 8).unwrap();
        let pictures: Vec<_> = (0..5).map(|i| picture(i * 40)).collect();
        for (i, p) in pictures.iter().enumerate() {
            assert_eq!(writer.push(p), Push::Queued(i as u64));
        }
        // A picture of another size cannot go into this video.
        assert_eq!(writer.push(&Arc::new(RgbImage::new(20, 8))), Push::Unusable);
        let totals = writer.finish();
        assert_eq!(totals, Totals { written: 5, lost: 0, failure: None });

        let mut reader = Pictures::with_ffmpeg(&dir, fake_ffmpeg());
        reader.note_video(&serde_json::json!({"type": "video", "file": "cam0.mkv", "width": 16, "height": 8}));
        let at = |index: u64| PictureRef { file: None, video: Some("cam0.mkv".into()), index };
        assert_eq!(&reader.load(&at(0)).unwrap(), pictures[0].as_ref());
        assert_eq!(&reader.load(&at(3)).unwrap(), pictures[3].as_ref());
        // Going back starts the video again.
        assert_eq!(&reader.load(&at(1)).unwrap(), pictures[1].as_ref());
        assert!(reader.load(&at(9)).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_video_needs_even_sizes_and_a_missing_program_loses_the_pictures_it_was_given_and_says_why() {
        let dir = scratch("failure");
        assert!(VideoWriter::start(Path::new("x"), dir.join("a.mkv"), dir.join("a.log"), 15, 8, 20, 2).is_err());
        let mut writer = VideoWriter::start(&dir.join("no-such-ffmpeg"), dir.join("a.mkv"), dir.join("a.log"), 16, 8, 20, 4).unwrap();
        let image = picture(1);
        let mut accepted = 0;
        let started = std::time::Instant::now();
        while !writer.failed() && started.elapsed() < std::time::Duration::from_secs(10) {
            if matches!(writer.push(&image), Push::Queued(_)) {
                accepted += 1;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(writer.failed());
        assert_eq!(writer.push(&image), Push::Unusable);
        let totals = writer.finish();
        assert_eq!(totals.written + totals.lost, accepted, "every picture that was accepted is counted: {totals:?}");
        assert!(totals.failure.unwrap().contains("cannot start"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_picture_is_told_by_its_file_or_by_its_video() {
        let jpg = PictureRef::of_event(&serde_json::json!({"file": "frames/000001.jpg"})).unwrap();
        assert_eq!(jpg.name(), "frames/000001.jpg");
        let video = PictureRef::of_event(&serde_json::json!({"video": "cam2.mkv", "index": 7})).unwrap();
        assert_eq!((video.name().as_str(), video.index), ("cam2.mkv#7", 7));
        assert!(PictureRef::of_event(&serde_json::json!({"t": 1.0})).is_none());
    }

    /// With a real ffmpeg (`NICOCHESS_FFMPEG=C:\path\ffmpeg.exe cargo test -- --ignored a_real_ffmpeg`): pictures that
    /// go through x264 come back at the same size and close to what they were.
    #[test]
    #[ignore = "needs a real ffmpeg: set NICOCHESS_FFMPEG"]
    fn a_real_ffmpeg_writes_a_video_that_reads_back_close_to_the_pictures() {
        let Some(ffmpeg) = std::env::var_os("NICOCHESS_FFMPEG").map(PathBuf::from) else { panic!("set NICOCHESS_FFMPEG to an ffmpeg") };
        let dir = scratch("real");
        let mut writer = VideoWriter::start(&ffmpeg, dir.join("cam0.mkv"), dir.join("cam0.log"), 320, 240, CAMERA_CRF, 16).unwrap();
        let pictures: Vec<Arc<RgbImage>> = (0..30u32)
            .map(|i| {
                Arc::new(RgbImage::from_fn(320, 240, |x, y| {
                    image::Rgb([(x / 2 + i) as u8, (y / 2) as u8, if (x / 40 + y / 40) % 2 == 0 { 200 } else { 60 }])
                }))
            })
            .collect();
        for p in &pictures {
            while writer.push(p) == Push::Full {
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let totals = writer.finish();
        assert_eq!((totals.written, totals.lost, totals.failure), (30, 0, None));
        let mut reader = Pictures::with_ffmpeg(&dir, ffmpeg.clone());
        reader.note_video(&serde_json::json!({"file": "cam0.mkv", "width": 320, "height": 240}));
        let back = reader.load(&PictureRef { file: None, video: Some("cam0.mkv".into()), index: 17 }).unwrap();
        let (sum, count) = back
            .pixels()
            .zip(pictures[17].pixels())
            .fold((0u64, 0u64), |(s, c), (a, b)| (s + a.0.iter().zip(b.0).map(|(x, y)| u64::from(x.abs_diff(y))).sum::<u64>(), c + 3));
        assert!(sum as f64 / count as f64 <= 4.0, "mean difference {}", sum as f64 / count as f64);
        std::fs::remove_dir_all(&dir).ok();
    }
}
