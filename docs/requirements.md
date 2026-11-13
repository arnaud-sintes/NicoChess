# Requirements

What NicoChess must do. Agreed in discussion on 2026-11-04.
Items marked **proposed** are my suggestion and still need a yes from the project owner.

## Purpose

A webcam watches a real chessboard. The machine works out the moves, applies the chess rules,
records the game, and shows it live and in replay on a phone or tablet.

## Overview

| Part | Does |
|---|---|
| Backend (on the target) | HTTP server, webcam capture, image analysis, chess rules, recording and replay data |
| Interface (browser on a phone or tablet) | Start and follow games, calibrate, correct mistakes, replay old games |
| Network | The target exposes its own Wi-Fi access point (USB dongle). The WPA2 passphrase is the only protection; there is no login |

## Roles

- **Controller**: the first user to connect. Only the controller can start, stop, correct or calibrate.
- **Viewer**: every other user. A viewer can follow the live game and list and replay old games, and nothing else.
- Only one game is recorded at a time.
- **Proposed:** the role belongs to a token stored in the browser, so a phone that locks its screen keeps the role.
  The controller keeps it for the whole game. It can be released with a button, and outside a game it expires
  after 60 seconds without contact.

## Functional requirements

### Calibration
- FR-1. The interface has an always-reachable **Calibrate** button (controller only).
- FR-2. Calibration is done once by clicking the 4 board corners (a1, h1, h8, a8) and is kept on the target.
- FR-3. Starting a game with no calibration sends the controller to calibration first.
- FR-3a. **Auto-detect the board** finds the board in the picture and calibrates with it. It refuses (and says why) when it does not find a chessboard pattern, or when the pattern does not alternate light and dark; the corners can then be clicked by hand. White is taken to be on the side of the lighter pieces; **Swap sides** turns the labels when that is wrong.
- FR-3b. The picture can be turned by 90° in either direction (the calibration turns with it), and has auto contrast, brightness and contrast. Not during a game.
- FR-3c. Stabilization: during a game, while the board is quiet, the board is looked for again about every 1.5 s (less often on a slow machine); if it shifted by 0.04 to 0.45 of a square, the calibration moves along and the learned squares are kept. Over a whole game it may not move more than one square from where it began, and the file is written at most every 10 s. It can be switched off, and the choice is kept.

### Android app
- AR-1. The app shows the page itself (no browser) and uses the camera of the phone by default. The page is served on the loopback address (127.0.0.1:8080), which only the phone itself can reach (other apps on the phone included, which is why the page of the app and its bridge are not a secret from them); other devices on the network can open it only when the owner switches the remote server on (AR-2).
- AR-2. A switch lets other devices on the network open the page (remote server), and shows the addresses. It is off by default and kept between runs.
- AR-3. A switch uses an external USB camera, when Android lists one; the owner can also choose among the cameras of the phone. Neither can change during a game.
- AR-4. A foreground service keeps the server and the camera running with the screen off or another app in front, with a notification and a Stop button.

### Recording a game
- FR-4. The controller starts a new game. The standard position is the default; an optional FEN allows resuming a position.
- FR-5. Player names are optional and default to "White" and "Black".
- FR-6. The camera is opened for a game or for calibration and closed when idle. The exposure and white balance lock is applied on every open.
- FR-7. The backend infers each move from the images and the chess rules, per player turn, and records it in standard algebraic notation (SAN, for example `Nf3`).
- FR-8. Each move has a timestamp stored as **an offset in seconds from the start of the game**, not an absolute time.
  The game itself has a start date and time taken from the phone's browser, because the target may have no correct clock.
- FR-9. The live page shows a virtual board and the move list, updated by push.
- FR-10. Promotion: the page asks which piece, defaulting to queen.
- FR-11. End of game: checkmate and stalemate come from the rules. Resign, draw and abort are buttons. The result (1-0, 0-1, 1/2-1/2, or unfinished) is recorded.
- FR-12. Corrections (controller only): undo the last move, enter a move by hand, and choose from the candidates when the match is ambiguous.

### Past games
- FR-13. A list of games by date, and the player names and result of each.
- FR-14. Opening a game shows the virtual board and the move list.
- FR-15. Replay modes: in real time using the timestamps, as fast as possible, and one step per button press.
- FR-16. Any user, including viewers, can list and replay games and follow the live game.
- FR-17. A game can be exported as PGN.

### Storage and formats
- FR-18. One JSON file per game on the target is the source of truth: version, start date and time, names, starting FEN,
  moves (SAN, UCI, time offset), result and how it ended. Positions are recomputed from the moves.
- FR-19. PGN export is compatible with the standard (and with Lichess and chess.com imports, to be verified with a real import):
  the seven standard tags, `[FEN]` and `[SetUp "1"]` for a non-standard start, and the time spent on each move in a
  comment such as `{[%emt 0:00:12]}` (convention to verify).
- FR-20. The result and the player names are stored so game points can be counted later (1 / 0.5 / 0).
  Free-text names for now; a list of players later, to avoid typos splitting someone's score.

## Non-functional requirements

- NFR-1. Runs on the INEOS 600: Atom N270, 32-bit, single core, about 1 to 2 GB RAM. See [target-hardware.md](target-hardware.md).
- NFR-2. Fully offline. The interface uses no external fonts, scripts or images.
- NFR-3. Phone-friendly pages that also work on a tablet.
- NFR-4. A loss of power or a browser closing must not lose the game: moves are written to disk as they are recorded.
- NFR-5. Push to browsers with Server-Sent Events (agreed). The camera picture and the top-down board are pushed as Motion JPEG streams (`/frame.mjpg`, `/warp.mjpg`); the page only polls a small figure of speed.

## Out of scope for now

- A chess clock, saving camera snapshots per move, streaming video. (Engine analysis is under study, see below.)
- Recognising piece types by vision. Types come from the game state.
- Seeing captured pieces beside the board: possible later, so the warp keeps a margin around the board.
- A tournament feature that counts points automatically (the data to support it is stored).

## Under study (added 2026-11-04)

### Targets: the INEOS 600 or a laptop

The target may become a regular laptop, which has its own screen. What changes:

| | INEOS 600, headless (now) | Laptop |
|---|---|---|
| Screen | A phone or tablet through the Wi-Fi access point | The laptop screen, with the page opened locally. Phones can still join as viewers |
| Wi-Fi dongle and access point | Needed | Not needed (the laptop has Wi-Fi, and can share it as a hotspot) |
| Build | 32-bit, in a Debian 12 i386 container | Native 64-bit |
| Camera, resolution and speed | VGA or 720p, 3 fps, USB 2.0 | 1080p and more, 5 to 10 fps |
| Engine analysis | Slow, after the game only | Comfortable, even live |
| Operating system | Debian 12 i386 | Linux or Windows |

- **Linux laptop:** nothing changes in the program. Build natively (needs `clang`, `libclang-dev` and `linux-libc-dev` for the camera bindings).
  Run `nicochess --listen 127.0.0.1:8080 --open`: the laptop's own browser opens the page and, being the first to connect, is the controller.
- **Windows:** the server, the page, the tracker and the game store build and run on Windows, and since 2026-11-12 so does camera capture (Media Foundation, through the `nokhwa` crate, with a camera thread of its own). Tried on a PC's integrated webcam: the format list, 1280x720 MJPEG capture and the frame served by the page. Not yet tried: locking exposure and white balance, a real game.
- `--open` opens the default browser of the computer once the server is up; use F11 there for full screen.
- Decided (2026-11-04): if the target becomes a laptop, it runs Linux, the same distribution as the INEOS 600 (Debian). This was later overtaken: the Windows camera backend exists (2026-11-12) and a Windows package is built (`scripts\package-windows.ps1`), so a Windows laptop works too. A 64-bit Debian can be a newer release than 12, since the 32-bit limit only applies to the Atom.

### Engine analysis with Stockfish

A user asked whether Stockfish could run on the target for game analysis. It is feasible, with limits:

- **Builds.** Stockfish's official build instructions list 32-bit x86 targets (`x86-32`, `x86-32-sse2`, `x86-32-sse41-popcnt`), see the [Stockfish wiki](https://github.com/official-stockfish/Stockfish/wiki/Compiling-from-source).
  The Atom N270 has SSE2, SSE3 and SSSE3 but, to my knowledge, no SSE4.1 or popcnt: the setup script now reports those flags so the right build can be chosen.
  It also needs its NNUE network file (tens of megabytes, to check) next to the program or built in.
- **Speed on the Atom.** Not measured, but expect it to be slow: a single in-order 1.6 GHz core. It would suit analysis of a finished game with a limited depth or a time per move, not real-time analysis, and never while a game is recorded.
- **Speed on a laptop.** No problem, and live analysis during a game becomes possible.
- **How it would plug in.** NicoChess starts Stockfish as a separate program and talks to it with the UCI protocol (`--engine PATH`). An **Analyse** button on a finished game would evaluate each position in the background and store the result next to the game file.
  The replay page would show an evaluation graph, the best move, and the moves that lost a lot (inaccuracy, mistake, blunder), and the PGN export would add `[%eval ...]` comments, which Lichess understands.
- **Licence.** Stockfish is GPL v3. Running it as a separate program is fine. If its binary were ever distributed with NicoChess, its source must be offered too.
- Decided (2026-11-04): not now. Kept here as a design for later.

### Cameras

- The C170 stays the fallback. The Yealink UVC30 Room is the better candidate: see [target-hardware.md](target-hardware.md). The program already supports it as far as can be done without the device (NV12, autofocus off, zoom and focus options, `--probe`).

## Open questions

1. Real webcam capability: the C170 at 640x480 is marginal (see [design.md](design.md)). It will be judged on the real hardware, and may lead to a 720p camera.
2. PGN time comment: confirm that `[%emt]` imports correctly in Lichess and chess.com with a real import.
3. The controller role lifetime (kept for the whole game, 60 seconds idle outside a game, 30 minutes of silence ends a game's hold) is implemented as proposed. The owner has not confirmed it yet.

## Implementation status (updated 2026-11-13)

All of the requirements above are implemented and tested on a PC with a synthetic camera, including a whole game
recorded from the demo camera to a PGN, and the 32-bit Linux build running in a Debian 12 i386 container.
Not verified on the real thing: detection quality on a real board with any camera, the C170 and the Yealink, the INEOS 600, and the Android app on a phone.

| Step | Status |
|---|---|
| 1. Per-square detection, learned from the known start position | Done, tested with noise and a brightness shift. Real camera: not tried |
| 2. Stability gate and move tracker | Done. Follows the whole Opera game (33 plies, 17 moves), castling and captures included |
| 3. Game store and PGN | Done |
| 4. API and push events | Done (own HTTP server: ready-made small servers buffer the events) |
| 5. Interface | Done: live, calibration, games, replay, corrections |
| 6. Roles, names, results | Done |
| 7. Tuning on the target | To do. Tuning flags exist: `--stable-frames`, `--motion-threshold`, `--max-cost`, `--margin` |
| 8. Windows camera and Windows package | Done (2026-11-12 and 2026-11-13). Webcam tried on a PC; a real game not yet |
| 9. Picture settings, auto-detect of the board, stabilization (FR-3a to FR-3c) | Done, tested on synthetic pictures and, for the auto-detect, on a few photos of real boards. Not yet on a real game |
| 10. Android app: phone camera, remote server switch, camera choice, foreground service (AR-1 to AR-4) | Built and compiled. Not yet run on a phone |

Known limits:
- A game that was running when the power went off (or the program was stopped) is found at start and kept in the Idle state; the page offers **Resume** (the camera is opened and the squares are learned again from the position reached) or **Close it** (the game is ended as unfinished). The moves are on disk as they are recorded.
- Promotion is guessed as a queen, and the page offers the other pieces.
- Detection assumes a still camera. A small shift during a game (a knock, a phone slipping on its stand) is followed by the stabilization (FR-3c); after a larger move, calibrate again (the squares are learned again from the current position).
- Repetition and the 50-move rule are not detected: the game ends by checkmate, stalemate or insufficient material, or by a button.
- Two moves made between two still pictures (an instant reply) match no single legal move: the page stays unsure until the moves are entered by hand.
- The warp keeps a 25 pixel margin around the board for the future "captured pieces" idea; nothing uses it yet.
