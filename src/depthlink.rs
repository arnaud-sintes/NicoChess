//! The depth camera that goes with the colour camera, as the engine sees it.
//!
//! A camera such as the Intel RealSense D415 shows up on Windows as two cameras: "... RGB" and "... Depth". The
//! page offers it as one camera, with a box to also use the depth. The depth part is read on a thread of its
//! own (see `Engine::spawn_depth`); this holds what is shared with the rest: whether it is wanted, its latest
//! picture, where the squares are in it, and the alignment that is asked for and found.
//!
//! The depth part cannot be lined up with the colour part from a calibration alone (the two sensors are some
//! centimetres apart): the squares are found in the height picture from the position the board is known to be in
//! (see `depth::align`), when a game starts or when the person asks.

use crate::calibration::Calibration;
use crate::choice::CameraInfo;
use crate::depth::{self, DepthFilter, DepthFrame, Plane};
use crate::fsutil::write_atomic;
use anyhow::{Context, Result};
use image::RgbImage;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The horizontal angle of view of the colour and of the depth sensor, in degrees (Intel RealSense D415: 69.4 and
/// 63.4 for the 16:9 depth picture), to take the colour calibration over to the depth picture as a first guess.
const COLOUR_FOV: f32 = 69.4;
const DEPTH_FOV: f32 = 63.4;

/// Where the board is in the picture of the depth camera of the demo (320x240): where the colour calibration of the
/// synthetic board says, taken over by the fields of view, and then a little elsewhere, as two real sensors are.
pub fn demo_depth_corners() -> [[f32; 2]; 4] {
    let colour = crate::synthetic::corners();
    let guess = depth::guess_corners(&colour, (crate::synthetic::WIDTH, crate::synthetic::HEIGHT), (320, 240), COLOUR_FOV, DEPTH_FOV);
    guess.map(|c| [c[0] + 12.0, c[1] - 8.0])
}

/// A depth picture older than this is not used: the depth camera has stopped.
pub const DEPTH_FRESH: Duration = Duration::from_secs(2);

/// One way the depth camera can give its pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DepthMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// What the depth camera is asked for, and how its readings are used. It is set on the Calibrate tab and kept in
/// `depth.json`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DepthSettings {
    /// Readings nearer or farther than this, in millimetres, are not used: what is beyond the board (walls, screens)
    /// only confuses the search for the plane of the board.
    pub near_mm: u16,
    pub far_mm: u16,
    /// The picture asked of the camera (the nearest mode it has is used) and how many pictures a second. A smaller
    /// picture and fewer pictures take less of the cable and of the computer; a larger one shows more detail.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for DepthSettings {
    fn default() -> Self {
        DepthSettings { near_mm: 100, far_mm: 4000, width: 848, height: 480, fps: 30 }
    }
}

impl DepthSettings {
    pub fn mode(&self) -> DepthMode {
        DepthMode { width: self.width, height: self.height, fps: self.fps }
    }

    /// Says what is wrong with settings that cannot be used.
    pub fn validate(&self) -> Result<()> {
        if !(100..=3000).contains(&self.near_mm) {
            anyhow::bail!("the nearest distance must be between 10 and 300 cm");
        }
        if !(200..=4000).contains(&self.far_mm) {
            anyhow::bail!("the farthest distance must be between 20 and 400 cm");
        }
        if self.far_mm < self.near_mm + 200 {
            anyhow::bail!("the farthest distance must be at least 20 cm beyond the nearest");
        }
        if !(160..=1280).contains(&self.width) || !(90..=720).contains(&self.height) || !(1..=90).contains(&self.fps) {
            anyhow::bail!("the picture of the depth camera: from 160x90 to 1280x720, 1 to 90 pictures a second");
        }
        Ok(())
    }
}

/// Which of the settings the page sends (the others stay as they are).
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct DepthPatch {
    pub near_mm: Option<u16>,
    pub far_mm: Option<u16>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
}

impl DepthPatch {
    pub fn applied_to(&self, settings: DepthSettings) -> DepthSettings {
        DepthSettings {
            near_mm: self.near_mm.unwrap_or(settings.near_mm),
            far_mm: self.far_mm.unwrap_or(settings.far_mm),
            width: self.width.unwrap_or(settings.width),
            height: self.height.unwrap_or(settings.height),
            fps: self.fps.unwrap_or(settings.fps),
        }
    }
}

/// Where depth pictures come from: opened for one depth camera, read until it fails.
pub trait DepthSource: Send {
    fn describe(&self) -> String;
    fn next_frame(&mut self) -> Result<DepthFrame>;
    /// The modes the camera has, when it can say (the page offers them).
    fn modes(&self) -> Vec<DepthMode> {
        Vec::new()
    }
}

/// Opens the depth camera with this name (the name Windows gives it: "... Depth") in the mode nearest to the settings.
pub type DepthFactory = Box<dyn Fn(&str, &DepthSettings) -> Result<Box<dyn DepthSource>> + Send + Sync>;

/// The name of the depth part of a camera, from the name of its colour part: "Intel(R) RealSense(TM) Depth Camera 415
/// RGB" goes with "... 415 Depth". `None` for a camera that is not a colour part.
pub fn depth_name_of(colour_name: &str) -> Option<String> {
    let trimmed = colour_name.trim_end();
    let stem = trimmed.strip_suffix("RGB")?;
    Some(format!("{stem}Depth"))
}

fn same_name(a: &str, b: &str) -> bool {
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    squash(a) == squash(b)
}

/// The name of the depth part that goes with the camera `id`, when the list has one.
pub fn offer_for(cameras: &[CameraInfo], id: &str) -> Option<String> {
    let colour = cameras.iter().find(|c| c.id == id)?;
    let wanted = depth_name_of(&colour.name)?;
    cameras.iter().find(|c| same_name(&c.name, &wanted)).map(|c| c.name.clone())
}

/// The cameras the page lists: the depth part of a camera is not one more camera, it is the box beside its colour part.
pub fn without_depth_parts(cameras: Vec<CameraInfo>) -> Vec<CameraInfo> {
    let parts: Vec<String> = cameras.iter().filter_map(|c| depth_name_of(&c.name)).collect();
    cameras.into_iter().filter(|c| !parts.iter().any(|p| same_name(p, &c.name))).collect()
}

/// What `depth.json` holds.
#[derive(Serialize, Deserialize)]
struct Saved {
    on: bool,
    #[serde(default)]
    settings: DepthSettings,
}

/// The picture the depth thread read last.
#[derive(Clone)]
pub struct DepthStamp {
    pub frame: Arc<DepthFrame>,
    pub seq: u64,
    pub at: Instant,
}

/// Where the board is in the colour picture, and what is on it: what [`depth::align`] needs to be asked.
#[derive(Clone)]
pub struct AlignRequest {
    pub colour: Calibration,
    /// The size of the colour picture (turned as the person set it).
    pub colour_size: (u32, u32),
    pub rotation: u16,
    pub occupied: [bool; 64],
}

/// What the alignment found.
#[derive(Clone)]
pub struct AlignOutcome {
    pub calibration: Option<Calibration>,
    pub score: f32,
    pub message: String,
}

/// What the page is told.
#[derive(Clone, Default)]
pub struct DepthStatus {
    /// What the depth camera is, as its source says it ("" while it is off).
    pub source: String,
    pub error: Option<String>,
    /// How well the pieces of the board show in the height picture, where the board was found (0 to 1; 0 when not found).
    pub score: f32,
    /// What happened to the last alignment, for the page.
    pub message: String,
    /// The share of the last picture (0 to 1) that has a reading within the nearest and farthest distances: what
    /// is left of the picture to find the board in.
    pub valid: f32,
}

pub struct DepthLink {
    factory: DepthFactory,
    file: Option<PathBuf>,
    on: AtomicBool,
    /// The name of the depth part of the colour camera in use, when it has one.
    offer: Mutex<Option<String>>,
    latest: Mutex<Option<DepthStamp>>,
    status: Mutex<DepthStatus>,
    request: Mutex<Option<AlignRequest>>,
    outcome: Mutex<Option<AlignOutcome>>,
    /// Where the board is in the (turned) depth picture, once found.
    calibration: Mutex<Option<Calibration>>,
    /// The number of the last depth picture the tracker was given.
    fed: AtomicU64,
    seq: AtomicU64,
    filter: Mutex<DepthFilter>,
    settings: Mutex<DepthSettings>,
    /// The modes the depth camera said it has, when it was opened.
    modes: Mutex<Vec<DepthMode>>,
    /// Changes when the camera has to be opened again (another mode was asked for).
    generation: AtomicU64,
}

impl std::fmt::Debug for DepthLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DepthLink").field("on", &self.on()).field("offer", &self.offer()).finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DepthLink {
    /// `file` is where the box is kept (`depth.json`); a kept "on" is restored.
    pub fn new(factory: DepthFactory, file: Option<PathBuf>) -> Arc<DepthLink> {
        let saved = file.as_ref().and_then(|f| std::fs::read(f).ok()).and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok());
        let on = saved.as_ref().is_some_and(|s| s.on);
        // Settings that cannot be used (a file edited by hand) are not used.
        let settings = saved.map(|s| s.settings).filter(|s| s.validate().is_ok()).unwrap_or_default();
        Arc::new(DepthLink {
            factory,
            file,
            on: AtomicBool::new(on),
            offer: Mutex::new(None),
            latest: Mutex::new(None),
            status: Mutex::new(DepthStatus::default()),
            request: Mutex::new(None),
            outcome: Mutex::new(None),
            calibration: Mutex::new(None),
            fed: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            filter: Mutex::new(DepthFilter::default()),
            settings: Mutex::new(settings),
            modes: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
        })
    }

    /// Keeps the box and the settings for the next start.
    fn persist(&self, on: bool, settings: DepthSettings) -> Result<()> {
        if let Some(file) = &self.file {
            write_atomic(file, &serde_json::to_vec_pretty(&Saved { on, settings })?).with_context(|| "cannot keep the depth settings")?;
        }
        Ok(())
    }

    pub fn settings(&self) -> DepthSettings {
        *lock(&self.settings)
    }

    /// Uses these settings (they were checked) and keeps them. True when the camera has to be opened again, because
    /// the picture it gives is another one: what was found about where the board is no longer holds.
    pub fn set_settings(&self, next: DepthSettings) -> Result<bool> {
        let old = self.settings();
        if next == old {
            return Ok(false);
        }
        self.persist(self.on(), next)?;
        *lock(&self.settings) = next;
        // The pictures kept so far were taken out with other limits.
        *lock(&self.filter) = DepthFilter::default();
        *lock(&self.latest) = None;
        let reopen = next.mode() != old.mode();
        if reopen {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.forget();
        }
        Ok(reopen)
    }

    /// Changes whenever the camera has to be opened again.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn modes(&self) -> Vec<DepthMode> {
        lock(&self.modes).clone()
    }

    pub fn set_modes(&self, modes: Vec<DepthMode>) {
        *lock(&self.modes) = modes;
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// The box: use the depth too. Kept for the next start.
    pub fn set_on(&self, on: bool) -> Result<()> {
        self.persist(on, self.settings())?;
        self.on.store(on, Ordering::SeqCst);
        if !on {
            self.forget();
        }
        Ok(())
    }

    /// The depth part of the colour camera in use (`None`: this camera has none).
    pub fn set_offer(&self, name: Option<String>) {
        let mut offer = lock(&self.offer);
        if *offer != name {
            *offer = name;
            drop(offer);
            self.forget();
        }
    }

    pub fn offer(&self) -> Option<String> {
        lock(&self.offer).clone()
    }

    /// The depth is wanted and the camera in use has it.
    pub fn wanted(&self) -> bool {
        self.on() && lock(&self.offer).is_some()
    }

    pub fn open(&self, colour_name: &str) -> Result<Box<dyn DepthSource>> {
        (self.factory)(colour_name, &self.settings())
    }

    /// Nothing is known about where the squares are in the depth picture (another camera, or the depth was switched off).
    pub fn forget(&self) {
        *lock(&self.calibration) = None;
        *lock(&self.latest) = None;
        *lock(&self.request) = None;
        *lock(&self.outcome) = None;
        let mut status = lock(&self.status);
        status.score = 0.0;
        status.message.clear();
        status.error = None;
    }

    pub fn status(&self) -> DepthStatus {
        lock(&self.status).clone()
    }

    pub fn set_source(&self, source: String) {
        lock(&self.status).source = source;
    }

    pub fn set_error(&self, error: Option<String>) {
        lock(&self.status).error = error;
    }

    pub fn set_message(&self, message: &str) {
        lock(&self.status).message = message.to_string();
    }

    pub fn calibration(&self) -> Option<Calibration> {
        *lock(&self.calibration)
    }

    pub fn aligned(&self) -> bool {
        lock(&self.calibration).is_some()
    }

    /// A picture came from the depth camera: steadied, then kept as the latest.
    pub fn publish(&self, frame: &DepthFrame) {
        let settings = self.settings();
        let kept = depth::clip(frame, settings.near_mm, settings.far_mm);
        lock(&self.status).valid = depth::valid_share(&kept);
        let steady = lock(&self.filter).push(&kept);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        *lock(&self.latest) = Some(DepthStamp { frame: Arc::new(steady), seq, at: Instant::now() });
    }

    /// The latest picture, if it is recent.
    pub fn fresh(&self) -> Option<DepthStamp> {
        lock(&self.latest).clone().filter(|s| s.at.elapsed() < DEPTH_FRESH)
    }

    /// The height picture of the latest depth picture, turned as the colour picture is, for the tracker: with the
    /// number of the depth picture it was made from. `None` when there is no recent picture, none that was not
    /// given yet (`new_only`), or no flat surface in it. The plane is looked for around the board when it is known.
    pub fn height_picture(&self, rotation: u16, new_only: bool) -> Option<(u64, RgbImage)> {
        let stamp = self.fresh()?;
        if new_only && stamp.seq == self.fed.load(Ordering::SeqCst) {
            return None;
        }
        let frame = stamp.frame.rotated(rotation);
        let calibration = self.calibration();
        let plane = Plane::fit(&frame, calibration.as_ref().map(|c| &c.corners))?;
        Some((stamp.seq, depth::height_picture(&frame, &plane)))
    }

    /// The tracker was given the depth picture with this number.
    pub fn mark_fed(&self, seq: u64) {
        self.fed.store(seq, Ordering::SeqCst);
    }

    /// The squares are to be looked for in the depth picture (at the next picture the depth thread reads).
    pub fn ask_to_align(&self, request: AlignRequest) {
        *lock(&self.request) = Some(request);
    }

    pub fn take_request(&self) -> Option<AlignRequest> {
        lock(&self.request).take()
    }

    /// Looks for the squares in the latest depth picture. Slow (about a second): the depth thread does it.
    pub fn align(&self, request: &AlignRequest) -> AlignOutcome {
        let failed = |message: &str| AlignOutcome { calibration: None, score: 0.0, message: message.to_string() };
        let Some(stamp) = self.fresh() else { return failed("the depth camera gives no picture") };
        let frame = stamp.frame.rotated(request.rotation);
        let guess = depth::guess_corners(&request.colour, request.colour_size, (frame.width, frame.height), COLOUR_FOV, DEPTH_FOV);
        let Some(plane) = Plane::fit(&frame, Some(&guess)) else { return failed("no flat surface to be the board in the depth picture") };
        let picture = depth::height_picture(&frame, &plane);
        match depth::align(&picture, &guess, &request.occupied) {
            Some(found) => AlignOutcome {
                calibration: Some(found.calibration),
                score: found.score,
                message: format!("the board was found in the depth picture ({:.0}% match with the pieces on it)", 100.0 * found.score),
            },
            None => failed("the board was not found in the depth picture: are the pieces in the position the game starts from?"),
        }
    }

    /// The result of an alignment, to be put to use once.
    pub fn publish_outcome(&self, outcome: AlignOutcome) {
        {
            let mut status = lock(&self.status);
            status.score = outcome.score;
            status.message.clone_from(&outcome.message);
        }
        *lock(&self.calibration) = outcome.calibration;
        *lock(&self.outcome) = Some(outcome);
    }

    pub fn take_outcome(&self) -> Option<AlignOutcome> {
        lock(&self.outcome).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(id: &str, name: &str) -> CameraInfo {
        CameraInfo { id: id.into(), name: name.into() }
    }

    fn realsense() -> Vec<CameraInfo> {
        vec![
            camera("0", "Integrated Webcam"),
            camera("1", "Intel(R) RealSense(TM) Depth Camera 415  Depth"),
            camera("2", "Intel(R) RealSense(TM) Depth Camera 415  RGB"),
        ]
    }

    #[test]
    fn the_depth_part_goes_with_the_colour_part_by_name() {
        assert_eq!(
            depth_name_of("Intel(R) RealSense(TM) Depth Camera 415  RGB").as_deref(),
            Some("Intel(R) RealSense(TM) Depth Camera 415  Depth")
        );
        assert_eq!(depth_name_of("Integrated Webcam"), None);
        assert_eq!(offer_for(&realsense(), "2").as_deref(), Some("Intel(R) RealSense(TM) Depth Camera 415  Depth"));
        assert_eq!(offer_for(&realsense(), "0"), None, "a plain webcam has no depth");
        assert_eq!(offer_for(&realsense(), "1"), None, "nor does the depth part itself");
        // A colour part whose depth part is not there (unplugged halfway) has none.
        assert_eq!(offer_for(&[camera("0", "Foo RGB")], "0"), None);
    }

    #[test]
    fn the_page_lists_the_depth_part_no_more_than_a_camera() {
        let listed = without_depth_parts(realsense());
        assert_eq!(listed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["0", "2"]);
        // Without a colour part, a camera named Depth stays: it may be anything.
        let alone = without_depth_parts(vec![camera("0", "Some Depth")]);
        assert_eq!(alone.len(), 1);
    }

    #[test]
    fn the_settings_are_checked_kept_and_used_by_the_pictures() {
        let dir = std::env::temp_dir().join(format!("nicochess-depthset-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("depth.json");
        let factory = || -> DepthFactory { Box::new(|_, _| anyhow::bail!("no depth camera here")) };
        let link = DepthLink::new(factory(), Some(file.clone()));
        assert_eq!(link.settings(), DepthSettings::default());

        // What cannot be used is refused.
        let bad = |patch: DepthPatch| patch.applied_to(DepthSettings::default()).validate().is_err();
        assert!(bad(DepthPatch { near_mm: Some(50), ..Default::default() }));
        assert!(bad(DepthPatch { far_mm: Some(5000), ..Default::default() }));
        assert!(bad(DepthPatch { near_mm: Some(1000), far_mm: Some(1100), ..Default::default() }), "too close together");
        assert!(bad(DepthPatch { width: Some(5000), ..Default::default() }));

        // The distances work at once, without opening the camera again.
        let narrow = DepthPatch { near_mm: Some(300), far_mm: Some(1000), ..Default::default() }.applied_to(link.settings());
        let generation = link.generation();
        assert!(!link.set_settings(narrow).unwrap());
        assert_eq!(link.generation(), generation);
        let mut mm = vec![0u16; 60 * 40];
        for (i, z) in mm.iter_mut().enumerate() {
            // Half the picture is the board at 600 mm, half is a wall at 2500 mm.
            *z = if i % 60 < 30 { 600 } else { 2500 };
        }
        link.publish(&DepthFrame::new(60, 40, mm));
        assert!((link.status().valid - 0.5).abs() < 0.01, "{}", link.status().valid);
        let stamp = link.fresh().unwrap();
        assert!(stamp.frame.mm.iter().all(|&z| z == 0 || z == 600), "the wall is not in the picture");

        // Another picture needs the camera opened again, and what was found about the board is forgotten.
        let bigger = DepthPatch { width: Some(1280), height: Some(720), fps: Some(15), ..Default::default() }.applied_to(link.settings());
        assert!(link.set_settings(bigger).unwrap());
        assert_eq!(link.generation(), generation + 1);
        assert!(link.fresh().is_none());

        // Kept for the next start, in the same file as the box.
        link.set_on(true).unwrap();
        let again = DepthLink::new(factory(), Some(file.clone()));
        assert_eq!(again.settings(), bigger);
        assert!(again.on());
        // A file that was edited into something unusable is not used.
        std::fs::write(&file, r#"{"on": true, "settings": {"near_mm": 10, "far_mm": 20}}"#).unwrap();
        assert_eq!(DepthLink::new(factory(), Some(file.clone())).settings(), DepthSettings::default());
        // A file from before there were settings still works.
        std::fs::write(&file, r#"{"on": true}"#).unwrap();
        let old = DepthLink::new(factory(), Some(file));
        assert!(old.on());
        assert_eq!(old.settings(), DepthSettings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_box_is_kept_and_a_height_picture_needs_a_recent_flat_picture() {
        let dir = std::env::temp_dir().join(format!("nicochess-depthlink-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("depth.json");
        let factory = || -> DepthFactory { Box::new(|_, _| anyhow::bail!("no depth camera here")) };
        let link = DepthLink::new(factory(), Some(file.clone()));
        assert!(!link.on() && !link.wanted());
        link.set_offer(Some("X Depth".into()));
        link.set_on(true).unwrap();
        assert!(link.wanted());
        assert!(DepthLink::new(factory(), Some(file.clone())).on(), "kept for the next start");
        assert!(link.open("X RGB").is_err());

        assert!(link.height_picture(0, false).is_none(), "no picture yet");
        link.publish(&DepthFrame::new(60, 40, vec![600; 2400]));
        let (seq, picture) = link.height_picture(0, false).expect("a flat picture has a plane");
        assert_eq!((picture.width(), picture.height()), (60, 40));
        link.mark_fed(seq);
        assert!(link.height_picture(0, true).is_none(), "the same picture is not given twice");
        // Another camera: nothing is known any more.
        link.set_offer(None);
        assert!(link.fresh().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
