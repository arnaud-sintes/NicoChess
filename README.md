# NicoChess

Monitor a real chessboard with a USB webcam and track the game automatically.

## Goal

An old webcam on a tripod looks at the board at about 45° vertical tilt and about 20° lateral offset.
Software on a small PC captures frames, works out the moves, applies the chess rules and records the game.
Implementation language: Rust.

The target machine runs headless. A USB Wi-Fi dongle will expose its own access point, so a phone or tablet
can join it, open a web page, start a game and see the board and the moves live.

## Documents

- [Requirements](docs/requirements.md): what the software must do, roles, formats, status.
- [Design](docs/design.md): feasibility, approach, pipeline, Rust stack, milestones.
- [Plan](PLAN.md): what to install and configure on the target, and how.
- [Target hardware](docs/target-hardware.md): INEOS 600 (Atom N270) constraints, Linux choice, webcam, checks to run.

## Status

Everything is built and tested on a PC with a synthetic camera. **Nothing has run on the real camera yet.**

| Done | Not yet |
|---|---|
| Web page for phone, tablet and PC (no external resources), updates pushed by server-sent events | Real webcam: formats, exposure lock, detection quality on a C170 |
| Controller / viewer roles: the first user controls, the others watch and replay | Tuning of the detection thresholds on real lighting |
| Calibration by clicking the 4 corners, kept on disk | Wi-Fi access point on the target ([PLAN.md](PLAN.md) section 8) |
| Per-square empty / white / black detection, learned from the known start position | |
| Stability gate and legal-move matching, with castling, en passant, promotion | |
| Live board and move list with timestamps (offsets from the start of the game) | |
| Manual corrections: undo, enter a move, pick among candidates, change a promotion | |
| End of game by the rules, or by resignation, draw, abort | |
| Games stored as JSON, list by date, replay (real time, as fast as possible, step by step), PGN export | |
| 32-bit Linux build, run in a Debian 12 i386 container | Run on the INEOS 600 |

## Try it on a PC (no camera)

```
cargo run --release -- --source demo
```

Open http://localhost:8080. The demo source plays a whole game (Morphy's "Opera game", 33 moves) on a synthetic board.

1. Go to **Calibrate**, press **Open the camera**, and click the 4 corners of the board in the order a1, h1, h8, a8
   (counter-clockwise on screen). The corners are about (120,440), (520,425), (470,90), (190,100) in the 640x480 image.
2. Go back to **Live**, press **Start game**. The moves appear as the demo plays them, and the game ends by checkmate.
3. Open **Games** to replay it, or download the PGN.

Open the same address from a second browser or a phone on the same network: it is a viewer and cannot start anything.
Other sources: `--source synthetic` (the starting position only) or `--source some-image.jpg`.
`--open` opens the page in this computer’s browser once the server is up (handy when the target is a laptop).

`cargo test` runs the 46 tests, including a full game recorded from the demo camera to a PGN.
`cargo run -- --help` lists all the options.

## Cameras

- Logitech C170 (VGA): the fallback, marginal at 640x480.
- Yealink UVC30 Room (4K, 120°): the better candidate, not tried yet. The program accepts MJPEG, YUYV and NV12, turns autofocus off, and
  `./nicochess --probe` (Linux) lists what a camera offers. Details and open points in [docs/target-hardware.md](docs/target-hardware.md).

## Under study

A laptop as the target (own screen, 64-bit, faster) and engine analysis with Stockfish: see "Under study" in [docs/requirements.md](docs/requirements.md).

## Build for the target (Debian 12, 32-bit)

```
.\scripts\build-target.ps1
```

This builds in a Debian 12 i386 container (Docker Desktop must be running) and writes `dist\nicochess`.
It needs the container because the camera crate generates its bindings at build time and needs the Linux headers.
[PLAN.md](PLAN.md) covers installing, configuring and deploying the target.

## Code layout

| File | Role |
|---|---|
| `src/main.rs` | Options and start-up |
| `src/http.rs` | Small HTTP server on the standard library (needed for event streams that must not be buffered) |
| `src/server.rs` | Routes and the JSON API, listed at the top of the file |
| `src/engine.rs` | State machine: camera on/off, game, tracker, store, roles, push to pages |
| `src/camera.rs` | Frame sources: V4L2 camera, demo game, synthetic board, image file |
| `src/calibration.rs` | Corner validation, save/load, warp to the top-down board |
| `src/vision.rs` | Per-square sampling and the learned classifier |
| `src/tracker.rs` | Stability gate and the match of the board to a legal move |
| `src/game.rs` | Game record, SAN/UCI, PGN export |
| `src/store.rs` | One JSON file per game |
| `src/roles.rs` | Controller and viewers |
| `src/synthetic.rs` | Generated chessboard in perspective, for tests and PC development |
| `web/index.html` | The page |
