//! Synthetic camera frames: a chessboard seen in perspective, for any position.
//! It lets the whole pipeline be developed and tested on a PC without a camera,
//! with the corner positions known exactly.

use crate::calibration::Calibration;
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_filled_circle_mut;
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use shakmaty::san::San;
use shakmaty::{Chess, Color, Move, Position, Role, Square};

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
    Calibration::new([[120.0, 440.0], [520.0, 425.0], [470.0, 90.0], [190.0, 100.0]])
}

/// The board seen from above, white at the bottom, without any margin.
pub fn board_top_down(square_px: u32, grid: &PieceGrid) -> RgbImage {
    let size = 8 * square_px;
    let mut img = RgbImage::from_fn(size, size, |x, y| {
        let (col, row) = (x / square_px, y / square_px);
        // a1 (col 0, bottom row) is a dark square.
        if (col + (7 - row)) % 2 == 0 { DARK_SQUARE } else { LIGHT_SQUARE }
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
    let image_points = [(c[0][0], c[0][1]), (c[1][0], c[1][1]), (c[2][0], c[2][1]), (c[3][0], c[3][1])];
    let projection = Projection::from_control_points(top_down, image_points).expect("synthetic corners define a valid projection");

    let mut out = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
    warp_into(&board, projection, Interpolation::Bilinear, Border::Constant(BACKGROUND), &mut out);
    out
}

/// The starting position as a camera frame.
pub fn frame() -> RgbImage {
    frame_from_grid(&start_grid())
}

/// A harder camera than `frame_from_grid`, to stress the tracker the way a real game does: the light
/// changes during the game, a shadow moves over the board, a glare spot sits on one square, and the
/// pieces differ in size and lean over the square behind them.
#[derive(Clone, Copy, Debug)]
pub struct Conditions {
    /// The brightness at the end of the game relative to the start (1.0: it does not change).
    pub final_gain: f32,
    /// The colour at the end of the game: positive is warmer (more red, less blue).
    pub final_warmth: f32,
    /// How dark the shadow is (0 to 1). It crosses the board from one side to the other during the game.
    pub shadow: f32,
    /// How bright the glare spot is (0 for none), and the square it is on.
    pub glare: f32,
    pub glare_square: Square,
    /// Pieces of different sizes, leaning over the squares behind them.
    pub lean: bool,
    /// Amplitude of the noise on every channel.
    pub noise: i32,
}

impl Conditions {
    /// What `frame_from_grid` gives: a flat board, steady light, no noise.
    pub const EASY: Conditions =
        Conditions { final_gain: 1.0, final_warmth: 0.0, shadow: 0.0, glare: 0.0, glare_square: Square::E5, lean: false, noise: 0 };
    /// The light fades and warms and a soft shadow moves over the board.
    pub const DRIFT: Conditions =
        Conditions { final_gain: 0.72, final_warmth: 0.10, shadow: 0.30, glare: 0.0, glare_square: Square::E5, lean: false, noise: 10 };
    /// Drift, glare on e5, and tall pieces that lean over their neighbours.
    pub const HARD: Conditions =
        Conditions { final_gain: 0.72, final_warmth: 0.10, shadow: 0.30, glare: 0.55, glare_square: Square::E5, lean: true, noise: 12 };
}

/// How big a piece looks, and how far its top is from its base (in squares), by kind.
fn piece_look(role: Role) -> (f32, f32) {
    match role {
        Role::Pawn => (0.21, 0.04),
        Role::Knight => (0.27, 0.16),
        Role::Bishop => (0.26, 0.22),
        Role::Rook => (0.29, 0.10),
        Role::Queen => (0.33, 0.34),
        Role::King => (0.35, 0.40),
    }
}

/// A camera frame of a position, under the given conditions. `progress` (0 at the start of the game,
/// 1 at its end) says how far the light has drifted; `seed` gives the noise of this frame.
pub fn frame_from_position(pos: &Chess, conditions: &Conditions, progress: f32, seed: u64) -> RgbImage {
    let square_px = 50u32;
    let size = (8 * square_px) as f32;
    let sq = square_px as f32;
    let mut board = board_top_down(square_px, &[None; 64]);

    // Pieces, from the back of the board to the front, so that a piece in front hides what leans over it.
    for rank in (0..8usize).rev() {
        for file in 0..8usize {
            let square = Square::from_coords(shakmaty::File::new(file as u32), shakmaty::Rank::new(rank as u32));
            let Some(piece) = pos.board().piece_at(square) else { continue };
            let (radius, height) = if conditions.lean { piece_look(piece.role) } else { (0.33, 0.0) };
            let cx = (file as f32 + 0.5) * sq;
            let cy = (7 - rank) as f32 * sq + 0.5 * sq;
            let (body, rim) =
                if piece.color == Color::White { (WHITE_PIECE, Rgb([150, 150, 145])) } else { (BLACK_PIECE, Rgb([90, 90, 95])) };
            // The base, a body going up toward the back of the board, and the top.
            let steps = 4;
            for step in 0..=steps {
                let t = step as f32 / steps as f32;
                let (x, y) = (cx as i32, (cy - height * sq * t) as i32);
                let r = (radius * sq * (1.0 - 0.25 * t)) as i32;
                draw_filled_circle_mut(&mut board, (x, y), r + 1, rim);
                draw_filled_circle_mut(&mut board, (x, y), r, body);
            }
        }
    }

    // The light, in the plane of the board.
    let gain = 1.0 + (conditions.final_gain - 1.0) * progress;
    let warm = conditions.final_warmth * progress;
    let shadow_x = (0.1 + 0.8 * progress) * size;
    let glare_file = usize::from(conditions.glare_square) % 8;
    let glare_rank = usize::from(conditions.glare_square) / 8;
    let (glare_x, glare_y) = ((glare_file as f32 + 0.5) * sq, (7 - glare_rank) as f32 * sq + 0.5 * sq);
    for (x, y, pixel) in board.enumerate_pixels_mut() {
        let (fx, fy) = (x as f32, y as f32);
        let shadow = 1.0 - conditions.shadow * (-((fx - shadow_x) / (0.18 * size)).powi(2)).exp();
        let glare = 1.0 + conditions.glare * (-(((fx - glare_x).powi(2) + (fy - glare_y).powi(2)) / (0.3 * sq).powi(2))).exp();
        let factor = gain * shadow * glare;
        let channel = |value: u8, tint: f32| (f32::from(value) * factor * tint).round().clamp(0.0, 255.0) as u8;
        *pixel = Rgb([channel(pixel[0], 1.0 + warm), channel(pixel[1], 1.0), channel(pixel[2], 1.0 - warm)]);
    }

    // The same perspective as `frame_from_grid`.
    let top_down = [(0.0, size), (size, size), (size, 0.0), (0.0, 0.0)];
    let c = corners().corners;
    let image_points = [(c[0][0], c[0][1]), (c[1][0], c[1][1]), (c[2][0], c[2][1]), (c[3][0], c[3][1])];
    let projection = Projection::from_control_points(top_down, image_points).expect("synthetic corners define a valid projection");
    let mut out = RgbImage::from_pixel(WIDTH, HEIGHT, BACKGROUND);
    warp_into(&board, projection, Interpolation::Bilinear, Border::Constant(BACKGROUND), &mut out);

    // Noise, different in every frame.
    if conditions.noise > 0 {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        for pixel in out.pixels_mut() {
            for channel in pixel.0.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let jitter = (state % (2 * conditions.noise as u64 + 1)) as i32 - conditions.noise;
                *channel = (i32::from(*channel) + jitter).clamp(0, 255) as u8;
            }
        }
    }
    out
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
    "e4", "e5", "Nf3", "d6", "d4", "Bg4", "dxe5", "Bxf3", "Qxf3", "dxe5", "Bc4", "Nf6", "Qb3", "Qe7", "Nc3", "c6", "Bg5", "b5", "Nxb5",
    "cxb5", "Bxb5+", "Nbd7", "O-O-O", "Rd8", "Rxd7", "Rxd7", "Rd1", "Qe6", "Bxd7+", "Nxd7", "Qb8+", "Nxb8", "Rd8#",
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
        let mv = text.parse::<San>().expect("valid SAN").to_move(&pos).expect("legal move");
        let before = grid_from_position(&pos);
        pos.play_unchecked(mv);
        steps.push(Step { mv, before, after: grid_from_position(&pos) });
    }
    steps
}
