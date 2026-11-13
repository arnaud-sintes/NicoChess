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
- **To release:** change the version, write the CHANGELOG section, commit, push, then `.\scripts\release.ps1` (from Bash: `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release.ps1`). It refuses unless the tree is clean and pushed and the version is new, builds the three packages, checks that the program reports the version, and publishes a GitHub release `vx.y.z` with: `NicoChess-windows.zip`, `nicochess-debug-apk.zip` (the APK zipped with install notes; `build-android.ps1` also makes it), `nicochess-linux-i386.tar.gz` and `SHA256SUMS.txt`. `-NoPublish` builds and stops. The release is the newest one on the page, so `releases/latest` always points at it. The repository is private: only people with access see the release.
- The APK is a **debug** build, signed with the debug key kept in the Docker volume `nicochess-android-keystore`; there is no release signing yet.

## Run for development

```
cargo run --release -- --source demo          # a whole game on a drawn board
cargo run --release -- --source synthetic     # the starting position only
cargo run --release -- --source photo.jpg     # a still JPEG
cargo run --release -- --probe                # list the cameras, formats and controls
cargo run --release -- --device 0 --width 1280 --height 720 --open    # Windows webcam (Linux: --device /dev/video0)
```

`--open` opens the page in the browser of this computer. Default address `0.0.0.0:8080`; on Windows, if that port is reserved or taken, the program takes the next free one among 28080-28089, then any, and prints the addresses. For tests and scripts give the address yourself: `--listen 127.0.0.1:28080`. `--data-dir` and `--calibration` default to the current folder (`./data`, `./calibration.json`); `picture.json` goes next to the calibration file. `--help` lists every option, including the tuning flags (`--stable-frames`, `--motion-threshold`, `--max-cost`, `--margin`).

## Test

- `cargo test` at the root. It covers: the chess rules and PGN, the vision and tracker on synthetic pictures (a whole game of 33 plies), board auto-detection (also rotated, noisy, scaled), picture settings, atomic file writes, the HTTP server (including the stoppable listener), and the engine through the API (roles, calibration, pictures, streams, a game from the demo camera to a PGN, resume, stabilization). It takes about a minute because the engine tests run real capture threads.
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
