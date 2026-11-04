//! Frame sources: the real camera (Linux, V4L2), a still image, or a synthetic board.

use crate::synthetic;
use anyhow::{Context, Result, bail};
use image::RgbImage;
use std::thread;
use std::time::{Duration, Instant};

/// Something that produces camera frames.
pub trait FrameSource: Send {
    fn describe(&self) -> String;
    /// Blocks until the next frame is due, then returns it.
    fn next_frame(&mut self) -> Result<RgbImage>;
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
        let image = image::open(path)
            .with_context(|| format!("cannot open image {path}"))?
            .to_rgb8();
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
}

impl Synthetic {
    pub fn new(interval: Duration) -> Self {
        Synthetic { image: synthetic::frame(), pacer: Pacer::new(interval) }
    }
}

impl FrameSource for Synthetic {
    fn describe(&self) -> String {
        "synthetic board".to_string()
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        self.pacer.wait();
        Ok(self.image.clone())
    }
}

/// Converts packed YUYV (YUV 4:2:2) to RGB, BT.601 full range.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

/// Settings for the real camera.
#[derive(Clone, Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct CameraSettings {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub interval: Duration,
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

/// Prints what a camera offers. Only available on Linux.
#[cfg(not(target_os = "linux"))]
pub fn probe(_path: &str) -> Result<String> {
    bail!("--probe is only available on Linux")
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
        min_interval: Duration,
        last: Option<Instant>,
    }

    impl V4lCamera {
        pub fn open(settings: &CameraSettings) -> Result<Self> {
            let device = Device::with_path(&settings.device)
                .with_context(|| format!("cannot open {}", settings.device))?;

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
            eprintln!(
                "camera: {} {}x{} {:?}",
                settings.device, format.width, format.height, pixel_format
            );

            apply_controls(&device, settings);

            let stream = Stream::with_buffers(&device, Type::VideoCapture, 4)
                .context("cannot start the capture stream")?;

            Ok(V4lCamera {
                path: settings.device.clone(),
                _device: device,
                stream,
                width: format.width,
                height: format.height,
                pixel_format,
                min_interval: settings.interval,
                last: None,
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
                let (buffer, meta) = self.stream.next().context("camera read failed")?;
                let due = self.last.is_none_or(|last| last.elapsed() >= self.min_interval);
                if !due {
                    continue;
                }
                let used = (meta.bytesused as usize).min(buffer.len());
                let data = &buffer[..used];
                let image = match self.pixel_format {
                    PixelFormat::Yuyv => yuyv_to_rgb(data, self.width, self.height)?,
                    PixelFormat::Nv12 => nv12_to_rgb(data, self.width, self.height)?,
                    PixelFormat::Mjpeg => {
                        image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)
                            .context("bad MJPEG frame")?
                            .to_rgb8()
                    }
                };
                self.last = Some(Instant::now());
                return Ok(image);
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
        DemoGame { script, tick: 0, cache: None, pacer: Pacer::new(frame_interval) }
    }
}

impl FrameSource for DemoGame {
    fn describe(&self) -> String {
        "demo game (Opera game, synthetic board)".to_string()
    }

    fn restart(&mut self) {
        self.tick = 0;
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        self.pacer.wait();
        let grid = self.script[self.tick.min(self.script.len() - 1)];
        self.tick += 1;
        if let Some((cached, image)) = &self.cache
            && *cached == grid
        {
            return Ok(image.clone());
        }
        let image = synthetic::frame_from_grid(&grid);
        self.cache = Some((grid, image.clone()));
        Ok(image)
    }
}

/// Where frames come from.
pub enum SourceSpec {
    Camera(CameraSettings),
    /// The starting position on a synthetic board.
    Synthetic,
    /// A scripted game, see [`DemoGame`].
    Demo { move_interval: Duration },
    /// A fixed image file.
    Image(String),
}

/// Opens a source each time the camera is needed, and drops it when it is not.
pub type SourceFactory = Box<dyn Fn() -> Result<Box<dyn FrameSource>> + Send + Sync>;

pub fn factory(spec: SourceSpec, interval: Duration) -> SourceFactory {
    Box::new(move || -> Result<Box<dyn FrameSource>> {
        match &spec {
            SourceSpec::Camera(settings) => open_camera(settings),
            SourceSpec::Synthetic => Ok(Box::new(Synthetic::new(interval))),
            SourceSpec::Demo { move_interval } => Ok(Box::new(DemoGame::new(interval, *move_interval))),
            SourceSpec::Image(path) => Ok(Box::new(StillImage::open(path, interval)?)),
        }
    })
}

#[cfg(target_os = "linux")]
fn open_camera(settings: &CameraSettings) -> Result<Box<dyn FrameSource>> {
    Ok(Box::new(V4lCamera::open(settings)?))
}

#[cfg(not(target_os = "linux"))]
fn open_camera(_settings: &CameraSettings) -> Result<Box<dyn FrameSource>> {
    bail!("camera capture is only implemented on Linux; use --source demo, synthetic or an image file")
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
}
