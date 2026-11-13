//! NicoChess core: follow a chess game on a real board with a camera.
//!
//! This library holds everything that does not depend on the platform: the chess
//! rules, the board vision, the game recording and the web server with its page.
//! The programs built on it supply the rest:
//!
//! - `src/main.rs`: the command-line program for Windows and Linux,
//! - the Android app (`android/`): a native library fed with frames by the app.
//!
//! Each platform provides its own camera as a [`camera::FrameSource`].

/// The version of NicoChess (`[workspace.package]` in `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git commit the program was built from, or "" when unknown (see `build.rs`).
pub const COMMIT: &str = env!("NICOCHESS_COMMIT");

/// The version as shown to people: "0.2.0 (3f2a9c1e)", or just "0.2.0" when the commit is unknown.
pub fn version_text() -> String {
    if COMMIT.is_empty() { VERSION.to_string() } else { format!("{VERSION} ({COMMIT})") }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_text_starts_with_the_version_and_adds_the_commit_when_known() {
        let text = version_text();
        assert!(text.starts_with(VERSION), "{text}");
        assert_eq!(text.contains('('), !COMMIT.is_empty());
    }

    /// The Android build reads the version from the first `version = "x.y.z"` line of Cargo.toml
    /// (android/app/build.gradle.kts): that line must be the version of the program, and the Android
    /// library must follow it (not carry a version of its own).
    #[test]
    fn the_version_is_defined_once_and_the_android_build_can_read_it() {
        let cargo = include_str!("../Cargo.toml");
        let first = cargo.lines().find(|line| line.trim().starts_with("version = \"")).expect("a version line");
        assert_eq!(first.trim(), format!("version = \"{VERSION}\""));
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "x.y.z");
        assert!(parts.iter().all(|part| part.parse::<u32>().is_ok()), "{VERSION}");
        let native = include_str!("../android/native/Cargo.toml");
        assert!(native.contains("version.workspace = true"), "the Android library takes the version from the workspace");
    }
}
