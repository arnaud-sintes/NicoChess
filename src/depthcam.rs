//! Reading the depth of a depth camera (an Intel RealSense, say) on Windows, through Media Foundation, the way the
//! colour cameras are read, and without the camera maker's library.
//!
//! The depth part of such a camera is a camera of its own for Windows ("... Depth"), whose pictures are 16 bits
//! per point (format `Z16`): the distance in millimetres with the camera's default settings, 0 where it has no
//! reading. `nokhwa` cannot read that format, so this talks to Media Foundation itself.

use crate::depthlink::{DepthFactory, DepthSource};

/// Without Windows there is no depth camera to read (yet).
#[cfg(not(windows))]
pub fn factory() -> DepthFactory {
    Box::new(|_| -> anyhow::Result<Box<dyn DepthSource>> { anyhow::bail!("reading a depth camera is only done on Windows") })
}

#[cfg(windows)]
pub use windows_impl::factory;

#[cfg(windows)]
mod windows_impl {
    use super::{DepthFactory, DepthSource};
    use crate::depth::DepthFrame;
    use crate::depthlink::{DepthMode, DepthSettings};
    use anyhow::{Context, Result, bail};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::core::PWSTR;

    pub fn factory() -> DepthFactory {
        Box::new(|depth_name, settings| Ok(Box::new(MfDepth::open(depth_name, settings)?) as Box<dyn DepthSource>))
    }

    struct MfDepth {
        reader: IMFSourceReader,
        _source: IMFMediaSource,
        width: u32,
        height: u32,
        fps: u32,
        name: String,
        /// Every depth mode the camera has.
        modes: Vec<DepthMode>,
    }

    // The reader is made and used on the depth thread alone (it was started free-threaded).
    unsafe impl Send for MfDepth {}

    impl Drop for MfDepth {
        fn drop(&mut self) {
            // Safe to call: Media Foundation counts the starts, the colour cameras have their own.
            unsafe {
                let _ = MFShutdown();
            }
        }
    }

    fn same(a: &str, b: &str) -> bool {
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        squash(a) == squash(b)
    }

    impl MfDepth {
        fn open(depth_name: &str, settings: &DepthSettings) -> Result<MfDepth> {
            unsafe {
                // A thread that already has COM in another way says so: nothing to do about it here.
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                MFStartup(MF_VERSION, MFSTARTUP_FULL).context("Media Foundation did not start")?;
                match Self::open_started(depth_name, settings) {
                    Ok(camera) => Ok(camera),
                    Err(e) => {
                        let _ = MFShutdown();
                        Err(e)
                    }
                }
            }
        }

        unsafe fn open_started(depth_name: &str, settings: &DepthSettings) -> Result<MfDepth> {
            unsafe {
                let mut attributes: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attributes, 1)?;
                let attributes = attributes.context("no attributes")?;
                attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
                let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
                let mut count = 0u32;
                MFEnumDeviceSources(&attributes, &mut list, &mut count)?;
                let devices = std::slice::from_raw_parts(list, count as usize);
                let mut found: Option<IMFActivate> = None;
                for device in devices.iter().flatten() {
                    let mut name = PWSTR::null();
                    let mut length = 0u32;
                    if device.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut length).is_ok() {
                        let text = name.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(name.0 as *const _));
                        if found.is_none() && same(&text, depth_name) {
                            found = Some(device.clone());
                        }
                    }
                }
                CoTaskMemFree(Some(list as *const _));
                let device = found.with_context(|| format!("the depth camera \"{depth_name}\" is not plugged in"))?;
                let source: IMFMediaSource =
                    device.ActivateObject().context("the depth camera cannot be opened (is another program using it?)")?;
                let reader = MFCreateSourceReaderFromMediaSource(&source, None)?;
                let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

                // The 16-bit depth type nearest to the wanted size at the wanted speed.
                let z16 = u32::from_le_bytes(*b"Z16 ");
                let mut best: Option<(f32, IMFMediaType)> = None;
                let mut modes: Vec<DepthMode> = Vec::new();
                let mut index = 0;
                while let Ok(media_type) = reader.GetNativeMediaType(stream, index) {
                    index += 1;
                    let Ok(subtype) = media_type.GetGUID(&MF_MT_SUBTYPE) else { continue };
                    if subtype.data1 != z16 {
                        continue;
                    }
                    let Ok(size) = media_type.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
                    let rate = media_type.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
                    let (w, h) = ((size >> 32) as f32, (size as u32) as f32);
                    let fps = if rate as u32 > 0 { (rate >> 32) as f32 / (rate as u32) as f32 } else { 0.0 };
                    let mode = DepthMode { width: w as u32, height: h as u32, fps: fps.round() as u32 };
                    if !modes.contains(&mode) {
                        modes.push(mode);
                    }
                    let (want_w, want_h, want_fps) = (settings.width as f32, settings.height as f32, settings.fps as f32);
                    let distance = (w - want_w).abs() / want_w + (h - want_h).abs() / want_h + (fps - want_fps).abs() / want_fps;
                    if best.as_ref().is_none_or(|(d, _)| distance < *d) {
                        best = Some((distance, media_type));
                    }
                }
                let (_, media_type) = best.context("the camera offers no 16-bit depth format")?;
                reader.SetCurrentMediaType(stream, None, &media_type).context("the depth format was refused")?;
                let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
                let (width, height) = ((size >> 32) as u32, size as u32);
                if width == 0 || height == 0 {
                    bail!("the depth format has no size");
                }
                let rate = media_type.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
                let fps = if rate as u32 > 0 { ((rate >> 32) as f32 / (rate as u32) as f32).round() as u32 } else { 0 };
                modes.sort_by_key(|m| (m.width, m.height, m.fps));
                Ok(MfDepth { reader, _source: source, width, height, fps, name: depth_name.to_string(), modes })
            }
        }
    }

    impl DepthSource for MfDepth {
        fn describe(&self) -> String {
            format!("{} ({}x{} depth, {} a second)", self.name.trim(), self.width, self.height, self.fps)
        }

        fn modes(&self) -> Vec<DepthMode> {
            self.modes.clone()
        }

        fn next_frame(&mut self) -> Result<DepthFrame> {
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            unsafe {
                // A few empty reads (stream ticks) are normal; a camera that goes on giving nothing is not working.
                for _ in 0..200 {
                    let mut flags = 0u32;
                    let mut sample: Option<IMFSample> = None;
                    self.reader
                        .ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))
                        .context("the depth camera stopped")?;
                    if flags & (MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                        bail!("the depth camera stopped (unplugged?)");
                    }
                    let Some(sample) = sample else { continue };
                    let buffer = sample.ConvertToContiguousBuffer()?;
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut length = 0u32;
                    buffer.Lock(&mut data, None, Some(&mut length))?;
                    let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
                    buffer.Unlock()?;
                    let points = (self.width * self.height) as usize;
                    if bytes.len() < points * 2 {
                        bail!("a depth picture of {} bytes is too small for {}x{}", bytes.len(), self.width, self.height);
                    }
                    let mm = bytes.as_chunks::<2>().0.iter().take(points).map(|c| u16::from_le_bytes(*c)).collect();
                    return Ok(DepthFrame::new(self.width, self.height, mm));
                }
            }
            bail!("the depth camera gives no pictures")
        }
    }
}
