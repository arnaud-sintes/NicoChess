//! NicoChess: follow a chess game on a real board with a webcam.
//!
//! The program serves a web page (for phones and tablets), watches the board with
//! a camera, works out the moves with the chess rules, and records the games.

use anyhow::{Result, bail};
use nicochess::app::{self, ServerOptions};
use nicochess::camera::{self, CameraSettings, SourceSpec};
use nicochess::http;
use nicochess::record::RecordOptions;
use nicochess::replay::{self, TrackerOverrides};
use nicochess::tracker::TrackerConfig;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "\
nicochess: track a chess game with a webcam

USAGE:
    nicochess [OPTIONS]

OPTIONS:
    --listen <ADDR>          HTTP address                        [default: 0.0.0.0:8080; on Windows, if that
                             port is reserved or taken, the next free one among 28080-28089, then any]
    --source <SRC>           camera | demo | synthetic | <image.jpg>   [default: camera]
    --device <X>             camera: /dev/videoN (Linux) or its number 0, 1... (Windows)  [default: /dev/video0, or 0 on Windows]
    --device2 <X>            a second camera, to look at the board from another side; the pictures of both are used
                             together. On Windows it can also be chosen on the page and is then kept. Without it the
                             program works with one camera
    --second-camera          with --source demo or synthetic: a second camera that shows the board from the other
                             side, to try two cameras without any
    --depth-camera           with --source demo: a depth camera beside the camera, to try the depth without one (a
                             camera with a depth part, such as an Intel RealSense, offers it on the page)
    --width <N>              camera width                        [default: 640]
    --height <N>             camera height                       [default: 480]
    --fps <N>                pictures per second kept from the camera (the page has a slider for it)
                             [default: 25 on Windows, 3 elsewhere]
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

  Recording games, to look into them later:
    --record                 keep a recording of every game: a folder named after the start (date and time) with
                             the pictures the detection saw, what it decided, and what the people did
    --record-dir <DIR>       where the recordings go             [default: recordings]
    --record-every <SECS>    time between two pictures kept while the board is quiet; while something happens
                             a few a second are kept. 0 keeps all the quiet ones  [default: 2]
    --record-all             record, and keep EVERY picture each camera gives, the moment it is captured, whether
                             the detection looks at it or not (the depth camera too): a game that must be followed
                             again picture by picture, for example a blitz game. Use it instead of --record. The
                             pictures that wait for the disk or the encoder take memory, up to --record-budget; past
                             it some are left out, and the recording says exactly which (`gap` lines)
    --record-budget <MB>     with --record-all: the memory for pictures that wait for the disk or the encoder
                             [default: 2048]
    --ffmpeg <PATH>          the ffmpeg program (a portable ffmpeg.exe is enough) that writes the recorded pictures
                             as video, five to twenty times smaller than JPEG files, and reads them back for a
                             replay. Without it: the one next to the program, the one named by NICOCHESS_FFMPEG, then
                             the one on PATH; with none, the pictures are JPEG files
    --no-video               record JPEG files even when ffmpeg is there
    --replay <DIR>           no camera, no page: run a recording (or all those in a folder) through the
                             detection again and compare it with what happened. The tuning options below
                             replace the recorded ones, to try them on real pictures
    --replay-verbose         with --replay: also list every change of the tracker's status
    --replay-all             with --replay: feed the tracker every picture of a recording made with --record-all,
                             one after the other at the moment it was captured, instead of the pictures it was
                             given live (how the program will use the pictures later)

  Tuning the move detection:
    --stable-frames <N>      pictures in a row that must agree   [default: 3]
    --stable-time <S>        seconds the board must be still     [default: 0.2]
    --motion-span <S>        a picture is compared with the one taken this many seconds before, to see whether
                             something moves (a hand that creeps in changes little from one picture to the next)
                             [default: 0.1]
    --motion-threshold <X>   colour change that means 'a hand is there'  [default: 4]
    --max-excess <X>         misfit left unexplained that is still accepted (nats)  [default: 60]
    --margin <X>             how much likelier than the runner-up a move must be (nats)  [default: 6]
    --max-plies <N>          moves in a row that may be played between two pictures, 1 to 3  [default: 3]
    --soft-margin <X>        a move that leads by this much (nats) but less than --margin is taken and flagged as
                             doubtful on the page, with a one-click correction; --margin or more asks instead  [default: 2]

    -V, --version            show the version

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
    if args.contains(["-V", "--version"]) {
        println!("nicochess {}", nicochess::version_text());
        return Ok(());
    }

    let listen_option: Option<String> = args.opt_value_from_str("--listen")?;
    let source: String = args.opt_value_from_str("--source")?.unwrap_or_else(|| "camera".into());
    let default_device = if cfg!(windows) { "0" } else { "/dev/video0" };
    let device_option: Option<String> = args.opt_value_from_str("--device")?;
    // Without --device the camera chosen last time on the page is used (if it is still there).
    let remember_camera = device_option.is_none();
    let device: String = device_option.unwrap_or_else(|| default_device.into());
    let second_device: Option<String> = args.opt_value_from_str("--device2")?;
    let second_synthetic = args.contains("--second-camera");
    let depth_synthetic = args.contains("--depth-camera");
    let width: u32 = args.opt_value_from_str("--width")?.unwrap_or(640);
    let height: u32 = args.opt_value_from_str("--height")?.unwrap_or(480);
    // Pictures a second: what was asked, else what the page set last time, else the default (a small box would
    // not keep up with 25 a second: the page's slider sets it for that one).
    let fps_option: Option<f32> = args.opt_value_from_str("--fps")?;
    let data_dir: PathBuf = args.opt_value_from_str("--data-dir")?.unwrap_or_else(|| PathBuf::from("data"));
    let calibration_path: PathBuf = args.opt_value_from_str("--calibration")?.unwrap_or_else(|| PathBuf::from("calibration.json"));
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

    // Only the values that are given: a replay keeps the recorded ones for the others.
    let overrides = TrackerOverrides {
        stable_frames: args.opt_value_from_str("--stable-frames")?,
        stable_time: args.opt_value_from_str("--stable-time")?,
        motion_span: args.opt_value_from_str("--motion-span")?,
        motion_threshold: args.opt_value_from_str("--motion-threshold")?,
        max_excess: args.opt_value_from_str("--max-excess")?,
        margin: args.opt_value_from_str("--margin")?,
        max_plies: args.opt_value_from_str("--max-plies")?,
        soft_margin: args.opt_value_from_str("--soft-margin")?,
    };
    let tracker = overrides.apply(TrackerConfig::default());

    let record_all = args.contains("--record-all");
    let record = args.contains("--record") || record_all;
    let record_dir: PathBuf = args.opt_value_from_str("--record-dir")?.unwrap_or_else(|| PathBuf::from("recordings"));
    let record_every_option: Option<f32> = args.opt_value_from_str("--record-every")?;
    if record_all && record_every_option.is_some() {
        bail!("--record-all keeps every picture already: leave --record-every out");
    }
    let record_every = record_every_option.unwrap_or(2.0);
    let replay: Option<PathBuf> = args.opt_value_from_str("--replay")?;
    let replay_verbose = args.contains("--replay-verbose");
    let replay_all = args.contains("--replay-all");
    let record_budget_mb: Option<usize> = args.opt_value_from_str("--record-budget")?;
    let ffmpeg_option: Option<PathBuf> = args.opt_value_from_str("--ffmpeg")?;
    let no_video = args.contains("--no-video");

    let rest = args.finish();
    if !rest.is_empty() {
        bail!("unknown arguments: {rest:?} (see --help)");
    }
    if let Some(path) = &ffmpeg_option {
        if !path.is_file() {
            bail!("--ffmpeg {}: there is no such file", path.display());
        }
        if no_video {
            bail!("--ffmpeg and --no-video contradict each other");
        }
        nicochess::video::set_explicit(path.clone());
    }
    if let Some(path) = replay {
        let feed = if replay_all { replay::Feed::EveryPicture } else { replay::Feed::AsLive };
        return replay::run(&path, &overrides, replay_verbose, feed);
    }
    if replay_verbose || replay_all {
        bail!("--replay-verbose and --replay-all go with --replay");
    }
    if record_budget_mb.is_some() && !record_all {
        bail!("--record-budget goes with --record-all");
    }
    if record_budget_mb == Some(0) {
        bail!("--record-budget must be at least 1 (megabytes)");
    }
    if !(record_every >= 0.0 && record_every.is_finite()) {
        bail!("--record-every must be 0 or more");
    }
    if probe {
        print!("{}", camera::probe(&device)?);
        return Ok(());
    }
    let fps = fps_option.or_else(|| nicochess::engine::remembered_fps(&calibration_path)).unwrap_or(if cfg!(windows) { 25.0 } else { 3.0 });
    if !(fps > 0.0 && fps <= 30.0) {
        bail!("--fps must be between 0 and 30");
    }
    if demo_secs <= 0.0 {
        bail!("--demo-interval must be positive");
    }
    let interval = Duration::from_secs_f32(1.0 / fps);
    if second_device.is_some() && source != "camera" {
        bail!("--device2 goes with --source camera (with the demo or the synthetic board, use --second-camera)");
    }
    if second_device.as_deref().is_some_and(|d| d == device) {
        bail!("--device2 is the same camera as --device");
    }
    if depth_synthetic && source != "demo" {
        bail!("--depth-camera goes with --source demo (a camera with a depth part offers it on the page)");
    }
    if second_synthetic && !matches!(source.as_str(), "demo" | "synthetic") {
        bail!("--second-camera goes with --source demo or synthetic (with a camera, use --device2)");
    }

    let spec = match source.as_str() {
        "camera" => SourceSpec::Camera(CameraSettings {
            device,
            width,
            height,
            rate: nicochess::camera::FrameRate::new(fps),
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

    // Where to listen. Without --listen: port 8080 and, on Windows, a way out when the system
    // keeps that port for something else (Docker and Hyper-V reserve whole ranges of ports).
    let candidates: Vec<String> = match &listen_option {
        Some(addr) => vec![addr.clone()],
        None if cfg!(windows) => ["0.0.0.0:8080"]
            .into_iter()
            .map(String::from)
            .chain((28080..28090).map(|p| format!("0.0.0.0:{p}")))
            .chain(["0.0.0.0:0".to_string()])
            .collect(),
        None => vec!["0.0.0.0:8080".to_string()],
    };
    let listener = http::bind_first(&candidates).map_err(|e| anyhow::anyhow!("{e:#}{}", PORT_HINT))?;
    let bound = listener.local_addr()?;
    let local_url = format!("http://localhost:{}", bound.port());
    let from_network = bound.ip().is_unspecified().then(|| local_ip().map(|ip| format!("http://{ip}:{}", bound.port()))).flatten();
    let on_ready: Option<Box<dyn FnOnce()>> = Some(Box::new(move || {
        eprintln!();
        eprintln!("NicoChess {} is running.", nicochess::version_text());
        eprintln!("  On this computer:   {local_url}");
        if let Some(url) = &from_network {
            eprintln!("  From a phone or tablet on the same network:   {url}");
        }
        eprintln!("  Close this window (or press Ctrl+C) to stop.");
        eprintln!();
        if open {
            open_browser(&local_url);
        }
    }));
    let options = ServerOptions {
        listen: bound.to_string(),
        source: spec,
        frame_interval: interval,
        data_dir,
        calibration_path,
        tracker,
        // A little less than the time between pictures, so jitter never skips one.
        track_interval: interval.mul_f32(0.8),
        idle_timeout: Duration::from_secs(idle_secs),
        game_timeout: Duration::from_secs(game_mins * 60),
        max_viewers,
        record: record.then(|| {
            let ffmpeg = if no_video { None } else { nicochess::video::find_ffmpeg() };
            match &ffmpeg {
                Some(path) => eprintln!("record: the pictures are written as video through {}", path.display()),
                None if no_video => eprintln!("record: the pictures are written as JPEG files (--no-video)"),
                None => eprintln!(
                    "record: no ffmpeg found (--ffmpeg, next to the program, NICOCHESS_FFMPEG or PATH): the pictures are written as JPEG files, about ten times bigger"
                ),
            }
            let options = if record_all {
                let keeping = RecordOptions::keeping_all(record_dir);
                match record_budget_mb {
                    Some(mb) => keeping.with_budget(mb.saturating_mul(1 << 20)),
                    None => keeping,
                }
            } else {
                RecordOptions::new(record_dir, Duration::from_secs_f32(record_every))
            };
            options.with_ffmpeg(ffmpeg)
        }),
        remember_camera,
        second_device,
        second_synthetic,
        depth_synthetic,
    };
    app::serve_on(options, listener, on_ready)
}

const PORT_HINT: &str = "\n  The port may be taken or reserved. On Windows, Docker Desktop and Hyper-V reserve ranges of ports\n  (see them with: netsh int ipv4 show excludedportrange protocol=tcp). Choose a free port with\n  --listen 0.0.0.0:PORT (for example 28080).";

/// The address of this computer on the local network, if it has one. Nothing is sent: a UDP
/// socket only works out which interface it would use.
fn local_ip() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
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
