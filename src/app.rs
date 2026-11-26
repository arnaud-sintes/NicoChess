//! Starting the whole thing: engine, camera thread and web server.
//!
//! The command-line program and the Android app both call [`serve_on`]; they only
//! differ in the options they build and in the camera they plug in.

use crate::calibration::Calibration;
use crate::camera::{self, SourceSpec};
use crate::choice::CameraSelection;
use crate::depthlink::DepthLink;
use crate::engine::{Engine, EngineConfig};
use crate::http::Handler;
use crate::record::RecordOptions;
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
    /// Keep a recording of every game (the command-line option --record); the Android app has none.
    pub record: Option<RecordOptions>,
    /// Use the camera the person chose last time from the page (`camera.json` next to the calibration), unless
    /// the camera was named on the command line (`--device`).
    pub remember_camera: bool,
    /// A second camera (`--device2`): the device it opens, in the way `--device` names one. Without it, the second
    /// camera is the one chosen last time on the page (Windows), or none.
    pub second_device: Option<String>,
    /// With the demo or the synthetic board: also a second camera, which shows the board from the other side
    /// (`--second-camera`), to try two cameras without any.
    pub second_synthetic: bool,
    /// With the demo: also a depth camera beside the camera (`--depth-camera`), to try the depth without one.
    pub depth_synthetic: bool,
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

    // With a camera of this computer the page may choose among them (Windows); the choice is kept.
    let camera = match &options.source {
        SourceSpec::Camera(settings) => {
            let selection = CameraSelection::new(settings.device.clone(), Some(options.calibration_path.with_file_name("camera.json")));
            if options.remember_camera {
                selection.restore();
            }
            Some(Arc::new(selection))
        }
        _ => None,
    };
    // A second camera: one to choose (on Windows, or when --device2 names it), or the synthetic one.
    let camera2 = match &options.source {
        SourceSpec::Camera(_) if cfg!(windows) || options.second_device.is_some() => {
            let selection = CameraSelection::new(
                options.second_device.clone().unwrap_or_default(),
                Some(options.calibration_path.with_file_name("camera2.json")),
            );
            if options.second_device.is_none() && options.remember_camera {
                selection.restore();
            }
            Some(Arc::new(selection))
        }
        _ => None,
    };
    let second_fixed = options.second_synthetic && matches!(options.source, SourceSpec::Synthetic | SourceSpec::Demo { .. });
    // The demo game seen by two cameras, or by a camera and a depth camera: they all show the same moment of it.
    let demo_trio = match &options.source {
        SourceSpec::Demo { move_interval } if second_fixed || options.depth_synthetic => {
            Some(camera::demo_trio(options.frame_interval, *move_interval))
        }
        _ => None,
    };
    let (demo_first, demo_second, demo_depth) = match demo_trio {
        Some((first, second, depth)) => (Some(first), Some(second), Some(depth)),
        None => (None, None, None),
    };
    // A camera of this computer may have a depth part beside its colour part (Windows): the page offers to use it.
    let depth = match (&options.source, demo_depth) {
        (SourceSpec::Camera(_), _) if cfg!(windows) => {
            Some(DepthLink::new(crate::depthcam::factory(), Some(options.calibration_path.with_file_name("depth.json"))))
        }
        (_, Some(factory)) if options.depth_synthetic => {
            let link = DepthLink::new(factory, None);
            link.set_offer(Some("demo depth camera".into()));
            Some(link)
        }
        _ => None,
    };
    // A real camera keeps the pictures a second that the page sets; the other sources come at a pace of their own.
    let rate = match &options.source {
        SourceSpec::Camera(settings) => Some(Arc::clone(&settings.rate)),
        _ => None,
    };
    let config = EngineConfig {
        rate,
        fresh: EngineConfig::default().fresh,
        depth: depth.clone(),
        idle_timeout: options.idle_timeout,
        game_timeout: options.game_timeout,
        tracker: options.tracker,
        track_interval: options.track_interval,
        record: options.record,
        camera: camera.clone(),
        camera2: camera2.clone(),
        second_fixed,
        // Testing only: an analysis that cannot keep up with the cameras, to see what the program does then.
        analysis_delay: Duration::ZERO,
        look_delay: std::env::var("NICOCHESS_ANALYSIS_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(Duration::ZERO, Duration::from_millis),
        thin: crate::analysis::global_thin(),
        thin_quiet: crate::tags::QUIET_FOR,
    };
    let engine = Engine::new(config, store, options.calibration_path, calibration);
    // The demo's cameras see the same moment of the game; the other sources have nothing to keep in step.
    let (first_source, second_source) = match (demo_first, demo_second) {
        (Some(first), Some(second)) => (first, second),
        _ => (
            camera::factory_with(options.source.clone(), options.frame_interval, camera),
            camera::second_factory(options.source, options.frame_interval, camera2.clone()),
        ),
    };
    if camera2.is_some() || second_fixed {
        engine.spawn_capture_second(second_source)?;
    }
    engine.spawn_capture(first_source)?;
    if depth.is_some() {
        engine.spawn_depth()?;
    }

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
            record: None,
            remember_camera: false,
            second_device: None,
            second_synthetic: false,
            depth_synthetic: false,
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
