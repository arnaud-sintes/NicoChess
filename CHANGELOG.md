# Changelog

The version is the one in `Cargo.toml` (`[workspace.package]`), shown in the page, by `nicochess --version`,
and as the version of the Android app. Each release is a tag `vX.Y.Z` on GitHub with the packages attached
(see `scripts/release.ps1`). Versions follow semantic versioning while the project is below 1.0: a new
minor number may change behaviour, a patch number only fixes things.

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
