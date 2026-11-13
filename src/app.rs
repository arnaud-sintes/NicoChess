//! Starting the whole thing: engine, camera thread and web server.
//!
//! The command-line program and the Android app both call [`serve_on`]; they only
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
use std::sync::Arc;
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
    /// The engine, for the host program to ask about (is a game running?).
    pub engine: std::sync::Arc<Engine>,
    pub handler: Handler,
    /// Connections allowed at once on one listener.
    pub max_connections: usize,
    pub listen: String,
}

/// Loads the calibration, opens the store, starts the camera thread and makes the handler.
pub fn build(options: ServerOptions) -> Result<Built> {
    // A calibration file that cannot be read is reported and ignored, as the picture settings are:
    // the program must start (it is a service that restarts, and an app) and the board be calibrated again.
    let calibration = Calibration::load(&options.calibration_path).unwrap_or_else(|e| {
        eprintln!("calibration: {e:#}; using none, calibrate again");
        None
    });
    match &calibration {
        Some(_) => eprintln!("calibration: loaded {}", options.calibration_path.display()),
        None => eprintln!("calibration: none yet, open the web page and use Calibrate"),
    }
    let store = Store::open(&options.data_dir)?;
    eprintln!("store: games are kept in {}", options.data_dir.display());

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
        handler: server::handler(Arc::clone(&engine), options.max_viewers),
        engine,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Request;
    use std::time::Duration;

    #[test]
    fn a_calibration_file_that_cannot_be_read_does_not_stop_the_program() {
        let dir = std::env::temp_dir().join(format!("nicochess-corrupt-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("calibration.json"), "{ this is not json").unwrap();
        let options = ServerOptions {
            listen: "127.0.0.1:0".into(),
            source: SourceSpec::Synthetic,
            frame_interval: Duration::from_millis(50),
            data_dir: dir.join("games"),
            calibration_path: dir.join("calibration.json"),
            tracker: TrackerConfig::default(),
            track_interval: Duration::ZERO,
            idle_timeout: Duration::from_secs(60),
            game_timeout: Duration::from_secs(60),
            max_viewers: 4,
        };
        let built = build(options).expect("the program starts without a calibration");
        let reply = (built.handler)(Request::for_test("GET", "/api/state?token=", &[], ""));
        let crate::http::Reply::Bytes { status, body, .. } = reply else { panic!("expected bytes") };
        assert_eq!(status, 200);
        let state: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(state["calibration"].is_null(), "no calibration: the board is calibrated again");
        std::fs::remove_dir_all(&dir).ok();
    }
}
