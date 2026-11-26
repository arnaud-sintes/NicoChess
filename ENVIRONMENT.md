# Environment: organisation, build, test, deploy

## Code organisation

```
Cargo.toml, Cargo.lock      the Rust workspace: the root crate "nicochess" and android/native; ONE Cargo.lock
rust-toolchain.toml         pinned Rust version (rust-version is also set in Cargo.toml)
rustfmt.toml                formatting rules
.cargo/config.toml          Windows: link the C runtime into the program (no Visual C++ redistributable needed)
.github/workflows/ci.yml    CI: Linux (fmt, clippy -D warnings, tests, type check of the Android crate) and Windows (tests), all with --locked
.gitignore, .gitattributes  ignored: target/ dist/ downloads/ usb/ data/ calibration.json picture.json android build dirs *.apk *.log
CLAUDE.md PURPOSE.md ARCHITECTURE.md DESIGN.md ENVIRONMENT.md   these documents
README.md  PLAN.md          user-level overview; setup of the Linux target
src/                        the Rust library and command-line program (module map: ARCHITECTURE.md)
web/index.html              the page, built into the program
android/
  app/                      Kotlin app (package com.nicochess.app), AndroidManifest.xml, build.gradle.kts
  native/                   the JNI library crate (cdylib "nicochess_android"), depends on the root crate by path
  build.gradle.kts, settings.gradle.kts, gradle.properties
docker/
  build.Dockerfile          Debian 12 i386 image for the Linux 32-bit program
  android.Dockerfile        JDK 17, Android SDK 34, NDK 27, Gradle 8.10.2, Rust with the Android targets, cargo-ndk
  android-build.sh          what runs inside the Android container
scripts/
  build-target.ps1          Linux 32-bit program -> dist\nicochess
  build-android.ps1         Android app -> dist\nicochess-debug.apk
  package-windows.ps1       Windows package -> dist\NicoChess-windows.zip
  make-usb-bundle.ps1       the folder usb\ for the USB key
  deploy.ps1                build and install on a target over SSH
  test-camera-docker.ps1    try a USB camera through WSL and the container
  target-setup.sh           report on the target machine (CPU, memory, camera, Wi-Fi)
  usb/                      files for the key: install.sh, setup-ap.sh, fetch-debs.sh, package lists, README.txt
  windows/                  files of the Windows package: the .bat launchers and README.txt
docs/                       requirements.md, design.md (study), target-hardware.md
usb/  dist/  downloads/     not in git: the key content, build outputs, downloaded ISO and tools
```

## Prerequisites

| To | You need |
|---|---|
| Build and test on Windows | Rust (the pinned version, via rustup), the Visual Studio Build Tools (MSVC linker). No administrator rights are needed |
| Build and test on Linux | Rust, `build-essential`, `clang`, `libclang-dev`, `linux-libc-dev`, `pkg-config` (the camera crate generates bindings) |
| Linux 32-bit program, Android app | Docker Desktop running. Nothing else: the toolchains are in the images |
| Package for Windows | Rust, PowerShell |
| Check the page in a browser | Chrome and Node 22 (optional, see "Browser check") |

`android/native` does not compile on Windows (it uses Unix calls). **Never run `--workspace` builds or checks on Windows**; build the root crate (`cargo build`, `cargo test`, `cargo clippy` at the root) and let Docker build the Android library.

## Build

| What | Command | Output |
|---|---|---|
| Program for this machine | `cargo build --release` | `target\release\nicochess.exe` (or `nicochess`) |
| Linux 32-bit (Debian 12 i386) | `.\scripts\build-target.ps1` | `dist\nicochess` (uses `--locked`; the first run builds a 1 GB image) |
| Android app | `.\scripts\build-android.ps1` (add `-Abis arm64-v8a` for 64-bit only, faster) | `dist\nicochess-debug.apk` (the first run builds a 3 GB image) |
| Windows package | `.\scripts\package-windows.ps1` | `dist\NicoChess-windows\` and `dist\NicoChess-windows.zip` (it refuses to package an exe that still needs a Visual C++ runtime DLL) |
| USB key folder | `.\scripts\make-usb-bundle.ps1` | `usb\` (about 71 MB) |

Notes:
- Run the Docker scripts from Bash as `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-android.ps1` (PowerShell takes Docker's stderr for an error).
- **Read the log after a Docker build**, not only the exit code: look for `Finished` (Rust), `BUILD SUCCESSFUL` (Gradle), and the date and size of the file in `dist\`.
- Toolchain: `rust-version` in `Cargo.toml` and `rust-toolchain.toml` pin the Rust version; the Dockerfiles install the same one. The Android crate shares the root `Cargo.lock`; builds use `--locked`.
- Android: there is no Gradle wrapper; Gradle 8.10.2 is in the image. Only debug APKs are built. The debug signing key is kept in the Docker volume `nicochess-android-keystore` so that updates install over the old app. Gradle, cargo and target caches are named volumes too (`nicochess-gradle`, `nicochess-cargo-registry-android`, `nicochess-android-target` mounted at `/work/target` because the Android crate is a workspace member, `nicochess-cargo-registry`, `nicochess-target`).

## Version and releases

- **One version, in one place:** `version = "x.y.z"` under `[workspace.package]` in `Cargo.toml`. The program, the page footer, `nicochess --version`, the Android app (`versionName`, and `versionCode` = x*10000 + y*100 + z, read by `android/app/build.gradle.kts`) and the packages all take it from there. A test checks that the first `version = "..."` line of `Cargo.toml` is the program's version, because Gradle relies on it.
- **The commit** the program was built from is added by `build.rs` ("0.2.0 (3f2a9c1e)", "-dirty" with uncommitted changes). The Docker builds cannot run git, so the scripts pass it as `NICOCHESS_COMMIT`.
- **Versioning:** semantic versioning, below 1.0 a new minor number may change behaviour. Describe each version in `CHANGELOG.md` (a section `## x.y.z`: it becomes the release notes).
- **To release:** change the version, write the CHANGELOG section, commit, push, then `.\scripts\release.ps1` (from Bash: `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release.ps1`). It refuses unless the tree is clean and pushed and the version is new, builds the three packages, checks that the program reports the version, and publishes a GitHub release `vx.y.z` with: `NicoChess-windows.zip`, `nicochess-debug-apk.zip` (the APK zipped with install notes; `build-android.ps1` also makes it), `nicochess-linux-i386.tar.gz` and `SHA256SUMS.txt`. `-NoPublish` builds and stops. The release is the newest one on the page, so `releases/latest` always points at it. The repository is public, so anyone can download a release: publish nothing that should not be public (the script already refuses a program that contains the user name or the company name).
- The APK is a **debug** build, signed with the debug key kept in the Docker volume `nicochess-android-keystore`; there is no release signing yet.
- **No personal data in the packages.** Commits use the GitHub no-reply address (set `git config user.email` to it in this clone; the history was rewritten to it before the repository became public). The Windows program is built by `package-windows.ps1` with `--remap-path-prefix` flags (through `CARGO_ENCODED_RUSTFLAGS`, which repeats the static-runtime flag, since it replaces `.cargo/config.toml`), because the compiler otherwise writes `C:\Users\<name>\.cargo\...` into panic messages; `release.ps1` refuses to publish a program that contains the user name or the company name. The Linux and Android builds run in Docker and are clean by construction.

## Run for development

```
cargo run --release -- --source demo          # a whole game on a drawn board
cargo run --release -- --source synthetic     # the starting position only
cargo run --release -- --source photo.jpg     # a still JPEG
cargo run --release -- --probe                # list the cameras, formats and controls
cargo run --release -- --device 0 --width 1280 --height 720 --open    # Windows webcam (Linux: --device /dev/video0)
cargo run --release -- --device 0 --device2 1 --open                 # two cameras (Windows numbers; Linux: --device2 /dev/video2)
cargo run --release -- --source demo --second-camera                 # the demo seen by two cameras, no hardware needed
```

`--open` opens the page in the browser of this computer. Default address `0.0.0.0:8080`; on Windows, if that port is reserved or taken, the program takes the next free one among 28080-28089, then any, and prints the addresses. For tests and scripts give the address yourself: `--listen 127.0.0.1:28080`. `--data-dir` and `--calibration` default to the current folder (`./data`, `./calibration.json`); `picture.json` and `rate.json` go next to the calibration file; so do the files of the second camera (`calibration2.json`, `picture2.json`, `camera2.json`). `--fps` (pictures per second kept from the camera: 25 on Windows, 3 elsewhere; the page's slider changes it and keeps it in `rate.json`, which `--fps` beats). `--help` lists every option, including the tuning flags (`--stable-frames`, `--stable-time`, `--motion-threshold`, `--max-excess`, `--margin`, `--soft-margin`, `--max-plies`).

## Record and replay a game (investigating a bad session, tuning the detection)

`--record` keeps a recording of every game. At the start of each game (new, resumed, or reopened by undo) a folder
named after the start, `2026-11-16_11-30-05` (the clock of the PC on Windows, UTC on other systems; `session.json`
has both), is made in `--record-dir` (default `./recordings`; the Windows package's `Start NicoChess (record).bat`
uses `%LOCALAPPDATA%\NicoChess\recordings`). In it:

- `session.json`: the program and commit, the source, the calibration, the picture settings, the tracker settings
  and the position the tracker started from.
- `frames/000001.jpg`, ...: the pictures the detection was given, as it saw them (turned and adjusted). One every
  `--record-every` seconds (default 2) while the board is quiet, and a few a second (at least 0.15 s apart) while
  something happens (learning, a hand moving, unsure, a move found). `--record-every 0` keeps every quiet picture.
  `--record-all` (instead of `--record`; the Windows package's `Start NicoChess (record all).bat` does it at 25
  pictures a second) keeps every picture the detection looks at, whatever is going on, with a longer queue for the
  disk: several megabytes a second at 1280x720, for a fast game that has to be replayed picture by picture. Roughly
  100 to 250 KB each at 1280x720 (the noise of a cheap camera makes JPEG big).
- `cam0.mkv`, `cam1.mkv`, `cam2.mkv` instead of `frames/` when an `ffmpeg` is found (see below): one video for each
  camera (`cam2` is the height picture of the depth camera), H.264 at 25 pictures a second as a label only, the real
  time of each picture is in `events.jsonl`.
- `events.jsonl`: what happened, in order, with `t` in seconds: `frame` (file and the tracker's status), `detect` (the
  moves found and the position after them), `unsure` (the candidates offered), `rejected`, `manual` (a move typed by
  hand), `undo`, `promotion`, `relearn`, `calibration`, `end`, `closed`. Also `pipe`, one for every look of the
  tracker (also when its pictures are not kept): `step_ms` (how long the look took), `lock_ms` (how long the capture
  thread waited for the engine's lock before it), and for each view `age_ms` (from the camera to the start of the look),
  `skipped` (pictures of that camera that came since the one looked at the time before and were never looked at),
  `since_last_ms`, `gap_ms` (between two pictures as the camera made them: several pictures long means the camera or its
  driver lost some) and `wait_ms` (how long the capture thread waited for the picture: near 0 means it was already there,
  the program was not keeping up). The `closed` line carries the sum of the game in `pipeline` (per view: pictures
  received, looked at and never looked at; mean, 95th percentile (to within 30%) and maximum of age, wait and gap; the
  longest wait between two looks; how many looks were behind). The same figures come to the console every 30 seconds
  while playing (`pipeline, last 30 s: ...`, with `BEHIND` when a picture was over 0.5 s old or two looks 0.3 s
  apart). `cargo run --release --example pipeline -- <recording>` prints them window by window beside the moves and the
  unsure stretches, to match a failure with a lag (`tools/groundtruth/README.md`). A recording made before these lines
  has none, and the tool says so. The time of a camera picture comes from the camera thread on Windows; elsewhere it
  is the time the program got it, so `age_ms` leaves out any time the picture waited in the driver.

### Recording as video (much smaller): ffmpeg

A JPEG file for every picture is big, and `--record-all` at 25 pictures a second with a depth camera writes several
megabytes a second. When an `ffmpeg` program is found, the pictures are written as video instead: five to twenty
times smaller (on the recordings of 2026-11-24, about 31 KB a picture of the camera and 8 KB of the depth view,
against 250 and 83 KB as JPEG; [docs/video-recording.md](docs/video-recording.md) has the measurements), and the
encoding costs about half a core of a PC. The program only starts `ffmpeg` and pipes the raw pictures to it
(`src/video.rs`): nothing is linked, so the Windows package stays small and the Android and Linux builds do not change.

- Where to get it: any build of `ffmpeg` with `libx264` (the "GPL" builds); on Windows the portable
  `ffmpeg-n8.1-latest-win64-gpl-8.1.zip` of the BtbN builds
  (https://github.com/BtbN/FFmpeg-Builds/releases) is enough: unzip it anywhere, no administrator rights, and use
  `bin\ffmpeg.exe`. It is not part of the NicoChess package (about 170 MB): check the checksum file next to the
  download.
- Which one is used, in this order: `--ffmpeg <path>`, the variable `NICOCHESS_FFMPEG`, an `ffmpeg` (`ffmpeg.exe`)
  next to `nicochess`, then the first on `PATH`. The program says at its start which it found, or that it found none
  and writes JPEG files. `--no-video` records JPEG files even when one is found.
- The encoder: `libx264`, preset `veryfast`, `-crf 23` for a camera and for the depth view (`src/video.rs`),
  a key picture every 50 pictures, run at a lower priority with two threads; Matroska files (`.mkv`), which can be
  read even when the program was killed.
- Never silent: when the encoder is too slow its queue fills and the pictures are dropped and counted (the `closed` line
  of `events.jsonl` says `dropped`, as for JPEG); when it cannot start or dies, the recording notes a `video_lost` line
  and that camera goes on in JPEG files; the `closed` line lists `video_errors` and the folder keeps
  `ffmpeg-camN.log` when `ffmpeg` said something.
- Reading: `--replay`, `tools/groundtruth` (`keyframes`, `compare --replay`) start `ffmpeg` to decode the video, so
  the same `ffmpeg` must be found when a recording in video is replayed (`--ffmpeg` also works with `--replay`); there
  is a clear error when it is not.
- A video is lossy: on the two recordings it was tried on the tracker decided exactly as it did on the JPEG pictures
  (a replay of the original and of a copy that went through the codec gave the same report, give or take a point of
  certainty; at `-crf 28` it no longer did), see [docs/video-recording.md](docs/video-recording.md). A replay of a
  video recording is as faithful as one of JPEG pictures is, which is: not exact (a JPEG at 92 is lossy as well, and the
  live tracker saw the camera's own picture).

`--replay <folder>` needs no camera and starts no page. It runs the pictures through the current tracker at their
recorded times, applies what the people did, and prints what the replay found beside what happened, with the picture
file of each line, then a summary: how many moves the program found and how many were typed during the game, and
how many detections of the replay fit the game, how many do not, and at how many places the replay was behind (the
number of moves it would have made someone type), and the position the replay ends in (compare it with the last colour
picture of the recording: when the live game was lost, "do not fit" only means the live game never had that position,
not that the replay is wrong). The recorded game is the truth. A folder that holds recordings is
replayed recording by recording. The tuning options replace the recorded ones, to try a change on real pictures
(`--replay recordings\2026-11-16_11-30-05 --max-excess 80 --margin 9`); `--replay-verbose` also lists every change of
status and every list of candidates. Open the picture files named in the report to see what the camera saw.

Limits: a replay sees only the pictures that were kept, so with the default spacing it follows the live run closely
only around moves (where a few pictures a second are kept); use `--record-all` for a faithful replay of a short session. A
move that the people undid after the program found it counts as played. When the disk or the encoding of the
pictures is too slow, pictures are dropped (the end of the replay report says so) and the game is never delayed.
Two to four threads encode and write the pictures (0.11.1 had one, and `--record-all` lost about half of the
pictures with two cameras, the depth view first); not yet confirmed by a live run. Recordings show whatever the camera
sees: they stay on the PC, and look at them before sending them anywhere.

## Test

- `cargo test` at the root. It covers: the chess rules and PGN, the vision and tracker on synthetic pictures (a whole game of 33 plies), board auto-detection (also rotated, noisy, scaled), picture settings, atomic file writes, the HTTP server (including the stoppable listener), and the engine through the API (roles, calibration, pictures, streams, a game from the demo camera to a PGN, resume, stabilization). It takes about a minute because the engine tests run real capture threads.
- `STRESS_CAMERAS=2` and `STRESS_HAND=0.4` (see below) add a second camera and hands over the squares of the moves to the stress test.
- `cargo run --release --example stress` measures how well the tracker follows whole games on synthetic pictures
  with a poor camera: `easy`, `drift` (fading, warming light and a moving shadow), `hard` (plus glare and leaning
  pieces), `dark`, `noisy`, and `catch-up` (every fifth move is never photographed). A simulated person types the
  move whenever the tracker is unsure or wrong. At the end it prints how often the first candidate was the right one when the tracker was unsure, by its lead over the second. It prints, per game, how many plies were found by themselves, how
  many were wrong, how many had to be typed, and the longest run of typed moves. It takes some minutes.
  `STRESS_ONLY=hard,dark` runs some scenarios, `STRESS_VERBOSE=1` lists each failure, `STRESS_MARGIN=9` tries another
  `--margin`. Run it after any change to `vision.rs` or `tracker.rs`; the pictures are synthetic, so the numbers
  say that nothing got worse, not how a real board and camera behave.
  `STRESS_CAMERAS=2` adds a second camera on the other side of the board, and `STRESS_HAND=0.4` puts a hand (an
  arm, hiding a square of the move and what is around it) over a square of the move in 40% of the plies, in each
  camera by itself, until the move is judged. Running it with one and with two cameras shows what the second is worth.
- Temporary folders are `nicochess-<name>-<pid>` under the system temp folder and are removed afterwards.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must pass; CI runs them with the tests (`--locked`), on Linux and Windows.
- Android (Kotlin and JNI) has no unit tests: the Docker build is the compile check, and a run on a phone is the real test (not done yet).
- The camera code cannot be tested without a camera: use `--probe` and a short run on the machine that has the camera.

### Browser check (optional)

To look at the page in a real browser without a phone: start the program on the synthetic source (`--source synthetic --listen 127.0.0.1:28080 --data-dir <scratch> --calibration <scratch>\cal.json`), start Chrome with `--headless=new --remote-debugging-port=<port> --user-data-dir=<private folder>`, and drive it from a Node 22 script through the DevTools protocol (the global `WebSocket`): `Page.navigate`, `Runtime.evaluate` to click buttons, `Page.captureScreenshot`. For the Android card, inject a fake `window.NicoAndroid` with `Page.addScriptToEvaluateOnNewDocument`.
Stop only the processes you started: `taskkill /IM chrome.exe` would close the user's own browser. Headless Chrome ignores window widths under about 500 px, so do not trust phone-width screenshots.

## Package and deploy

**Windows.** Send `dist\NicoChess-windows.zip`. The user unzips it and double-clicks `Start NicoChess.bat` (camera number as the first argument), `Demo (no camera).bat`, or `List cameras.bat`. Games, calibration and picture settings go to `%LOCALAPPDATA%\NicoChess`. The first run asks Windows to allow the network for private networks, so that phones can join. The launchers call the exe by explicit path (`"%~dp0nicochess.exe"`) because some PCs do not search the current folder.

**Linux target, with the USB key.** `.\scripts\make-usb-bundle.ps1`, copy the content of `usb\` to a FAT32 key, then on the target (as root) mount the key and run `bash /mnt/install.sh` (`--demo` first to try the page without the camera, `--wifi-firmware` for dongle firmware). It installs the packages offline, the program in `/opt/nicochess`, the user `nicochess`, the settings file `/etc/default/nicochess` (`NICOCHESS_ARGS`), the data in `/var/lib/nicochess` (`games/`, `calibration.json`, `picture.json`) and the service `nicochess`. `setup-ap.sh` makes the Wi-Fi access point. Steps in `scripts/usb/README.txt` and PLAN.md sections 6 and 10.

**Linux target, over SSH.** `.\scripts\deploy.ps1 -Target user@host [-SkipBuild]` builds, copies to `/tmp`, installs to `/opt/nicochess` and restarts the service. `install.sh` must have been run once; the user needs `sudo`.

**Android.** Copy `dist\nicochess-debug.apk` to the phone and install it (allow installs from that source). Updates install over the old app as long as the debug key (volume `nicochess-android-keystore`) is the same; after losing it, uninstall the app once. The app asks for the camera and, on Android 13 and later, notification permission when it opens. Logs: `adb logcat -s nicochess`.

## Runtime files, per target

| Target | Games | Calibration and picture settings |
|---|---|---|
| Any, run by hand | `--data-dir` (default `./data`) | `--calibration` (default `./calibration.json`) and `picture.json` next to it |
| Windows package | `%LOCALAPPDATA%\NicoChess\games` | `%LOCALAPPDATA%\NicoChess\calibration.json`, `picture.json` |
| Linux service | `/var/lib/nicochess/games` | `/var/lib/nicochess/calibration.json`, `picture.json` |
| Android | app files folder, `games/` | app files folder, `calibration.json`, `picture.json`; choices in SharedPreferences `nicochess` |

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `cannot listen on 0.0.0.0:8080 ... forbidden by its access permissions (os error 10013)` | Windows reserved the port (Docker Desktop, Hyper-V). List them with `netsh int ipv4 show excludedportrange protocol=tcp` and pass `--listen` with a free port, for example `127.0.0.1:28080`. Without `--listen` the Windows program picks one by itself |
| Black or frozen camera picture | Another program uses the camera (Teams, Zoom, the Camera app): close it and restart. Run `--probe` to see what the camera offers. A camera that stops delivering pictures is reported on the page and reopened |
| Docker script ends "successfully" but there is no new file | Docker Desktop was not running or a step failed quietly. Read the log; check the date of `dist\` files; start Docker Desktop and retry |
| `docker` errors turn into PowerShell errors | Run the script through `powershell -NoProfile -ExecutionPolicy Bypass -File ...` from Bash |
| Android: "App not installed" or the update is refused | The APK is signed with another debug key. Uninstall the app once, install again |
| Android: the camera shows an error on the page | Permission refused (allow it in the settings of the app), camera in use by another app, or "external camera selected but none connected" (plug it in, or switch the external camera off) |
| A `.bat` file says a program is "not recognized" | The PC does not search the current folder: call it by explicit path |
| `cargo` fails in `android/native` on Windows | Expected: it is built only in Docker. Do not use `--workspace` on Windows |
| Moves are missed or wrong on a real board | Check the green grid in the **Board, top-down** view, lighting (no glare, constant), then the tuning flags; relearn the squares with the position on the board |
