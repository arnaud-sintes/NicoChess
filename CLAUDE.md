# NicoChess: entry point

NicoChess watches a real chessboard with a camera, works out the moves with the chess rules, records the games
(SAN with time offsets, PGN export) and shows them live and in replay on a web page for a phone, tablet or PC.
One Rust program (library plus command line) runs on Windows and Linux; an Android app wraps the same core.
The page is one HTML file built into the program. Everything works offline.

## Documents

| File | Read it for |
|---|---|
| [PURPOSE.md](PURPOSE.md) | What it is for, scenarios, features, non-goals, known limits |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Languages and why, module map, data flow, threads and lock order, files, targets, Android structure |
| [DESIGN.md](DESIGN.md) | Patterns and coding rules, how to add a route, a setting, a camera source |
| [ENVIRONMENT.md](ENVIRONMENT.md) | Folders, prerequisites, build, test, package, deploy, troubleshooting |
| [README.md](README.md), [PLAN.md](PLAN.md), `docs/` | User-level overview, setup of the Linux target, requirements, design study, hardware notes |

## Commands (from the repository root)

| To | Run |
|---|---|
| Try it without a camera | `cargo run --release -- --source demo` (also `--source synthetic`, `--source photo.jpg`), page on http://localhost:8080 |
| Use a camera | `cargo run --release -- --probe` lists cameras (Windows: `--device 0`; Linux: `--device /dev/video0`), then `--device N --width 1280 --height 720 --open` |
| Test | `cargo test` (a minute or so, runs real capture threads on the synthetic source) |
| Format, lint | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (CI runs both) |
| Windows package | `.\scripts\package-windows.ps1` gives `dist\NicoChess-windows.zip` |
| Linux 32-bit program | `.\scripts\build-target.ps1` gives `dist\nicochess` (Docker Desktop running) |
| Android app | `.\scripts\build-android.ps1 [-Abis arm64-v8a]` gives `dist\nicochess-debug.apk` (Docker Desktop running) |
| USB key / SSH deploy | `.\scripts\make-usb-bundle.ps1`, `.\scripts\deploy.ps1 -Target user@host` |

`web/index.html` is built into the program with `include_str!`: after editing it, rebuild.

## Rules that matter most (details in DESIGN.md)

1. Comments, errors and docs are plain English prose that explain why. No emojis.
2. Errors: `anyhow` with context; the API answers 403 (not controller), 400 (bad request), 409 (not possible now), 500 (server-side I/O failure), as JSON `{"ok":false,"error":...}`.
3. Files are written with `fsutil::write_atomic` (temporary file, flush, rename), never with a plain `fs::write`.
4. The engine has one `inner` mutex: never hold it during slow work (detection, JPEG encoding, disk). Lock order: `inner` before `camera_wanted`, `frame`, `frame_count`.
5. Everything user-supplied that reaches the page goes through `esc()`. The page has no external resource.
6. Calibration corner order is a1, h1, h8, a8, counter-clockwise on screen, in the pixels of the rotated picture. Do not change it.
7. Kotlin: camera state is touched only on the main thread; camera callbacks post to it; bridge methods run on a WebView thread.
8. Tests drive the engine through `server::tests::rig` with the synthetic or demo source. No real camera in tests.
9. Add a dependency only with a good reason (the target is a 32-bit Atom, and the Windows package is under 1 MB).
10. Never commit unless asked. Scratch files go in the session scratchpad, never in the repository.

## Environment gotchas learned the hard way

- **Git Bash mangles paths** (`/c/...`, arguments starting with `/`). Use PowerShell, or set `MSYS_NO_PATHCONV=1`.
- **PowerShell treats Docker's stderr as an error.** Run the Docker scripts from Bash through
  `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/build-android.ps1`.
- **A finished Docker build can hide a failure behind exit code 0** (for example Docker not running). Read the log content (`BUILD SUCCESSFUL`, `Finished`, the output file's size and date).
- **Windows reserves port ranges around 8080** (Docker Desktop, Hyper-V) and the ranges change between restarts. For tests use `--listen 127.0.0.1:28080`. Without `--listen` the Windows program falls back to 28080-28089, then any free port.
- **Never run `taskkill /IM chrome.exe`**: it closes the user's own browser. Stop only the process you started (by its PID, or by a command line that contains your private `--user-data-dir`). For `nicochess.exe` started for a test, `taskkill /IM nicochess.exe` is fine.
- **The managed PC has no administrator rights.** Nothing may need admin: no installers, no `usbipd bind`, no firewall changes.
- **`NoDefaultCurrentDirectoryInExePath=1` is set**: `cmd` does not look in the current folder. A `.bat` file must call programs by explicit path (`"%~dp0nicochess.exe"`), and a script is run as `.\script.bat`.
- **Headless Chrome ignores window widths under about 500 px**, so phone-width screenshots are unreliable. Check small-screen layout by reading the CSS.
- **Long here-documents in the Bash tool can break** (a hook rewrites commands). Write scripts with the Write tool, then run them.
- **`android/native` does not compile on Windows** (it uses Unix calls). Never run `cargo build --workspace` or `cargo check --workspace` on Windows; the Android build runs in Docker only.
- **An APK signed with another key will not update the installed app.** The debug key lives in the Docker volume `nicochess-android-keystore`; if it is lost, uninstall the app once.
- Check that `dist/` files are new before reporting a build as done (stale files stay there).

## When you change something

- Keep the docs true: a new flag goes in `--help` and README or ENVIRONMENT.md; a new route goes in the list at the top of `src/server.rs`; a new file goes in the module map of ARCHITECTURE.md.
- Run `cargo test` and `cargo clippy` before saying a change is done. For Android code, run the Docker build; it is the only compile check.
- Say plainly what was not tested (nothing has run on a real board with a camera yet; the Android app has not been run on a phone).
