# Architecture

## Choices and why

| Choice | Why |
|---|---|
| **Rust 2024**, one crate that is a library (`src/lib.rs`) plus a command-line program (`src/main.rs`) | Small, fast, no runtime: it must run on a 32-bit Atom and be a one-file Windows program. The library holds everything that does not depend on the platform, so the Android app can reuse it unchanged |
| **Own small HTTP server** (`src/http.rs`), thread per connection, `Connection: close` | The page needs server-sent events and Motion JPEG streams, which must leave the machine at once; the usual small servers buffer their output. Few clients, so threads are fine |
| **Server-sent events**, not WebSocket | One direction (server to page) is all that is needed; commands are plain `POST`s. Works through any proxy and needs no handshake code |
| **One HTML file** (`web/index.html`) built into the program with `include_str!` | No build step, no external font, script or image: it works offline, and the program is a single file |
| **Vision from the known position**: homography warp, per-square empty/white/black classifier learned from the start position, legal-move matching | A cheap camera cannot tell piece types, but it can tell empty from white from black. The chess rules (`shakmaty`) do the rest, and piece types come from the game |
| **Pure Rust image code** (`image`, `imageproc`, no OpenCV) | Nothing to install, builds everywhere, including the container for the 32-bit target |
| **Kotlin shell plus JNI** for Android | Only Kotlin can reach Camera2, the foreground service, permissions and the WebView. Everything else (server, vision, rules) is the same Rust core in a native library |
| **Docker** for the Linux 32-bit and the Android builds | The `v4l` crate needs libclang and the Linux headers at build time; the Android build needs the SDK, NDK and Gradle. Neither belongs on the Windows PC |
| **Plain JSON files** for games, calibration and picture settings | Easy to inspect and back up; a game's file is the source of truth, positions are recomputed from the moves |

## Targets

| Target | Platform | Camera | Built by |
|---|---|---|---|
| Windows PC | x86-64, `cargo build --release`, C runtime linked in (`.cargo/config.toml`) | Media Foundation through `nokhwa`, on a camera thread of its own | `scripts\package-windows.ps1` |
| Linux headless box | i386, Debian 12, glibc of the target (the INEOS 600 is an Atom N270, 32-bit, single core, 1-2 GB RAM) | V4L2 through `v4l` | `scripts\build-target.ps1` (Docker) |
| Android phone or tablet | arm64-v8a and armeabi-v7a, minSdk 24, target and compile SDK 34 | Camera2 in Kotlin, pictures pushed into the core | `scripts\build-android.ps1` (Docker) |

## Module map

### Rust library and program (`src/`)

| File | Responsibility |
|---|---|
| `lib.rs` | Declares the modules; the library is platform independent |
| `main.rs` | Command-line options, help text, source choice, port choice (Windows fallback), startup banner, `--open`, `--probe` |
| `app.rs` | `build()` makes the engine, the capture thread and the web handler; `serve()` / `serve_on()` run the server |
| `http.rs` | HTTP/1.1 server: `serve`, `serve_on`, `bind_first`, a stoppable `Listener` (`spawn`), request limits, timeouts, `Host` check, TCP keepalive |
| `server.rs` | Routes, JSON API, MJPEG and event streams; the route list is the comment at the top |
| `engine.rs` | The state machine: modes (Idle, Calibrating, Playing), camera on demand, capture loop, tracker feeding, games, calibration and picture commands, stabilization, snapshots for the pages, SSE hub |
| `camera.rs` | `FrameSource` trait, `SourceSpec` and `factory`; V4L2 (Linux) and Media Foundation (Windows) cameras; demo, synthetic and image sources; `--probe` |
| `pushed.rs` | `FrameInbox` and `PushedSource`: a camera driven from outside (Android); YUV 4:2:0 to RGB |
| `calibration.rs` | The four corners and the size of the picture they were made for, validation, warp to the top-down board (50 px squares, 25 px margin) |
| `boarddetect.rs` | Finds the 49 inner corners (saddle points), fits the lattice, gives the four outer corners and which one is a1; `refine` follows a board that moved |
| `picture.rs` | Rotation, brightness, contrast, auto contrast, the follow switch; saved in `picture.json` |
| `vision.rs` | Sampling of each square and the learned empty/white/black classifier |
| `tracker.rs` | Stability gate and the choice of the legal move (or, when none fits, the pair of moves) that best explains the picture |
| `game.rs` | The game record (moves, result), SAN/UCI, PGN, file format version, the draw rules (repetition, 50 and 75 moves) |
| `store.rs` | One JSON file per game, ids, listing |
| `build.rs` (root) | Adds the git commit to the program (`COMMIT`, with `VERSION` from Cargo.toml: `lib.rs`) |
| `fsutil.rs` | `write_atomic`: temporary file, flush to disk, rename |
| `roles.rs` | Controller and viewer, tokens, expiry |
| `synthetic.rs` | A chessboard drawn in perspective, the Opera game; used by the demo source and by the tests |

### Page (`web/index.html`)
One file: markup, style and script. Tabs Live, Games, Calibrate; the page state comes from the event stream; commands go through `api()`.

### Android (`android/`)

| File | Responsibility |
|---|---|
| `native/src/lib.rs` | The JNI library: `start`, `setRemote`, `cameraWanted`, `cameraFailed`, `pushFrame`; keeps the handler and the listeners |
| `app/.../MainActivity.kt` | Shows the page in a WebView, asks for permissions, starts the service; WebView clients for dialogs and downloads |
| `app/.../CoreService.kt` | Foreground service: starts the core, owns the camera, polls `cameraWanted`, applies the settings, notification with Stop |
| `app/.../PhoneCamera.kt` | Camera2: opens the chosen camera, locks exposure, white balance and focus, pushes about 16 pictures per second |
| `app/.../CameraCatalog.kt` | Lists the cameras (back, front, external) and picks one from the settings |
| `app/.../AndroidBridge.kt` | `window.NicoAndroid` for the page: info, remote switch, external switch, camera choice |
| `app/.../Settings.kt` | The owner's choices in SharedPreferences, and the two ports |
| `app/.../NativeCore.kt` | The `external fun` declarations that match the JNI names |

## Data flow: camera to PGN

```
camera --(FrameSource::next_frame)--> capture thread (engine::on_frame)
   rotate + brightness/contrast (picture.rs)  ->  Latest { image, raw, seq }   -> preview: /frame.mjpg, /warp.mjpg
   every track_interval, while Playing:
      warp (calibration.rs) -> sample squares (vision.rs) -> stability gate + legal-move match (tracker.rs)
      -> Detection::Move (or Detection::Pair: two moves)  -> GameRecord::push_move (game.rs) -> pending_save -> flush_save after the lock is released (store.rs, atomic)
   about every 1.5 s (longer on a slow machine: at least 20 times the search time), while Playing and quiet: boarddetect::refine
   -> move the calibration if the board shifted (never more than one square from where the game began; the file is written at most every 10 s)
every change -> Hub::notify -> each event stream wakes, builds its snapshot (engine::snapshot) -> page
page --POST /api/...--> server.rs route -> engine command (checks the token and the mode) -> Hub::notify
```
PGN is produced on request from the stored moves (`GameRecord::pgn`); positions are never stored.

## Threads and locks

| Thread | What it does |
|---|---|
| `capture` (one) | Opens a `FrameSource` when the camera is wanted, loops over frames, runs `on_frame`; sleeps on a condvar when no camera is wanted. A panic while processing a picture is caught (the picture is skipped and the loop goes on); a panic inside the camera source itself ends the thread, which is logged |
| `camera` (Windows only) | Owns the Media Foundation camera (it is not `Send`) and sends frames over a channel |
| `http` (one per connection) | Reads one request, runs the handler, answers; stream replies (events, MJPEG) keep the thread until the page leaves |
| `http-accept` (Android, remote listener) | Polls a non-blocking listener ten times a second so it can be stopped |
| `nicochess-server` (Android) | Runs the loopback listener |
| `nicochess-camera` (Android) | The image reader's thread: receives the pictures and hands them to the core. Camera2 callbacks themselves run on the main thread |
| `logcat` (Android) | Reads the program's stderr from a pipe and writes it to logcat |
| `CoreService` worker (Android) | Starts the core and opens or closes the remote listener, which can take a moment |

State: one `Mutex<Inner>` in the engine holds mode, game, tracker, calibration, picture settings, roles, notices. Separate small locks: `frame` (latest picture, `RwLock`), `camera_wanted` with a condvar, `frame_count` with a condvar (streams wait on it), `stats` (speed figures), `save_lock` (one game writer at a time), the SSE hub's subscriber list.
**Lock order: `save_lock` before `inner`; `inner` before `camera_wanted`, `frame`, `frame_count`. No cycle exists; keep it that way.** (`wait_for_frame` takes `frame_count` and then only reads `frame`; it never takes `inner`.) Poisoned locks are recovered, not propagated. Slow work on the capture thread (detection, JPEG encoding, writing a game) is done outside `inner`; the rare commands of the owner write their small settings file under it.

## Persistence

| File | Where | Content |
|---|---|---|
| `games/<id>.json` | `--data-dir` (Android: `filesDir/games`; Windows package: `%LOCALAPPDATA%\NicoChess\games`; Linux service: `/var/lib/nicochess/games`) | One game: version, start date, names, start FEN, moves (SAN, UCI, time offset), result, termination. Id = digits of the browser's start time (`YYYYMMDDTHHMMSS`, `-2` on a clash, `undated` otherwise) |
| `calibration.json` | `--calibration` (default `./calibration.json`) | Corners a1, h1, h8, a8 in pixels of the rotated picture, and the size of the picture they were made for. The engine warns when the picture has another size |
| `picture.json` | next to `calibration.json` | Rotation, brightness, contrast, the follow switch |

All three are written by `fsutil::write_atomic`: after a power cut a file is the old one or the new one. An unfinished game found at start is loaded into the Idle state; the page offers **Resume** (`POST /api/game/resume`) or **Close it**.

## API and roles

The full route list is the comment at the top of `src/server.rs`. Summary: `GET` for the page, pictures, state, event stream and games; `POST` for commands (claim, release, calibration, picture, camera, game start/end/move/undo/promotion/relearn/resume); one `DELETE` (forget the calibration).
Identity is a token of 8 to 64 hexadecimal characters made by the page: header `X-Client-Token` on commands, `?token=` on the state and event `GET`s. The first token seen takes the controller role; a release disables auto-claim until someone presses Take control; the role expires after 60 s of silence outside a game (30 min during one). Commands check the controller and the mode (`ApiError`: 403, 400, 409, 500).
The `Host` header must name an address, `localhost`, a name without a dot, or a local name (`.local`, `.lan`, `.home`, `.home.arpa`, `.internal`, `.localdomain`): this stops DNS rebinding, and refuses a fully qualified name such as `pc.corp.example.com` (use the address), and accepted sockets get TCP keepalive. There is no login: the network is the protection.

## Platform abstraction

`camera::FrameSource` (`describe`, `next_frame`, `restart`) is the one interface the engine knows. `next_frame` must return within a few seconds (a camera that goes silent gives an error, and the engine's retry reopens it). `SourceSpec` says which source to build (`Camera`, `Demo`, `Synthetic`, `Image`, `Pushed`); `camera::factory` turns it into a `SourceFactory`, which the capture thread calls whenever the camera is wanted and drops when it is not.
`pushed.rs` is the Android case: the app calls `pushFrame` from Camera2, the converted picture goes into a `FrameInbox` (latest picture only), and `PushedSource` reads it. The inbox also carries the `wanted` flag: the engine sets it when it needs pictures, the service polls it every 300 ms and opens or closes the camera.

## Android structure

```
MainActivity (WebView, permissions) --starts--> CoreService (foreground, type camera)
CoreService: NativeCore.start -> server on 127.0.0.1:8080 (for the WebView)
             NativeCore.setRemote(on, 8081) -> second listener on 0.0.0.0:8081 (other devices), only when the owner allows it
             poll cameraWanted -> PhoneCamera (Camera2) -> NativeCore.pushFrame -> FrameInbox -> engine
page (in the WebView) <-- window.NicoAndroid (AndroidBridge): "This phone" card
```
The core starts once per process (the Activity and the service both ask; a mutex in the native library makes the second wait). The loopback listener is reachable by every app on the phone; the remote listener by the network. The Stop button of the notification ends the process. Only camera settings restart the camera; the remote switch does not.

Details of the Android shell worth knowing:
- The WebView has a `WebChromeClient` that shows the page's `confirm()` and `alert()` as dialogs (without one, every `confirm()` answers "no" and End game, Abort, Forget calibration and Re-learn do nothing), and a `DownloadListener` that hands a game's PGN to the share sheet (a WebView ignores the `download` attribute of a link).
- All camera callbacks run on the main thread (`PhoneCamera` gives Camera2 the main handler); only the pictures arrive on the image reader's own thread. This removes the races between `close()` and a camera that is still opening.
- A partial wake lock is held while the camera is open, so that the processor does not sleep with the screen off.
- A service of the camera type needs the camera permission (Android 11 and later): without it the service stays an ordinary one and is started with `startService`, not `startForegroundService` (which would oblige it to call `startForeground` within seconds).
- A failed camera closes itself and reports the reason to the core; the engine retries every two seconds, which is also how a USB camera that is plugged in later is picked up.
- A picture that cannot be converted is skipped and logged; it does not fail the source (failing would close and reopen the camera and redo the exposure settling).
