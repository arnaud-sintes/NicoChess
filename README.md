# NicoChess

NicoChess watches a real chessboard with a camera (or two), works out the moves with the rules of chess, records the
game and shows it live, and afterwards in replay, on a web page for a phone, a tablet or a PC. Everything runs offline,
in one small program.

Play on a normal board with normal pieces. Point a camera at it, calibrate once, press Start. The moves appear on the
page with their times, the game is saved, and it can be exported as PGN.

## How it works

The camera cannot tell a knight from a bishop, and does not have to. For each of the 64 squares the program only asks
whether it looks empty, white or black, using a model of that square that is learned from the known starting position
and keeps adapting to the light. The program itself knows the rules and the position, so it knows the piece types.
After each pause in the movement it scores every way the game may have gone on (no move, any legal move, up to three
moves played between two pictures) by how well the picture fits, and takes the likeliest one when it is clearly ahead.

- **Quick.** A move is on the page about 0.7 s after it is made (25 pictures a second; the slider on the Calibrate tab
  sets the rate). The page shows how long each player has been thinking from the first move, can be turned a quarter
  at a time, and a move can be made on it by dragging a piece (or tapping it and then the square) when the camera
  missed it. The pieces each player has taken stand beside the board (White's on the left, Black's on the right, with
  the lead in points under them), also when a game is replayed. A pawn that reaches the last rank asks for the new
  piece as four icons to tap or to drag onto the square.
- **A chess clock, if you want one.** Give each player a time (and an extra time per move) when starting a game: the
  clocks count down on every page, the space bar (or a big button) hands the time over as you move, there is a pause,
  and the game ends when a time runs out.
- **Not sure?** A move that leads only narrowly is taken anyway and flagged next to the board ("Not completely sure
  about d4, 78%"), with the other likely moves and their chances as buttons that replace it, and an Undo. When nothing
  is likely enough the page asks. Moves can always be typed, and tracking resumes by itself afterwards.
- **Made for the screen it is on.** A phone held upright has the board across the screen with the clocks above and below
  it; held sideways, the board as high as the screen and the rest beside it; a tablet or a computer, the board on the
  left and the moves and controls on the right. Light or dark as the device is set (or as chosen behind the role button
  at the top right, where Release control is too), and a button beside the status shows only the board and the clocks.
- **One camera or two.** A second camera on the opposite side sees what the first cannot: the squares hidden by a hand,
  an arm or a tall piece. With one camera nothing changes ([Two cameras](#two-cameras)).
- **Follows a board that moves.** During a game the calibration follows the board if the camera or the board shifts a
  little.
- **Records what it saw.** `--record` keeps the pictures, the decisions and the corrections of each game, and `--replay`
  runs them through the detection again, with other tuning values if wanted ([Record and replay](#record-and-replay)).

## Where it runs

| | |
|---|---|
| **Windows PC** | The main target. `NicoChess-windows.zip`: unzip and double-click, nothing to install (about 1.2 MB). Webcams through Media Foundation; the page lets you choose among the cameras of the computer |
| **Android phone or tablet** | An app that is both camera and screen: the server runs inside it and the page is shown in it. It keeps recording with the screen off, and can let other devices connect |
| **Linux** | The same program as a service on a headless box (32-bit i386 build for small machines), reached from a phone or a tablet; V4L2 cameras ([PLAN.md](PLAN.md)) |

## Quick start

**Without a camera**, to see everything work: download `NicoChess-windows.zip` from the
[latest release](https://github.com/arnaud-sintes/NicoChess/releases/latest), unzip it and double-click
**Demo (no camera).bat** (or `cargo run --release -- --source demo` from the source). Open the address it prints, then:

1. **Calibrate** > **Open the camera** > **Auto-detect the board** (or click the four corners a1, h1, h8, a8, counter-clockwise on the picture).
2. **Live** > **Start game**. A whole game (Morphy's "Opera game", 33 plies) is played by a drawn board and ends in checkmate.
3. **Games** to replay it, or download the PGN.

**With a camera:** double-click **Start NicoChess.bat** (`Start NicoChess.bat 1` uses camera number 1; on the command
line `--device N --width 1280 --height 720`, `--probe` lists what the cameras offer). Set up the pieces, calibrate, and
press **Start game** with the pieces in their starting positions, because that is what the squares are learned from.
Use **Picture** on the Calibrate tab to turn the picture or adjust the contrast. A phone or tablet on the same network
opens the address that the program prints: the first page controls, the others only watch.

Other sources for trying things: `--source synthetic` (the starting position), `--source photo.jpg`, and
`--second-camera` with the demo for two cameras without hardware. `--open` opens the page in the browser of this
computer, `--listen 127.0.0.1:28080` chooses the address, `--help` lists every option.

## Two cameras

One camera sees the board at an angle: a tall piece hides the square behind it, a hand or an arm hides the squares it is
over, a glare spoils a few squares. A second camera on the opposite side sees what the first cannot. What the two
pictures say is put together square by square, each camera counting as far as it can explain what it sees there.

- **Windows:** on the **Calibrate** tab choose a camera for **Camera 2** ("No second camera" switches it off and keeps its
  calibration for later). Or `--device2 1`. The choice is kept.
- **Linux:** `--device2 /dev/video2`. **Android:** one camera only.
- Each camera is set up by itself: with two cameras the Calibrate tab has a button for each, and the picture, the
  calibration (clicks or Auto-detect, which finds a1 from the pieces when it can), the top-down view and the picture settings are those of
  the one chosen. The Live tab says "Two cameras are watching".
- If one camera stops (unplugged, taken by another program) the page says so and the game goes on with the other; when it
  comes back it is used again.

Put the cameras on opposite sides of the board: two on the same side add little. Two 1280x720 streams may not fit the USB
bandwidth of one hub; if not, use a smaller `--width`/`--height` or another port.

## Record and replay

```
nicochess --record                      # a folder per game in ./recordings (Windows: Start NicoChess (record).bat)
nicochess --replay recordings\2026-11-16_11-30-05 --replay-verbose
nicochess --replay recordings --margin 9 --max-excess 80     # all the recordings, with other tuning values
```

A recording has the pictures the detection was given (both cameras', when there are two), what it decided, and what the
people did (typed moves, undos). A replay feeds them through the detection again and tells where it agrees with what
happened, which moves it would have made someone type, and which it got wrong. This is how a bad session is looked into
and how the tuning options are tried on real pictures ([ENVIRONMENT.md](ENVIRONMENT.md)).

## Status

Built and tested on a PC with a synthetic camera (over 140 tests, a benchmark of whole games under drifting light, glare,
leaning pieces and noise, and CI on Windows and Linux). **Not yet seen by real cameras on a real game.** What has and has
not been tried:

| Tried | Not yet tried |
|---|---|
| The page, the calibration and the whole pipeline on the demo, in the Windows release program | A real game on a real board, with real lighting and real pieces: the detection thresholds are tuned on synthetic pictures only |
| An integrated Windows webcam: camera list, 1280x720 capture, the picture on the page (2026-11-12) | Two real cameras at once (also the USB bandwidth), and how much the second camera gains: measured on synthetic pictures only, and the one-camera figure with hands over the moves (66% of the moves by itself) has no two-camera counterpart yet |
| Auto-detect of the board on a few photos of real boards from the web | The Android app on a phone since its service and switches were added; the external USB camera |
| The 32-bit Linux build, in a Debian 12 i386 container | The Linux target with its real camera |
| The camera chooser on a laptop with two cameras (listing only) | Changing cameras with the page while a USB camera is plugged in or pulled out |

Details of what is built, with the limits, are in [PURPOSE.md](PURPOSE.md), and every change in [CHANGELOG.md](CHANGELOG.md).

## Download and build

Releases (open to everyone): https://github.com/arnaud-sintes/NicoChess/releases/latest, with `NicoChess-windows.zip`,
`nicochess-debug-apk.zip` (the Android app, a debug build) and `nicochess-linux-i386.tar.gz`. The version is at the bottom
of the page and shown by `nicochess --version`.

From the source (Rust 2024, one workspace; Docker Desktop for the Linux and Android builds):

| To | Run |
|---|---|
| Run, test, lint | `cargo run --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings` |
| Windows package | `.\scripts\package-windows.ps1` gives `dist\NicoChess-windows.zip` |
| Linux 32-bit program | `.\scripts\build-target.ps1` gives `dist\nicochess` (built in a Debian 12 i386 container, because the camera crate needs the Linux headers) |
| Android app | `.\scripts\build-android.ps1 [-Abis arm64-v8a]` gives `dist\nicochess-debug.apk` |
| Release | `.\scripts\release.ps1` builds all three and publishes the GitHub release of the version in `Cargo.toml` |

## A small headless box

The program also runs on a small Linux box with no screen (the first target was an Atom N270 mini PC): a USB Wi-Fi
dongle exposes an access point, and a phone or tablet joins it to open the page. `.\scripts\make-usb-bundle.ps1` builds
a USB key with the program, an installer, the Debian packages for an offline install and the access point script, and
`scripts\deploy.ps1 -Target user@host` updates it over SSH. How to install and configure it: [PLAN.md](PLAN.md), and
its constraints and camera checks: [docs/target-hardware.md](docs/target-hardware.md). That box has never run the program
with a real camera.

Under study: a laptop as the target (own screen, faster) and engine analysis with Stockfish
([docs/requirements.md](docs/requirements.md), "Under study").

## Licence

GNU General Public License, version 3 or (at your option) any later version: see [LICENSE](LICENSE). The chess rules
come from the `shakmaty` crate, which is under that licence, so the program as a whole has to be. The other
Rust dependencies are under MIT, Apache-2.0, BSD, ISC, Zlib or Unlicense terms, which are compatible with it. Every package of a release carries the licence text.

## Documents

- [CLAUDE.md](CLAUDE.md): entry point for an assistant or a new maintainer: commands, rules, gotchas.
- [PURPOSE.md](PURPOSE.md): what it is for, scenarios, features, non-goals, known limits.
- [ARCHITECTURE.md](ARCHITECTURE.md): modules, threads, files, targets, the data flow from camera to PGN.
- [DESIGN.md](DESIGN.md): patterns and coding rules, how to add a route, a setting or a camera source.
- [ENVIRONMENT.md](ENVIRONMENT.md): folders, build, test, package, deploy, record and replay, troubleshooting.
- [CHANGELOG.md](CHANGELOG.md): what changed in each version.
- [docs/requirements.md](docs/requirements.md), [docs/design.md](docs/design.md): the requirements and the early design study.
- [PLAN.md](PLAN.md), [docs/target-hardware.md](docs/target-hardware.md): the Linux target.

## Code layout

The long version, with the threads and the files, is in [ARCHITECTURE.md](ARCHITECTURE.md).

| File | Role |
|---|---|
| `src/main.rs`, `src/app.rs` | The command-line program: options and start-up; builds the engine, the camera threads and the web handler (also used by the Android library) |
| `src/http.rs`, `src/server.rs` | A small HTTP server on the standard library (event streams must not be buffered); routes and JSON API, listed at the top of `server.rs` |
| `src/engine.rs` | The state machine: cameras on and off, games, the tracker, the store, roles, push to the pages |
| `src/camera.rs`, `src/choice.rs`, `src/pushed.rs` | Frame sources (V4L2, Media Foundation, demo, synthetic board, image file); the choice among the cameras; a camera driven from outside (the Android app) |
| `src/calibration.rs`, `src/boarddetect.rs`, `src/picture.rs` | Corners and the warp to the top-down board; finding the board by itself and following it; turn, brightness, contrast |
| `src/vision.rs`, `src/tracker.rs` | The per-square models that keep learning; the stability gate, the evidence of one or two cameras and the match to a legal move |
| `src/game.rs`, `src/clock.rs`, `src/store.rs`, `src/fsutil.rs`, `src/roles.rs` | The game record, SAN/UCI and PGN; the chess clock; one JSON file per game; atomic file writes; controller and viewers |
| `src/record.rs`, `src/replay.rs` | Recording of games and their replay |
| `src/synthetic.rs` | A generated chessboard in perspective, for the demo, the tests and the benchmark (`examples/stress.rs`) |
| `web/index.html` | The page (one file, built into the program) |
| `android/` | The Android app: Kotlin shell (`android/app`) and the JNI library (`android/native`) |
| `docker/`, `scripts/` | Build images; build, package, release and deploy scripts (PowerShell), the USB key files and the Windows launchers |
| `docs/` | Requirements, design study, target hardware |
