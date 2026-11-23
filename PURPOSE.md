# Purpose

## What it is for

A chess player (or a club) plays on a real, physical board. NicoChess watches that board with a camera, works out every
move by itself, and records the game, so nobody has to write the moves down. The game can be followed live on a phone or
tablet, replayed later, and exported as PGN.

The idea that makes it practical: it does not try to recognize pieces from scratch. It starts from the known position and asks
"which legal move explains what changed on the board?". A cheap camera is then enough.

## Who uses it

- **The person who owns the setup** (the controller): sets the camera up, calibrates, starts and ends games, corrects mistakes.
- **Other people** (viewers): follow the live game, list and replay old games, export a PGN. They cannot change anything.
- **Later, maybe**: a club that counts points from the stored results (the data is kept for it; no feature yet).

## Scenarios

1. **Record a game on a real board.** The camera is fixed above the board. The controller opens the page, calibrates once (press
   **Auto-detect the board**, or click the four corners), presses **Start game**. The moves appear as they are played, each with the time
   spent. The game ends by checkmate, stalemate or insufficient material, or by a button (resign, draw, abort).
2. **Watch from a phone or tablet.** Any device on the network opens the same page and sees the virtual board and the move list update
   by themselves. The first device to connect is the controller; the others are viewers.
3. **Correct a mistake.** Undo the last move, type a move, pick among the candidate moves when the program is unsure, change a promotion
   (promotion is guessed as a queen), or learn the squares again.
4. **Replay a past game.** In real time using the timestamps, as fast as possible, or one step per press. Any viewer can do it.
5. **Export.** A game downloads as PGN, with the time spent on each move in an `[%emt h:mm:ss]` comment.
6. **Try it without a camera.** A demo source plays a whole game (Morphy's "Opera game") on a drawn board.

## What each target offers

| Target | What you get |
|---|---|
| **Windows PC** | A zip to unzip and double-click (`Start NicoChess.bat`), nothing to install. Uses a USB or integrated webcam. The page opens in the PC's browser; phones on the network can join as viewers |
| **Linux headless box** (the original target: an INEOS 600 mini PC, Atom N270, 32-bit, Debian 12) | A service started at boot, a Wi-Fi access point of its own (USB dongle), no screen: a phone or tablet joins that Wi-Fi and opens the page. Installed from a USB key |
| **Android app** | The phone is both the camera and the screen: the server runs inside the app and the page is shown in it. Optional: let other devices connect (remote server), use an external USB camera, choose among the phone's cameras. Keeps recording with the screen off |

## Features

- **Calibration**: click the four corners (a1, h1, h8, a8), or **Auto-detect the board** (finds the 49 inner corners of the squares; **Swap sides** if a1 landed at the wrong dark corner).
- **Picture settings**: turn by 90 degrees, auto contrast, brightness and contrast (done in software, same on every camera). Not during a game.
- **Stabilization**: during a game the calibration follows the board when the camera or the board shifts a little (switchable).
- **Move detection**: stability gate (no hand, a few identical pictures), per-square empty/white/black reading learned from the known position, legal-move matching with castling, en passant and promotion.
- **Doubt**: when two moves look nearly alike, the program takes the likelier one if it leads a little (`--soft-margin`) instead of asking, and says so on the page: "Not completely sure about d4 (78% sure)", with the other likely moves and their chances as one-click buttons, and an Undo that takes the move back and offers the choice. When no move is likely enough, it asks as before, now with a chance next to each move.
- **Roles**: controller and viewers, by a token kept in the browser; the controller role is kept while a game runs, released by a button, and expires when the controller goes silent.
- **Live page**: virtual board (drawn with its own pieces: white cream with a dark outline, black near black with a light outline, a1 a dark square), move list with times, camera picture and top-down board as Motion JPEG streams, push by server-sent events.
- **Recording for investigation** (`--record`, any target that has a command line; the Windows package has a launcher
  for it): for every game, a folder with still pictures of what the detection saw, what it decided and what the
  people did, and `--replay` to run it through the detection again, also with other tuning values. For finding out
  why a move was missed in a test session and for tuning on real pictures; it is not a video of the game.
- **Chess clock (optional)**: when a game is started, the time for each player (minutes) and an extra time added after each move (seconds) can be set, with presets (1+0, 3+2, 5+0, 10+0, 15+10). The clocks count down on every page; the player whose time runs is highlighted and goes red when little is left. The clock starts with the first press or the first move that is seen (the first move costs no time), and a move that is seen adds the extra time to the player who made it and gives the time to the other one. The controller presses the big button or the space bar when moving: that hands the time over at once, without waiting for the camera (a move seen later changes nothing but the extra time, and a press by mistake is mended by the next move). A pause button stops both clocks (moves seen during a pause change no time). When a time runs out the other player wins ("time forfeit"), or the game is a draw when that player could not checkmate with any moves. The time left after each move is kept in the game, shown in the replay, and written to the PGN (`[%clk]`, and a `TimeControl` tag). Taking a move back takes the clocks back.
- **Games**: one JSON file per game, a list by date, replay, PGN export, resume of an unfinished game after a restart.
- **Two cameras (optional)**: a second camera on the opposite side of the board. What the two pictures say is put together square by square, each camera counting as far as it can explain what it sees on that square, so a hand, an arm or a tall piece that hides squares from one is made up for by the other. Each camera has its own calibration and picture settings. The game goes on with one if the other stops; one camera works exactly as before. The second camera is chosen on the page (Windows) or with `--device2`; not on Android.
- **Camera choice**: on Windows the page lists the cameras of the computer (Calibrate > Camera) and switches to the one
  picked, not during a game; the choice is kept by the camera's name. On Android: see above, with the remote server.

## Non-goals

- No recognition of piece types by vision (types come from the game state).
- No engine analysis for now (Stockfish is a design for later, see `docs/requirements.md`).
- No HTTPS, no accounts, no login: the network (the Wi-Fi passphrase, or the phone itself) is the only protection.
- No video recording of games to watch (the debugging recording keeps still pictures), no tournament scoring. The chess clock is optional: a game without one only shows how long each player has been thinking.
- Not for the Internet: do not expose the port outside a trusted network.

## Known limits

- Draws by repetition and by the 50-move rule follow the FIDE rules: fivefold repetition and the 75-move rule end the game by themselves; threefold repetition and the 50-move rule only show a "Claim the draw" button, because a player has to claim them.
- Up to three moves made between two still pictures (an instant reply, or a moment nobody was looking) are recognised when the board matches that sequence clearly better than any shorter one and any other sequence; otherwise the page stays unsure, offers the likeliest explanations, and the moves can be entered by hand ("e4 e5", up to three). Since the camera only sees empty, white or black squares, two different sequences that leave the same squares occupied by the same colours cannot be told apart: such a case stays unsure, or the first one is taken, in an order the board cannot confirm (the page marks moves that were seen together).
- Fast games (a move every second or two, hands over the board between moves) are followed as long as the board is still for a fifth of a second now and then. A hand that rests on a piece it has just moved, or hovers over one it is about to lift, hides that square: the tracker waits for the hand to go, and does not learn the hand as the look of the square. If the game runs more than three moves ahead of the tracker (no still moment for a long time), a longer search (up to eight moves) takes over once the board has been still for half a second, and the moves it finds are flagged as doubtful. Measured on synthetic pictures only, with hands that are plain discs: no real blitz game has been tried yet.
- The look of each square is learned and keeps adapting to a light that fades, warms or moves. After a move typed by hand the next still picture is adopted as the look of that position, so tracking resumes by itself. Very dark pictures with strong glare, or a heavy-noise camera, can still make the program unsure or, rarely, wrong; the page shows what was recorded, and a wrong move is undone by hand. All of this is measured on synthetic pictures only.
- The chess clock: the time of a move that the camera sees is the time the board became still, a little after the move, so a player who does not press the clock is charged for that moment; two or three moves seen together are all charged to the time before them (press the clock to avoid it). The clock thread looks about four times a second, so a time that runs out ends the game within a fraction of a second. Only tried with the demo source and in tests, not with players at a real board.
- Promotion is guessed as a queen; the page offers the other pieces.
- Two cameras: what they gain is measured on synthetic pictures only (see `examples/stress.rs`); the two pictures are not synchronised (each is the latest of its camera), and two high-resolution streams may not fit the USB bandwidth of one hub. The cameras are expected on opposite sides; two on the same side add little.
- Detection quality on a real board, with real lighting and real pieces, is **not yet tested** with any camera. The auto-detect of the board was only tried on a few photos found on the web, and the stabilization only on synthetic pictures.
- The Android app has been built but not yet run on a phone since the service and the settings were added; a USB camera works only on phones whose Android lists it to apps.
- The Linux target (INEOS 600) has never run the program with the real camera.
