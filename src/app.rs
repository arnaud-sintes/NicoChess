//! Starting the whole thing: engine, camera thread and web server.
//!
//! The command-line program and the Android app both call [`serve`]; they only
//! differ in the options they build and in the camera they plug in.

use crate::calibration::Calibration;
use crate::camera::{self, SourceSpec};
use crate::engine::{Engine, EngineConfig};
use crate::tracker::TrackerConfig;
use crate::{http, server, store::Store};
use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

pub struct ServerOptions {
    /// Address to listen on, e.g. `0.0.0.0:8080` or `127.0.0.1:8080`.
    pub listen: String,
    pub source: SourceSpec,
    /// Time between two frames kept from the camera.
    pub frame_interval: Duration,
    pub data_dir: PathBuf,
    pub calibration_path: PathBuf,
    pub tracker: TrackerConfig,
    /// The tracker looks at a picture at most this often.
    pub track_interval: Duration,
    /// Controller released after this silence, outside a game.
    pub idle_timeout: Duration,
    /// ... and during a game.
    pub game_timeout: Duration,
    /// Open pages allowed at once.
    pub max_viewers: usize,
}

/// Runs the server until it fails: this call does not return normally.
/// `on_ready` is called once the address is bound and requests are accepted.
pub fn serve(options: ServerOptions, on_ready: Option<Box<dyn FnOnce()>>) -> Result<()> {
    let calibration = Calibration::load(&options.calibration_path)?;
    match &calibration {
        Some(_) => eprintln!("calibration: loaded {}", options.calibration_path.display()),
        None => eprintln!("calibration: none yet, open the web page and use Calibrate"),
    }
    let store = Store::open(&options.data_dir)?;
    eprintln!("games: stored in {}", options.data_dir.display());

    let config = EngineConfig {
        idle_timeout: options.idle_timeout,
        game_timeout: options.game_timeout,
        tracker: options.tracker,
        track_interval: options.track_interval,
    };
    let engine = Engine::new(config, store, options.calibration_path, calibration);
    engine.spawn_capture(camera::factory(options.source, options.frame_interval))?;

    // Event streams each hold a connection, so leave room for ordinary requests too.
    http::serve(&options.listen, options.max_viewers + 16, server::handler(engine, options.max_viewers), on_ready)
}
