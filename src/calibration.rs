//! Board calibration: the four corners of the board in the camera image, and the
//! warp that turns the camera view into a top-down 8x8 grid.
//!
//! Corner order is fixed: a1, h1, h8, a8, as seen in the camera image.
//! In the warped image white is at the bottom, the a-file on the left.

use anyhow::{Context, Result, bail};
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_line_segment_mut;
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Names of the corners, in the order they are clicked.
pub const CORNER_NAMES: [&str; 4] = ["a1", "h1", "h8", "a8"];

/// Side of one square in the warped image, in pixels.
pub const SQUARE_PX: u32 = 50;

/// Border kept around the board in the warped image, in pixels. It leaves room
/// to see things next to the board later (for example captured pieces).
pub const MARGIN_PX: u32 = 25;

/// Side of the whole warped image, in pixels.
pub const BOARD_PX: u32 = 8 * SQUARE_PX + 2 * MARGIN_PX;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// Corners a1, h1, h8, a8 in camera image pixels, as `[x, y]`.
    pub corners: [[f32; 2]; 4],
    /// Width and height of the picture the corners were made for. A calibration made for another
    /// size (another camera or resolution) is wrong, and the engine says so. Files from before
    /// this field have none: the first picture received fixes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<[u32; 2]>,
}

impl Calibration {
    /// Corners with no known picture size.
    pub fn new(corners: [[f32; 2]; 4]) -> Self {
        Calibration { corners, size: None }
    }

    /// Corners made for a picture of this size.
    pub fn for_frame(corners: [[f32; 2]; 4], width: u32, height: u32) -> Self {
        Calibration { corners, size: Some([width, height]) }
    }

    /// Checks that the corners are inside the frame and form a convex quadrilateral
    /// of a reasonable size.
    pub fn validate(&self, width: u32, height: u32) -> Result<()> {
        let (w, h) = (width as f32, height as f32);
        for (name, p) in CORNER_NAMES.iter().zip(&self.corners) {
            if !p[0].is_finite() || !p[1].is_finite() {
                bail!("corner {name} is not a number");
            }
            if p[0] < 0.0 || p[0] > w || p[1] < 0.0 || p[1] > h {
                bail!("corner {name} is outside the {width}x{height} frame");
            }
        }

        // Convexity: the turn direction must be the same at every corner. It must also
        // be counter-clockwise on screen (y points down, so the cross product is
        // negative): a camera sees a1, h1, h8, a8 in that direction from either side
        // of the board, so the opposite direction means a wrong click order.
        let c = &self.corners;
        let mut sign = 0.0f32;
        for i in 0..4 {
            let (a, b, d) = (c[i], c[(i + 1) % 4], c[(i + 2) % 4]);
            let cross = (b[0] - a[0]) * (d[1] - b[1]) - (b[1] - a[1]) * (d[0] - b[0]);
            if cross.abs() < 1e-3 {
                bail!("corners are aligned");
            }
            if sign == 0.0 {
                sign = cross.signum();
            } else if cross.signum() != sign {
                bail!("corners do not form a convex shape (wrong click order?)");
            }
        }
        if sign > 0.0 {
            bail!("wrong click order: go a1, h1, h8, a8 (counter-clockwise on screen)");
        }

        // Shoelace area.
        let mut area2 = 0.0f32;
        for i in 0..4 {
            let (a, b) = (c[i], c[(i + 1) % 4]);
            area2 += a[0] * b[1] - b[0] * a[1];
        }
        if area2.abs() / 2.0 < 0.02 * w * h {
            bail!("the board covers less than 2% of the frame");
        }
        Ok(())
    }

    /// Projection from camera pixels to the warped image (board plus margin).
    pub fn projection(&self) -> Option<Projection> {
        let near = MARGIN_PX as f32;
        let far = (MARGIN_PX + 8 * SQUARE_PX) as f32;
        let from = [
            (self.corners[0][0], self.corners[0][1]),
            (self.corners[1][0], self.corners[1][1]),
            (self.corners[2][0], self.corners[2][1]),
            (self.corners[3][0], self.corners[3][1]),
        ];
        // a1 bottom-left, h1 bottom-right, h8 top-right, a8 top-left.
        let to = [(near, far), (far, far), (far, near), (near, near)];
        Projection::from_control_points(from, to)
    }

    /// Loads a calibration file. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let cal = serde_json::from_str(&text).with_context(|| format!("invalid calibration file {}", path.display()))?;
                Ok(Some(cal))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Saves the calibration, replacing the file in one step so a power loss
    /// cannot leave a half-written file.
    pub fn save(&self, path: &Path) -> Result<()> {
        crate::fsutil::write_atomic(path, serde_json::to_string_pretty(self)?.as_bytes())
    }
}

/// Warps the camera image to a top-down board (with a margin around it).
pub fn warp_board(image: &RgbImage, cal: &Calibration) -> Result<RgbImage> {
    let projection = cal.projection().context("the corners do not define a valid projection")?;
    let mut out = RgbImage::new(BOARD_PX, BOARD_PX);
    warp_into(image, projection, Interpolation::Bilinear, Border::Constant(Rgb([0, 0, 0])), &mut out);
    Ok(out)
}

/// Draws the 8x8 grid lines on a warped board image.
pub fn draw_grid(image: &mut RgbImage) {
    let near = MARGIN_PX as f32;
    let far = (MARGIN_PX + 8 * SQUARE_PX) as f32;
    let color = Rgb([0, 220, 120]);
    for i in 0..=8 {
        let p = (MARGIN_PX + i * SQUARE_PX) as f32;
        draw_line_segment_mut(image, (p, near), (p, far), color);
        draw_line_segment_mut(image, (near, p), (far, p), color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthetic;

    #[test]
    fn synthetic_corners_are_valid() {
        synthetic::corners().validate(synthetic::WIDTH, synthetic::HEIGHT).unwrap();
    }

    #[test]
    fn rejects_bad_corners() {
        let good = synthetic::corners();

        let mut outside = good;
        outside.corners[0] = [-5.0, 100.0];
        assert!(outside.validate(640, 480).is_err());

        // Swapping two neighbours makes a bow-tie shape.
        let mut crossed = good;
        crossed.corners.swap(2, 3);
        assert!(crossed.validate(640, 480).is_err());

        // Clicking the same corners the other way round (a1, a8, h8, h1) is rejected.
        let mut reversed = good;
        reversed.corners.swap(1, 3);
        assert!(reversed.validate(640, 480).is_err());

        let tiny = Calibration::new([[100.0, 100.0], [110.0, 100.0], [110.0, 110.0], [100.0, 110.0]]);
        assert!(tiny.validate(640, 480).is_err());

        let aligned = Calibration::new([[10.0, 10.0], [20.0, 20.0], [30.0, 30.0], [40.0, 40.0]]);
        assert!(aligned.validate(640, 480).is_err());
    }

    #[test]
    fn warp_recovers_the_top_down_board() {
        let frame = synthetic::frame();
        let warped = warp_board(&frame, &synthetic::corners()).unwrap();
        let reference = synthetic::board_top_down(SQUARE_PX, &synthetic::start_grid());

        // Compare the centre of every square.
        for row in 0..8 {
            for col in 0..8 {
                let x = col * SQUARE_PX + SQUARE_PX / 2;
                let y = row * SQUARE_PX + SQUARE_PX / 2;
                let (a, b) = (warped.get_pixel(x + MARGIN_PX, y + MARGIN_PX), reference.get_pixel(x, y));
                for ch in 0..3 {
                    let diff = (a[ch] as i32 - b[ch] as i32).abs();
                    assert!(diff < 40, "square row {row} col {col}: {a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("nicochess-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calibration.json");

        assert_eq!(Calibration::load(&path).unwrap(), None);
        let cal = synthetic::corners();
        cal.save(&path).unwrap();
        assert_eq!(Calibration::load(&path).unwrap(), Some(cal));

        fs::remove_dir_all(&dir).ok();
    }
}
