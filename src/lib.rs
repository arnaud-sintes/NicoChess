//! NicoChess core: follow a chess game on a real board with a camera.
//!
//! This library holds everything that does not depend on the platform: the chess
//! rules, the board vision, the game recording and the web server with its page.
//! The programs built on it supply the rest:
//!
//! - `src/main.rs`: the command-line program for Windows and Linux,
//! - the Android app (to come): a native library fed with frames by the app.
//!
//! Each platform provides its own camera as a [`camera::FrameSource`].

pub mod app;
pub mod boarddetect;
pub mod calibration;
pub mod camera;
pub mod engine;
pub mod fsutil;
pub mod game;
pub mod http;
pub mod picture;
pub mod pushed;
pub mod roles;
pub mod server;
pub mod store;
pub mod synthetic;
pub mod tracker;
pub mod vision;
