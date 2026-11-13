//! HTTP routes: the page, the camera images, the JSON API and the event stream.
//!
//!   GET    / or /index.html            the page
//!   GET    /frame.jpg                  latest camera frame
//!   GET    /frame.mjpg                 the camera frames as they come (Motion JPEG stream)
//!   GET    /warp.jpg[?grid=0]          top-down board (needs a calibration)
//!   GET    /warp.mjpg                  the top-down board as a Motion JPEG stream
//!   GET    /api/camera/stats           pictures per second and time per picture
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
//!   POST   /api/camera/follow          {"on": true}  follow the board if it moves during a game
//!   DELETE /api/calibration            forget the calibration
//!   POST   /api/camera/rotate          {"turns": 1}  quarter turns clockwise (-1: counter-clockwise)
//!   POST   /api/camera/picture         {"brightness": 0, "contrast": 1}
//!   POST   /api/camera/auto-contrast   stretch the picture of the board over the whole range
//!   POST   /api/camera/picture/reset   brightness and contrast back to neutral
//!   POST   /api/game/start             {"white","black","fen"?,"started_at"?}
//!   POST   /api/game/resume            go on with the unfinished game found at start (the board must show its position)
//!   POST   /api/game/end               {"result","reason"}  (also closes an unfinished game that is not running)
//!   POST   /api/game/move              {"move": "e2e4" or "Nf3"}
//!   POST   /api/game/undo              take back the last move
//!   POST   /api/game/promotion         {"piece": "q"|"r"|"b"|"n"}
//!   POST   /api/game/relearn           learn the look of the squares again

use crate::engine::{ApiError, ApiResult, Engine, StartRequest};
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

        ("GET", "/frame.jpg") => match engine.latest_frame() {
            Some(frame) => Reply::bytes(200, "image/jpeg", frame.jpeg().as_ref().clone()),
            None => Reply::text(503, "the camera is off"),
        },
        ("GET", "/frame.mjpg") => frame_stream(engine, false),
        ("GET", "/warp.mjpg") => frame_stream(engine, true),
        ("GET", "/api/camera/stats") => Reply::json(200, &engine.camera_stats()),
        ("GET", "/warp.jpg") => {
            let grid = !request.query.split('&').any(|q| q == "grid=0");
            match engine.warped_jpeg(grid) {
                Ok(bytes) => Reply::bytes(200, "image/jpeg", bytes),
                Err(e) => Reply::text(e.status(), &e.message()),
            }
        }

        ("GET", "/api/state") => {
            let token = request.query_param("token").unwrap_or("");
            Reply::json(200, &engine.snapshot(token))
        }
        ("GET", "/api/events") => events(engine, request.query_param("token").unwrap_or("").to_string(), max_viewers),

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
            done(body_json::<Body>(&request).and_then(|b| engine.set_calibration(&token, b.corners)))
        }
        ("POST", "/api/calibration/detect") => match engine.detect_calibration(&token) {
            Ok(message) => Reply::json(200, &json!({ "ok": true, "message": message })),
            Err(e) => failure(e),
        },
        ("DELETE", "/api/calibration") => done(engine.clear_calibration(&token)),
        ("POST", "/api/camera/follow") => {
            #[derive(Deserialize)]
            struct Body {
                on: bool,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.set_follow(&token, b.on)))
        }

        ("POST", "/api/camera/rotate") => {
            #[derive(Deserialize)]
            struct Body {
                turns: i32,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.rotate_picture(&token, b.turns)))
        }
        ("POST", "/api/camera/picture") => {
            #[derive(Deserialize)]
            struct Body {
                brightness: f32,
                contrast: f32,
            }
            done(body_json::<Body>(&request).and_then(|b| engine.set_picture_adjust(&token, b.brightness, b.contrast)))
        }
        ("POST", "/api/camera/auto-contrast") => done(engine.auto_contrast(&token)),
        ("POST", "/api/camera/picture/reset") => done(engine.reset_picture_adjust(&token)),

        ("POST", "/api/game/start") => {
            #[derive(Deserialize, Default)]
            struct Body {
                #[serde(default)]
                white: String,
                #[serde(default)]
                black: String,
                fen: Option<String>,
                started_at: Option<String>,
            }
            done(body_json::<Body>(&request).and_then(|b| {
                engine.start_game(&token, StartRequest { white: b.white, black: b.black, fen: b.fen, started_at: b.started_at })
            }))
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
fn frame_stream(engine: &Arc<Engine>, top_down: bool) -> Reply {
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
                match engine.wait_for_frame(last.as_ref(), Duration::from_secs(1)) {
                    Some(frame) => {
                        silent_since = std::time::Instant::now();
                        let jpeg: Arc<Vec<u8>> = if top_down {
                            // The board view needs no more than a few pictures per second.
                            std::thread::sleep(TOP_DOWN_PAUSE);
                            let newest = engine.latest_frame().unwrap_or_else(|| Arc::clone(&frame));
                            let made = engine.warped_jpeg_of(&newest, true);
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
                        std::thread::sleep(Duration::from_millis(80));
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

    fn rig(name: &str, source: SourceSpec) -> Rig {
        let dir = std::env::temp_dir().join(format!("nicochess-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let store = Store::open(&dir.join("games")).unwrap();
        let engine = Engine::new(EngineConfig::default(), store, dir.join("calibration.json"), None);
        engine.spawn_capture(camera::factory(source, Duration::from_millis(5))).unwrap();
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
            assert!(started.elapsed() < Duration::from_secs(90), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_first_user_controls_and_the_others_only_watch() {
        let rig = rig("roles", SourceSpec::Synthetic);
        assert_eq!(rig.state(ALICE)["role"], "controller");
        assert_eq!(rig.state(BOB)["role"], "viewer");

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
        assert!(rig.engine.latest_frame().is_none());
        assert_eq!(rig.call("GET", "/frame.jpg", "", "").0, 503);
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 409);

        assert_eq!(rig.call("POST", "/api/calibration/open", ALICE, "").0, 200);
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        wait_for("camera off", || rig.engine.latest_frame().is_none());
        assert_eq!(rig.state(ALICE)["mode"], "idle");
    }

    #[test]
    fn turning_the_picture_turns_the_calibration_with_it() {
        let rig = rig("turn", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());

        assert_eq!(rig.call("POST", "/api/camera/picture", BOB, r#"{"brightness": 10, "contrast": 1}"#).0, 403);
        assert_eq!(rig.call("POST", "/api/camera/picture", ALICE, r#"{"brightness": 10, "contrast": 0}"#).0, 400);
        assert_eq!(rig.call("POST", "/api/camera/picture", ALICE, r#"{"brightness": 40, "contrast": 1.5}"#).0, 200);
        let state = rig.state(ALICE);
        assert_eq!(state["picture"]["brightness"], 40.0);
        assert_eq!(state["picture"]["contrast"], 1.5);
        // What the page shows is adjusted, the raw picture is not.
        wait_for("the adjusted picture", || {
            let frame = rig.engine.latest_frame().unwrap();
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());

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
        wait_for("a frame", || rig.engine.latest_frame().is_some());

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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        let engine = Engine::new(EngineConfig::default(), store, dir.join("calibration.json"), calibration);
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
        assert_eq!(rig.call("POST", "/api/calibration", ALICE, &corners_json()).0, 200);
        assert_eq!(rig.call("POST", "/api/game/resume", ALICE, "").0, 200);
        // Take the only move back: an empty game, which is saved like any other.
        assert_eq!(rig.call("POST", "/api/game/undo", ALICE, "").0, 200);
        assert_eq!(rig.engine.list_games().unwrap().len(), 1);
        assert_eq!(rig.call("POST", "/api/game/end", ALICE, r#"{"result": "*", "reason": "abandoned"}"#).0, 200);
        assert!(rig.engine.list_games().unwrap().is_empty(), "the empty game is gone from the disk too");
    }

    /// A running game on the synthetic source, with a calibration: for the commands that play moves.
    fn running_game(name: &str) -> Rig {
        let rig = rig(name, SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
        assert_eq!(rig.call("POST", "/api/game/move", ALICE, r#"{"move": "Nf3 Nc6 Bb5"}"#).0, 400, "three at once is too many");
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
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
    fn manual_moves_end_and_abort() {
        let rig = rig("manual", SourceSpec::Synthetic);
        rig.state(ALICE);
        rig.call("POST", "/api/calibration/open", ALICE, "");
        wait_for("a frame", || rig.engine.latest_frame().is_some());
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
