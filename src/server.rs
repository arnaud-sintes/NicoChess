//! HTTP routes: the page, the camera images, the JSON API and the event stream.
//!
//!   GET    /                           the page
//!   GET    /frame.jpg                  latest camera frame
//!   GET    /warp.jpg[?grid=0]          top-down board (needs a calibration)
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
//!   DELETE /api/calibration            forget the calibration
//!   POST   /api/game/start             {"white","black","fen"?,"started_at"?}
//!   POST   /api/game/end               {"result","reason"}
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
        ("DELETE", "/api/calibration") => done(engine.clear_calibration(&token)),

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
                engine.start_game(
                    &token,
                    StartRequest { white: b.white, black: b.black, fen: b.fen, started_at: b.started_at },
                )
            }))
        }
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
        _ => Reply::text(400, "unsupported method"),
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

        // It was saved, listed, and can be replayed.
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
