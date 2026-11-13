# NicoChess

Monitor a real chessboard with a USB webcam and track the game automatically.

## Goal

An old webcam on a tripod looks at the board at about 45° vertical tilt and about 20° lateral offset.
Software on a small PC captures frames, works out the moves, applies the chess rules and records the game.
Implementation language: Rust.

The target machine runs headless. A USB Wi-Fi dongle will expose its own access point, so a phone or tablet
can join it, open a web page, start a game and see the board and the moves live.

## Documents

- [CLAUDE.md](CLAUDE.md): entry point for an assistant or a new maintainer: commands, rules, gotchas.
- [Purpose](PURPOSE.md): what it is for, scenarios, features, non-goals, known limits.
- [Architecture](ARCHITECTURE.md): languages, modules, threads, files, targets.
- [Design](DESIGN.md): patterns and coding rules.
- [Environment](ENVIRONMENT.md): folders, build, test, package, deploy, troubleshooting.
- [Requirements](docs/requirements.md): what the software must do, roles, formats, status.
- [Design study](docs/design.md): feasibility, approach, pipeline, Rust stack, milestones.
- [Plan](PLAN.md): what to install and configure on the target, and how.
- [Target hardware](docs/target-hardware.md): INEOS 600 (Atom N270) constraints, Linux choice, webcam, checks to run.

## Status

Everything is built and tested on a PC with a synthetic camera. The integrated webcam of a Windows PC was tried on 2026-11-12
(camera list, 1280x720 capture, the picture on the page). **Not yet tried: a real game on a real board, the C170, the Yealink, the INEOS 600.**

| Done | Not yet |
|---|---|
| Web page for phone, tablet and PC (no external resources), updates pushed by server-sent events | Real webcam: formats, exposure lock, detection quality on a C170 |
| Controller / viewer roles: the first user controls, the others watch and replay | Tuning of the detection thresholds on real lighting |
| Calibration by clicking the 4 corners, or by the **Auto-detect the board** button (tried on a few photos of real boards found on the web), kept on disk | Wi-Fi access point on the target ([PLAN.md](PLAN.md) section 8) |
| Picture settings: turn by 90°, auto contrast, brightness and contrast (software, same on every camera) | Camera-side controls on Android (exposure compensation, focus, white balance) |
| Android app: server and phone camera in one app, runs on with the screen off (foreground service), remote-server switch, choice of camera, external USB camera when Android offers it | The Android app has not been run on a phone since the service and the switches were added; the USB camera is untested |
| Windows package: unzip and double-click, nothing to install ([below](#windows-package)) | Tried on this PC only |
| Stabilization: during a game the calibration follows the board if the camera shifts a little (can be switched off) | Tried only on synthetic pictures: not yet on a real board |
| Camera and top-down pictures pushed as Motion JPEG streams (no polling) | Speed of the preview on a real phone not measured: see the figures under the picture |
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

Open http://localhost:8080. The demo source plays a whole game (Morphy's "Opera game", 33 plies: 17 moves) on a synthetic board.

1. Go to **Calibrate**, press **Open the camera**, and click the 4 corners of the board in the order a1, h1, h8, a8
   (counter-clockwise on screen). The corners are about (120,440), (520,425), (470,90), (190,100) in the 640x480 image.
2. Go back to **Live**, press **Start game**. The moves appear as the demo plays them, and the game ends by checkmate.
3. Open **Games** to replay it, or download the PGN.

Instead of clicking, press **Auto-detect the board**: it finds the 49 inner corners of the squares and works out the 4 outer corners. If a1 is at the wrong dark corner (a1 and h8 are both dark), press **Swap sides**.

Open the same address from a second browser or a phone on the same network: it is a viewer and cannot start anything.
Other sources: `--source synthetic` (the starting position only) or `--source some-image.jpg`.
`--open` opens the page in this computer’s browser once the server is up (handy when the target is a laptop).

`cargo test` runs the test suite, including a full game recorded from the demo camera to a PGN.
`cargo run -- --help` lists all the options.

## Go to the target with a USB key

```
.\scripts\make-usb-bundle.ps1
```

builds `usb\`: the program, an installer, the Debian packages for an offline install, a Wi-Fi access point script and a README.
Copy its content to a FAT32 key and follow `README.txt` on the target. Details in [PLAN.md](PLAN.md) section 10.
`scripts\deploy.ps1 -Target user@host` updates the program over SSH when a network is available.

## Android app

`.\scripts\build-android.ps1` (Docker Desktop must be running) builds `dist\nicochess-debug.apk` for 64-bit and 32-bit ARM phones and tablets; `-Abis arm64-v8a` builds only the 64-bit one, which is faster (an x86 emulator build is not set up).
The app is the same program as on the PC: the server runs inside the app and the page is shown in a WebView, so everything works offline.
The phone camera is the default. On the **Calibrate** tab, a **This phone** card (only in the app) has:

- **Let other devices connect (remote server)**: off by default. When on, the page is also served on port 8081 to the network, and the card shows the addresses.
  Anyone on the network can then watch; the first page to connect while nobody controls can take control (for example a tablet while the phone only films).
- **Use an external camera (USB)**: uses the first camera Android lists as external. Android only offers a USB camera to apps on phones that support it; when none is seen the card says so.
- **Camera**: which camera of the phone, when it has several (back, ultra-wide, telephoto, front). Not changeable during a game.

A notification shows while the app runs (Android requires it for a service that uses the camera); its **Stop** button ends everything.
The app asks for the camera permission, and for notifications on Android 13 and later, when it opens.

## Windows package

`.\scripts\package-windows.ps1` builds `dist\NicoChess-windows.zip` (under 1 MB): `nicochess.exe`, three launchers and a README.
Nothing to install: the C runtime is linked into the program. Unzip, then:

- **Start NicoChess.bat** opens the page; `Start NicoChess.bat 1` uses camera number 1.
- **Demo (no camera).bat** plays a whole game on a drawn board.
- **List cameras.bat** shows the cameras, their formats and controls.

Games, calibration and picture settings are kept in `%LOCALAPPDATA%\NicoChess`. Without `--listen` the program takes port 8080 and, if Windows reserves it
(Docker Desktop and Hyper-V do), the next free one, and prints the addresses to use from a phone.

## Cameras

- Logitech C170 (VGA): the fallback, marginal at 640x480.
- Yealink UVC30 Room (4K, 120°): the better candidate, not tried yet. The program accepts MJPEG, YUYV and NV12, turns autofocus off, and
  `./nicochess --probe` (Linux and Windows) lists what a camera offers. Details and open points in [docs/target-hardware.md](docs/target-hardware.md).

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

The long version, with the threads and the files, is in [ARCHITECTURE.md](ARCHITECTURE.md) and [ENVIRONMENT.md](ENVIRONMENT.md).

| File | Role |
|---|---|
| `src/lib.rs` | The library: everything that does not depend on the platform |
| `src/main.rs` | The command-line program (Windows, Linux): options and start-up |
| `src/app.rs` | Builds the engine, the camera thread and the web handler; runs the server (used by main.rs and by the Android library) |
| `src/http.rs` | Small HTTP server on the standard library (needed for event streams that must not be buffered) |
| `src/server.rs` | Routes and the JSON API, listed at the top of the file |
| `src/engine.rs` | State machine: camera on/off, game, tracker, store, roles, push to pages |
| `src/camera.rs` | Frame sources: V4L2 camera on Linux, Media Foundation camera on Windows, demo game, synthetic board, image file |
| `src/pushed.rs` | Camera driven from outside (the Android app pushes its pictures in) |
| `src/calibration.rs` | Corner validation, save/load, warp to the top-down board |
| `src/boarddetect.rs` | Finds the board by itself (inner corners of the squares, lattice fit) and follows it when the camera moves |
| `src/picture.rs` | Turn, brightness, contrast, and the follow switch, saved in `picture.json` |
| `src/vision.rs` | Per-square sampling and the learned classifier |
| `src/tracker.rs` | Stability gate and the match of the board to a legal move |
| `src/game.rs` | Game record, SAN/UCI, PGN export |
| `src/store.rs` | One JSON file per game |
| `src/fsutil.rs` | Atomic file writes (temporary file, flush to disk, rename) shared by the store, the calibration and the picture settings |
| `src/roles.rs` | Controller and viewers |
| `src/synthetic.rs` | Generated chessboard in perspective, for tests and PC development |
| `web/index.html` | The page (one file, built into the program) |
| `android/` | The Android app: Kotlin shell (`android/app`) and the JNI library (`android/native`) |
| `docker/` | Build images for the 32-bit Linux program and for the Android app |
| `scripts/` | Build, package and deploy scripts (PowerShell), the USB key files (`scripts/usb`) and the Windows launchers (`scripts/windows`) |
| `docs/` | Requirements, design study, target hardware |
