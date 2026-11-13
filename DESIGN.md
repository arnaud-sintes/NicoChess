# Design: patterns and coding rules

These rules describe how the code is written today. Follow them when you change it; if a rule has to change, change it here too.

## Style

- **Comments and messages are plain English prose** and say why, not what. No emojis, no ticket numbers, no jokes. A comment that the code has outgrown is deleted or corrected, not left.
- **Names**: Rust `snake_case` and `CamelCase` as usual; names say what a thing is for (`track_interval`, `FrameInbox`), not how it works. Avoid names that look like keywords or std types (`Option_`).
- **Formatting and lint**: `rustfmt` with the repository's `rustfmt.toml`, and `clippy` with no warnings (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`). CI runs both.
- **Logging** is `eprintln!` with a prefix that names the part: `camera:`, `game:`, `store:`, `http:`, `follow:`, `calibration:`, `picture:`; the command-line program prints its final failure as `error:`. No levels, no timestamps (journald adds them). Messages are short sentences a user can act on.

## Errors

- Inside the core: `anyhow::Result` with `.context(...)` that names the file, address or value involved. No `unwrap()` on anything that can fail at run time (locks are the exception, see below).
- At the edge (`engine.rs` commands, `server.rs` routes): `ApiError`, which maps to a status:
  `Forbidden` 403 (not the controller), `BadRequest` 400 (the request is wrong), `Conflict` 409 (fine, but not possible now: a game is running, the camera is off), `Internal` 500 (a server-side failure such as a file that cannot be written).
  Never turn a disk error into a 400.
- Command answers are JSON `{"ok":true}` or `{"ok":false,"error":"..."}`; the page shows `error` as is, so it must be written for a person.
- A camera that fails is not fatal: the capture loop reports it (`camera_error` on the page), drops the source and tries again after a pause.

## Files

- Every file the program writes goes through `fsutil::write_atomic` (temporary file, `sync_all`, rename). Never `fs::write` on a file that matters.
- A corrupt `calibration.json` or `picture.json` must not stop the program from starting: report it and use the defaults.
- Game files carry a `version`; a reader of a newer version must refuse, not guess.

## Locking and threads

- The engine has one `inner` mutex. Take it, change what must change, release it. **The capture thread and the per-picture paths never hold it during slow work**: board detection, JPEG encoding, warping, writing a file, a JNI call. Copy what you need, release, work, take the lock again and check that nothing changed in between (see `follow_board`). A game is saved by marking it (`save` sets `pending_save` under the lock) and writing it after the lock is released (`flush_save`, which takes `save_lock`, so that one writer at a time writes the newest version). The rare commands of the owner (calibration, picture settings) write their small file while holding the lock: that takes milliseconds and is not on a per-picture path.
- Lock order: `save_lock` before `inner`; `inner` before `camera_wanted`, `frame`, `frame_count`. Never the other way round. (`wait_for_frame` takes `frame_count` and then only reads `frame`: it never touches `inner`.)
- Locks are recovered when poisoned (a panic elsewhere must not make every later request panic).
- The capture loop survives a panic in one frame. Connection threads release their slot even when they panic.
- The hub only says "something changed" (`Hub::notify`); each stream builds its own snapshot. After any change that a page shows, call `hub.notify()` once the lock is released.
- Streams (events, MJPEG) must end when the page goes away (a write error) and must not hold a camera-sized buffer longer than one picture.

## The page (`web/index.html`)

- One file, no external resource (font, script, image, CDN). It must work offline on a phone.
- **Everything that comes from outside** (player names, game data, error text, camera labels) goes through `esc()` before it is put in `innerHTML`. Prefer `textContent`.
- State comes from the event stream snapshot (`S`); the page does not keep its own copy of server state. Commands go through `api(method, path, body)`, which shows errors as a notice and returns `null` on failure.
- Camera pictures are `<img>` elements fed by the MJPEG streams; do not poll pictures.
- Anything that only exists in the Android app is behind `window.NicoAndroid` and hidden otherwise.
- Layout works from 320 px wide; there is no framework and no build step.

## Android (Kotlin)

- Camera objects (`device`, `session`, `reader`, handler thread) are touched **only on the main thread**. Camera callbacks are given the main handler, so they run there too; only the pictures arrive on the image reader's own thread, and `close()` closes the reader on that thread, after a picture in flight.
- Bridge methods (`AndroidBridge`) are called from a WebView thread: they read settings and post to the main thread; they never touch the camera directly.
- JNI calls that can block (`NativeCore.start`, `setRemote`) run on a worker thread, never on the main thread.
- Settings live in SharedPreferences through `Settings`; only camera settings restart the camera. A setting the owner cannot change during a game is refused in Kotlin, not only greyed out in the page.
- The foreground service is started with `startForegroundService` only when the camera permission is granted (Android refuses a camera service without it, and a service started that way must call `startForeground` within seconds).
- JNI function names must match `NativeCore.kt` exactly (`Java_com_nicochess_app_NativeCore_*`); a mismatch shows up only at run time, so change both together. Native functions that run on a camera thread catch panics.

## Tests

- The engine is tested through `server::tests::rig`: a real engine with a capture thread on the **synthetic or demo source**, driven with `Request::for_test`, no sockets.
- Vision and detection are tested on synthetic pictures (`synthetic.rs`: the board in perspective, the Opera game); detection is also checked after rotation, noise and scaling.
- No real camera, no network, no sleeping longer than needed: wait for a condition with `wait_for(...)` (a fixed sleep is only for checking that something does NOT happen). Temporary files go in `std::env::temp_dir()` under `nicochess-<name>-<pid>` and are removed.
- A bug fix comes with a test that fails without it.
- Kotlin and JNI code has no unit tests; its check is the Docker build. Say so when you change it.

## Do not

- Add a dependency lightly. The target is a 32-bit Atom and the Windows package is under 1 MB; every crate costs build time and size. Prefer a few lines of code.
- Break the lock order, or hold `inner` across slow work.
- Put page-shaped JSON in new engine code when it can be avoided: new commands return typed results, and `server.rs` shapes the JSON.
- Change the order of the calibration corners (a1, h1, h8, a8, counter-clockwise on screen, in pixels of the rotated picture) or the meaning of a stored file without bumping its version.
- Commit generated or private files: `dist/`, `usb/`, `downloads/`, `data/`, logs, keystores, `calibration.json`, `picture.json`.
- Run `taskkill /IM chrome.exe` or anything else that stops processes you did not start.

## How to add...

**A route.** Add the arm in `server.rs::route` (method and path), parse the body with a `#[derive(Deserialize)]` struct through `body_json`, call one engine method, answer with `done(...)` (or a typed JSON for data). Add the line to the route list at the top of `server.rs`. The engine method starts with `self.controlled(token)?` for a command, checks the mode, does its work, releases the lock, calls `hub.notify()`. Add a test through `rig`: 403 for a viewer, the success case, the refused case.

**A setting.** If it describes the picture or camera, add a field to `Picture` (with `#[serde(default)]` so old files load) and save it with `Picture::save`. Expose it in the snapshot (`engine::snapshot`), add the command, add the control to the page, and add a test that it survives a restart (save, then load). A setting only the Android owner makes goes in `Settings.kt` and the bridge, not in the engine.

**A camera source.** Implement `camera::FrameSource` (`describe`, `next_frame`, and `restart` if it is scripted). `next_frame` must return within a few seconds, with an error if the camera went silent. Add a `SourceSpec` variant and its arm in `camera::factory`; add the option in `main.rs` (and in `--help`). Frames are `RgbImage`; keep platform code behind `cfg` and out of the engine. Test the conversion code on small hand-made buffers.

## Rules added by the audits (keep them)

- A new picture source or long-running read must have a timeout: `FrameSource::next_frame` returns an error within a few seconds if the camera stays silent.
- A connection slot, a registered stream or a lock is released by a guard object (`Drop`), never by code at the end of a function that a panic can skip.
- Anything a server-side failure can cause (files, disk) is `ApiError::Internal` (500), not a 400.
- A setting that does not concern the camera must not restart the camera (a game may be running).
- The calibration records the picture size it was made for; code that creates one uses `Calibration::for_frame`.
