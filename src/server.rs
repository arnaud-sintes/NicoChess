//! HTTP routes: the page, the camera images, the JSON API and the event stream.
//!
//!   GET    / or /index.html            the page
//!   GET    /frame.jpg                  latest camera frame
//!   GET    /frame.mjpg                 the camera frames as they come (Motion JPEG stream)
//!   GET    /warp.jpg[?grid=0]          top-down board (needs a calibration)
//!   GET    /warp.mjpg                  the top-down board as a Motion JPEG stream
//!   GET    /api/camera/stats           pictures per second and time per picture
//!   GET    /api/camera/list            the cameras of the computer and the ones in use (Windows)
//!   GET    /depth.jpg                  what the depth camera sees (the height picture), if the program reads one
//!   GET    /depth-warp.jpg             the same as a top-down board, once the board was found in it
//!
//! Everything that is about one camera (the five pictures and figures above, the calibration, the picture settings
//! and the choice of the camera below) takes `?cam=2` for the second camera, and the main one without it (or `?cam=1`).
//!   GET    /api/state?token=T          full state for one user (JSON)
//!   GET    /api/events?token=T         the same state, pushed on every change (SSE)
//!   GET    /api/games                  list of games, most recent first
//!   GET    /api/games/ID               one game with the position after each move
//!   GET    /api/games/ID/pgn           one game as PGN
//!
//! Commands. They need the header `X-Client-Token` and the caller must be the
//! controller (except `claim`):
//!   POST   /api/claim                  take control when nobody has it
//!   POST   /api/release                give control up
//!   POST   /api/calibration/open       camera on for the calibration page
//!   POST   /api/calibration/close      camera off again
//!   POST   /api/calibration            {"corners": [[x,y] x4]}  order a1, h1, h8, a8
//!   POST   /api/calibration/detect     find the board in the picture and calibrate with it
//!   POST   /api/camera/rate            {"fps": 25}  pictures per second kept from the camera (1 to 30), at once
//!   GET    /api/game/legal             {"moves": ["e2e4", ...]}  the legal moves of the game, for dragging a piece
//!   POST   /api/depth                  {"on": true}  also use the depth of the camera (when it has a depth part)
//!   POST   /api/depth/align            look for the board in the depth picture now (the pieces in their positions)
//!   POST   /api/depth/settings         {"near_mm","far_mm","width","height","fps"} (any of them) what the depth camera
//!                                      gives and which distances count; not during a game
//!   POST   /api/camera/follow          {"on": true}  follow the board if it moves during a game
//!   POST   /api/camera/select          {"id": "1"}  use that camera (not during a game; forgets the calibration);
//!                                      with ?cam=2 also {"id": ""}: no second camera
//!   DELETE /api/calibration            forget the calibration
//!   POST   /api/camera/rotate          {"turns": 1}  quarter turns clockwise (-1: counter-clockwise)
//!   POST   /api/camera/picture         {"brightness": 0, "contrast": 1}
//!   POST   /api/camera/auto-contrast   stretch the picture of the board over the whole range
//!   POST   /api/camera/picture/reset   brightness and contrast back to neutral
//!   POST   /api/game/start             {"white","black","fen"?,"started_at"?,"minutes"?,"extra_seconds"?}  with minutes, a
//!                                      game with a clock: that many minutes for each player, and the extra seconds
//!                                      added after each move
//!   POST   /api/game/clock             {"action": "switch"|"pause"|"resume"}  the button or space bar of the clock (starts
//!                                      it, goes on after a pause, or hands the time over), a pause, going on
//!   POST   /api/game/resume           go on with the unfinished game found at start (the board must show its position)
//!   POST   /api/game/end               {"result","reason"}  (also closes an unfinished game that is not running)
//!   POST   /api/game/move              {"move": "e2e4" or "Nf3"}
//!   POST   /api/game/undo              take back the last move
//!   POST   /api/game/promotion         {"piece": "q"|"r"|"b"|"n"}
//!   POST   /api/game/relearn           learn the look of the squares again
//!   POST   /api/game/doubt             {"move": "d2d4"} put a doubtful move right with another likely one; {} takes it back and asks

use crate::engine::{ApiError, ApiResult, Engine, StartRequest};
use crate::game::TimeControl;
use crate::http::{Handler, Reply, Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

const INDEX_HTML: &str = include_str!("../web/index.html");

/// The top-down board is redrawn at most about every this long.
const TOP_DOWN_PAUSE: Duration = Duration::from_millis(100);

/// Longest silence on an event stream before a keep-alive line is sent.
const KEEPALIVE: Duration = Duration::from_secs(15);

pub fn handler(engine: Arc<Engine>, max_viewers: usize) -> Handler {
    Arc::new(move |request: Request| route(&engine, max_viewers, request))
}

fn ok() -> Reply {
    Reply::json(200, &json!({ "ok": true }))
}

fn failure(error: ApiError) -> Reply {
    Reply::json(error.status(), &json!({ "ok": false, "error": error.message() }))
}

fn done(result: ApiResult) -> Reply {
    match result {
        Ok(()) => ok(),
        Err(e) => failure(e),
    }
}

fn body_json<T: for<'de> Deserialize<'de>>(request: &Request) -> ApiResult<T> {
    serde_json::from_slice(&request.body).map_err(|e| ApiError::BadRequest(format!("bad JSON: {e}")))
}

fn route(engine: &Arc<Engine>, max_viewers: usize, request: Request) -> Reply {
    let method = request.method.as_str();
    let path = request.path.as_str();
    let token = request.header("x-client-token").unwrap_or("").to_string();

    match (method, path) {
        ("GET", "/" | "/index.html") => Reply::bytes(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes().to_vec()),

        ("GET", "/frame.jpg") => match cam_of(&request) {
            Ok(cam) => match engine.latest_frame(cam) {
                Some(frame) => Reply::bytes(200, "image/jpeg", frame.jpeg().as_ref().clone()),
                None => Reply::text(503, "the camera is off"),
            },
            Err(e) => Reply::text(e.status(), &e.message()),
        },
        ("GET", "/frame.mjpg") => match cam_of(&request) {
            Ok(cam) => frame_stream(engine, false, cam),
            Err(e) => Reply::text(e.status(), &e.message()),
        },
        ("GET", "/warp.mjpg") => match cam_of(&request) {
            Ok(cam) => frame_stream(engine, true, cam),
            Err(e) => Reply::text(e.status(), &e.message()),
        },
        ("GET", "/api/camera/stats") => match cam_of(&request) {
            Ok(cam) => Reply::json(200, &engine.camera_stats(cam)),
            Err(e) => Reply::text(e.status(), &e.message()),
        },
        ("GET", "/api/camera/list") => Reply::json(200, &engine.cameras()),
        ("GET", "/depth.jpg" | "/depth-warp.jpg") => match engine.depth_jpeg(path == "/depth-warp.jpg") {
            Ok(bytes) => Reply::bytes(200, "image/jpeg", bytes),
            Err(e) => Reply::text(503, &e.message()),
        },
        ("GET", "/warp.jpg") => {
            let grid = !request.query.split('&').any(|q| q == "grid=0");
            match cam_of(&request).and_then(|cam| engine.warped_jpeg(cam, grid)) {
                Ok(bytes) => Reply::bytes(200, "image/jpeg", bytes),
                Err(e) => Reply::text(e.status(), &e.message()),
            }
        }

        ("GET", "/api/state") => {
            let token = request.query_param("token").unwrap_or("");
            Reply::json(200, &engine.snapshot(token))
        }
        ("GET", "/api/events") => events(engine, request.query_param("token").unwrap_or("").to_string(), max_viewers),

        ("GET", "/api/game/legal") => Reply::json(200, &json!({ "moves": engine.legal_moves() })),
        ("GET", "/api/games") => match engine.list_games() {
            Ok(list) => Reply::json(200, &json!({ "games": list })),
            Err(e) => Reply::json(500, &json!({ "ok": false, "error": format!("{e:#}") })),
        },
        ("GET", p) if p.starts_with("/api/games/") => game_route(engine, &p["/api/games/".len()..]),

        ("POST", "/api/claim") => done(engine.claim(&token)),
        ("POST", "/api/release") => done(engine.release(&token)),

        ("POST", "/api/calibration/open") => done(engine.open_calibration(&token)),
        ("POST", "/api/calibration/close") => done(engine.close_calibration(&token)),
        ("POST", "/api/calibration") => {
            #[derive(Deserialize)]
            struct Body {
                corners: [[f32; 2]; 4],
            }
            done(cam_of(&request).and_then(|cam| body_json::<Body>(&request).and_then(|b| engine.set_calibration(&token, cam, b.corners))))
        }
        ("POST", "/api/calibration/detect") => match cam_of(&request).and_then(|cam| engine.detect_calibration(&token, cam)) {
            Ok(message) => Reply::json(200, &json!({ "ok": true, "message": message })),
            Err(e) => failure(e),
        },
        ("DELETE", "/api/calibration") => done(cam_of(&request).and_then(|cam| engine.clear_calibration(&token, cam))),
        ("POST", "/api/game/doubt") => {
            #[derive(Deserialize)]
            struct Body {
                #[serde(rename = "move", default)]
                mv: Option<String>,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.resolve_doubt(&token, b.mv.as_deref().filter(|m| !m.is_empty()))))
        }
        ("POST", "/api/camera/follow") => {
            #[derive(Deserialize)]
            struct Body {
                on: bool,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.set_follow(&token, b.on)))
        }

        ("POST", "/api/depth") => {
            #[derive(Deserialize)]
            struct Body {
                on: bool,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.set_depth(&token, b.on)))
        }
        ("POST", "/api/depth/align") => done(engine.align_depth(&token)),
        ("POST", "/api/depth/settings") => {
            done(body_json::<crate::depthlink::DepthPatch>(&request).and_then(|p| engine.set_depth_settings(&token, p)))
        }
        ("POST", "/api/camera/rate") => {
            #[derive(Deserialize)]
            struct Body {
                fps: f32,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.set_fps(&token, b.fps)))
        }

        ("POST", "/api/camera/select") => {
            #[derive(Deserialize)]
            struct Body {
                id: String,
            }
            done(cam_of(&request).and_then(|cam| body_json::<Body>(&request).and_then(|b| engine.select_camera(&token, cam, &b.id))))
        }
        ("POST", "/api/camera/rotate") => {
            #[derive(Deserialize)]
            struct Body {
                turns: i32,
            }
            done(cam_of(&request).and_then(|cam| body_json::<Body>(&request).and_then(|b| engine.rotate_picture(&token, cam, b.turns))))
        }
        ("POST", "/api/camera/picture") => {
            #[derive(Deserialize)]
            struct Body {
                brightness: f32,
                contrast: f32,
            }
            done(
                cam_of(&request).and_then(|cam| {
                    body_json::<Body>(&request).and_then(|b| engine.set_picture_adjust(&token, cam, b.brightness, b.contrast))
                }),
            )
        }
        ("POST", "/api/camera/auto-contrast") => done(cam_of(&request).and_then(|cam| engine.auto_contrast(&token, cam))),
        ("POST", "/api/camera/picture/reset") => done(cam_of(&request).and_then(|cam| engine.reset_picture_adjust(&token, cam))),

        ("POST", "/api/game/start") => {
            #[derive(Deserialize, Default)]
            struct Body {
                #[serde(default)]
                white: String,
                #[serde(default)]
                black: String,
                fen: Option<String>,
                started_at: Option<String>,
                /// Minutes each player has; none (or 0) for a game without a clock.
                #[serde(default)]
                minutes: Option<f64>,
                /// Seconds added to the time of a player after each move.
                #[serde(default)]
                extra_seconds: Option<f64>,
            }
            done(body_json::<Body>(&request).and_then(|b| {
                let time_control = match b.minutes.filter(|m| *m != 0.0) {
                    Some(minutes) => Some(
                        TimeControl::checked(minutes * 60.0, b.extra_seconds.unwrap_or(0.0))
                            .map_err(|e| ApiError::BadRequest(e.to_string()))?,
                    ),
                    None => None,
                };
                engine
                    .start_game(&token, StartRequest { white: b.white, black: b.black, fen: b.fen, started_at: b.started_at, time_control })
            }))
        }
        ("POST", "/api/game/clock") => {
            #[derive(Deserialize)]
            struct Body {
                action: String,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.clock_action(&token, &b.action)))
        }
        ("POST", "/api/game/resume") => done(engine.resume_game(&token)),
        ("POST", "/api/game/end") => {
            #[derive(Deserialize)]
            struct Body {
                result: String,
                reason: String,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.end_game(&token, &b.result, &b.reason)))
        }
        ("POST", "/api/game/move") => {
            #[derive(Deserialize)]
            struct Body {
                #[serde(rename = "move")]
                text: String,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.manual_move(&token, &b.text)))
        }
        ("POST", "/api/game/undo") => done(engine.undo(&token)),
        ("POST", "/api/game/promotion") => {
            #[derive(Deserialize)]
            struct Body {
                piece: String,
            }
            done(body_json::<Body>(&request).and_then(|b| {
                let mut chars = b.piece.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => engine.set_promotion(&token, c),
                    _ => Err(ApiError::BadRequest("piece must be one letter".into())),
                }
            }))
        }
        ("POST", "/api/game/relearn") => done(engine.relearn(&token)),

        ("GET" | "POST" | "DELETE", _) => Reply::text(404, "not found"),
        _ => Reply::text(405, "method not allowed").with_header("Allow", "GET, POST, DELETE"),
    }
}

/// The camera a request is about: `?cam=2` is the second camera, no `cam` (or `cam=1`) the main one. The engine
/// refuses the second camera when there is none.
fn cam_of(request: &Request) -> ApiResult<usize> {
    match request.query_param("cam") {
        None | Some("" | "1") => Ok(0),
        Some("2") => Ok(1),
        Some(_) => Err(ApiError::BadRequest("cam must be 1 or 2".into())),
    }
}

/// `ID` or `ID/pgn`.
fn game_route(engine: &Engine, rest: &str) -> Reply {
    let (id, want_pgn) = match rest.strip_suffix("/pgn") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    if !crate::store::valid_id(id) {
        return Reply::text(400, "invalid game id");
    }
    if want_pgn {
        match engine.game_pgn(id) {
            Ok(Some(pgn)) => Reply::bytes(200, "application/x-chess-pgn; charset=utf-8", pgn.into_bytes())
                .with_header("Content-Disposition", &format!("attachment; filename=\"{id}.pgn\"")),
            Ok(None) => Reply::text(404, "no such game"),
            Err(e) => Reply::text(500, &format!("{e:#}")),
        }
    } else {
        match engine.game_detail(id) {
            Ok(Some(game)) => Reply::json(200, &game),
            Ok(None) => Reply::text(404, "no such game"),
            Err(e) => Reply::text(500, &format!("{e:#}")),
        }
    }
}

/// The camera frames as a Motion JPEG stream: one connection, a new picture as soon as
/// there is one. The page shows it in a plain <img>. Only the newest picture is sent,
/// so a slow link skips pictures instead of falling behind.
fn frame_stream(engine: &Arc<Engine>, top_down: bool, cam: usize) -> Reply {
    if cam == 1 && !engine.has_second_camera() {
        return Reply::text(400, "there is no second camera");
    }
    let Some(guard) = engine.open_frame_stream() else {
        return Reply::text(503, "too many picture streams are open");
    };
    let engine = Arc::clone(engine);
    Reply::Stream {
        content_type: "multipart/x-mixed-replace; boundary=frame".to_string(),
        run: Box::new(move |out: &mut dyn Write| {
            let _guard = guard;
            let mut last = None;
            let mut silent_since = std::time::Instant::now();
            let mut uncalibrated_since: Option<std::time::Instant> = None;
            loop {
                match engine.wait_for_frame(cam, last.as_ref(), Duration::from_secs(1)) {
                    Some(frame) => {
                        silent_since = std::time::Instant::now();
                        let jpeg: Arc<Vec<u8>> = if top_down {
                            // The board view needs no more than a few pictures per second.
                            std::thread::sleep(TOP_DOWN_PAUSE);
                            let newest = engine.latest_frame(cam).unwrap_or_else(|| Arc::clone(&frame));
                            let made = engine.warped_jpeg_of(cam, &newest, true);
                            last = Some(newest);
                            match made {
                                Ok(bytes) => {
                                    uncalibrated_since = None;
                                    Arc::new(bytes)
                                }
                                // Not calibrated (any more): nothing to show. Nothing is written, so a page that
                                // has left would never be noticed: end the stream after a few seconds (the page
                                // opens it again when there is a calibration).
                                Err(_) => {
                                    if uncalibrated_since.get_or_insert_with(std::time::Instant::now).elapsed() > Duration::from_secs(3) {
                                        return Ok(());
                                    }
                                    continue;
                                }
                            }
                        } else {
                            last = Some(Arc::clone(&frame));
                            frame.jpeg()
                        };
                        // One write: a header, the picture and the line end go out together.
                        let mut part = Vec::with_capacity(jpeg.len() + 80);
                        write!(part, "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", jpeg.len())?;
                        part.extend_from_slice(&jpeg);
                        part.extend_from_slice(b"\r\n");
                        out.write_all(&part)?;
                        out.flush()?;
                    }
                    None => {
                        // The camera is off, or silent: do not hold the connection for ever.
                        if engine.camera_is_idle() && silent_since.elapsed() > Duration::from_secs(10) {
                            return Ok(());
                        }
                    }
                }
            }
        }),
    }
}

/// Server-sent events: the state is sent when the stream opens and again after
/// every change. It is the whole state each time, so a page that was asleep catches
/// up with the first message after it reconnects.
fn events(engine: &Arc<Engine>, token: String, max_viewers: usize) -> Reply {
    let Some((guard, changes)) = engine.open_stream(max_viewers) else {
        return Reply::text(503, "too many pages are open");
    };
    let engine = Arc::clone(engine);
    Reply::Stream {
        content_type: "text/event-stream".to_string(),
        run: Box::new(move |out: &mut dyn Write| {
            let _guard = guard;
            out.write_all(b"retry: 3000\n\n")?;
            let mut last = String::new();
            loop {
                // Looking at the state also tells the server this user is still here.
                let state: Value = engine.snapshot(&token);
                let text = state.to_string();
                if text != last {
                    write!(out, "event: state\ndata: {text}\n\n")?;
                    out.flush()?;
                    last = text;
                }
                match changes.recv_timeout(KEEPALIVE) {
                    Ok(()) => {
                        // Let a burst of changes settle, then send one message.
                        std::thread::sleep(Duration::from_millis(15));
                        while changes.try_recv().is_ok() {}
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        out.write_all(b": keepalive\n\n")?;
                        out.flush()?;
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{self, SourceSpec};
    use crate::engine::EngineConfig;
    use crate::store::Store;
    use crate::synthetic;
    use std::time::Instant;

    const ALICE: &str = "a11ce000a11ce000";
    const BOB: &str = "b0b00000b0b00000";

    struct Rig {
        engine: Arc<Engine>,
        handler: Handler,
        dir: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    /// The controller must not lose the role to the clock on a slow machine: some tests play a whole game, and
    /// the role is released after a silence (60 seconds by default), which a loaded test machine can exceed.
    /// The tracker of the tests counts pictures, not time: the demo of a test plays a move every few dozen milliseconds.
    fn quick_tracker() -> crate::tracker::TrackerConfig {
        crate::tracker::TrackerConfig { stable_time: 0.0, motion_span: 0.0, ..crate::tracker::TrackerConfig::default() }
    }

    fn patient() -> EngineConfig {
        EngineConfig {
            idle_timeout: Duration::from_secs(3600),
            game_timeout: Duration::from_secs(3600),
            tracker: quick_tracker(),
            ..EngineConfig::default()
        }
    }

    fn rig(name: &str, source: SourceSpec) -> Rig {
        rig_with(name, source, Duration::from_millis(5), false)
    }

    /// `record`: every game is recorded in the folder `recordings` of the rig, every picture the tracker sees.
    fn rig_with(name: &str, source: SourceSpec, frame_interval: Duration, record: bool) -> Rig {
        rig_tracking(name, source, frame_interval, record, quick_tracker())
    }

    /// The same with a tracker of its own choosing.
    fn rig_tracking(name: &str, source: SourceSpec, frame_interval: Duration, record: bool, tracker: crate::tracker::TrackerConfig) -> Rig {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        // Every picture, and none dropped whatever the load of the machine running the tests.
        let record = record.then(|| crate::record::RecordOptions {
            backlog: 100_000,
            busy_every: Duration::ZERO,
            ..crate::record::RecordOptions::new(dir.join("recordings"), Duration::ZERO)
        });
        let engine = Engine::new(EngineConfig { record, tracker, ..patient() }, store, dir.join("calibration.json"), None);
        engine.spawn_capture(camera::factory(source, frame_interval)).unwrap();
        let handler = handler(Arc::clone(&engine), 8);
        Rig { engine, handler, dir }
    }

    impl Rig {
        fn call(&self, method: &str, target: &str, token: &str, body: &str) -> (u16, Value) {
            let headers: Vec<(&str, &str)> = if token.is_empty() { vec![] } else { vec![("X-Client-Token", token)] };
            let reply = (self.handler)(Request::for_test(method, target, &headers, body));
            match reply {
                Reply::Bytes { status, body, .. } => {
                    (status, serde_json::from_slice(&body).unwrap_or(Value::String(String::from_utf8_lossy(&body).into())))
                }
                Reply::Stream { .. } => panic!("unexpected stream"),
            }
        }

        fn state(&self, token: &str) -> Value {
            self.call("GET", &format!("/api/state?token={token}"), "", "").1
        }
    }

    fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(180), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_first_user_controls_and_the_others_only_watch() {
        let rig = rig("roles", SourceSpec::Synthetic);
        assert_eq!(rig.state(ALICE)["role"], "controller");
        assert_eq!(rig.state(BOB)["role"], "viewer");
        // The page shows the version of the program.
        assert!(rig.state(ALICE)["version"].as_str().unwrap().starts_with(crate::VERSION));

        let (status, body) = rig.call("POST", "/api/calibration/open", BOB, "");
        assert_eq!((status, body["ok"].clone()), (403, json!(false)));
        let (status, _) = rig.call("POST", "/api/game/start", BOB, "{}");
        assert_eq!(status, 403);
        let (status, _) = rig.call("POST", "/api/claim", BOB, "");
        assert_eq!(status, 409, "cannot take a role that is held");

        // A user with no token can look but never control.
        assert_eq!(rig.state("")["role"], "viewer");
        assert_eq!(rig.call("POST", "/api/game/undo", "", "").0, 403);

        // Releasing hands the role to whoever claims it.
        assert_eq!(rig.call("POST", "/api/release", ALICE, "").0, 200);
        assert_eq!(rig.state(BOB)["can_claim"], true);
        assert_eq!(rig.call("POST", "/api/claim", BOB, "").0, 200);
        assert_eq!(rig.state(BOB)["role"], "controller");
        assert_eq!(rig.state(ALICE)["role"], "viewer");
    }

    #[test]
    fn a_game_needs_a_calibration_and_the_camera_follows_the_mode() {
        let rig = rig("calib", SourceSpec::Synthetic);
        rig.state(ALICE);

        let (status, body) = rig.call("POST", "/api/game/start", ALICE, "{}");
        assert_eq!(status, 409);
        assert!(body["error"].as_str().unwrap().contains("calibrate"));

        // The camera is off until the calibration page asks for it.
        assert!(rig.engine.latest_frame(0).is_none());
        assert_eq!(rig.call("GET", "/frame.jpg", "", "").0, 503);
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 409);

        assert_eq!(rig.call("POST", "/api/calibration/open", ALICE, "").0, 200);
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.state(ALICE)["mode"], "calibrating");
        assert_eq!(rig.call("GET", "/frame.jpg", "", "").0, 200);

        // Bad corners are refused, good ones are kept.
        let bad = r#"{"corners": [[120,440],[190,100],[470,90],[520,425]]}"#;
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, bad).0, 400);
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert!(rig.state(ALICE)["calibration"].is_array());
        assert_eq!(rig.call("GET", "/warp.jpg", "", "").0, 200);
        assert!(rig.dir.join("calibration.json").exists());

        assert_eq!(rig.call("POST", "/api/calibration/close", ALICE, "").0, 200);
        wait_for("camera off", || rig.engine.latest_frame(0).is_none());
        assert_eq!(rig.state(ALICE)["mode"], "idle");
    }

    #[test]
    fn turning_the_picture_turns_the_calibration_with_it() {
        let rig = rig("turn", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        let before = rig.state(ALICE);
        let (w, h) = (before["camera"]["width"].as_u64().unwrap(), before["camera"]["height"].as_u64().unwrap());

        // Only the controller may turn it.
        assert_eq!(rig.call("POST", "/api/camera/rotate", BOB, r#"{"turns": 1}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/camera/rotate", ALICE, r#"{"turns": 1}"#).0, 200);
        let turned = rig.state(ALICE);
        assert_eq!(turned["picture"]["rotation"], 90);
        // The picture is on its side now, at once, without waiting for the next frame.
        assert_eq!((turned["camera"]["width"].as_u64().unwrap(), turned["camera"]["height"].as_u64().unwrap()), (h, w));
        // The corner a1 moved from (x, y) to (h - y, x).
        let a1 = turned["calibration"][0].as_array().unwrap();
        let old = before["calibration"][0].as_array().unwrap();
        assert_eq!(a1[0].as_f64().unwrap(), h as f64 - old[1].as_f64().unwrap());
        assert_eq!(a1[1].as_f64().unwrap(), old[0].as_f64().unwrap());
        assert!(rig.dir.join("picture.json").exists());
        assert_eq!(rig.call("GET", "/warp.jpg", "", "").0, 200, "the calibration is still valid");

        // The other way brings everything back.
        assert_eq!(rig.call("POST", "/api/camera/rotate", ALICE, r#"{"turns": -1}"#).0, 200);
        let back = rig.state(ALICE);
        assert_eq!(back["picture"]["rotation"], 0);
        assert_eq!(back["calibration"], before["calibration"]);
        assert_eq!(rig.call("POST", "/api/camera/rotate", ALICE, r#"{"turns": 4}"#).0, 200);
        assert_eq!(rig.state(ALICE)["picture"]["rotation"], 0);

        // Not during a game.
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, "{}").0, 200);
        assert_eq!(rig.call("POST", "/api/camera/rotate", ALICE, r#"{"turns": 1}"#).0, 409);
    }

    #[test]
    fn brightness_contrast_and_auto_contrast() {
        let rig = rig("picture", SourceSpec::Synthetic);
        rig.state(ALICE);
        // The camera is off: there is nothing to measure.
        assert_eq!(rig.call("POST", "/api/camera/auto-contrast", ALICE, "").0, 409);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());

        assert_eq!(rig.call("POST", "/api/camera/picture", BOB, r#"{"brightness": 10, "contrast": 1}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/camera/picture", ALICE, r#"{"brightness": 10, "contrast": 0}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/camera/picture", ALICE, r#"{"brightness": 40, "contrast": 1.5}"#).0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["picture"]["brightness"], 40.0);
        assert_eq!(state["picture"]["contrast"], 1.5);
        // What the page shows is adjusted, the raw picture is not.
        wait_for("the adjusted picture", || {
            let frame = rig.engine.latest_frame(0).unwrap();
            !Arc::ptr_eq(&frame.image, &frame.raw) && frame.image.as_raw() != frame.raw.as_raw()
        });

        assert_eq!(rig.call("POST", "/api/camera/picture/reset", ALICE, "").0, 200);
        let state = rig.state(ALICE);
        assert_eq!((state["picture"]["brightness"].as_f64(), state["picture"]["contrast"].as_f64()), (Some(0.0), Some(1.0)));

        assert_eq!(rig.call("POST", "/api/camera/auto-contrast", ALICE, "").0, 200);
        let contrast = rig.state(ALICE)["picture"]["contrast"].as_f64().unwrap();
        assert!((0.25..=4.0).contains(&contrast), "{contrast}");

        // The settings survive a restart.
        let saved = crate::picture::Picture::load(&rig.dir.join("picture.json")).unwrap();
        assert_eq!(saved.contrast as f64, contrast as f32 as f64);
    }

    #[test]
    fn the_picture_stream_sends_new_pictures() {
        struct Collect {
            data: Vec<u8>,
            parts: usize,
        }
        impl Write for Collect {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                if buf.starts_with(b"--frame") {
                    self.parts += 1;
                    if self.parts > 3 {
                        return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the page went away"));
                    }
                }
                self.data.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let rig = rig("stream", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());

        let reply = (rig.handler)(Request::for_test("GET", "/frame.mjpg", &[], ""));
        let Reply::Stream { content_type, run } = reply else { panic!("expected a stream") };
        assert!(content_type.starts_with("multipart/x-mixed-replace"));
        let mut out = Collect { data: Vec::new(), parts: 0 };
        let result = run(&mut out);
        assert!(result.is_err(), "the stream ends when the page goes away");
        assert_eq!(out.parts, 4);
        let text = String::from_utf8_lossy(&out.data);
        assert_eq!(text.matches("Content-Type: image/jpeg").count(), 3);
        assert!(out.data.windows(2).any(|w| w == [0xff, 0xd8]), "there are JPEG pictures in it");
    }

    #[test]
    fn the_board_is_found_by_itself() {
        let rig = rig("detect", SourceSpec::Synthetic);
        rig.state(ALICE);
        // The camera is off: nothing to look at.
        assert_eq!(rig.call("POST", "/api/calibration/detect", ALICE, "").0, 409);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());

        assert_eq!(rig.call("POST", "/api/calibration/detect", BOB, "").0, 403);
        let (status, body) = rig.call("POST", "/api/calibration/detect", ALICE, "");
        assert_eq!(status, 200, "{body}");
        assert!(body["message"].as_str().unwrap().contains("Board found"), "{body}");

        let found = rig.state(ALICE)["calibration"].clone();
        let truth = synthetic::corners().corners;
        for (n, corner) in truth.iter().enumerate() {
            let (x, y) = (found[n][0].as_f64().unwrap(), found[n][1].as_f64().unwrap());
            let distance = (x - f64::from(corner[0])).hypot(y - f64::from(corner[1]));
            assert!(distance < 2.5, "corner {n} is {distance:.1} px off");
        }
        assert_eq!(rig.call("GET", "/warp.jpg", "", "").0, 200);
        assert!(rig.dir.join("calibration.json").exists());
    }

    #[test]
    fn the_calibration_follows_a_board_that_moved_during_a_game() {
        let truth = synthetic::corners().corners;
        // A calibration that is a few pixels off, as after a knock to the camera.
        let off = truth.map(|c| [c[0] + 4.0, c[1] - 3.0]);
        let body = serde_json::to_string(&json!({ "corners": off })).unwrap();

        let rig = rig("follow", SourceSpec::Synthetic);
        rig.state(ALICE);
        assert_eq!(rig.state(ALICE)["stabilize"]["on"], true, "on unless switched off");
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &body).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, "{}").0, 200);
        wait_for("the calibration to move", || rig.state(ALICE)["stabilize"]["adjusted"].as_u64().unwrap() >= 1);
        let state = rig.state(ALICE);
        for (n, corner) in truth.iter().enumerate() {
            let (x, y) = (state["calibration"][n][0].as_f64().unwrap(), state["calibration"][n][1].as_f64().unwrap());
            let distance = (x - f64::from(corner[0])).hypot(y - f64::from(corner[1]));
            assert!(distance < 2.0, "corner {n} is still {distance:.1} px off");
        }
        // The file is written just after the pages are told (not while the engine is locked).
        wait_for("the moved calibration to be saved", || {
            crate::calibration::Calibration::load(&rig.dir.join("calibration.json"))
                .ok()
                .flatten()
                .is_some_and(|saved| (saved.corners[0][0] - truth[0][0]).abs() < 2.0)
        });
    }

    #[test]
    fn following_can_be_switched_off() {
        let truth = synthetic::corners().corners;
        let off = truth.map(|c| [c[0] + 4.0, c[1] - 3.0]);
        let body = serde_json::to_string(&json!({ "corners": off })).unwrap();

        let rig = rig("nofollow", SourceSpec::Synthetic);
        rig.state(ALICE);
        assert_eq!(rig.call("POST", "/api/camera/follow", BOB, r#"{"on": false}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/camera/follow", ALICE, r#"{"on": false}"#).0, 200);
        assert_eq!(rig.state(ALICE)["stabilize"]["on"], false);
        assert!(!crate::picture::Picture::load(&rig.dir.join("picture.json")).unwrap().follow, "kept for next time");
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        rig.call("POST", "/api/calibration", ALICE, &body);
        rig.call("POST", "/api/game/start", ALICE, "{}");
        // Plenty of time for it to have happened.
        // Waiting is the point of this test: it checks that nothing happens (any longer than the first
        // look at the board, which comes at once, would not change the result)
        std::thread::sleep(Duration::from_millis(2500));
        let state = rig.state(ALICE);
        assert_eq!(state["stabilize"]["adjusted"], 0);
        assert_eq!(state["calibration"][0][0].as_f64().unwrap() as f32, off[0][0]);
    }

    struct PartCounter {
        parts: usize,
        pictures: usize,
    }

    impl Write for PartCounter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if buf.starts_with(b"--frame") {
                self.parts += 1;
                if self.parts > 3 {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the page went away"));
                }
                self.pictures += buf.windows(2).filter(|w| *w == [0xff, 0xd8]).count().min(1);
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_board_stream_and_the_speed_figures() {
        let rig = rig("board-stream", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);

        let Reply::Stream { content_type, run } = (rig.handler)(Request::for_test("GET", "/warp.mjpg", &[], "")) else {
            panic!("expected a stream")
        };
        assert!(content_type.starts_with("multipart/x-mixed-replace"));
        let mut out = PartCounter { parts: 0, pictures: 0 };
        assert!(run(&mut out).is_err(), "ends when the page goes away");
        assert_eq!((out.parts, out.pictures), (4, 3));

        let stats = rig.call("GET", "/api/camera/stats", "", "").1;
        assert!(stats["fps"].as_f64().unwrap() > 0.0, "{stats}");
        assert!(stats["process_ms"].as_f64().unwrap() >= 0.0 && stats["encode_ms"].is_number(), "{stats}");
    }

    /// An engine whose store already holds an unfinished game: one move (e4) was played.
    fn rig_with_unfinished_game(name: &str, calibration: Option<crate::calibration::Calibration>) -> Rig {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let mut game = crate::game::GameRecord::new("20260101T100000".into(), None, "Ann", "Bob", None).unwrap();
        game.push_text("e4", 3.0).unwrap();
        store.save(&game).unwrap();
        let engine = Engine::new(patient(), store, dir.join("calibration.json"), calibration);
        engine.spawn_capture(camera::factory(SourceSpec::Synthetic, Duration::from_millis(5))).unwrap();
        let handler = handler(Arc::clone(&engine), 8);
        Rig { engine, handler, dir }
    }

    #[test]
    fn an_unfinished_game_is_found_at_start_and_can_be_resumed() {
        let rig = rig_with_unfinished_game("resume", None);
        let state = rig.state(ALICE);
        assert_eq!(state["mode"], "idle");
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 1);
        assert_eq!(state["game"]["over"], false);

        // Not while nobody controls, and not without a calibration.
        assert_eq!(rig.call("POST", "/api/game/resume", BOB, "").0, 403);
        assert_eq!(rig.call("POST", "/api/game/resume", ALICE, "").0, 409);
        // Nothing is being recorded: the piece of a promotion cannot be changed.
        assert_eq!(rig.call("POST", "/api/game/promotion", ALICE, r#"{"piece": "n"}"#).0, 409);

        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/resume", ALICE, "").0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["mode"], "playing");
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 1, "the moves are kept");
        assert_eq!(state["game"]["id"], "20260101T100000", "the same game goes on");
        // It is an ordinary running game now.
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result": "1/2-1/2", "reason": "agreement"}"#).0, 200);
        assert!(rig.engine.list_games().unwrap().iter().all(|g| g.result.is_some()));
    }

    #[test]
    fn an_unfinished_game_can_be_closed_without_resuming_it() {
        let rig = rig_with_unfinished_game("close", None);
        rig.state(ALICE);
        let (status, _) = rig.call("POST", "/api/game/end", ALICE, r#"{"result": "*", "reason": "abandoned"}"#);
        assert_eq!(status, 200);
        assert_eq!(rig.state(ALICE)["game"]["over"], true);
        assert!(rig.engine.list_games().unwrap().iter().all(|g| g.result.is_some()), "it is saved as finished");
        // Nothing is left to close.
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result": "*", "reason": "abandoned"}"#).0, 409);
    }

    #[test]
    fn a_calibration_for_another_picture_size_is_reported() {
        // Made for a 320x240 picture; the synthetic camera gives 640x480.
        let calibration = crate::calibration::Calibration::for_frame(synthetic::corners().corners, 320, 240);
        let rig = rig_with_unfinished_game("size", Some(calibration));
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("the warning", || rig.state(ALICE)["notice"].as_str().is_some_and(|n| n.contains("calibrate again")));
    }

    #[test]
    fn an_unfinished_game_can_be_closed_while_calibrating() {
        let rig = rig_with_unfinished_game("close-cal", None);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        assert_eq!(rig.state(ALICE)["mode"], "calibrating");
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result": "*", "reason": "abandoned"}"#).0, 200);
        assert_eq!(rig.state(ALICE)["mode"], "idle");
    }

    #[test]
    fn abandoning_a_game_with_no_move_leaves_no_file() {
        let rig = rig_with_unfinished_game("empty", None);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/resume", ALICE, "").0, 200);
        // Take the only move back: an empty game, which is saved like any other.
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        assert_eq!(rig.engine.list_games().unwrap().len(), 1);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result": "*", "reason": "abandoned"}"#).0, 200);
        assert!(rig.engine.list_games().unwrap().is_empty(), "the empty game is gone from the disk too");
    }

    /// A running game on the synthetic source, with a calibration: for the commands that play moves. The tracker never
    /// decides anything here (the board of the synthetic camera never changes, and shows by chance the position that
    /// a shuffle of the knights comes back to, which the tracker would then find by itself before the test typed it).
    fn running_game(name: &str) -> Rig {
        let never = crate::tracker::TrackerConfig { stable_frames: usize::MAX / 2, ..quick_tracker() };
        let rig = rig_tracking(name, SourceSpec::Synthetic, Duration::from_millis(5), false, never);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, "{}").0, 200);
        rig
    }

    fn play(rig: &Rig, moves: &[&str]) {
        for text in moves {
            let body = json!({ "move": text }).to_string();
            assert_eq!(rig.call("POST", "/api/game/move", ALICE, &body).0, 200, "{text}");
        }
    }

    #[test]
    fn a_draw_by_repetition_is_claimed_by_a_player_and_ends_at_the_fifth_time() {
        let rig = running_game("claim");
        let claim = |reason: &str| rig.call("POST", "/api/game/end", ALICE, &json!({ "result": "1/2-1/2", "reason": reason }).to_string());
        let shuffle = ["Nf3", "Nf6", "Ng1", "Ng8"];
        play(&rig, &shuffle);
        assert!(rig.state(ALICE)["game"]["draw_claim"].is_null(), "twice is not enough");
        assert_eq!(claim("threefold repetition").0, 409, "nothing to claim yet");
        play(&rig, &shuffle);
        assert_eq!(rig.state(ALICE)["game"]["draw_claim"], "threefold repetition");
        assert_eq!(rig.state(ALICE)["mode"], "playing", "a claim is up to the players");

        // The wrong rule, or another result, is refused; the right claim ends the game.
        assert_eq!(claim("50-move rule").0, 409);
        let wrong = rig.call("POST", "/api/game/end", ALICE, r#"{"result": "1-0", "reason": "threefold repetition"}"#);
        assert_eq!(wrong.0, 400);
        assert_eq!(claim("threefold repetition").0, 200);
        let state = rig.state(ALICE);
        assert_eq!(
            (state["game"]["result"].as_str(), state["game"]["termination"].as_str()),
            (Some("1/2-1/2"), Some("threefold repetition"))
        );
        assert_eq!(state["mode"], "idle");
    }

    #[test]
    fn fivefold_repetition_ends_the_game_by_itself() {
        let rig = running_game("fivefold");
        let shuffle = ["Nf3", "Nf6", "Ng1", "Ng8"];
        for _ in 0..4 {
            play(&rig, &shuffle);
        }
        let state = rig.state(ALICE);
        assert_eq!(state["game"]["termination"], "fivefold repetition");
        assert_eq!(state["game"]["result"], "1/2-1/2");
        assert_eq!(state["mode"], "idle");
        // It is taken back like a checkmate that was a misreading.
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        assert_eq!(rig.state(ALICE)["mode"], "playing");
    }

    #[test]
    fn two_moves_can_be_entered_at_once_all_or_nothing() {
        let rig = running_game("pair");
        play(&rig, &["e4 e5"]);
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 2);
        // The second move is illegal: the first is not kept either.
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move": "Nf3 Nf3"}"#).0, 400);
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 2);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move": "Nf3 Nc6 Bb5 a6"}"#).0, 400, "four at once is too many");
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move": "   "}"#).0, 400);
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 2);
    }

    fn corners_json() -> String {
        json!({ "corners": synthetic::corners().corners }).to_string()
    }

    #[test]
    fn records_a_whole_game_from_the_camera_to_a_pgn() {
        let rig = rig("game", SourceSpec::Demo { move_interval: Duration::from_millis(40) });
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);

        let start = json!({"white": "Morphy", "black": "Duke", "started_at": "2026-11-04T10:15:30.000Z"});
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, &start.to_string()).0, 200);
        assert_eq!(rig.state(BOB)["mode"], "playing");
        // A viewer can follow the game but not act on it.
        assert_eq!(rig.call("POST", "/api/game/undo", BOB, "").0, 403);

        // The demo plays the Opera game; the game ends by itself on checkmate.
        wait_for("checkmate", || rig.state(BOB)["game"]["over"] == true);
        let state = rig.state(BOB);
        assert_eq!(state["game"]["result"], "1-0");
        assert_eq!(state["game"]["termination"], "checkmate");
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 33);
        assert_eq!(state["game"]["moves"][32]["san"], "Rd8#");
        assert_eq!(state["mode"], "idle", "the camera is released when the game ends");

        let id = state["game"]["id"].as_str().unwrap().to_string();
        assert_eq!(id, "20261104T101530");

        // It was saved (a moment after the pages were told: the file is written outside the lock),
        // listed, and can be replayed.
        wait_for("the game to be written", || rig.engine.list_games().unwrap().first().is_some_and(|g| g.moves == 33));
        let (_, list) = rig.call("GET", "/api/games", "", "");
        assert_eq!(list["games"][0]["id"], id.as_str());
        assert_eq!(list["games"][0]["moves"], 33);
        let (status, detail) = rig.call("GET", &format!("/api/games/{id}"), "", "");
        assert_eq!(status, 200);
        assert_eq!(detail["fens"].as_array().unwrap().len(), 34);
        assert_eq!(detail["white"], "Morphy");

        let reply = (rig.handler)(Request::for_test("GET", &format!("/api/games/{id}/pgn"), &[], ""));
        match reply {
            Reply::Bytes { status, body, extra_headers, .. } => {
                assert_eq!(status, 200);
                let pgn = String::from_utf8(body).unwrap();
                assert!(pgn.contains("[White \"Morphy\"]") && pgn.contains("[Result \"1-0\"]"), "{pgn}");
                assert!(pgn.contains("17. Rd8# "), "{pgn}");
                assert!(extra_headers.iter().any(|(k, _)| k == "Content-Disposition"));
            }
            _ => panic!("expected bytes"),
        }
        assert_eq!(rig.call("GET", "/api/games/nope", "", "").0, 404);
        assert_eq!(rig.call("GET", "/api/games/..%2Fx", "", "").0, 400);

        // Undo reopens the game that the rules ended.
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["mode"], "playing");
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 32);
        assert_eq!(state["game"]["over"], false);
    }

    #[test]
    fn a_recorded_game_is_kept_in_a_folder_and_a_replay_of_it_finds_the_same_moves() {
        use crate::replay::{TrackerOverrides, replay_folder};
        // Slow enough for the disk of any machine: every picture the tracker sees is kept.
        let rig = rig_with("recording", SourceSpec::Demo { move_interval: Duration::from_millis(600) }, Duration::from_millis(100), true);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        wait_for("checkmate", || rig.state(BOB)["game"]["over"] == true);

        // One folder, named after the start; the recording is closed by its own thread a moment after the game.
        let folders: Vec<_> = std::fs::read_dir(rig.dir.join("recordings")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(folders.len(), 1, "{folders:?}");
        let folder = folders[0].clone();
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.len() == 19 && name.as_bytes()[10] == b'_', "{name}");
        wait_for("the recording to be closed", || {
            std::fs::read_to_string(folder.join("events.jsonl")).is_ok_and(|text| text.contains("\"type\":\"closed\""))
        });

        let events: Vec<Value> =
            std::fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let kind = |e: &Value| e["type"].as_str().unwrap().to_string();
        let moves: usize = events.iter().filter(|e| kind(e) == "detect").map(|e| e["moves"].as_array().unwrap().len()).sum();
        assert_eq!(moves, 33, "every move of the game is in the recording");
        let ends: Vec<&Value> = events.iter().filter(|e| kind(e) == "end").collect();
        assert_eq!(ends.len(), 1);
        assert_eq!((ends[0]["result"].as_str(), ends[0]["reason"].as_str()), (Some("1-0"), Some("checkmate")));
        assert_eq!(kind(events.last().unwrap()), "closed");
        assert_eq!(events.last().unwrap()["dropped"], 0, "no picture was lost");
        let session: crate::record::SessionMeta = serde_json::from_slice(&std::fs::read(folder.join("session.json")).unwrap()).unwrap();
        assert_eq!(session.game_id, "undated", "the page gave no start time");
        assert_eq!(session.calibration.map(|c| (c.corners, c.size)), Some((synthetic::corners().corners, Some([640, 480]))));
        assert!(session.source.contains("demo"), "{}", session.source);

        // The replay goes through the same pictures and gets to the same game.
        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert_eq!(report.summary.wrong, 0, "{}", report.text);
        assert_eq!(report.summary.behind, 0, "{}", report.text);
        assert_eq!((report.summary.live_typed, report.summary.live_undone), (0, 0));
        assert!(report.summary.found > 0 && report.summary.pictures >= 33, "{}", report.text);
    }

    #[test]
    fn what_the_players_do_is_in_the_recording_with_the_position_after_it() {
        let rig = rig_with("recording-manual", SourceSpec::Synthetic, Duration::from_millis(60), true);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        rig.call("POST", "/api/calibration", ALICE, &corners_json());
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e2e4"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e5 Nf3"}"#).0, 200, "two moves at once");
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"0-1","reason":"resignation"}"#).0, 200);

        let folder = std::fs::read_dir(rig.dir.join("recordings")).unwrap().next().unwrap().unwrap().path();
        wait_for("the recording to be closed", || {
            std::fs::read_to_string(folder.join("events.jsonl")).is_ok_and(|text| text.contains("\"type\":\"closed\""))
        });
        let events: Vec<Value> =
            std::fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        // What the people did: not the pictures, nor the notes of the program (which cameras took part, and that the
        // tracker was unsure: the synthetic board never changes, so it may well say so between the moves typed).
        let people: Vec<&Value> =
            events.iter().filter(|e| !["frame", "views", "unsure", "closed"].contains(&e["type"].as_str().unwrap_or(""))).collect();
        let kinds: Vec<&str> = people.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["manual", "manual", "undo", "end"], "{people:?}");
        assert_eq!(people[0]["text"], "e2e4");
        assert_eq!(people[1]["text"], "e5 Nf3");
        // The position after each: the replay and the people reading it look at these.
        assert!(people[0]["fen"].as_str().unwrap().starts_with("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b "), "{}", people[0]["fen"]);
        assert!(
            people[1]["fen"].as_str().unwrap().starts_with("rnbqkbnr/pppp1ppp/8/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R b "),
            "{}",
            people[1]["fen"]
        );
        assert!(
            people[2]["fen"].as_str().unwrap().starts_with("rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w "),
            "e5 stays after the undo of Nf3: {}",
            people[2]["fen"]
        );
        assert_eq!((people[3]["result"].as_str(), people[3]["reason"].as_str()), (Some("0-1"), Some("resignation")));
        // Times are in order.
        let times: Vec<f64> = events.iter().map(|e| e["t"].as_f64().unwrap()).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    }

    #[test]
    fn the_camera_is_chosen_from_the_page_and_the_calibration_of_the_other_one_is_forgotten() {
        use crate::choice::{CameraInfo, CameraSelection};
        use std::sync::Mutex;
        let dir = std::env::temp_dir().join(format!("nicochess-choose-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let cameras =
            vec![CameraInfo { id: "0".into(), name: "Integrated Camera".into() }, CameraInfo { id: "1".into(), name: "USB Webcam".into() }];
        let selection =
            Arc::new(CameraSelection::with_lister("0".into(), Some(dir.join("camera.json")), Box::new(move || Ok(cameras.clone()))));
        let store = Store::open(&dir.join("games")).unwrap();
        let config = EngineConfig { camera: Some(Arc::clone(&selection)), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        // A source that notes which camera each opening asked for, as the real one reads the selection.
        let opened: Arc<Mutex<Vec<String>>> = Arc::default();
        let inner = camera::factory(SourceSpec::Synthetic, Duration::from_millis(5));
        let (noted, chosen) = (Arc::clone(&opened), Arc::clone(&selection));
        engine
            .spawn_capture(Box::new(move || {
                noted.lock().unwrap().push(chosen.device());
                inner()
            }))
            .unwrap();
        let rig = Rig { handler: handler(Arc::clone(&engine), 8), engine, dir: dir.clone() };

        rig.state(ALICE);
        assert_eq!(rig.state(ALICE)["camera"]["choice"], true);
        let (status, list) = rig.call("GET", "/api/camera/list", "", "");
        assert_eq!(status, 200);
        assert_eq!((list["selectable"].clone(), list["selected"].clone()), (json!(true), json!("0")));
        assert_eq!(list["cameras"][1], json!({"id": "1", "name": "USB Webcam"}));

        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert!(!rig.state(ALICE)["calibration"].is_null());

        // Only the controller, and only a camera that exists.
        assert_eq!(rig.call("POST", "/api/camera/select", BOB, r#"{"id": "1"}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "7"}"#).0, 400);
        assert_eq!(selection.device(), "0");
        // The same camera again changes nothing: the calibration stays and the camera is not opened again.
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "0"}"#).0, 200);
        assert!(!rig.state(ALICE)["calibration"].is_null());

        // Another camera: used at once (the one that is on is closed and the new one opened), kept for the next
        // start, and the calibration, made on the picture of the other, is forgotten.
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "1"}"#).0, 200);
        wait_for("the camera to be opened again", || opened.lock().unwrap().len() >= 2);
        assert_eq!(*opened.lock().unwrap(), ["0", "1"]);
        assert_eq!(selection.device(), "1");
        assert!(rig.state(ALICE)["calibration"].is_null());
        assert!(std::fs::read_to_string(dir.join("camera.json")).unwrap().contains("USB Webcam"));
        assert_eq!(rig.call("GET", "/api/camera/list", "", "").1["selected"], "1");
        wait_for("a picture from the new camera", || rig.engine.latest_frame(0).is_some());

        // Not during a game.
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "0"}"#).0, 409);
        assert_eq!(selection.device(), "1");
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"*","reason":"abandoned"}"#).0, 200);
    }

    /// A rig whose tracker takes every move on the soft margin: each move comes with a doubt.
    fn rig_doubtful(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let tracker = crate::tracker::TrackerConfig { margin: 1000.0, soft_margin: 0.5, ..quick_tracker() };
        let engine = Engine::new(EngineConfig { tracker, ..patient() }, store, dir.join("calibration.json"), None);
        engine
            .spawn_capture(camera::factory(SourceSpec::Demo { move_interval: Duration::from_millis(1500) }, Duration::from_millis(100)))
            .unwrap();
        let rig = Rig { handler: handler(Arc::clone(&engine), 8), engine, dir };
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        // The first move of the demo is taken with a doubt.
        wait_for("a move in doubt", || !rig.state(BOB)["tracker"]["doubt"].is_null());
        rig
    }

    #[test]
    fn a_move_taken_in_doubt_is_reported_and_can_be_replaced_by_another_likely_one() {
        let rig = rig_doubtful("doubt-replace");
        let doubt = rig.state(BOB)["tracker"]["doubt"].clone();
        assert_eq!(doubt["played"], "e4");
        assert!(doubt["certainty"].as_f64().unwrap() > 0.5, "{doubt}");
        let alternatives = doubt["alternatives"].as_array().unwrap();
        assert!(!alternatives.is_empty());
        let other = alternatives[0]["uci"].as_str().unwrap().to_string();
        assert!(alternatives[0]["chance"].as_f64().unwrap() < doubt["certainty"].as_f64().unwrap());

        // Only the controller, only one of the offered moves.
        assert_eq!(rig.call("POST", "/api/game/doubt", BOB, &json!({"move": other}).to_string()).0, 403);
        assert_eq!(rig.call("POST", "/api/game/doubt", ALICE, r#"{"move": "a2a3"}"#).0, 400);
        assert_eq!(rig.state(ALICE)["game"]["moves"][0]["uci"], "e2e4", "nothing changed");

        // Replaced: the game has the other move, and the doubt is gone.
        let (status, body) = rig.call("POST", "/api/game/doubt", ALICE, &json!({"move": other}).to_string());
        assert_eq!(status, 200, "{body}");
        let state = rig.state(ALICE);
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 1);
        assert_eq!(state["game"]["moves"][0]["uci"], other.as_str());
        assert!(state["tracker"]["doubt"].is_null());
        // Nothing is in doubt any more: a second answer is refused.
        assert_eq!(rig.call("POST", "/api/game/doubt", ALICE, &json!({"move": other}).to_string()).0, 409);
    }

    #[test]
    fn taking_back_a_move_in_doubt_offers_the_choice_and_waits_for_a_move() {
        let rig = rig_doubtful("doubt-undo");
        let doubt = rig.state(ALICE)["tracker"]["doubt"].clone();
        let alternatives = doubt["alternatives"].as_array().unwrap().len();
        let (status, body) = rig.call("POST", "/api/game/doubt", ALICE, "{}");
        assert_eq!(status, 200, "{body}");
        let state = rig.state(ALICE);
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 0, "the move was taken back");
        assert!(state["tracker"]["doubt"].is_null());
        // The move that was taken and the other likely ones are offered, as when the tracker is unsure.
        let candidates = state["tracker"]["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1 + alternatives);
        assert_eq!(candidates[0]["san"], "e4");
        wait_for("the tracker to wait for the person", || rig.state(ALICE)["tracker"]["status"] == "unsure");
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 0, "nothing is decided meanwhile");
        // The person picks one of them as any move: the game goes on from there.
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move": "e2e4"}"#).0, 200);
        assert_eq!(rig.state(ALICE)["game"]["moves"][0]["san"], "e4");
    }

    #[test]
    fn a_program_with_no_choice_of_camera_says_so() {
        let rig = rig("nochoice", SourceSpec::Synthetic);
        assert_eq!(rig.state(ALICE)["camera"]["choice"], false);
        let (status, list) = rig.call("GET", "/api/camera/list", "", "");
        assert_eq!((status, list["selectable"].clone()), (200, json!(false)));
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "1"}"#).0, 409);
    }

    #[test]
    fn a_program_with_one_camera_has_no_second_one_and_says_so() {
        let rig = rig("onecamera", SourceSpec::Synthetic);
        assert!(rig.state(ALICE)["second"].is_null());
        assert!(rig.call("GET", "/api/camera/list", "", "").1["second"].is_null());
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        // The second camera is refused everywhere, and the first is not touched; so is a camera that cannot be.
        assert_eq!(rig.call("POST", "/api/calibration?cam=2", ALICE, &corners_json()).0, 400);
        assert_eq!(rig.call("POST", "/api/calibration/detect?cam=2", ALICE, "").0, 400);
        assert_eq!(rig.call("POST", "/api/camera/rotate?cam=2", ALICE, r#"{"turns": 1}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "1"}"#).0, 400);
        assert_eq!(rig.call("GET", "/frame.mjpg?cam=2", "", "").0, 400);
        assert_eq!(rig.call("GET", "/frame.jpg?cam=2", "", "").0, 503);
        assert_eq!(rig.call("GET", "/warp.jpg?cam=3", "", "").0, 400);
        assert_eq!(rig.call("POST", "/api/calibration?cam=1", ALICE, &corners_json()).0, 200);
        assert!(rig.state(ALICE)["calibration"].is_array());
        // A game follows with the one camera, and the tracker says which cameras it looks through.
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, "{}").0, 200);
        wait_for("the squares to be learned", || rig.state(ALICE)["tracker"]["status"] == "watching");
        assert_eq!(rig.state(ALICE)["tracker"]["views"], json!([true, false]));
    }

    /// A rig with the demo game seen by two cameras on opposite sides of the board.
    fn rig_two_cameras(name: &str, move_interval: Duration, record: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let record = record.then(|| crate::record::RecordOptions {
            backlog: 100_000,
            busy_every: Duration::ZERO,
            ..crate::record::RecordOptions::new(dir.join("recordings"), Duration::ZERO)
        });
        let config = EngineConfig { record, second_fixed: true, track_interval: Duration::from_millis(80), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        let (first, second) = camera::demo_pair(Duration::from_millis(100), move_interval);
        engine.spawn_capture(first).unwrap();
        engine.spawn_capture_second(second).unwrap();
        Rig { handler: handler(Arc::clone(&engine), 8), engine, dir }
    }

    fn second_corners_json() -> String {
        json!({ "corners": synthetic::second_corners().corners }).to_string()
    }

    /// Both cameras opened and calibrated, one at a time.
    fn calibrate_both(rig: &Rig) {
        rig.state(ALICE);
        assert_eq!(rig.state(ALICE)["second"]["enabled"], true);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("pictures from both cameras", || rig.engine.latest_frame(0).is_some() && rig.engine.latest_frame(1).is_some());
        // Each camera has its own calibration, picture and top-down view.
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        let state = rig.state(ALICE);
        assert!(state["calibration"].is_array() && state["second"]["calibration"].is_null(), "only the first is calibrated");
        assert_eq!(rig.call("GET", "/warp.jpg?cam=2", "", "").0, 409, "the second is not calibrated yet");
        assert_eq!(rig.call("POST", "/api/calibration?cam=2", ALICE, &second_corners_json()).0, 200);
        let state = rig.state(ALICE);
        assert!(state["calibration"].is_array() && state["second"]["calibration"].is_array());
        assert_ne!(state["calibration"], state["second"]["calibration"]);
        assert_eq!(rig.call("GET", "/warp.jpg?cam=2", "", "").0, 200);
        assert!(rig.dir.join("calibration.json").exists() && rig.dir.join("calibration2.json").exists());
        assert!(state["second"]["source"].as_str().unwrap().contains("other side"), "{}", state["second"]["source"]);
    }

    #[test]
    fn two_cameras_are_calibrated_one_by_one_follow_a_whole_game_and_the_recording_replays_with_both() {
        use crate::replay::{TrackerOverrides, replay_folder};
        let rig = rig_two_cameras("two-cameras", Duration::from_millis(600), true);
        calibrate_both(&rig);

        // The second camera can be turned and adjusted without touching the first.
        let before = rig.state(ALICE);
        assert_eq!(rig.call("POST", "/api/camera/picture?cam=2", ALICE, r#"{"brightness": 20, "contrast": 1.2}"#).0, 200);
        let after = rig.state(ALICE);
        assert_eq!(after["second"]["picture"]["brightness"], 20.0);
        assert_eq!(after["picture"], before["picture"], "the first camera's picture is as it was");
        assert_eq!(rig.call("POST", "/api/camera/picture/reset?cam=2", ALICE, "").0, 200);
        assert!(rig.dir.join("picture2.json").exists());

        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        wait_for("both cameras to be used", || rig.state(BOB)["tracker"]["views"] == json!([true, true]));
        wait_for("checkmate", || rig.state(BOB)["game"]["over"] == true);
        let state = rig.state(BOB);
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 33);
        assert_eq!(state["game"]["moves"][32]["san"], "Rd8#");

        // The recording has the pictures of both cameras, given together, and a replay finds the game with both.
        let folder = std::fs::read_dir(rig.dir.join("recordings")).unwrap().next().unwrap().unwrap().path();
        wait_for("the recording to be closed", || {
            std::fs::read_to_string(folder.join("events.jsonl")).is_ok_and(|text| text.contains("\"type\":\"closed\""))
        });
        let events: Vec<Value> =
            std::fs::read_to_string(folder.join("events.jsonl")).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let frames: Vec<&Value> = events.iter().filter(|e| e["type"] == "frame").collect();
        assert!(frames.iter().any(|e| e["cam"] == 0) && frames.iter().any(|e| e["cam"] == 1));
        assert!(frames.iter().all(|e| e["feed"].is_u64()));
        let together = frames.windows(2).filter(|w| w[0]["feed"] == w[1]["feed"] && w[0]["cam"] != w[1]["cam"]).count();
        assert!(together > 5, "the two pictures of a time carry one number: {together}");
        assert!(events.iter().any(|e| e["type"] == "views"), "which cameras took part is noted");
        let session: crate::record::SessionMeta = serde_json::from_slice(&std::fs::read(folder.join("session.json")).unwrap()).unwrap();
        assert_eq!(session.calibration2.map(|c| c.corners), Some(synthetic::second_corners().corners));
        assert!(session.source.contains(" + "), "{}", session.source);
        let report = replay_folder(&folder, &TrackerOverrides::default(), false).unwrap();
        assert_eq!((report.summary.wrong, report.summary.behind), (0, 0), "{}", report.text);
        assert!(report.summary.found > 0 && report.summary.pictures >= 66, "{}", report.text);
    }

    /// A camera that works for a number of pictures, and is gone after that (and cannot be opened again).
    struct Dies {
        inner: Box<dyn camera::FrameSource>,
        left: usize,
    }

    impl camera::FrameSource for Dies {
        fn describe(&self) -> String {
            self.inner.describe()
        }
        fn next_frame(&mut self) -> anyhow::Result<image::RgbImage> {
            if self.left == 0 {
                anyhow::bail!("the camera was unplugged");
            }
            self.left -= 1;
            self.inner.next_frame()
        }
        fn restart(&mut self) {
            self.inner.restart();
        }
    }

    #[test]
    fn a_second_camera_that_goes_away_during_a_game_does_not_stop_it() {
        let dir = std::env::temp_dir().join(format!("nicochess-second-dies-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        // The demo plays a move every 600 ms: a camera that has been silent for a few pictures is left out at once, not after
        // the two seconds that people can wait (the game would be too many moves ahead of the tracker by then).
        let config = EngineConfig { second_fixed: true, fresh: Duration::from_millis(350), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        let (first, second) = camera::demo_pair(Duration::from_millis(100), Duration::from_millis(600));
        engine.spawn_capture(first).unwrap();
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (inner, count) = (second, Arc::clone(&opened));
        engine
            .spawn_capture_second(Box::new(move || {
                // The first opening gives a camera that is gone after a while; later ones find nothing.
                if count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                    anyhow::bail!("no camera is there");
                }
                // The demo has 18 pictures before the first move and 6 for each move: 94 ends in the middle of the
                // still pictures of a move, not on the boundary between two moves (90 did, and a camera that goes
                // just as a move is made leaves the tracker behind: that is not what this test is about).
                Ok(Box::new(Dies { inner: inner()?, left: 94 }) as Box<dyn camera::FrameSource>)
            }))
            .unwrap();
        let rig = Rig { handler: handler(Arc::clone(&engine), 8), engine, dir };
        calibrate_both(&rig);

        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        wait_for("both cameras to be used", || rig.state(BOB)["tracker"]["views"] == json!([true, true]));
        // The second goes away in the middle of the game (after about nine seconds of pictures): it is reported, and
        // the game goes on to its end with the first.
        wait_for("the second camera to be reported", || !rig.state(BOB)["second"]["error"].is_null());
        assert!(rig.state(BOB)["game"]["moves"].as_array().unwrap().len() < 33, "it went away before the end");
        wait_for("checkmate", || rig.state(BOB)["game"]["over"] == true);
        let state = rig.state(BOB);
        assert_eq!(state["game"]["moves"].as_array().unwrap().len(), 33);
        assert!(state["camera"]["error"].is_null(), "the first camera is fine");
    }

    #[test]
    fn the_second_camera_is_chosen_and_switched_off_from_the_page() {
        use crate::choice::{CameraInfo, CameraSelection};
        let dir = std::env::temp_dir().join(format!("nicochess-choose-second-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let cameras = vec![
            CameraInfo { id: "0".into(), name: "Integrated Camera".into() },
            CameraInfo { id: "1".into(), name: "USB Webcam".into() },
            CameraInfo { id: "2".into(), name: "Other Webcam".into() },
        ];
        let lister = |cameras: Vec<CameraInfo>| -> Box<dyn Fn() -> anyhow::Result<Vec<CameraInfo>> + Send + Sync> {
            Box::new(move || Ok(cameras.clone()))
        };
        let first = Arc::new(CameraSelection::with_lister("0".into(), Some(dir.join("camera.json")), lister(cameras.clone())));
        // No second camera chosen yet.
        let second = Arc::new(CameraSelection::with_lister(String::new(), Some(dir.join("camera2.json")), lister(cameras)));
        let store = Store::open(&dir.join("games")).unwrap();
        let config = EngineConfig { camera: Some(Arc::clone(&first)), camera2: Some(Arc::clone(&second)), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        engine.spawn_capture(camera::factory(SourceSpec::Synthetic, Duration::from_millis(5))).unwrap();
        engine.spawn_capture_second(camera::second_factory(SourceSpec::Synthetic, Duration::from_millis(5), None)).unwrap();
        let rig = Rig { handler: handler(Arc::clone(&engine), 8), engine, dir: dir.clone() };

        // A second camera can be had, and there is none yet: the first works alone.
        let state = rig.state(ALICE);
        assert_eq!((state["second"]["enabled"].clone(), state["second"]["choice"].clone()), (json!(false), json!(true)));
        let (_, list) = rig.call("GET", "/api/camera/list", "", "");
        assert_eq!(list["second"], json!({"choice": true, "selected": "", "fixed": false}));
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a picture from the first camera", || rig.engine.latest_frame(0).is_some());
        std::thread::sleep(Duration::from_millis(300));
        assert!(rig.engine.latest_frame(1).is_none(), "no second camera is opened until one is chosen");
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", BOB, r#"{"id": "1"}"#).0, 403);

        // Choosing one opens it at once, and keeps the choice.
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "7"}"#).0, 400, "no such camera");
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "0"}"#).0, 409, "the first camera has it");
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "1"}"#).0, 200);
        assert_eq!(second.device(), "1");
        assert!(std::fs::read_to_string(dir.join("camera2.json")).unwrap().contains("USB Webcam"));
        wait_for("a picture from the second camera", || rig.engine.latest_frame(1).is_some());
        assert_eq!(rig.state(ALICE)["second"]["enabled"], true);
        assert_eq!(rig.call("POST", "/api/camera/select", ALICE, r#"{"id": "1"}"#).0, 409, "the second camera has it");
        assert_eq!(first.device(), "0");

        // Calibrated, then another camera for the second: its calibration was made on the other and is forgotten.
        assert_eq!(rig.call("POST", "/api/calibration?cam=2", ALICE, &second_corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "2"}"#).0, 200);
        assert!(rig.state(ALICE)["second"]["calibration"].is_null());
        assert!(!dir.join("calibration2.json").exists());
        wait_for("a picture from the other camera", || rig.engine.latest_frame(1).is_some());

        // Switched off: the camera closes, the first goes on, and what was calibrated stays for the next time. (The old
        // picture is still there for a moment after the choice of another camera, then gone until the new one comes:
        // the calibration is refused in between, so it is tried until it is taken.)
        wait_for("the calibration of the second camera", || {
            rig.call("POST", "/api/calibration?cam=2", ALICE, &second_corners_json()).0 == 200
        });
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": ""}"#).0, 200);
        assert_eq!(second.device(), "");
        wait_for("the second camera to close", || rig.engine.latest_frame(1).is_none());
        let state = rig.state(ALICE);
        assert_eq!(state["second"]["enabled"], false);
        assert!(state["second"]["calibration"].is_array(), "kept while it is off");
        assert!(rig.engine.latest_frame(0).is_some(), "the first camera did not notice");
        // And back on with the same camera it works again (the thread was only waiting).
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": "2"}"#).0, 200);
        wait_for("a picture from the second camera again", || rig.engine.latest_frame(1).is_some());

        // Not during a game.
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, "{}").0, 200);
        assert_eq!(rig.call("POST", "/api/camera/select?cam=2", ALICE, r#"{"id": ""}"#).0, 409);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"*","reason":"abandoned"}"#).0, 200);
    }

    #[test]
    fn manual_moves_end_and_abort() {
        let rig = rig("manual", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        rig.call("POST", "/api/calibration", ALICE, &corners_json());

        // Nothing was played: aborting leaves no game behind.
        rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"*","reason":"abandoned"}"#).0, 200);
        assert!(rig.state(ALICE)["game"].is_null());
        assert_eq!(rig.call("GET", "/api/games", "", "").1["games"].as_array().unwrap().len(), 0);

        rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e2e4"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e5"}"#).0, 200, "SAN works too");
        let (status, body) = rig.call("POST", "/api/game/move", ALICE, r#"{"move":"Ke4"}"#);
        assert_eq!(status, 400, "{body}");
        assert_eq!(rig.state(ALICE)["game"]["moves"][1]["san"], "e5");
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 1);

        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"0-1","reason":"resignation"}"#).0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["game"]["result"], "0-1");
        assert_eq!(state["game"]["termination"], "resignation");
        assert_eq!(rig.call("GET", "/api/games", "", "").1["games"][0]["result"], "0-1");
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"0-1","reason":"resignation"}"#).0, 409);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"2-0","reason":"x"}"#).0, 409, "no game is running");
    }

    #[test]
    fn a_game_with_a_clock_starts_it_hands_the_time_over_pauses_and_goes_back_with_undo() {
        let rig = rig("clock", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        rig.call("POST", "/api/calibration", ALICE, &corners_json());

        // The time of each player and the extra time are checked.
        for bad in [
            r#"{"minutes": 100000}"#,
            r#"{"minutes": -5}"#,
            r#"{"minutes": 5, "extra_seconds": -1}"#,
            r#"{"minutes": 5, "extra_seconds": 99999}"#,
        ] {
            assert_eq!(rig.call("POST", "/api/game/start", ALICE, bad).0, 400, "{bad}");
        }
        // No clock asked for: a game without one, and no clock to press.
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B","minutes":0}"#).0, 200);
        assert!(rig.state(ALICE)["clock"].is_null());
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"switch"}"#).0, 409);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"*","reason":"abandoned"}"#).0, 200);

        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B","minutes":5,"extra_seconds":3}"#).0, 200);
        let clock = rig.state(ALICE)["clock"].clone();
        assert_eq!((clock["base"].clone(), clock["increment"].clone()), (json!(300.0), json!(3.0)));
        assert_eq!(
            (clock["started"].clone(), clock["paused"].clone(), clock["running"].clone()),
            (json!(false), json!(false), Value::Null)
        );
        assert_eq!(clock["left"], json!([300.0, 300.0]), "nothing runs before the start");

        // Only the controller presses the clock, and only with a known action.
        assert_eq!(rig.call("POST", "/api/game/clock", BOB, r#"{"action":"switch"}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"nonsense"}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"pause"}"#).0, 409, "not started");

        // The first press starts the player to move; the next hands the time over.
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"switch"}"#).0, 200);
        assert_eq!(rig.state(ALICE)["clock"]["running"], "white");
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"switch"}"#).0, 200);
        assert_eq!(rig.state(ALICE)["clock"]["running"], "black");

        // A pause stops both; going on gives the time to the player to move (White, nothing was played).
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"pause"}"#).0, 200);
        let paused = rig.state(ALICE)["clock"].clone();
        assert_eq!((paused["paused"].clone(), paused["running"].clone()), (json!(true), Value::Null));
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"pause"}"#).0, 409);
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"resume"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"resume"}"#).0, 409);
        assert_eq!(rig.state(ALICE)["clock"]["running"], "white");

        // A move earns the extra time, keeps what the players have left, and gives the time to the other player.
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e2e4"}"#).0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["clock"]["running"], "black");
        let left = state["game"]["moves"][0]["left"].clone();
        assert!(left[0].as_f64().unwrap() > 302.0 && left[0].as_f64().unwrap() <= 303.0, "{left}");
        assert_eq!(left[1], json!(300.0));

        // Taking the move back takes the clock back to the start.
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        let back = rig.state(ALICE)["clock"].clone();
        assert_eq!(back["left"], json!([300.0, 300.0]));
        assert_eq!(back["started"], false);

        // The end of the game stops the clock, and what the game says stays for the page.
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move":"e2e4"}"#).0, 200);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"1-0","reason":"resignation"}"#).0, 200);
        let over = rig.state(ALICE);
        assert_eq!(over["clock"]["running"], Value::Null);
        assert_eq!(over["clock"]["base"], json!(300.0));
        assert_eq!(rig.call("POST", "/api/game/clock", ALICE, r#"{"action":"switch"}"#).0, 409, "no game is running");
        // The stored game has the clock, and the PGN says so.
        let id = over["game"]["id"].as_str().unwrap().to_string();
        let detail = rig.call("GET", &format!("/api/games/{id}"), "", "").1;
        assert_eq!(detail["time_control"], json!({"base": 300.0, "increment": 3.0}));
        let pgn = rig.engine.game_pgn(&id).unwrap().unwrap();
        assert!(pgn.contains("[TimeControl \"300+3\"]"), "{pgn}");
        assert!(pgn.contains("[%clk 0:05:0"), "{pgn}");
    }

    /// A depth camera that sees the start position of the game from a spot of its own.
    struct FakeDepth {
        pictures: u32,
        corners: [[f32; 2]; 4],
        occupied: [bool; 64],
        black: [bool; 64],
    }

    impl crate::depthlink::DepthSource for FakeDepth {
        fn describe(&self) -> String {
            "Fake depth (480x360)".into()
        }

        fn modes(&self) -> Vec<crate::depthlink::DepthMode> {
            use crate::depthlink::DepthMode;
            vec![DepthMode { width: 320, height: 240, fps: 30 }, DepthMode { width: 480, height: 360, fps: 30 }]
        }

        fn next_frame(&mut self) -> anyhow::Result<crate::depth::DepthFrame> {
            std::thread::sleep(Duration::from_millis(25));
            self.pictures += 1;
            Ok(crate::depth::fake::frame(480, 360, &self.corners, &self.occupied, &self.black, true, 0.03, self.pictures))
        }
    }

    #[test]
    fn a_depth_camera_beside_the_colour_one_is_found_on_the_board_learned_and_used() {
        let dir = std::env::temp_dir().join(format!("nicochess-depth-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        // The depth camera sees the board where the colour calibration says, taken over by the fields of view, and
        // then a little elsewhere: the two sensors are some centimetres apart.
        let guess = crate::depth::guess_corners(&synthetic::corners(), (synthetic::WIDTH, synthetic::HEIGHT), (480, 360), 69.4, 63.4);
        let truth = guess.map(|c| [c[0] + 12.0, c[1] - 8.0]);
        let mut occupied = [false; 64];
        let mut black = [false; 64];
        for square in 0..64 {
            occupied[square] = !(16..48).contains(&square);
            black[square] = square >= 48;
        }
        let factory: crate::depthlink::DepthFactory = Box::new(move |_, _| {
            Ok(Box::new(FakeDepth { pictures: 0, corners: truth, occupied, black }) as Box<dyn crate::depthlink::DepthSource>)
        });
        let link = crate::depthlink::DepthLink::new(factory, Some(dir.join("depth.json")));
        link.set_offer(Some("Fake Depth".into()));
        // The tracker looks every so often, not at every picture of a camera that is much faster than it needs to be.
        let config = EngineConfig { depth: Some(Arc::clone(&link)), track_interval: Duration::from_millis(100), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        engine.spawn_capture(camera::factory(SourceSpec::Synthetic, Duration::from_millis(5))).unwrap();
        engine.spawn_depth().unwrap();
        let handler = handler(Arc::clone(&engine), 8);
        let rig = Rig { engine, handler, dir };

        // The page is told that the camera has a depth part, and not yet used.
        let state = rig.state(ALICE);
        assert_eq!(state["depth"]["offered"], true);
        assert_eq!(state["depth"]["on"], false);
        assert_eq!(rig.call("POST", "/api/depth", BOB, r#"{"on": true}"#).0, 403, "only the controller");
        assert_eq!(rig.call("POST", "/api/depth", ALICE, r#"{"on": true}"#).0, 200);
        assert_eq!(rig.state(ALICE)["depth"]["on"], true);

        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        wait_for("the depth picture", || rig.call("GET", "/depth.jpg", "", "").0 == 200);
        // Before the board is calibrated there is nothing to look for.
        assert_eq!(rig.call("POST", "/api/depth/align", ALICE, "").0, 409);
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);

        // The game starts: the squares are looked for in the depth picture, then learned, and used.
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);
        wait_for("the board in the depth picture", || rig.state(ALICE)["depth"]["aligned"] == true);
        let state = rig.state(ALICE);
        assert!(state["depth"]["message"].as_str().unwrap().contains("found"), "{}", state["depth"]);
        wait_for("the depth to be used", || {
            let state = rig.state(ALICE);
            state["depth"]["in_use"] == true && state["tracker"]["status"] == "watching"
        });
        let state = rig.state(ALICE);
        assert_eq!(state["depth"]["ignored"], false);
        assert_eq!(state["tracker"]["views"], json!([true, false]), "the page counts the cameras, not the depth");
        assert_eq!(rig.call("GET", "/depth-warp.jpg", "", "").0, 200);
        // Nothing moves on the board, so no move may be invented while the two look at it.
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(rig.state(ALICE)["game"]["moves"].as_array().unwrap().len(), 0);

        // Unticking stops it.
        assert_eq!(rig.call("POST", "/api/depth", ALICE, r#"{"on": false}"#).0, 200);
        wait_for("the depth to be left out", || rig.state(ALICE)["depth"]["in_use"] == false);
        assert_eq!(rig.state(ALICE)["tracker"]["status"], "watching");
    }

    #[test]
    fn the_depth_settings_are_chosen_checked_and_used() {
        let dir = std::env::temp_dir().join(format!("nicochess-depthsettings-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let guess = crate::depth::guess_corners(&synthetic::corners(), (synthetic::WIDTH, synthetic::HEIGHT), (480, 360), 69.4, 63.4);
        let truth = guess.map(|c| [c[0] + 12.0, c[1] - 8.0]);
        let mut occupied = [false; 64];
        let mut black = [false; 64];
        for square in 0..64 {
            occupied[square] = !(16..48).contains(&square);
            black[square] = square >= 48;
        }
        let factory: crate::depthlink::DepthFactory = Box::new(move |_, _| {
            Ok(Box::new(FakeDepth { pictures: 0, corners: truth, occupied, black }) as Box<dyn crate::depthlink::DepthSource>)
        });
        let link = crate::depthlink::DepthLink::new(factory, Some(dir.join("depth.json")));
        link.set_offer(Some("Fake Depth".into()));
        let config = EngineConfig { depth: Some(Arc::clone(&link)), track_interval: Duration::from_millis(100), ..patient() };
        let engine = Engine::new(config, store, dir.join("calibration.json"), None);
        engine.spawn_capture(camera::factory(SourceSpec::Synthetic, Duration::from_millis(5))).unwrap();
        engine.spawn_depth().unwrap();
        let handler = handler(Arc::clone(&engine), 8);
        let rig = Rig { engine, handler, dir };

        rig.state(ALICE);
        assert_eq!(rig.call("POST", "/api/depth", ALICE, r#"{"on": true}"#).0, 200);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("the modes of the camera", || rig.state(ALICE)["depth"]["modes"].as_array().is_some_and(|m| m.len() == 2));
        let state = rig.state(ALICE);
        assert_eq!(state["depth"]["settings"], json!({"near_mm": 100, "far_mm": 4000, "width": 848, "height": 480, "fps": 30}));

        // Only the controller; nothing that cannot be used; no picture that the camera does not have.
        assert_eq!(rig.call("POST", "/api/depth/settings", BOB, r#"{"near_mm": 300}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"near_mm": 20}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"near_mm": 2000, "far_mm": 2100}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"width": 640, "height": 360, "fps": 30}"#).0, 400);
        assert_eq!(rig.state(ALICE)["depth"]["settings"]["near_mm"], 100, "nothing was changed");

        // The distances work at once: the board is at about 600 mm.
        wait_for("a picture with readings", || rig.state(ALICE)["depth"]["valid"].as_f64().unwrap_or(0.0) > 0.8);
        let generation = link.generation();
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"near_mm": 300, "far_mm": 1200}"#).0, 200);
        let state = rig.state(ALICE);
        assert_eq!(
            (state["depth"]["settings"]["near_mm"].clone(), state["depth"]["settings"]["far_mm"].clone()),
            (json!(300), json!(1200))
        );
        assert_eq!(link.generation(), generation, "the camera is not opened again for that");
        wait_for("the picture still has readings", || rig.state(ALICE)["depth"]["valid"].as_f64().unwrap_or(0.0) > 0.8);
        // Ending before the board leaves nothing of it.
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"near_mm": 100, "far_mm": 400}"#).0, 200);
        wait_for("nothing left in the picture", || rig.state(ALICE)["depth"]["valid"].as_f64().unwrap_or(1.0) < 0.05);

        // Another size opens the camera again.
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"width": 320, "height": 240, "fps": 30}"#).0, 200);
        assert_eq!(link.generation(), generation + 1);
        assert_eq!(rig.state(ALICE)["depth"]["settings"]["width"], 320);
        wait_for("the camera opened again", || !rig.state(ALICE)["depth"]["source"].as_str().unwrap_or("").is_empty());

        // Kept for the next start.
        let kept = std::fs::read_to_string(rig.dir.join("depth.json")).unwrap();
        assert!(kept.contains("\"width\": 320") && kept.contains("\"far_mm\": 400"), "{kept}");

        // Not during a game.
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        let (status, body) = rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#);
        assert_eq!(status, 200, "{body}");
        assert_eq!(rig.call("POST", "/api/depth/settings", ALICE, r#"{"near_mm": 200}"#).0, 409);
    }

    /// The Opera game on the demo board, with a depth camera beside the camera that shows the same moment of it: the
    /// squares are found in the depth picture while the game is on, learned, and the first moves are followed.
    #[test]
    fn the_first_moves_of_a_demo_game_are_followed_with_the_depth_camera_helping() {
        let dir = std::env::temp_dir().join(format!("nicochess-demodepth-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let options = crate::app::ServerOptions {
            listen: "127.0.0.1:0".into(),
            // Fifteen pictures a move, of which the tracker looks at every third: with the depth each look takes long in
            // a test build.
            source: SourceSpec::Demo { move_interval: Duration::from_millis(300) },
            frame_interval: Duration::from_millis(20),
            data_dir: dir.join("games"),
            calibration_path: dir.join("calibration.json"),
            tracker: quick_tracker(),
            track_interval: Duration::from_millis(60),
            idle_timeout: Duration::from_secs(3600),
            game_timeout: Duration::from_secs(3600),
            max_viewers: 8,
            record: None,
            remember_camera: false,
            second_device: None,
            second_synthetic: false,
            depth_synthetic: true,
        };
        let built = crate::app::build(options).unwrap();
        let rig = Rig { engine: built.engine, handler: built.handler, dir };

        assert_eq!(rig.state(ALICE)["depth"]["offered"], true);
        assert_eq!(rig.call("POST", "/api/depth", ALICE, r#"{"on": true}"#).0, 200);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame(0).is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        wait_for("the depth picture", || rig.call("GET", "/depth.jpg", "", "").0 == 200);
        assert_eq!(rig.call("POST", "/api/game/start", ALICE, r#"{"white":"A","black":"B"}"#).0, 200);

        // The first moves of the game, with captures, are enough to see that the depth is in step with the colour.
        let mut helped = false;
        wait_for("fourteen moves", || {
            let state = rig.state(ALICE);
            helped |= state["depth"]["in_use"] == true && state["depth"]["ignored"] == false;
            state["game"]["moves"].as_array().unwrap().len() >= 14
        });
        let state = rig.state(ALICE);
        let played: Vec<String> =
            state["game"]["moves"].as_array().unwrap().iter().take(14).map(|m| m["san"].as_str().unwrap().to_string()).collect();
        assert_eq!(played, synthetic::OPERA_GAME[..14].iter().map(|m| m.to_string()).collect::<Vec<_>>());
        assert!(helped, "the depth took part in the game: {}", state["depth"]);
        assert!(state["depth"]["aligned"] == true, "{}", state["depth"]);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result":"*","reason":"abandoned"}"#).0, 200);
    }

    #[test]
    fn a_program_without_a_depth_camera_says_so() {
        let rig = rig("nodepth", SourceSpec::Synthetic);
        assert!(rig.state(ALICE)["depth"].is_null());
        assert_eq!(rig.call("POST", "/api/depth", ALICE, r#"{"on": true}"#).0, 409);
        assert_eq!(rig.call("GET", "/depth.jpg", "", "").0, 503);
    }

    #[test]
    fn unknown_routes_and_bad_bodies() {
        let rig = rig("errors", SourceSpec::Synthetic);
        assert_eq!(rig.call("GET", "/nope", "", "").0, 404);
        rig.state(ALICE);
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, "not json").0, 400);
        assert_eq!(rig.call("POST", "/api/game/promotion", ALICE, r#"{"piece":"queen"}"#).0, 400);
        let (status, page) = rig.call("GET", "/", "", "");
        assert_eq!(status, 200);
        assert!(page.as_str().unwrap().contains("NicoChess"));
    }
}
