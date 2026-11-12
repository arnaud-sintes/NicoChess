//! NicoChess: follow a chess game on a real board with a webcam.
//!
//! The program serves a web page (for phones and tablets), watches the board with
//! a camera, works out the moves with the chess rules, and records the games.

use anyhow::{Result, bail};
use nicochess::app::{self, ServerOptions};
use nicochess::camera::{self, CameraSettings, SourceSpec};
use nicochess::tracker::TrackerConfig;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
nicochess: track a chess game with a webcam

USAGE:
    nicochess [OPTIONS]

OPTIONS:
    --listen <ADDR>          HTTP address                        [default: 0.0.0.0:8080]
    --source <SRC>           camera | demo | synthetic | <image.jpg>   [default: camera]
    --device <X>             camera: /dev/videoN (Linux) or its number 0, 1... (Windows)  [default: /dev/video0]
    --width <N>              camera width                        [default: 640]
    --height <N>             camera height                       [default: 480]
    --fps <N>                frames per second kept              [default: 3]
    --data-dir <DIR>         where games are stored              [default: data]
    --calibration <FILE>     calibration file                    [default: calibration.json]
    --no-lock-controls       do not turn off auto exposure / white balance
    --exposure <N>           manual exposure (Linux: units of 100 us; Windows: the driver value, see --probe)
    --white-balance <K>      manual white balance temperature (Kelvin)
    --focus <N>              manual focus position (turns autofocus off)
    --zoom <N>               zoom level of the camera (for cameras that crop or zoom on their own)
    --probe                  print the formats and controls of the camera, then exit
    --open                   open the page in this computer’s browser once the server is up
    --max-viewers <N>        open pages allowed at once          [default: 8]
    --idle-timeout <SECS>    controller released after this silence, outside a game  [default: 60]
    --game-timeout <MINS>    ... and during a game               [default: 30]
    --demo-interval <SECS>   time between moves of the demo game [default: 4]

  Tuning the move detection:
    --stable-frames <N>      frames in a row that must agree     [default: 3]
    --motion-threshold <X>   colour change that means 'a hand is there'  [default: 4]
    --max-cost <X>           largest mismatch accepted for a move  [default: 1.6]
    --margin <X>             how much better than the runner-up    [default: 1]

    -h, --help               show this help

'demo' plays a whole game on a synthetic board and 'synthetic' shows the starting
position: both let you try everything on a PC without a camera.";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = pico_args::Arguments::from_env();
    if args.contains(["-h", "--help"]) {
        println!("{HELP}");
        return Ok(());
    }

    let listen: String = args.opt_value_from_str("--listen")?.unwrap_or_else(|| "0.0.0.0:8080".into());
    let source: String = args.opt_value_from_str("--source")?.unwrap_or_else(|| "camera".into());
    let device: String = args.opt_value_from_str("--device")?.unwrap_or_else(|| "/dev/video0".into());
    let width: u32 = args.opt_value_from_str("--width")?.unwrap_or(640);
    let height: u32 = args.opt_value_from_str("--height")?.unwrap_or(480);
    let fps: f32 = args.opt_value_from_str("--fps")?.unwrap_or(3.0);
    let data_dir: PathBuf = args.opt_value_from_str("--data-dir")?.unwrap_or_else(|| PathBuf::from("data"));
    let calibration_path: PathBuf = args
        .opt_value_from_str("--calibration")?
        .unwrap_or_else(|| PathBuf::from("calibration.json"));
    let lock_controls = !args.contains("--no-lock-controls");
    let exposure: Option<i64> = args.opt_value_from_str("--exposure")?;
    let white_balance: Option<i64> = args.opt_value_from_str("--white-balance")?;
    let focus: Option<i64> = args.opt_value_from_str("--focus")?;
    let zoom: Option<i64> = args.opt_value_from_str("--zoom")?;
    let probe = args.contains("--probe");
    let open = args.contains("--open");
    let max_viewers: usize = args.opt_value_from_str("--max-viewers")?.unwrap_or(8);
    let idle_secs: u64 = args.opt_value_from_str("--idle-timeout")?.unwrap_or(60);
    let game_mins: u64 = args.opt_value_from_str("--game-timeout")?.unwrap_or(30);
    let demo_secs: f32 = args.opt_value_from_str("--demo-interval")?.unwrap_or(4.0);

    let defaults = TrackerConfig::default();
    let tracker = TrackerConfig {
        stable_frames: args.opt_value_from_str("--stable-frames")?.unwrap_or(defaults.stable_frames),
        motion_threshold: args.opt_value_from_str("--motion-threshold")?.unwrap_or(defaults.motion_threshold),
        max_cost: args.opt_value_from_str("--max-cost")?.unwrap_or(defaults.max_cost),
        margin: args.opt_value_from_str("--margin")?.unwrap_or(defaults.margin),
    };

    let rest = args.finish();
    if !rest.is_empty() {
        bail!("unknown arguments: {rest:?} (see --help)");
    }
    if probe {
        print!("{}", camera::probe(&device)?);
        return Ok(());
    }
    if !(fps > 0.0 && fps <= 30.0) {
        bail!("--fps must be between 0 and 30");
    }
    if demo_secs <= 0.0 {
        bail!("--demo-interval must be positive");
    }
    let interval = Duration::from_secs_f32(1.0 / fps);

    let spec = match source.as_str() {
        "camera" => SourceSpec::Camera(CameraSettings {
            device,
            width,
            height,
            interval,
            lock_controls,
            exposure,
            white_balance,
            focus,
            zoom,
        }),
        "demo" => SourceSpec::Demo { move_interval: Duration::from_secs_f32(demo_secs) },
        "synthetic" => SourceSpec::Synthetic,
        path => SourceSpec::Image(path.to_string()),
    };

    let on_ready: Option<Box<dyn FnOnce()>> = if open {
        let url = format!("http://{}", listen.replace("0.0.0.0", "localhost"));
        Some(Box::new(move || open_browser(&url)))
    } else {
        None
    };
    let options = ServerOptions {
        listen,
        source: spec,
        frame_interval: interval,
        data_dir,
        calibration_path,
        tracker,
        idle_timeout: Duration::from_secs(idle_secs),
        game_timeout: Duration::from_secs(game_mins * 60),
        max_viewers,
    };
    app::serve(options, on_ready)
}

/// Opens an address in the default browser of this computer.
fn open_browser(url: &str) {
    use std::process::Command;
    #[cfg(windows)]
    let started = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let started = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let started = Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = started {
        eprintln!("cannot open the browser ({e}); open {url} by hand");
    }
}
