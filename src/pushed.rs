//! A camera that is driven from outside: the host program (the Android app)
//! captures the pictures and pushes them in; the engine reads them like any other
//! source.
//!
//! The engine opens a source when it needs the camera and drops it when it does
//! not. The host learns about that through [`FrameInbox::wanted`], and starts or
//! stops its own camera accordingly.

use crate::camera::FrameSource;
use anyhow::{Result, bail};
use image::RgbImage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long the engine waits for a picture before it reports the camera as silent.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    frame: Option<RgbImage>,
    error: Option<String>,
}

/// Where the host drops its pictures. Shared between the host and the engine.
#[derive(Default)]
pub struct FrameInbox {
    state: Mutex<State>,
    arrived: Condvar,
    wanted: AtomicBool,
}

impl FrameInbox {
    pub fn new() -> Arc<Self> {
        Arc::new(FrameInbox::default())
    }

    /// True while the engine wants pictures: the host should have its camera open.
    pub fn wanted(&self) -> bool {
        self.wanted.load(Ordering::SeqCst)
    }

    /// Hands over a picture. Only the newest one is kept: the engine reads a few
    /// per second, so there is no point in queueing the rest.
    pub fn push(&self, image: RgbImage) {
        let mut state = self.state.lock().unwrap();
        state.frame = Some(image);
        state.error = None;
        self.arrived.notify_all();
    }

    /// The host could not open or keep its camera: the engine reports it on the page.
    pub fn fail(&self, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.error = Some(message.into());
        self.arrived.notify_all();
    }
}

/// The engine's end of the inbox. Created when the camera is opened, dropped when
/// it is closed.
pub struct PushedSource {
    inbox: Arc<FrameInbox>,
}

impl PushedSource {
    pub fn open(inbox: Arc<FrameInbox>) -> Self {
        {
            // A picture left over from the last time would be old.
            let mut state = inbox.state.lock().unwrap();
            state.frame = None;
            state.error = None;
        }
        inbox.wanted.store(true, Ordering::SeqCst);
        PushedSource { inbox }
    }
}

impl Drop for PushedSource {
    fn drop(&mut self) {
        self.inbox.wanted.store(false, Ordering::SeqCst);
    }
}

impl FrameSource for PushedSource {
    fn describe(&self) -> String {
        "camera of the device".to_string()
    }

    fn next_frame(&mut self) -> Result<RgbImage> {
        let state = self.inbox.state.lock().unwrap();
        let (mut state, timeout) = self
            .inbox
            .arrived
            .wait_timeout_while(state, FRAME_TIMEOUT, |s| s.frame.is_none() && s.error.is_none())
            .unwrap();
        if let Some(message) = state.error.take() {
            bail!("{message}");
        }
        match state.frame.take() {
            Some(frame) => Ok(frame),
            None if timeout.timed_out() => bail!("no picture from the camera for {} seconds", FRAME_TIMEOUT.as_secs()),
            None => bail!("the camera stopped"),
        }
    }
}

/// Converts a YUV 4:2:0 picture as Android delivers it (`YUV_420_888`) to RGB,
/// BT.601 full range.
///
/// The luma plane has `y_stride` bytes per row. The two chroma planes have half
/// the rows and columns; `uv_stride` bytes per row, and `uv_pixel_stride` bytes
/// from one sample to the next (1 when the planes are separate, 2 when U and V
/// are interleaved, as in NV12 and NV21). Samples missing at the end of a buffer
/// count as grey.
#[allow(clippy::too_many_arguments)]
pub fn yuv420_to_rgb(
    width: u32,
    height: u32,
    y: &[u8],
    y_stride: usize,
    u: &[u8],
    v: &[u8],
    uv_stride: usize,
    uv_pixel_stride: usize,
) -> Result<RgbImage> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || y_stride < w || uv_pixel_stride == 0 {
        bail!("bad picture geometry: {width}x{height}, strides {y_stride}/{uv_stride}/{uv_pixel_stride}");
    }
    if y.len() < (h - 1) * y_stride + w {
        bail!("the luma plane is too short: {} bytes for {width}x{height}", y.len());
    }
    let clamp = |x: i32| x.clamp(0, 255) as u8;
    let mut out = vec![0u8; w * h * 3];
    for row in 0..h {
        let luma_row = &y[row * y_stride..row * y_stride + w];
        let out_row = &mut out[row * w * 3..(row + 1) * w * 3];
        let chroma_row = (row / 2) * uv_stride;
        let last_needed = chroma_row + ((w - 1) / 2) * uv_pixel_stride;
        if last_needed < u.len() && last_needed < v.len() {
            // The usual case: two pixels share one chroma sample, so work out its
            // contribution once for both.
            for (pair, (lumas, pixels)) in luma_row.chunks(2).zip(out_row.chunks_mut(6)).enumerate() {
                let at = chroma_row + pair * uv_pixel_stride;
                let (cu, cv) = (i32::from(u[at]) - 128, i32::from(v[at]) - 128);
                let (r, g, b) = ((359 * cv) >> 8, ((88 * cu) >> 8) + ((183 * cv) >> 8), (454 * cu) >> 8);
                for (pixel, &luma) in pixels.chunks_exact_mut(3).zip(lumas) {
                    let luma = i32::from(luma);
                    pixel[0] = clamp(luma + r);
                    pixel[1] = clamp(luma - g);
                    pixel[2] = clamp(luma + b);
                }
            }
        } else {
            // The chroma planes end early (some devices cut the last row short).
            for col in 0..w {
                let at = chroma_row + (col / 2) * uv_pixel_stride;
                let cu = i32::from(u.get(at).copied().unwrap_or(128)) - 128;
                let cv = i32::from(v.get(at).copied().unwrap_or(128)) - 128;
                let luma = i32::from(luma_row[col]);
                let o = col * 3;
                out_row[o] = clamp(luma + ((359 * cv) >> 8));
                out_row[o + 1] = clamp(luma - (((88 * cu) >> 8) + ((183 * cv) >> 8)));
                out_row[o + 2] = clamp(luma + ((454 * cu) >> 8));
            }
        }
    }
    RgbImage::from_raw(width, height, out).ok_or_else(|| anyhow::anyhow!("bad RGB buffer size"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn grey(w: u32, h: u32, shade: u8) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb([shade; 3]))
    }

    #[test]
    fn source_asks_for_the_camera_and_releases_it() {
        let inbox = FrameInbox::new();
        assert!(!inbox.wanted());
        let source = PushedSource::open(inbox.clone());
        assert!(inbox.wanted());
        drop(source);
        assert!(!inbox.wanted());
    }

    #[test]
    fn a_pushed_picture_is_read_once() {
        let inbox = FrameInbox::new();
        let mut source = PushedSource::open(inbox.clone());
        let pusher = {
            let inbox = inbox.clone();
            thread::spawn(move || inbox.push(grey(4, 2, 99)))
        };
        let frame = source.next_frame().unwrap();
        pusher.join().unwrap();
        assert_eq!((frame.width(), frame.height()), (4, 2));
        assert_eq!(frame.get_pixel(0, 0).0, [99, 99, 99]);
    }

    #[test]
    fn only_the_newest_picture_is_kept() {
        let inbox = FrameInbox::new();
        let mut source = PushedSource::open(inbox.clone());
        inbox.push(grey(2, 2, 10));
        inbox.push(grey(2, 2, 20));
        assert_eq!(source.next_frame().unwrap().get_pixel(0, 0).0[0], 20);
    }

    #[test]
    fn a_failure_of_the_host_is_reported() {
        let inbox = FrameInbox::new();
        let mut source = PushedSource::open(inbox.clone());
        inbox.fail("camera in use by another app");
        let error = source.next_frame().unwrap_err().to_string();
        assert!(error.contains("another app"), "{error}");
    }

    #[test]
    fn a_picture_from_before_the_camera_was_opened_is_dropped() {
        let inbox = FrameInbox::new();
        inbox.push(grey(2, 2, 77));
        let _source = PushedSource::open(inbox.clone());
        assert!(inbox.state.lock().unwrap().frame.is_none());
    }

    #[test]
    fn yuv_separate_planes_grey_and_colour() {
        // 4x2 picture, chroma 2x1. Left half grey, right half strongly red.
        let y = [100u8; 8];
        let u = [128u8, 90];
        let v = [128u8, 240];
        let image = yuv420_to_rgb(4, 2, &y, 4, &u, &v, 2, 1).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [100, 100, 100]);
        let right = image.get_pixel(3, 1).0;
        assert!(right[0] > 200 && right[1] < 100 && right[2] < 100, "{right:?}");
    }

    #[test]
    fn yuv_interleaved_planes_match_separate_ones() {
        // NV21-like: V and U alternate in one buffer; the planes are views at offsets 1 and 0.
        let y = [100u8; 8];
        let interleaved = [240u8, 90, 128, 128]; // V0 U0 V1 U1 -> columns 0-1 red-ish, 2-3 grey
        let from_interleaved = yuv420_to_rgb(4, 2, &y, 4, &interleaved[1..], &interleaved[0..], 4, 2).unwrap();
        let separate = yuv420_to_rgb(4, 2, &y, 4, &[90, 128], &[240, 128], 2, 1).unwrap();
        assert_eq!(from_interleaved, separate);
    }

    #[test]
    fn yuv_respects_row_padding_and_short_chroma_tails() {
        // Rows of 6 bytes for 4 pixels; the last chroma sample is missing.
        let y = [10u8, 10, 10, 10, 0, 0, 200, 200, 200, 200, 0, 0];
        let u = [128u8, 128];
        let v = [128u8, 128];
        let image = yuv420_to_rgb(4, 2, &y, 6, &u[..1], &v[..1], 2, 1).unwrap();
        assert_eq!(image.get_pixel(0, 0).0, [10, 10, 10]);
        assert_eq!(image.get_pixel(3, 1).0, [200, 200, 200]);
    }

    #[test]
    fn yuv_rejects_a_short_luma_plane() {
        assert!(yuv420_to_rgb(4, 2, &[0u8; 5], 4, &[0], &[0], 2, 1).is_err());
    }
}
