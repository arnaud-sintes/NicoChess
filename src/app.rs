//! Starting the whole thing: engine, camera thread and web server.
//!
//! The command-line program and the Android app both call [`serve`]; they only
//! differ in the options they build and in the camera they plug in.

use crate::calibration::Calibration;
use crate::camera::{self, SourceSpec};
use crate::engine::{Engine, EngineConfig};
use crate::http::Handler;
use crate::tracker::TrackerConfig;
use crate::{http, server, store::Store};
use anyhow::Result;
use std::net::TcpListener;
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

/// The engine is running and the web handler is ready; nothing listens yet.
pub struct Built {
    pub handler: Handler,
    /// Connections allowed at once on one listener.
    pub max_connections: usize,
    pub listen: String,
}

/// Loads the calibration, opens the store, starts the camera thread and makes the handler.
pub fn build(options: ServerOptions) -> Result<Built> {
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

    // A page holds up to three long connections (events, camera picture, top-down board) and
    // makes ordinary requests besides: leave room for all of that.
    Ok(Built {
        handler: server::handler(engine, options.max_viewers),
        max_connections: options.max_viewers * 3 + 8,
        listen: options.listen,
    })
}

/// Runs the server until it fails: this call does not return normally.
/// `on_ready` is called once the address is bound and requests are accepted.
pub fn serve(options: ServerOptions, on_ready: Option<Box<dyn FnOnce()>>) -> Result<()> {
    let built = build(options)?;
    http::serve(&built.listen, built.max_connections, built.handler, on_ready)
}

/// Like [`serve`], on a listener that is already open (the options' `listen` is not used).
pub fn serve_on(options: ServerOptions, listener: TcpListener, on_ready: Option<Box<dyn FnOnce()>>) -> Result<()> {
    let built = build(options)?;
    http::serve_on(listener, built.max_connections, built.handler, on_ready)
}
