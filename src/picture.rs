//! How the picture of the camera is turned and adjusted before anyone looks at it:
//! rotation in quarter turns, and brightness and contrast.
//!
//! It is done here, on every picture, so it works the same with every camera
//! (phone, USB webcam on Windows or Linux). The tracker sees the adjusted picture,
//! so the squares are learned from what the page shows.
//!
//! The settings are saved in `picture.json`, next to the calibration. They go
//! together: the corners of the calibration are positions in the turned picture.
//! The file also keeps the "follow the board" switch (stabilization), which is a
//! setting of the same kind: a property of this camera and this board.

use anyhow::{Context, Result, bail};
use image::{Rgb, RgbImage, imageops};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const BRIGHTNESS_RANGE: (f32, f32) = (-255.0, 255.0);
pub const CONTRAST_RANGE: (f32, f32) = (0.25, 4.0);

/// What "auto contrast" aims for: the darkest and the lightest parts of the board
/// end up near these levels.
const AUTO_LOW: f32 = 10.0;
const AUTO_HIGH: f32 = 245.0;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(default)]
pub struct Picture {
    /// Clockwise, in degrees: 0, 90, 180 or 270.
    pub rotation: u16,
    /// Added to every level, after the contrast (0 = unchanged).
    pub brightness: f32,
    /// Factor around mid-grey (1 = unchanged).
    pub contrast: f32,
    /// During a game, move the calibration along when the camera or the board shifts a little.
    pub follow: bool,
}

impl Default for Picture {
    fn default() -> Self {
        Picture { rotation: 0, brightness: 0.0, contrast: 1.0, follow: true }
    }
}

impl Picture {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.rotation, 0 | 90 | 180 | 270) {
            bail!("the rotation must be 0, 90, 180 or 270 degrees");
        }
        if !self.brightness.is_finite() || !(BRIGHTNESS_RANGE.0..=BRIGHTNESS_RANGE.1).contains(&self.brightness) {
            bail!("the brightness must be between {} and {}", BRIGHTNESS_RANGE.0, BRIGHTNESS_RANGE.1);
        }
        if !self.contrast.is_finite() || !(CONTRAST_RANGE.0..=CONTRAST_RANGE.1).contains(&self.contrast) {
            bail!("the contrast must be between {} and {}", CONTRAST_RANGE.0, CONTRAST_RANGE.1);
        }
        Ok(())
    }

    /// True when brightness and contrast leave the picture alone.
    pub fn is_neutral(&self) -> bool {
        self.brightness == 0.0 && self.contrast == 1.0
    }

    /// The same settings turned by `turns` quarter turns clockwise (negative: counter-clockwise).
    pub fn turned(&self, turns: i32) -> Picture {
        let degrees = (i32::from(self.rotation) + 90 * turns).rem_euclid(360);
        Picture { rotation: degrees as u16, ..*self }
    }

    /// Turns a picture as the settings say.
    pub fn rotate(&self, image: RgbImage) -> RgbImage {
        match self.rotation {
            90 => imageops::rotate90(&image),
            180 => imageops::rotate180(&image),
            270 => imageops::rotate270(&image),
            _ => image,
        }
    }

    /// Applies brightness and contrast, or returns `None` when there is nothing to do.
    pub fn adjust(&self, image: &RgbImage) -> Option<RgbImage> {
        if self.is_neutral() {
            return None;
        }
        let mut table = [0u8; 256];
        for (level, out) in table.iter_mut().enumerate() {
            let v = (level as f32 - 128.0) * self.contrast + 128.0 + self.brightness;
            *out = v.round().clamp(0.0, 255.0) as u8;
        }
        let mut out = image.clone();
        for Rgb(pixel) in out.pixels_mut() {
            pixel[0] = table[pixel[0] as usize];
            pixel[1] = table[pixel[1] as usize];
            pixel[2] = table[pixel[2] as usize];
        }
        Some(out)
    }

    /// Brightness and contrast that stretch the picture so that its darkest and
    /// lightest parts (ignoring 1% of the pixels at each end) use most of the range.
    pub fn auto_contrast(image: &RgbImage) -> Result<(f32, f32)> {
        let mut histogram = [0u64; 256];
        for Rgb([r, g, b]) in image.pixels() {
            let luma = (299 * u32::from(*r) + 587 * u32::from(*g) + 114 * u32::from(*b)) / 1000;
            histogram[luma as usize] += 1;
        }
        let total: u64 = histogram.iter().sum();
        if total == 0 {
            bail!("the picture is empty");
        }
        let percentile = |share: f64| -> f32 {
            let wanted = (total as f64 * share) as u64;
            let mut seen = 0u64;
            for (level, count) in histogram.iter().enumerate() {
                seen += count;
                if seen > wanted {
                    return level as f32;
                }
            }
            255.0
        };
        let (low, high) = (percentile(0.01), percentile(0.99));
        if high - low < 8.0 {
            bail!("the picture is too flat to stretch: is the camera looking at the board?");
        }
        let contrast = ((AUTO_HIGH - AUTO_LOW) / (high - low)).clamp(CONTRAST_RANGE.0, CONTRAST_RANGE.1);
        // The middle of what is there goes to the middle of the range.
        let middle = (low + high) / 2.0;
        let brightness = (127.5 - ((middle - 128.0) * contrast + 128.0)).clamp(BRIGHTNESS_RANGE.0, BRIGHTNESS_RANGE.1);
        Ok((contrast, brightness))
    }

    /// Loads the settings. A missing file means the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let picture: Picture = serde_json::from_str(&text).with_context(|| format!("invalid picture file {}", path.display()))?;
                picture.validate()?;
                Ok(picture)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Picture::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Saves the settings, replacing the file in one step.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::fsutil::write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())?;
        Ok(())
    }
}

/// Where a point of a `width` x `height` picture goes when the picture is turned by
/// `turns` quarter turns clockwise. Returns the new points and the new size.
pub fn turn_points(points: &[[f32; 2]; 4], width: u32, height: u32, turns: i32) -> ([[f32; 2]; 4], u32, u32) {
    let (mut w, mut h) = (width as f32, height as f32);
    let mut out = *points;
    for _ in 0..turns.rem_euclid(4) {
        // One quarter turn clockwise: (x, y) -> (h - y, x), and the picture becomes h x w.
        for p in &mut out {
            *p = [h - p[1], p[0]];
        }
        std::mem::swap(&mut w, &mut h);
    }
    (out, w as u32, h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture_with(low: u8, high: u8) -> RgbImage {
        // Half the pixels at `low`, half at `high`.
        RgbImage::from_fn(20, 10, |x, _| Rgb([if x < 10 { low } else { high }; 3]))
    }

    #[test]
    fn rotation_follows_the_settings() {
        let mut image = RgbImage::new(4, 2);
        image.put_pixel(0, 0, Rgb([255, 0, 0]));
        let cw = Picture { rotation: 90, ..Picture::default() }.rotate(image.clone());
        assert_eq!((cw.width(), cw.height()), (2, 4));
        // Clockwise: the top-left pixel goes to the top-right.
        assert_eq!(cw.get_pixel(1, 0).0, [255, 0, 0]);
        let half = Picture { rotation: 180, ..Picture::default() }.rotate(image.clone());
        assert_eq!(half.get_pixel(3, 1).0, [255, 0, 0]);
        let ccw = Picture { rotation: 270, ..Picture::default() }.rotate(image);
        assert_eq!(ccw.get_pixel(0, 3).0, [255, 0, 0]);
    }

    #[test]
    fn turning_the_points_matches_turning_the_picture() {
        let mut image = RgbImage::new(8, 4);
        let spot = [[1.0f32, 0.0], [6.0, 1.0], [3.0, 3.0], [0.0, 2.0]];
        for p in &spot {
            image.put_pixel(p[0] as u32, p[1] as u32, Rgb([255, 255, 255]));
        }
        for turns in [1, 2, 3, -1] {
            let picture = Picture::default().turned(turns);
            let turned = picture.rotate(image.clone());
            let (points, w, h) = turn_points(&spot, 8, 4, turns);
            assert_eq!((w, h), (turned.width(), turned.height()), "turns {turns}");
            for p in &points {
                // Pixel (x, y) of the picture became pixel (h - 1 - y, x): the points are
                // positions of pixel corners, so allow for that offset of one.
                let (x, y) = (p[0] as u32, p[1] as u32);
                let hit = (x.saturating_sub(1)..=x.min(w - 1))
                    .any(|cx| (y.saturating_sub(1)..=y.min(h - 1)).any(|cy| turned.get_pixel(cx, cy).0 == [255, 255, 255]));
                assert!(hit, "turns {turns}: {p:?} does not land on a marked pixel");
            }
        }
    }

    #[test]
    fn four_quarter_turns_are_the_identity() {
        let points = [[10.0f32, 20.0], [30.0, 40.0], [50.0, 60.0], [70.0, 80.0]];
        let (back, w, h) = turn_points(&points, 640, 480, 4);
        assert_eq!((back, w, h), (points, 640, 480));
        let (a, ..) = turn_points(&points, 640, 480, 1);
        let (b, ..) = turn_points(&a, 480, 640, -1);
        assert_eq!(b, points);
    }

    #[test]
    fn turned_wraps_around() {
        assert_eq!(Picture::default().turned(-1).rotation, 270);
        assert_eq!(Picture { rotation: 270, ..Picture::default() }.turned(1).rotation, 0);
        assert_eq!(Picture { rotation: 90, ..Picture::default() }.turned(2).rotation, 270);
    }

    #[test]
    fn neutral_settings_change_nothing() {
        assert!(Picture::default().adjust(&picture_with(40, 200)).is_none());
    }

    #[test]
    fn contrast_and_brightness_move_the_levels() {
        let image = picture_with(100, 156);
        let more = Picture { contrast: 2.0, ..Picture::default() }.adjust(&image).unwrap();
        assert_eq!(more.get_pixel(0, 0).0[0], 72); // (100-128)*2+128
        assert_eq!(more.get_pixel(19, 0).0[0], 184);
        let lighter = Picture { brightness: 50.0, ..Picture::default() }.adjust(&image).unwrap();
        assert_eq!(lighter.get_pixel(0, 0).0[0], 150);
        let clipped = Picture { brightness: 255.0, contrast: 4.0, ..Picture::default() }.adjust(&image).unwrap();
        assert_eq!(clipped.get_pixel(19, 0).0[0], 255);
    }

    #[test]
    fn auto_contrast_stretches_a_dull_picture() {
        let dull = picture_with(90, 130);
        let (contrast, brightness) = Picture::auto_contrast(&dull).unwrap();
        assert!(contrast > 3.5, "{contrast}");
        let adjusted = Picture { contrast, brightness, ..Picture::default() }.adjust(&dull).unwrap();
        let (dark, light) = (adjusted.get_pixel(0, 0).0[0], adjusted.get_pixel(19, 0).0[0]);
        assert!(dark < 60 && light > 190, "dark {dark}, light {light}");
        assert!(light > dark + 120);
    }

    #[test]
    fn auto_contrast_leaves_a_good_picture_nearly_alone() {
        let (contrast, brightness) = Picture::auto_contrast(&picture_with(10, 245)).unwrap();
        assert!((contrast - 1.0).abs() < 0.05, "{contrast}");
        assert!(brightness.abs() < 5.0, "{brightness}");
    }

    #[test]
    fn auto_contrast_refuses_a_flat_picture() {
        assert!(Picture::auto_contrast(&picture_with(100, 104)).is_err());
    }

    #[test]
    fn settings_are_checked() {
        assert!(Picture { rotation: 45, ..Picture::default() }.validate().is_err());
        assert!(Picture { contrast: 0.0, ..Picture::default() }.validate().is_err());
        assert!(Picture { brightness: f32::NAN, ..Picture::default() }.validate().is_err());
        assert!(Picture { rotation: 270, brightness: -30.0, contrast: 1.5, follow: true }.validate().is_ok());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("nicochess-picture-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("picture.json");
        assert_eq!(Picture::load(&path).unwrap(), Picture::default());
        let picture = Picture { rotation: 90, brightness: 12.0, contrast: 1.5, follow: false };
        picture.save(&path).unwrap();
        assert_eq!(Picture::load(&path).unwrap(), picture);
        fs::remove_dir_all(&dir).ok();
    }
}
