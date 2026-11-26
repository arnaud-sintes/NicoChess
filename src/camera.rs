//! Frame sources: a real camera (V4L2 on Linux, Media Foundation on Windows), a still image,
//! a synthetic board, or a camera driven from outside (see `pushed`).

use crate::synthetic;
use anyhow::{Context, Result, bail};
use image::RgbImage;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// How long a real camera may stay silent before it counts as failed (the Android camera is the
/// app's business: see `pushed`).
#[cfg(any(windows, target_os = "linux"))]
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// How long opening a camera may take.
#[cfg(windows)]
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Something that produces camera frames.
pub trait FrameSource: Send {
    fn describe(&self) -> String;
    /// Blocks until the next frame is due, then returns it. It must not block for ever: a camera
    /// that stops delivering (unplugged, taken by another program) makes this return an error
    /// within a few seconds, so that the engine reports it and tries again.
    fn next_frame(&mut self) -> Result<RgbImage>;
    /// When the camera made the picture that `next_frame` returned last, for a source that knows it (one with a thread
    /// of its own that waits for the program). Without it the engine takes the time when `next_frame` returned, which
    /// hides any time the picture waited for the program to ask.
    fn captured_at(&self) -> Option<Instant> {
        None
    }
    /// Called when a game starts. Scripted sources begin again; a real camera does nothing.
    fn restart(&mut self) {}
}

/// Makes a source that has no timing of its own produce frames at a steady pace.
struct Pacer {
    interval: Duration,
    last: Option<Instant>,
}

impl Pacer {
    fn new(interval: Duration) -> Self {
        Pacer { interval, last: None }
    }

    fn wait(&mut self) {
        if let Some(last) = self.last {
            let elapsed = last.elapsed();
            if elapsed < self.interval {
                thread::sleep(self.interval - elapsed);
            }
        }
        self.last = Some(Instant::now());
    }
}

/// Always returns the same image, loaded from a file (JPEG).
pub struct StillImage {
    path: String,
    image: RgbImage,
    pacer: Pacer,
}

impl StillImage {
    pub fn open(path: &str, interval: Duration) -> Result<Self> {
        let image = image::open(path).with_context(|| format!("cannot open image {path}"))?.to_rgb8();
        Ok(StillImage { path: path.to_string(), image, pacer: Pacer::new(interval) })
    }
}

impl FrameSource for StillImage {
    fn describe(&self) -> String {
        format!("image file {}", self.path)
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        self.pacer.wait();
        Ok(self.image.clone())
    }
}

/// A generated chessboard in perspective, see [`synthetic`].
pub struct Synthetic {
    image: RgbImage,
    pacer: Pacer,
    second: bool,
}

impl Synthetic {
    pub fn new(interval: Duration) -> Self {
        Synthetic { image: synthetic::frame(), pacer: Pacer::new(interval), second: false }
    }

    /// The same board seen from the other side, as a second camera would.
    pub fn second(interval: Duration) -> Self {
        Synthetic { image: synthetic::seen_from_the_other_side(&synthetic::frame()), pacer: Pacer::new(interval), second: true }
    }
}

impl FrameSource for Synthetic {
    fn describe(&self) -> String {
        if self.second { "synthetic board, seen from the other side" } else { "synthetic board" }.to_string()
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        self.pacer.wait();
        Ok(self.image.clone())
    }
}

/// Converts packed YUYV (YUV 4:2:2) to RGB, BT.601 full range.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub fn yuyv_to_rgb(data: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let needed = width as usize * height as usize * 2;
    if data.len() < needed {
        bail!("YUYV frame too short: {} bytes, expected {needed}", data.len());
    }
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    let mut out = vec![0u8; width as usize * height as usize * 3];
    let (pairs_in, _) = data[..needed].as_chunks::<4>();
    let (pairs_out, _) = out.as_chunks_mut::<6>();
    for (src, dst) in pairs_in.iter().zip(pairs_out) {
        let (y0, u, y1, v) = (src[0] as i32, src[1] as i32 - 128, src[2] as i32, src[3] as i32 - 128);
        let r = (359 * v) >> 8;
        let g = ((88 * u) >> 8) + ((183 * v) >> 8);
        let b = (454 * u) >> 8;
        dst[0] = clamp(y0 + r);
        dst[1] = clamp(y0 - g);
        dst[2] = clamp(y0 + b);
        dst[3] = clamp(y1 + r);
        dst[4] = clamp(y1 - g);
        dst[5] = clamp(y1 + b);
    }
    RgbImage::from_raw(width, height, out).context("bad RGB buffer size")
}

/// Converts NV12 (a full-size Y plane, then interleaved half-size U and V) to RGB,
/// BT.601 full range. Many room and laptop cameras offer it.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub fn nv12_to_rgb(data: &[u8], width: u32, height: u32) -> Result<RgbImage> {
    let (w, h) = (width as usize, height as usize);
    let needed = w * h + (w * h) / 2;
    if w % 2 != 0 || h % 2 != 0 || data.len() < needed {
        bail!("NV12 frame has the wrong size: {} bytes for {width}x{height}", data.len());
    }
    let (luma, chroma) = data[..needed].split_at(w * h);
    let clamp = |v: i32| v.clamp(0, 255) as u8;
    let mut out = vec![0u8; w * h * 3];
    for row in 0..h {
        let uv_row = &chroma[(row / 2) * w..(row / 2) * w + w];
        for col in 0..w {
            let y = luma[row * w + col] as i32;
            let u = uv_row[col & !1] as i32 - 128;
            let v = uv_row[col | 1] as i32 - 128;
            let o = (row * w + col) * 3;
            out[o] = clamp(y + ((359 * v) >> 8));
            out[o + 1] = clamp(y - (((88 * u) >> 8) + ((183 * v) >> 8)));
            out[o + 2] = clamp(y + ((454 * u) >> 8));
        }
    }
    RgbImage::from_raw(width, height, out).context("bad RGB buffer size")
}

/// How many pictures a second are kept from a camera, which runs at its own pace (often 30 a second). Shared between
/// the engine, which the page tells, and the camera thread, which reads it for every picture: it can be changed while
/// the camera runs.
#[derive(Debug)]
pub struct FrameRate(AtomicU32);

impl FrameRate {
    pub fn new(fps: f32) -> Arc<FrameRate> {
        Arc::new(FrameRate(AtomicU32::new(fps.to_bits())))
    }

    pub fn fps(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, fps: f32) {
        self.0.store(fps.to_bits(), Ordering::Relaxed);
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs_f32(1.0 / self.fps().clamp(0.1, 1000.0))
    }
}

/// Chooses which of the pictures a camera delivers are kept, so that on average `interval` goes by between two kept
/// ones. A picture is kept when it comes at the due time or a little before it: a camera delivers at a steady pace
/// of its own, and "a picture every 33 ms" asked of a 30 pictures a second camera must keep every one of them. (Keeping
/// a picture only once a full interval has gone by since the last kept one loses every other picture there, and asking
/// for 25 or 30 a second gives 15.) The due times follow a schedule, so no error builds up.
#[derive(Debug, Default)]
pub struct Decimator {
    due: Option<Instant>,
}

impl Decimator {
    pub fn keep(&mut self, now: Instant, interval: Duration) -> bool {
        let Some(due) = self.due else {
            self.due = Some(now + interval);
            return true;
        };
        if now + interval / 4 < due {
            return false;
        }
        let next = due + interval;
        // After a long pause (a stalled program, a camera that was away) the schedule starts again from now.
        self.due = Some(if next + interval < now { now + interval } else { next });
        true
    }
}

/// How many pictures in a row that cannot be decoded are skipped before the camera is called failed.
const MAX_BAD_PICTURES: u32 = 10;

/// Counts the pictures in a row that could not be decoded. A truncated picture is routine on a busy USB bus (a webcam
/// beside another camera on one controller): it is skipped, not a reason to close the camera and open it again, which
/// would leave the game without pictures for several seconds. Only a camera that keeps giving them is failed.
#[derive(Default)]
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
struct BadPictures(u32);

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
impl BadPictures {
    /// Takes the result of decoding a picture: the picture, `None` for one to skip, or the error of a camera that fails.
    fn check(&mut self, decoded: Result<RgbImage>) -> Result<Option<RgbImage>> {
        match decoded {
            Ok(image) => {
                self.0 = 0;
                Ok(Some(image))
            }
            Err(e) => {
                self.0 += 1;
                if self.0 == 1 {
                    eprintln!("camera: a picture could not be decoded ({e:#}); skipped");
                }
                if self.0 >= MAX_BAD_PICTURES {
                    Err(e.context(format!("{MAX_BAD_PICTURES} pictures in a row could not be decoded")))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

/// Settings for the real camera.
#[derive(Clone, Debug)]
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub struct CameraSettings {
    /// Linux: the device path (/dev/video0). Windows: the camera number (0, 1, ...).
    pub device: String,
    pub width: u32,
    pub height: u32,
    /// How many pictures a second are kept. Shared, so that it can be changed while the camera runs.
    pub rate: Arc<FrameRate>,
    /// Turn off automatic exposure and white balance, which drift and upset detection.
    pub lock_controls: bool,
    /// Manual exposure time in units of 100 microseconds (UVC), if wanted.
    pub exposure: Option<i64>,
    /// Manual white balance temperature in Kelvin, if wanted.
    pub white_balance: Option<i64>,
    /// Manual focus position, if wanted (turns autofocus off).
    pub focus: Option<i64>,
    /// Zoom level of the camera, if wanted (a wide camera may crop or zoom by itself).
    pub zoom: Option<i64>,
}

#[cfg(target_os = "linux")]
pub use v4l_camera::{V4lCamera, probe};

#[cfg(windows)]
pub use windows_camera::{WindowsCamera, list_cameras as system_cameras, probe};

/// The cameras of this computer that the page can choose among. Only Windows has the choice for now (Linux
/// is given a device on the command line, and the Android app has its own).
#[cfg(not(windows))]
pub fn system_cameras() -> Result<Vec<crate::choice::CameraInfo>> {
    Ok(Vec::new())
}

/// Prints what a camera offers. Only available on Linux and Windows.
#[cfg(not(any(target_os = "linux", windows)))]
pub fn probe(_path: &str) -> Result<String> {
    bail!("--probe is only available on Linux and Windows")
}

/// Webcams on Windows, through Media Foundation.
#[cfg(windows)]
mod windows_camera {
    use super::*;
    use nokhwa::Camera;
    use nokhwa::utils::{
        ApiBackend, CameraFormat, CameraIndex, ControlValueSetter, FrameFormat, KnownCameraControl, RequestedFormat, RequestedFormatType,
    };
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Mutex, PoisonError};

    const BACKEND: ApiBackend = ApiBackend::MediaFoundation;
    const WANTED: &[FrameFormat] = &[FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12, FrameFormat::RAWRGB, FrameFormat::RAWBGR];

    /// "0", "1", ... pick a camera. Anything else (such as the Linux default
    /// /dev/video0) means the first one.
    fn index_of(device: &str) -> u32 {
        device.trim().parse().unwrap_or(0)
    }

    fn open_raw(index: u32) -> Result<Camera> {
        let request = RequestedFormat::with_formats(RequestedFormatType::None, WANTED);
        Camera::with_backend(CameraIndex::Index(index), request, BACKEND).map_err(|e| anyhow::anyhow!("cannot open camera {index}: {e}"))
    }

    /// The formats to try, best first, as on Linux: large frames are better as
    /// MJPEG, small ones as YUYV, which needs no decoding.
    fn rank(format: FrameFormat, width: u32, height: u32) -> usize {
        let large = u64::from(width) * u64::from(height) > 640 * 480;
        let order: [FrameFormat; 5] = if large {
            [FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12, FrameFormat::RAWRGB, FrameFormat::RAWBGR]
        } else {
            [FrameFormat::YUYV, FrameFormat::MJPEG, FrameFormat::NV12, FrameFormat::RAWRGB, FrameFormat::RAWBGR]
        };
        order.iter().position(|f| *f == format).unwrap_or(order.len())
    }

    fn describe_formats(camera: &mut Camera) -> Vec<String> {
        let mut formats = camera.compatible_camera_formats().unwrap_or_default();
        formats.sort_by_key(|f| (f.format().to_string(), f.resolution().width(), f.resolution().height(), f.frame_rate()));
        formats
            .iter()
            .map(|f| format!("{} {}x{} at {} fps", f.format(), f.resolution().width(), f.resolution().height(), f.frame_rate()))
            .collect::<Vec<_>>()
            .into_iter()
            .fold(Vec::new(), |mut unique, line| {
                // The same size is often listed several times, once per frame rate range.
                if !unique.contains(&line) {
                    unique.push(line);
                }
                unique
            })
    }

    /// The cameras Windows offers, with the number that `--device` takes.
    pub fn list_cameras() -> Result<Vec<crate::choice::CameraInfo>> {
        let cameras = nokhwa::query(BACKEND).map_err(|e| anyhow::anyhow!("cannot list the cameras: {e}"))?;
        Ok(cameras.iter().map(|info| crate::choice::CameraInfo { id: info.index().to_string(), name: info.human_name() }).collect())
    }

    pub fn probe(device: &str) -> Result<String> {
        let mut out = String::new();
        let cameras = nokhwa::query(BACKEND).map_err(|e| anyhow::anyhow!("cannot list the cameras: {e}"))?;
        out.push_str("Cameras (use --device <number>):\n");
        if cameras.is_empty() {
            out.push_str("  none found: is the camera plugged in, and allowed in Settings > Privacy > Camera?\n");
        }
        for info in &cameras {
            out.push_str(&format!("  {}: {} ({})\n", info.index(), info.human_name(), info.description()));
        }
        let index = index_of(device);
        let mut camera = open_raw(index)?;
        out.push_str(&format!("\nCamera {index}, formats and frame sizes:\n"));
        for line in describe_formats(&mut camera) {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str("\nControls (Windows units: for exposure usually a power of two, e.g. -6 is 1/64 s):\n");
        match camera.camera_controls() {
            Ok(controls) => {
                for c in controls {
                    out.push_str(&format!("  {c}\n"));
                }
            }
            Err(e) => out.push_str(&format!("  cannot list the controls: {e}\n")),
        }
        Ok(out)
    }

    /// The camera itself. It is not `Send`, so it lives on one thread only.
    struct Grabber {
        index: u32,
        camera: Camera,
        width: u32,
        height: u32,
        format: FrameFormat,
        rate: Arc<FrameRate>,
        keeper: Decimator,
        bad: BadPictures,
    }

    impl Grabber {
        fn open(settings: &CameraSettings) -> Result<Self> {
            let index = index_of(&settings.device);
            let mut camera = open_raw(index)?;
            let offered = camera.compatible_camera_formats().unwrap_or_default();
            for line in describe_formats(&mut camera) {
                eprintln!("camera: offers {line}");
            }

            // The size asked for, then the best format and a frame rate of at most 30.
            let mut candidates: Vec<CameraFormat> = offered
                .into_iter()
                .filter(|f| {
                    f.resolution().width() == settings.width && f.resolution().height() == settings.height && WANTED.contains(&f.format())
                })
                .collect();
            candidates.sort_by_key(|f| {
                let fps = f.frame_rate();
                (rank(f.format(), settings.width, settings.height), if fps <= 30 { 30 - fps } else { 1000 + fps })
            });
            let chosen = *candidates.first().with_context(|| {
                format!(
                    "the camera offers none of MJPEG, YUYV, NV12 or RGB at {}x{} (see the \"offers\" lines, or run --probe)",
                    settings.width, settings.height
                )
            })?;
            camera
                .set_camera_requset(RequestedFormat::with_formats(RequestedFormatType::Exact(chosen), WANTED))
                .map_err(|e| anyhow::anyhow!("cannot select {chosen}: {e}"))?;
            apply_controls(&mut camera, settings);
            camera.open_stream().map_err(|e| anyhow::anyhow!("cannot start the capture stream: {e}"))?;

            let format = camera.camera_format();
            eprintln!(
                "camera: {index} {}x{} {} at {} fps",
                format.resolution().width(),
                format.resolution().height(),
                format.format(),
                format.frame_rate()
            );
            Ok(Grabber {
                index,
                camera,
                width: format.resolution().width(),
                height: format.resolution().height(),
                format: format.format(),
                rate: Arc::clone(&settings.rate),
                keeper: Decimator::default(),
                bad: BadPictures::default(),
            })
        }
    }

    /// Best effort: a camera may not support these controls.
    ///
    /// Locking: a control is set to the value it has now. Media Foundation then
    /// takes it as manual, so exposure, white balance and focus stop drifting.
    fn apply_controls(camera: &mut Camera, settings: &CameraSettings) {
        let wanted: [(&str, KnownCameraControl, Option<i64>); 4] = [
            ("zoom", KnownCameraControl::Zoom, settings.zoom),
            ("focus", KnownCameraControl::Focus, settings.focus),
            ("white balance", KnownCameraControl::WhiteBalance, settings.white_balance),
            ("exposure", KnownCameraControl::Exposure, settings.exposure),
        ];
        for (name, control, value) in wanted {
            let setter = match value {
                Some(v) => Some(ControlValueSetter::Integer(v)),
                None if settings.lock_controls && control != KnownCameraControl::Zoom => {
                    camera.camera_control(control).ok().map(|c| c.value())
                }
                None => None,
            };
            let Some(setter) = setter else { continue };
            match camera.set_camera_control(control, setter) {
                Ok(()) => eprintln!("camera: {name}: set"),
                Err(e) => eprintln!("camera: {name}: not applied ({e})"),
            }
        }
    }

    impl Grabber {
        fn describe(&self) -> String {
            format!("camera {} ({}x{} {})", self.index, self.width, self.height, self.format)
        }

        fn next_frame(&mut self) -> Result<RgbImage> {
            // The camera runs at its own pace (often 30 fps): only the frames
            // that are due get converted.
            loop {
                let buffer = self.camera.frame().map_err(|e| anyhow::anyhow!("camera read failed: {e}"))?;
                if !self.keeper.keep(Instant::now(), self.rate.interval()) {
                    continue;
                }
                let data = buffer.buffer();
                let (w, h) = (self.width, self.height);
                let decoded = (|| -> Result<RgbImage> {
                    Ok(match buffer.source_frame_format() {
                        FrameFormat::YUYV => yuyv_to_rgb(data, w, h)?,
                        FrameFormat::NV12 => nv12_to_rgb(data, w, h)?,
                        // `into_rgb8` moves the decoded picture; `to_rgb8` would copy all of it.
                        FrameFormat::MJPEG => {
                            image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).context("bad MJPEG frame")?.into_rgb8()
                        }
                        FrameFormat::RAWRGB => RgbImage::from_raw(w, h, data.to_vec()).context("bad RGB frame size")?,
                        FrameFormat::RAWBGR => {
                            let mut rgb = data.to_vec();
                            rgb.as_chunks_mut::<3>().0.iter_mut().for_each(|p| p.swap(0, 2));
                            RgbImage::from_raw(w, h, rgb).context("bad BGR frame size")?
                        }
                        FrameFormat::GRAY => {
                            let rgb: Vec<u8> = data.iter().flat_map(|&g| [g, g, g]).collect();
                            RgbImage::from_raw(w, h, rgb).context("bad gray frame size")?
                        }
                    })
                })();
                if let Some(image) = self.bad.check(decoded)? {
                    return Ok(image);
                }
            }
        }
    }

    /// The cameras whose thread is still running. A driver that hangs leaves its thread blocked for good (it cannot be
    /// stopped from outside); opening the camera again would then leave one more such thread every few seconds, and
    /// find the camera "in use". So a camera is not opened while the thread of the last opening is still there.
    static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    struct Running(u32);

    impl Running {
        /// Waits a moment for the thread of a camera that was closed just now to end (it ends within one picture),
        /// then takes the camera. `None` when the thread is stuck.
        fn take(index: u32) -> Option<Running> {
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                {
                    let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
                    if !running.contains(&index) {
                        running.push(index);
                        return Some(Running(index));
                    }
                }
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for Running {
        fn drop(&mut self) {
            RUNNING.lock().unwrap_or_else(PoisonError::into_inner).retain(|&i| i != self.0);
        }
    }

    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    mod tests {
        use super::*;

        #[test]
        fn a_camera_is_not_opened_again_while_the_thread_of_the_last_opening_is_stuck() {
            // Camera numbers that no other test uses: the list is shared by the whole process.
            let stuck = Running::take(901).expect("free");
            let started = Instant::now();
            assert!(Running::take(901).is_none(), "the thread is still there");
            assert!(started.elapsed() >= Duration::from_millis(1400), "a moment is given for it to end");
            // Another camera is not held up.
            assert!(Running::take(902).is_some());
            // The thread of a camera that was just closed ends within a picture: its camera can be opened at once.
            let ending = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                drop(stuck);
            });
            assert!(Running::take(901).is_some(), "taken as soon as the thread ended");
            ending.join().unwrap();
        }
    }

    /// What the program uses: frames come from the thread that owns the camera.
    /// Dropping it stops the thread and closes the camera.
    pub struct WindowsCamera {
        description: String,
        /// Each picture comes with the time the camera thread got it from the camera: the picture may then wait here
        /// (one is kept ahead, and the camera waits for the program), and the engine wants to know how long.
        frames: Receiver<Result<(Instant, RgbImage)>>,
        captured: Option<Instant>,
    }

    impl WindowsCamera {
        pub fn open(settings: &CameraSettings) -> Result<Self> {
            let settings = settings.clone();
            let Some(running) = Running::take(index_of(&settings.device)) else {
                bail!("the camera driver did not answer the last time and has not let go of the camera: unplug it and plug it in again");
            };
            let (ready_tx, ready_rx) = mpsc::channel::<Result<String>>();
            // One frame at most is kept ahead: the camera waits for the program.
            let (frames_tx, frames_rx) = mpsc::sync_channel::<Result<(Instant, RgbImage)>>(1);
            thread::Builder::new()
                .name("camera".into())
                .spawn(move || {
                    // Dropped when this thread ends, whatever the way.
                    let _running = running;
                    let mut grabber = match Grabber::open(&settings) {
                        Ok(g) => {
                            let _ = ready_tx.send(Ok(g.describe()));
                            g
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                    loop {
                        let frame = grabber.next_frame().map(|image| (Instant::now(), image));
                        let failed = frame.is_err();
                        // The receiver is gone when the camera is closed.
                        if frames_tx.send(frame).is_err() || failed {
                            return;
                        }
                    }
                })
                .context("cannot start the camera thread")?;
            // Opening a camera can hang in the driver: do not wait for it for ever.
            let description = match ready_rx.recv_timeout(OPEN_TIMEOUT) {
                Ok(opened) => opened?,
                Err(mpsc::RecvTimeoutError::Timeout) => bail!("the camera did not open within {} seconds", OPEN_TIMEOUT.as_secs()),
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the camera thread stopped"),
            };
            Ok(WindowsCamera { description, frames: frames_rx, captured: None })
        }
    }

    impl FrameSource for WindowsCamera {
        fn describe(&self) -> String {
            self.description.clone()
        }

        fn next_frame(&mut self) -> Result<RgbImage> {
            match self.frames.recv_timeout(FRAME_TIMEOUT) {
                Ok(frame) => frame.map(|(at, image)| {
                    self.captured = Some(at);
                    image
                }),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!("no picture from the camera for {} seconds (is another program using it?)", FRAME_TIMEOUT.as_secs())
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the camera thread stopped"),
            }
        }

        fn captured_at(&self) -> Option<Instant> {
            self.captured
        }
    }
}

#[cfg(target_os = "linux")]
mod v4l_camera {
    use super::*;
    use v4l::buffer::Type;
    use v4l::control::{Control, Value};
    use v4l::io::mmap::Stream;
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;
    use v4l::{Device, FourCC};

    // Control ids from <linux/v4l2-controls.h>.
    const CID_AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
    const CID_WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
    const CID_EXPOSURE_AUTO: u32 = 0x009a_0901;
    const CID_EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    const CID_EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
    const CID_FOCUS_ABSOLUTE: u32 = 0x009a_090a;
    const CID_FOCUS_AUTO: u32 = 0x009a_090c;
    const CID_ZOOM_ABSOLUTE: u32 = 0x009a_090d;
    /// Value of `CID_EXPOSURE_AUTO` for manual mode.
    const EXPOSURE_MANUAL: i64 = 1;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum PixelFormat {
        Yuyv,
        Mjpeg,
        Nv12,
    }

    /// The formats to try, best first. Large frames go over USB as MJPEG, which is
    /// compact; small frames are better as YUYV, which needs no decoding.
    fn preference(width: u32, height: u32) -> [(&'static [u8; 4], PixelFormat); 3] {
        if u64::from(width) * u64::from(height) > 640 * 480 {
            [(b"MJPG", PixelFormat::Mjpeg), (b"YUYV", PixelFormat::Yuyv), (b"NV12", PixelFormat::Nv12)]
        } else {
            [(b"YUYV", PixelFormat::Yuyv), (b"MJPG", PixelFormat::Mjpeg), (b"NV12", PixelFormat::Nv12)]
        }
    }

    /// One line per pixel format: the frame sizes the camera offers.
    fn describe_formats(device: &Device) -> Vec<String> {
        let mut lines = Vec::new();
        let Ok(formats) = device.enum_formats() else { return lines };
        for format in formats {
            let mut sizes: Vec<String> = Vec::new();
            for size in device.enum_framesizes(format.fourcc).unwrap_or_default() {
                match size.size {
                    v4l::framesize::FrameSizeEnum::Discrete(d) => sizes.push(format!("{}x{}", d.width, d.height)),
                    v4l::framesize::FrameSizeEnum::Stepwise(s) => {
                        sizes.push(format!("{}x{} to {}x{}", s.min_width, s.min_height, s.max_width, s.max_height))
                    }
                }
            }
            sizes.dedup();
            lines.push(format!("{} ({}): {}", format.fourcc, format.description, sizes.join(", ")));
        }
        lines
    }

    /// Everything useful to know about a camera, as text: used by `--probe`.
    pub fn probe(path: &str) -> Result<String> {
        let device = Device::with_path(path).with_context(|| format!("cannot open {path}"))?;
        let mut out = String::new();
        if let Ok(caps) = device.query_caps() {
            out.push_str(&format!("{path}: {} (driver {}, {})\n", caps.card, caps.driver, caps.bus));
        }
        out.push_str("\nFormats and frame sizes:\n");
        let formats = describe_formats(&device);
        if formats.is_empty() {
            out.push_str("  none: this is probably not the video capture node, try the next /dev/video*\n");
        }
        for line in formats {
            out.push_str(&format!("  {line}\n"));
        }
        out.push_str("\nControls (name, id, range):\n");
        match device.query_controls() {
            Ok(controls) => {
                for c in controls {
                    out.push_str(&format!(
                        "  {} (0x{:08x}): {:?}, {} to {}, step {}, default {}\n",
                        c.name, c.id, c.typ, c.minimum, c.maximum, c.step, c.default
                    ));
                }
            }
            Err(e) => out.push_str(&format!("  cannot list the controls: {e}\n")),
        }
        Ok(out)
    }

    pub struct V4lCamera {
        path: String,
        // Kept alive for as long as the stream is used.
        _device: Device,
        stream: Stream<'static>,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        rate: Arc<FrameRate>,
        keeper: Decimator,
        bad: BadPictures,
    }

    impl V4lCamera {
        pub fn open(settings: &CameraSettings) -> Result<Self> {
            let device = Device::with_path(&settings.device).with_context(|| format!("cannot open {}", settings.device))?;

            for line in describe_formats(&device) {
                eprintln!("camera: offers {line}");
            }
            let mut chosen = None;
            for (code, pixel_format) in preference(settings.width, settings.height) {
                let wanted = v4l::Format::new(settings.width, settings.height, FourCC::new(code));
                match device.set_format(&wanted) {
                    Ok(got) if got.fourcc == FourCC::new(code) => {
                        chosen = Some((got, pixel_format));
                        break;
                    }
                    Ok(got) => eprintln!("camera: {} not accepted, driver offered {}", FourCC::new(code), got.fourcc),
                    Err(e) => eprintln!("camera: cannot set {}: {e}", FourCC::new(code)),
                }
            }
            let (format, pixel_format) =
                chosen.context("the camera offers none of YUYV, MJPEG or NV12 at this size (see the \"offers\" lines, or run --probe)")?;
            eprintln!("camera: {} {}x{} {:?}", settings.device, format.width, format.height, pixel_format);

            apply_controls(&device, settings);

            let mut stream = Stream::with_buffers(&device, Type::VideoCapture, 4).context("cannot start the capture stream")?;
            // A camera that stops delivering must end the read, not block it for ever.
            stream.set_timeout(FRAME_TIMEOUT);

            Ok(V4lCamera {
                path: settings.device.clone(),
                _device: device,
                stream,
                width: format.width,
                height: format.height,
                pixel_format,
                rate: Arc::clone(&settings.rate),
                keeper: Decimator::default(),
                bad: BadPictures::default(),
            })
        }
    }

    /// Best effort: a camera may not support these controls.
    fn apply_controls(device: &Device, settings: &CameraSettings) {
        let mut wanted: Vec<(&str, u32, Value)> = Vec::new();
        if settings.lock_controls {
            wanted.push(("auto white balance off", CID_AUTO_WHITE_BALANCE, Value::Boolean(false)));
            wanted.push(("manual exposure", CID_EXPOSURE_AUTO, Value::Integer(EXPOSURE_MANUAL)));
            wanted.push(("exposure auto priority off", CID_EXPOSURE_AUTO_PRIORITY, Value::Boolean(false)));
            // Autofocus hunting would change the picture while a game is played.
            wanted.push(("autofocus off", CID_FOCUS_AUTO, Value::Boolean(false)));
        }
        if let Some(zoom) = settings.zoom {
            wanted.push(("zoom", CID_ZOOM_ABSOLUTE, Value::Integer(zoom)));
        }
        if let Some(focus) = settings.focus {
            wanted.push(("autofocus off", CID_FOCUS_AUTO, Value::Boolean(false)));
            wanted.push(("focus position", CID_FOCUS_ABSOLUTE, Value::Integer(focus)));
        }
        if let Some(kelvin) = settings.white_balance {
            wanted.push(("white balance temperature", CID_WHITE_BALANCE_TEMPERATURE, Value::Integer(kelvin)));
        }
        if let Some(exposure) = settings.exposure {
            wanted.push(("exposure time", CID_EXPOSURE_ABSOLUTE, Value::Integer(exposure)));
        }
        for (name, id, value) in wanted {
            match device.set_control(Control { id, value }) {
                Ok(()) => eprintln!("camera: {name}: set"),
                Err(e) => eprintln!("camera: {name}: not applied ({e})"),
            }
        }
    }

    impl FrameSource for V4lCamera {
        fn describe(&self) -> String {
            format!("camera {} ({}x{} {:?})", self.path, self.width, self.height, self.pixel_format)
        }

        fn next_frame(&mut self) -> Result<RgbImage> {
            // The camera runs at its own pace (often 30 fps). Waiting for each
            // buffer is cheap; only the frames that are due get converted.
            loop {
                let (buffer, meta) =
                    self.stream.next().context("camera read failed (no picture within the time allowed: unplugged or in use?)")?;
                if !self.keeper.keep(Instant::now(), self.rate.interval()) {
                    continue;
                }
                let used = (meta.bytesused as usize).min(buffer.len());
                let data = &buffer[..used];
                let decoded = (|| -> Result<RgbImage> {
                    Ok(match self.pixel_format {
                        PixelFormat::Yuyv => yuyv_to_rgb(data, self.width, self.height)?,
                        PixelFormat::Nv12 => nv12_to_rgb(data, self.width, self.height)?,
                        PixelFormat::Mjpeg => {
                            image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).context("bad MJPEG frame")?.into_rgb8()
                        }
                    })
                })();
                if let Some(image) = self.bad.check(decoded)? {
                    return Ok(image);
                }
            }
        }
    }
}

/// A scripted game shown on the synthetic board: the Opera game, one move every
/// few seconds, with a piece lifted off the board for a moment before each move
/// (a hand over the board). It lets the whole program be tried without a camera.
pub struct DemoGame {
    /// What the board shows at each tick.
    script: Vec<synthetic::PieceGrid>,
    tick: usize,
    cache: Option<(synthetic::PieceGrid, RgbImage)>,
    pacer: Pacer,
    /// Seen from the other side of the board: the picture of a second camera.
    second: bool,
    /// Where the game is, shared with the other camera when there are two (see [`demo_pair`]).
    link: Option<Arc<AtomicUsize>>,
}

impl DemoGame {
    pub fn new(frame_interval: Duration, move_interval: Duration) -> Self {
        // Each move needs a lifted frame, then enough still frames to be recognised.
        let per_move = ((move_interval.as_secs_f32() / frame_interval.as_secs_f32()).round() as usize).max(6);
        let steps = synthetic::game_steps(&synthetic::OPERA_GAME);
        let mut script = Vec::new();
        // Time to start the game on the page before the first move.
        script.extend(std::iter::repeat_n(synthetic::start_grid(), per_move * 3));
        for step in &steps {
            let mut lifted = step.before;
            if let Some(from) = step.mv.from() {
                lifted[usize::from(from)] = None;
            }
            script.push(lifted);
            script.extend(std::iter::repeat_n(step.after, per_move - 1));
        }
        DemoGame { script, tick: 0, cache: None, pacer: Pacer::new(frame_interval), second: false, link: None }
    }

    /// One of two cameras that see the same game (see [`demo_pair`]): the first plays the game on and says where it
    /// is in `shared`, the second, on the other side of the board, shows the same moment of it.
    fn linked(frame_interval: Duration, move_interval: Duration, shared: Arc<AtomicUsize>, second: bool) -> DemoGame {
        DemoGame { second, link: Some(shared), ..DemoGame::new(frame_interval, move_interval) }
    }
}

impl FrameSource for DemoGame {
    fn describe(&self) -> String {
        if self.second { "demo game (Opera game, seen from the other side)" } else { "demo game (Opera game, synthetic board)" }.to_string()
    }

    fn restart(&mut self) {
        // The camera that follows the other one has nothing to start again: it shows where the first is.
        if !self.second {
            self.tick = 0;
            if let Some(shared) = &self.link {
                shared.store(0, Ordering::SeqCst);
            }
        }
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        self.pacer.wait();
        let last = self.script.len() - 1;
        let index = match (&self.link, self.second) {
            (Some(shared), true) => shared.load(Ordering::SeqCst).min(last),
            (link, _) => {
                let index = self.tick.min(last);
                self.tick += 1;
                if let Some(shared) = link {
                    shared.store(index, Ordering::SeqCst);
                }
                index
            }
        };
        let grid = self.script[index];
        if let Some((cached, image)) = &self.cache
            && *cached == grid
        {
            return Ok(image.clone());
        }
        let mut image = synthetic::frame_from_grid(&grid);
        if self.second {
            image = synthetic::seen_from_the_other_side(&image);
        }
        self.cache = Some((grid, image.clone()));
        Ok(image)
    }
}

/// Where frames come from.
#[derive(Clone)]
pub enum SourceSpec {
    Camera(CameraSettings),
    /// The starting position on a synthetic board.
    Synthetic,
    /// A scripted game, see [`DemoGame`].
    Demo {
        move_interval: Duration,
    },
    /// A fixed image file.
    Image(String),
    /// Pictures pushed in by the host program (the Android app), see [`crate::pushed`].
    Pushed(std::sync::Arc<crate::pushed::FrameInbox>),
}

/// Opens a source each time the camera is needed, and drops it when it is not.
pub type SourceFactory = Box<dyn Fn() -> Result<Box<dyn FrameSource>> + Send + Sync>;

pub fn factory(spec: SourceSpec, interval: Duration) -> SourceFactory {
    factory_with(spec, interval, None)
}

/// Like [`factory`]; when a camera is opened, the one that `selection` names at that moment is used, so that
/// the person can change it from the page.
pub fn factory_with(
    spec: SourceSpec,
    interval: Duration,
    selection: Option<std::sync::Arc<crate::choice::CameraSelection>>,
) -> SourceFactory {
    Box::new(move || -> Result<Box<dyn FrameSource>> {
        match &spec {
            SourceSpec::Camera(settings) => {
                let mut settings = settings.clone();
                if let Some(selection) = &selection {
                    settings.device = selection.device();
                }
                open_camera(&settings)
            }
            SourceSpec::Synthetic => Ok(Box::new(Synthetic::new(interval))),
            SourceSpec::Demo { move_interval } => Ok(Box::new(DemoGame::new(interval, *move_interval))),
            SourceSpec::Image(path) => Ok(Box::new(StillImage::open(path, interval)?)),
            SourceSpec::Pushed(inbox) => Ok(Box::new(crate::pushed::PushedSource::open(inbox.clone()))),
        }
    })
}

/// The demo game seen by two cameras on opposite sides of the board: the factories of the first and the second. The
/// second shows the moment of the game that the first shows. (Two clocks of their own would drift apart whenever one
/// camera's thread was held up, and the cameras would then show different moments, which no real pair does.)
pub fn demo_pair(frame_interval: Duration, move_interval: Duration) -> (SourceFactory, SourceFactory) {
    let (first, second, _) = demo_trio(frame_interval, move_interval);
    (first, second)
}

/// The same, and a depth camera beside the first that shows the same moment of the game too (`--depth-camera`).
pub fn demo_trio(frame_interval: Duration, move_interval: Duration) -> (SourceFactory, SourceFactory, crate::depthlink::DepthFactory) {
    let shared = Arc::new(AtomicUsize::new(0));
    let make = |second: bool| -> SourceFactory {
        let shared = Arc::clone(&shared);
        Box::new(move || -> Result<Box<dyn FrameSource>> {
            Ok(Box::new(DemoGame::linked(frame_interval, move_interval, Arc::clone(&shared), second)))
        })
    };
    // The depth pictures of the game are drawn once (there are not many different boards in it), when the camera is
    // first opened; drawing one takes longer than a frame.
    let drawn: Arc<std::sync::OnceLock<Arc<Vec<Arc<crate::depth::DepthFrame>>>>> = Arc::new(std::sync::OnceLock::new());
    let depth_shared = Arc::clone(&shared);
    let depth: crate::depthlink::DepthFactory = Box::new(move |_, _| {
        let pictures = drawn.get_or_init(|| Arc::new(DemoDepth::draw(frame_interval, move_interval))).clone();
        Ok(Box::new(DemoDepth { pictures, shared: Arc::clone(&depth_shared), pacer: Pacer::new(frame_interval) })
            as Box<dyn crate::depthlink::DepthSource>)
    });
    (make(false), make(true), depth)
}

/// The depth camera of the demo: what a depth camera beside the first camera would see of the demo game.
struct DemoDepth {
    /// The picture for each moment of the game.
    pictures: Arc<Vec<Arc<crate::depth::DepthFrame>>>,
    /// Where the game is, said by the first camera.
    shared: Arc<AtomicUsize>,
    pacer: Pacer,
}

impl DemoDepth {
    fn draw(frame_interval: Duration, move_interval: Duration) -> Vec<Arc<crate::depth::DepthFrame>> {
        use std::collections::HashMap;
        let script = DemoGame::new(frame_interval, move_interval).script;
        let corners = crate::depthlink::demo_depth_corners();
        let mut known: HashMap<synthetic::PieceGrid, Arc<crate::depth::DepthFrame>> = HashMap::new();
        script
            .iter()
            .enumerate()
            .map(|(i, grid)| {
                Arc::clone(known.entry(*grid).or_insert_with(|| {
                    let occupied = grid.map(|piece| piece.is_some());
                    let black = grid.map(|piece| piece == Some(shakmaty::Color::Black));
                    // Dark pieces give no reading on their top, as a dark matte surface may.
                    Arc::new(crate::depth::fake::frame(320, 240, &corners, &occupied, &black, true, 0.03, i as u32))
                }))
            })
            .collect()
    }
}

impl crate::depthlink::DepthSource for DemoDepth {
    fn describe(&self) -> String {
        "demo depth camera (320x240, the Opera game)".to_string()
    }

    fn next_frame(&mut self) -> Result<crate::depth::DepthFrame> {
        self.pacer.wait();
        let index = self.shared.load(Ordering::SeqCst).min(self.pictures.len() - 1);
        Ok((*self.pictures[index]).clone())
    }
}

/// The source of the second camera. A real camera is the one `selection` names (none chosen: an error, which the
/// engine does not ask about, because it opens a second camera only when one is chosen); the synthetic board
/// shows the same board from the other side. The demo's second camera comes with its first (see [`demo_pair`]).
pub fn second_factory(
    spec: SourceSpec,
    interval: Duration,
    selection: Option<std::sync::Arc<crate::choice::CameraSelection>>,
) -> SourceFactory {
    Box::new(move || -> Result<Box<dyn FrameSource>> {
        match &spec {
            SourceSpec::Camera(settings) => {
                let device = selection.as_ref().map(|s| s.device()).unwrap_or_default();
                if device.is_empty() {
                    bail!("no second camera is chosen");
                }
                let mut settings = settings.clone();
                settings.device = device;
                open_camera(&settings)
            }
            SourceSpec::Synthetic => Ok(Box::new(Synthetic::second(interval))),
            SourceSpec::Demo { .. } | SourceSpec::Image(_) | SourceSpec::Pushed(_) => bail!("this source has no second camera"),
        }
    })
}

#[cfg(target_os = "linux")]
fn open_camera(settings: &CameraSettings) -> Result<Box<dyn FrameSource>> {
    Ok(Box::new(V4lCamera::open(settings)?))
}

#[cfg(windows)]
fn open_camera(settings: &CameraSettings) -> Result<Box<dyn FrameSource>> {
    Ok(Box::new(WindowsCamera::open(settings)?))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn open_camera(_settings: &CameraSettings) -> Result<Box<dyn FrameSource>> {
    bail!("camera capture is only implemented on Linux and Windows; use --source demo, synthetic or an image file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_game_plays_the_whole_script() {
        let mut demo = DemoGame::new(Duration::from_millis(1), Duration::from_millis(6));
        let first = demo.next_frame().unwrap();
        assert_eq!(first, synthetic::frame());
        // 3 * 6 lead-in ticks + 33 moves * 6 ticks.
        assert_eq!(demo.script.len(), 3 * 6 + 33 * 6);
        // The last frames hold the final position forever.
        for _ in 0..demo.script.len() + 5 {
            demo.next_frame().unwrap();
        }
    }

    #[test]
    fn yuyv_gray_stays_gray() {
        let data = [128u8, 128, 128, 128];
        let img = yuyv_to_rgb(&data, 2, 1).unwrap();
        for px in img.pixels() {
            assert!(px.0.iter().all(|&c| (c as i32 - 128).abs() <= 1), "{px:?}");
        }
    }

    #[test]
    fn yuyv_extremes() {
        // Y=255 with neutral chroma is white; Y=0 is black.
        let img = yuyv_to_rgb(&[255, 128, 0, 128], 2, 1).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn nv12_gray_and_extremes() {
        // 2x2 frame: four luma values, then one U and one V sample for the whole block.
        let img = nv12_to_rgb(&[255, 0, 128, 128, 128, 128], 2, 2).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
        assert!(img.get_pixel(0, 1).0.iter().all(|&c| (c as i32 - 128).abs() <= 1));
    }

    #[test]
    fn nv12_colour_is_shared_by_a_block() {
        // Strong red chroma (V high, U low) with mid luma: red up and blue down, in all four pixels.
        let img = nv12_to_rgb(&[128, 128, 128, 128, 90, 220], 2, 2).unwrap();
        for px in img.pixels() {
            assert!(px[0] > 200 && px[2] < 100, "{px:?}");
        }
    }

    #[test]
    fn nv12_rejects_bad_sizes() {
        assert!(nv12_to_rgb(&[0u8; 5], 2, 2).is_err());
        assert!(nv12_to_rgb(&[0u8; 20], 3, 2).is_err(), "odd width");
    }

    #[test]
    fn yuyv_rejects_short_buffer() {
        assert!(yuyv_to_rgb(&[0u8; 3], 2, 1).is_err());
    }

    /// Pictures kept in ten seconds from a camera that delivers 30 a second, with the jitter of a real one.
    fn kept_from_a_30_fps_camera(fps: f32) -> usize {
        let start = Instant::now();
        let period = Duration::from_secs_f32(1.0 / 30.0);
        let interval = Duration::from_secs_f32(1.0 / fps);
        let mut decimator = Decimator::default();
        let mut state = 12345u32;
        (0..300)
            .filter(|i| {
                // Up to +-4 ms of jitter on each arrival.
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let jitter = Duration::from_micros(u64::from((state >> 16) % 8000));
                let at = start + period * *i + jitter;
                decimator.keep(at, interval)
            })
            .count()
    }

    #[test]
    fn a_30_picture_camera_gives_the_rate_asked_for_not_half_of_it() {
        // Asking for as many as the camera makes keeps them all (it once kept every other one: 15 a second).
        assert!(kept_from_a_30_fps_camera(30.0) >= 295, "{}", kept_from_a_30_fps_camera(30.0));
        // Asking for more than it can give keeps them all too.
        assert!(kept_from_a_30_fps_camera(60.0) >= 295);
        // In between, close to what was asked for.
        let kept = kept_from_a_30_fps_camera(25.0);
        assert!((240..=262).contains(&kept), "{kept}");
        let kept = kept_from_a_30_fps_camera(10.0);
        assert!((97..=103).contains(&kept), "{kept}");
        let kept = kept_from_a_30_fps_camera(3.0);
        assert!((29..=31).contains(&kept), "{kept}");
        let kept = kept_from_a_30_fps_camera(1.0);
        assert!((9..=11).contains(&kept), "{kept}");
    }

    #[test]
    fn the_schedule_starts_again_after_a_long_pause() {
        let start = Instant::now();
        let interval = Duration::from_millis(100);
        let mut decimator = Decimator::default();
        assert!(decimator.keep(start, interval));
        assert!(!decimator.keep(start + Duration::from_millis(40), interval));
        // The program was stopped for ten seconds: one picture is kept, not a hundred to catch up.
        let later = start + Duration::from_secs(10);
        assert!(decimator.keep(later, interval));
        assert!(!decimator.keep(later + Duration::from_millis(30), interval));
        assert!(decimator.keep(later + Duration::from_millis(100), interval));
    }

    #[test]
    fn a_picture_that_cannot_be_decoded_is_skipped_and_only_a_run_of_them_fails_the_camera() {
        let bad = || -> Result<RgbImage> { bail!("truncated") };
        let good = || -> Result<RgbImage> { Ok(RgbImage::new(2, 2)) };
        let mut counter = BadPictures::default();
        // A few bad pictures between good ones are skipped, and the count starts again after a good one.
        for _ in 0..3 {
            for _ in 0..(MAX_BAD_PICTURES - 1) {
                assert!(counter.check(bad()).unwrap().is_none());
            }
            assert!(counter.check(good()).unwrap().is_some());
        }
        // A camera that gives nothing else is failed, with the reason.
        let mut counter = BadPictures::default();
        for _ in 0..(MAX_BAD_PICTURES - 1) {
            assert!(counter.check(bad()).unwrap().is_none());
        }
        let error = counter.check(bad()).unwrap_err();
        assert!(format!("{error:#}").contains("in a row could not be decoded"), "{error:#}");
    }

    #[test]
    fn the_rate_can_be_changed_while_a_camera_runs() {
        let rate = FrameRate::new(25.0);
        assert!((rate.interval().as_secs_f32() - 0.04).abs() < 1e-6);
        rate.set(5.0);
        assert!((rate.fps() - 5.0).abs() < 1e-6);
        assert!((rate.interval().as_secs_f32() - 0.2).abs() < 1e-6);
    }
}
