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
- NFR-5. Push to browsers with Server-Sent Events (agreed). The camera preview during calibration is polled.

## Out of scope for now

- Engine analysis, a chess clock, saving camera snapshots per move, streaming video.
- Recognising piece types by vision. Types come from the game state.
- Seeing captured pieces beside the board: possible later, so the warp keeps a margin around the board.
- A tournament feature that counts points automatically (the data to support it is stored).

## Open questions

1. Real webcam capability: the C170 at 640x480 is marginal (see [design.md](design.md)). It will be judged on the real hardware, and may lead to a 720p camera.
2. PGN time comment: confirm that `[%emt]` imports correctly in Lichess and chess.com with a real import.
3. The controller role lifetime (kept for the whole game, 60 seconds idle outside a game, 30 minutes of silence ends a game's hold) is implemented as proposed. The owner has not confirmed it yet.

## Implementation status (2026-11-04)

All of the requirements above are implemented and tested on a PC with a synthetic camera, including a whole game
recorded from the demo camera to a PGN, and the 32-bit Linux build running in a Debian 12 i386 container.
Not verified on the real thing: the webcam itself (formats, exposure lock, detection quality) and the INEOS 600.

| Step | Status |
|---|---|
| 1. Per-square detection, learned from the known start position | Done, tested with noise and a brightness shift. Real camera: not tried |
| 2. Stability gate and move tracker | Done. Follows the whole 33-move Opera game, castling and captures included |
| 3. Game store and PGN | Done |
| 4. API and push events | Done (own HTTP server: ready-made small servers buffer the events) |
| 5. Interface | Done: live, calibration, games, replay, corrections |
| 6. Roles, names, results | Done |
| 7. Tuning on the target | To do. Tuning flags exist: `--stable-frames`, `--motion-threshold`, `--max-cost`, `--margin` |

Known limits:
- A game that was running when the power went off stays in the list as "in progress" and is not resumed.
- Promotion is guessed as a queen, and the page offers the other pieces.
- Detection assumes a still camera. After moving it, calibrate again (the squares are learned again from the current position).
- The warp keeps a 25 pixel margin around the board for the future "captured pieces" idea; nothing uses it yet.
