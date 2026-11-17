# Changelog

The version is the one in `Cargo.toml` (`[workspace.package]`), shown in the page, by `nicochess --version`,
and as the version of the Android app. Each release is a tag `vX.Y.Z` on GitHub with the packages attached
(see `scripts/release.ps1`). Versions follow semantic versioning while the project is below 1.0: a new
minor number may change behaviour, a patch number only fixes things.

## 0.6.0

**Two cameras (optional).** A second camera on the opposite side of the board sees what the first cannot: the squares
that a hand, an arm or a tall piece hides. With one camera nothing changes.

- **Setting it up:** on the Calibrate tab (Windows) choose a camera for Camera 2, or "No second camera"; the choice is
  kept in `camera2.json`. On the command line: `--device2 X` (the way on Linux). `--second-camera` with
  `--source demo` or `--source synthetic` shows the same board from the other side, to try it without hardware. The
  Android app keeps its one camera.
- **Each camera by itself:** with two cameras the Calibrate tab has a button for each; the picture, the calibration
  (by clicks or Auto-detect), the top-down view and the picture settings are those of the one chosen. The files of the
  second camera are `calibration2.json`, `picture2.json` and `camera2.json`. Every request about a camera takes
  `?cam=2` for the second one.
- **Putting them together:** the tracker takes, square by square, what each camera says, each counting as far as it
  can explain what it sees on that square, so a square one camera cannot see is read from the other. The result is on
  the scale of one camera, so the thresholds mean the same. The Live tab says "Two cameras are watching".
- **If a camera goes away** (unplugged, taken by another program) the page reports it and the game goes on with the
  other; when it comes back it is used again. A camera calibrated or opened in the middle of a game learns its squares
  at the next confirmed position. Starting a game waits a few seconds for a camera that is still opening.
- **Recordings** keep both pictures of each time (format 2: `cam` and `feed` on the picture lines, which cameras took
  part in `views` lines, the second camera's calibration in `session.json`); `--replay` runs both through the
  detection. Recordings of format 1 are still read.
- `examples/stress.rs`: `STRESS_CAMERAS=2` adds the second camera and `STRESS_HAND=0.4` puts a hand over a square of
  the move, in each camera by itself.

What was and was not checked: all on synthetic pictures. With an arm over the squares of a move, one camera cannot
find it and two cameras do (a test); a whole demo game with two cameras ends in checkmate and its recording replays
correctly; one camera alone gives the same stress figures as 0.5.0. In the stress test with a hand over the move in
40% of the plies, one camera followed 66% of the moves by itself (96 to 100% without hands); the same run with two
cameras was not completed (the machine ran short of memory), so how much the second camera gains is **not measured**.
Never tried with two real cameras: whether two streams fit the USB bandwidth of one computer is not known.

## 0.5.0

Three things testers asked for, and the CI:

- **The squares of the virtual board had the wrong colours** (a1 was light): fixed, a1 is dark.
- **New pieces:** the pieces are drawn by the page itself instead of coming from a font, with a thin outline: White is
  cream with a dark outline and Black near black with a light outline, so they cannot be mixed up on either colour of
  square (and some devices no longer show chess glyphs as emoji).
- **Doubt instead of questions:** when two moves look nearly alike, the likelier one is taken if it leads by the soft
  margin (`--soft-margin`, default 2 nats; `--margin` or more turns it off) instead of stopping to ask. The page
  says so under the board: "Not completely sure about d4 (78% sure)", with the other likely moves and their chances as
  buttons that replace the move, and an Undo that takes it back and offers the choice (the tracker decides nothing
  until a move is entered). When no move is likely enough the page still asks, now with the chance of each move. New
  route `POST /api/game/doubt`; the recording notes the doubt of each move, and `--replay` marks doubtful moves and
  says why the tracker was unsure.
- **CI:** the workflow file was invalid YAML (so GitHub never ran it): fixed. A server test that played a whole game
  could fail on a slow machine because the controller role timed out: the test rigs now have long timeouts.

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
