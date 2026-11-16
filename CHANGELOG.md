# Changelog

The version is the one in `Cargo.toml` (`[workspace.package]`), shown in the page, by `nicochess --version`,
and as the version of the Android app. Each release is a tag `vX.Y.Z` on GitHub with the packages attached
(see `scripts/release.ps1`). Versions follow semantic versioning while the project is below 1.0: a new
minor number may change behaviour, a patch number only fixes things.

## 0.4.0

- **Camera chooser on the page (Windows):** with several cameras (a built-in one and a USB one, say) the page
  lists them under Calibrate > Camera and switches to the one picked, at once if the camera is on. Not during a
  game. The choice is kept by the camera's name in `camera.json`, so it is found again at the next start even if
  Windows numbers the cameras differently; `--device N` on the command line still wins. The calibration belongs
  to the picture of one camera, so changing the camera forgets it. New routes: `GET /api/camera/list`,
  `POST /api/camera/select`.

## 0.3.0

Move detection that keeps working as a game goes on (testers reported that it lost track and needed every
move typed by hand):

- **Per-square models that keep learning** replace the fixed classifier learned once at the start: each square
  has its own idea of what empty, white and black look like, follows a light that fades, warms or moves, and
  learns from every move it sees clearly. Squares are described by eight numbers (colour of the middle and the
  ring, texture) averaged over small blocks, so camera noise matters less.
- **Likelihood instead of a fixed limit:** every way the game may have gone on is scored by how well it explains
  the picture, including what changed since the last confirmed picture; the best is accepted when it is clearly
  ahead and leaves little unexplained.
- **Catch-up:** up to three moves played between two pictures are recognised (it was two); a move typed by hand
  may also be two or three moves ("e4 e5 Nf3").
- **Tracking resumes after a move typed by hand:** the next still picture is adopted and learned, and nothing
  beyond one reply is believed right after it, so one odd square can no longer lead to invented moves.
- **`--record` and `--replay`, to look into test sessions:** with `--record` (and `--record-dir`, `--record-every`)
  every game gets a folder named after its start (date and time) with still pictures of what the detection saw,
  the decisions it took and what the people did (moves typed, undo, calibration). `--replay <folder>` runs it
  through the detection again, without camera or page, and compares what it finds with what happened; the tuning
  flags replace the recorded ones, so a change can be tried on real pictures. The Windows package has a
  `Start NicoChess (record).bat` launcher for testers.
- Tuning flags: `--max-cost` is replaced by `--max-excess`, and `--max-plies` is new.
- `examples/stress.rs`: a benchmark that plays whole games on synthetic pictures under drifting light, glare,
  leaning pieces, darkness, noise and unphotographed moves (see ENVIRONMENT.md). Synthetic only: nothing has run
  on a real board yet.

## 0.2.0

First numbered version. Everything since the first working program:

- **Android app:** the program runs inside the app, with the phone camera or an external USB camera when
  Android offers it; a "This phone" card to let other devices connect, to choose the camera; the app goes on
  recording with the screen off (foreground service); PGN export through the share sheet.
- **Board detection of its own:** an *Auto-detect the board* button finds the board from the inner corners of
  the squares; during a game the calibration follows a board that shifted a little (stabilization).
- **Picture settings:** turn by 90 degrees, auto contrast, brightness and contrast; the camera picture and
  the top-down board are pushed as streams (about 16 pictures per second from the phone).
- **Games:** an unfinished game found at start can be resumed or closed; draws by fivefold repetition and the
  75-move rule end the game, threefold repetition and the 50-move rule can be claimed; two moves played
  between two pictures are recognised as a pair; a move entered by hand may be two moves ("e4 e5").
- **Windows:** a package to unzip and double-click (no installation), with launchers; the program picks a
  free port when Windows reserves 8080.
- **Robustness:** camera reads time out instead of blocking; a panic cannot take the server down; games are
  written to the disk outside the engine lock, with a flush; a corrupt calibration file no longer stops the
  program; the HTTP server checks the Host header, limits the time of a request and notices dead clients.
- **Project:** one Cargo workspace and lock file, a pinned toolchain, rustfmt and clippy clean, CI, and the
  documents `CLAUDE.md`, `PURPOSE.md`, `ARCHITECTURE.md`, `DESIGN.md` and `ENVIRONMENT.md`.

Known limits: nothing has run on a real board with a real camera yet, and the Android app has not been run on a
phone; the APK is a debug build.
