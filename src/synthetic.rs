//! Synthetic camera frames: a chessboard seen in perspective, for any position.
//! It lets the whole pipeline be developed and tested on a PC without a camera,
//! with the corner positions known exactly.

use crate::calibration::Calibration;
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_filled_circle_mut;
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use shakmaty::san::San;
use shakmaty::{Chess, Color, Move, Position, Square};

pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;

const BACKGROUND: Rgb<u8> = Rgb([90, 95, 100]);
const LIGHT_SQUARE: Rgb<u8> = Rgb([232, 212, 170]);
const DARK_SQUARE: Rgb<u8> = Rgb([140, 100, 70]);
const WHITE_PIECE: Rgb<u8> = Rgb([245, 245, 240]);
const BLACK_PIECE: Rgb<u8> = Rgb([25, 25, 30]);

/// Which colour of piece stands on each square (`None` = empty), indexed by
/// square: a1 = 0, b1 = 1, ... h8 = 63.
pub type PieceGrid = [Option<Color>; 64];

pub fn grid_from_position(pos: &Chess) -> PieceGrid {
    let mut grid = [None; 64];
    for sq in Square::ALL {
        grid[usize::from(sq)] = pos.board().piece_at(sq).map(|p| p.color);
    }
    grid
}

pub fn start_grid() -> PieceGrid {
    grid_from_position(&Chess::default())
}

/// Where the board corners (a1, h1, h8, a8) are in the synthetic frame.
pub fn corners() -> Calibration {
    Calibration {
        corners: [[120.0, 440.0], [520.0, 425.0], [470.0, 90.0], [190.0, 100.0]],
    }
}

/// The board seen from above, white at the bottom, without any margin.
pub fn board_top_down(square_px: u32, grid: &PieceGrid) -> RgbImage {
    let size = 8 * square_px;
    let mut img = RgbImage::from_fn(size, size, |x, y| {
        let (col, row) = (x / square_px, y / square_px);
        // a1 (col 0, bottom row) is a dark square.
        if (col + (7 - row)) % 2 == 0 {
            DARK_SQUARE
        } else {
            LIGHT_SQUARE
        }
    });

    let radius = (square_px as f32 * 0.33) as i32;
    for (index, piece) in grid.iter().enumerate() {
        let Some(color) = piece else { continue };
        let (col, rank) = ((index % 8) as u32, (index / 8) as u32);
        let row = 7 - rank;
        let cx = (col * square_px + square_px / 2) as i32;
        let cy = (row * square_px + square_px / 2) as i32;
        let paint = if *color == Color::White { WHITE_PIECE } else { BLACK_PIECE };
        draw_filled_circle_mut(&mut img, (cx, cy), radius, paint);
    }
    img
}

/// A full camera frame showing the given position in perspective.
pub fn frame_from_grid(grid: &PieceGrid) -> RgbImage {
    let square_px = 50;
    let board = board_top_down(square_px, grid);
    let size = (8 * square_px) as f32;
    let top_down = [(0.0, size), (size, size), (size, 0.0), (0.0, 0.0)];
    let c = corners().corners;
    let image_points = [
        (c[0][0], c[0][1]),
        (c[1][0], c[1][1]),
        (c[2][0], c[2][1]),
        (c[3][0], c[3][1]),
    ];
    let projection = Projection::from_control_points(top_down, image_points)
        .expect("synthetic corners define a valid projection");

    let mut out = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
    warp_into(
        &board,
        projection,
        Interpolation::Bilinear,
        Border::Constant(BACKGROUND),
        &mut out,
    );
    out
}

/// The starting position as a camera frame.
pub fn frame() -> RgbImage {
    frame_from_grid(&start_grid())
}

/// Adds a brightness shift and pseudo-random noise, to mimic a poor camera.
#[cfg(test)]
pub fn degrade(image: &mut RgbImage, brightness: i32, noise: i32, seed: u64) {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for px in image.pixels_mut() {
        for channel in px.0.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let jitter = if noise > 0 { (state % (2 * noise as u64 + 1)) as i32 - noise } else { 0 };
            *channel = (*channel as i32 + brightness + jitter).clamp(0, 255) as u8;
        }
    }
}

/// Morphy against the Duke of Brunswick and Count Isouard, 1858 ("the Opera game"):
/// 33 plies with captures, castling, checks and a checkmate. Used by the demo
/// source and by the tests.
pub const OPERA_GAME: [&str; 33] = [
    "e4", "e5", "Nf3", "d6", "d4", "Bg4", "dxe5", "Bxf3", "Qxf3", "dxe5", "Bc4", "Nf6", "Qb3", "Qe7",
    "Nc3", "c6", "Bg5", "b5", "Nxb5", "cxb5", "Bxb5+", "Nbd7", "O-O-O", "Rd8", "Rxd7", "Rxd7", "Rd1",
    "Qe6", "Bxd7+", "Nxd7", "Qb8+", "Nxb8", "Rd8#",
];

/// One move of a scripted game, with the board before and after it.
pub struct Step {
    pub mv: Move,
    pub before: PieceGrid,
    pub after: PieceGrid,
}

/// Plays a list of SAN moves from the standard position.
pub fn game_steps(sans: &[&str]) -> Vec<Step> {
    let mut pos = Chess::default();
    let mut steps = Vec::new();
    for text in sans {
        let mv = text
            .parse::<San>()
            .expect("valid SAN")
            .to_move(&pos)
            .expect("legal move");
        let before = grid_from_position(&pos);
        pos.play_unchecked(mv);
        steps.push(Step { mv, before, after: grid_from_position(&pos) });
    }
    steps
}
