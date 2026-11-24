# Changelog

The version is the one in `Cargo.toml` (`[workspace.package]`), shown in the page, by `nicochess --version`,
and as the version of the Android app. Each release is a tag `vX.Y.Z` on GitHub with the packages attached
(see `scripts/release.ps1`). Versions follow semantic versioning while the project is below 1.0: a new
minor number may change behaviour, a patch number only fixes things.

## 0.11.0

**Licence.** NicoChess is under the GNU General Public License, version 3 or later (file `LICENSE`, `license` in both
`Cargo.toml` files). The chess rules come from the `shakmaty` crate, which has that licence, so the program has to have
it too. Every package of a release (the Windows zip, the Android zip, the Linux tarball) now carries the licence text.
The statements that the Windows package is under 1 MB were out of date (it is about 1.2 MB) and are corrected.

**Pieces taken, and promotion by drag and drop.** Both are on the page only; the detection is not changed.

- The pieces each player has taken stand on the two sides of the board, in the live game and in a replay: White's on the
  left, Black's on the right, the strongest first, with the lead in points (pawn 1, knight and bishop 3, rook 5, queen 9)
  written under the player who is ahead. The server says which piece each move took (`took` in the moves of the game),
  from the rules, so a pawn taken en passant and a piece taken on the promotion square are counted. The lead is counted
  from the pieces on the board, so a promotion counts for what it adds.
- The piece of a promotion is chosen from four icons, in the colour of the player, instead of text buttons: tap one, or
  drag it onto the blue square. It works the same when the camera saw the promotion and took a queen for it. A piece
  dropped anywhere else chooses nothing.
- Not tried with a real board or on a phone: checked in a browser on a staged game and on pointer events only.

**Times of moves seen together, and a recording of every picture.**

- When two or three moves are seen in one picture (fast play), their times were all the moment the board became still,
  so the time the players took went to the last move and the others got none. They are now spread evenly between the
  move before them and that moment (an estimate: the camera cannot see more). At the very start of a game, where the
  clock starts with the first move, they are all at 0 as before. The move list, the thinking times and the PGN take
  the new times.
- `--record-all` (instead of `--record`) keeps every picture the detection looks at, also while the board is quiet and
  not only a few a second while something happens, with a longer queue for the disk. The Windows package has
  "Start NicoChess (record all).bat" for it (25 pictures a second). It takes several megabytes a second: for a blitz
  game that has to be replayed picture by picture. `--record-all` with `--record-every` is refused.

**A new look for the page, made for the screen it is on.** The page was drawn again from the ground up; it does
everything it did, and the server is not changed.

- **Three layouts.** On a phone held upright the board takes the width, with a player and a clock above and below it,
  the status under it, and the three sections on a bar at the bottom. On a phone held sideways the board is as high as
  the screen, the sections are on a rail at the left and everything else is beside the board. On a tablet or a computer
  that is wider than high the board is on the left, as large as the height allows (and no larger than is sensible on a
  very large screen), with the clock button, the moves and the controls beside it. Nothing scrolls sideways from
  320 px wide.
- **The players and their clocks** are two lines, above and below the board as the players sit (they change places when
  the board is turned). The clock whose time runs is filled in, and goes red when little time is left.
- **The status** ("Watching the board", "Something is moving", "Not sure which move was played") has a colour for each
  state, and the header says whether the program is recording, calibrating, idle, or cannot be reached. The questions
  of the program (which move, a doubtful move, the piece of a promotion) come at the top of the panel, so they are on
  the screen without scrolling.
- **The moves** are a table with the times, and on a phone one line that scrolls sideways until "All moves" is pressed.
  A time that is an estimate (moves seen in one picture) is written with a "~" and says why.
- **Calibrate** shows the camera picture first, with the buttons and the next step above it (the four corners are
  ticked off as they are marked), and one folding section for each group of settings: Picture, Cameras, Depth camera,
  Speed and alignment, This phone. Each says its state in its title line. On a phone only Picture starts open.
- **Games** on a phone shows the list, then the game in its place (a Games button goes back); on a wide screen both.
  The replay has the same board, strips and clocks as the live game; the arrow keys, Home and End step through it.
- **Small things.** The role (Controller, Viewer) is a button: it opens a panel with what the role means, Release
  control, the theme (as the device is set, light, dark; kept on the device) and the version. "Take control" is a line
  across the top when nobody controls. A button beside the status shows only the board and the clocks (Escape, or the
  button again, brings the rest back), for a screen propped up beside the board. A piece that has just moved slides to
  its square. Errors of a command show for a few seconds at the bottom instead of replacing the notice at the top.
  Squares a piece may go to are dots, pieces it may take have a ring.
- **For every hand and eye.** Anything pressed is at least 44 px high on a touch screen, the keyboard reaches every
  control and shows where it is, colours keep a contrast of 4.5 to 1 in both themes, switches replace tick boxes,
  movement is short and stops for people who asked their device for less of it, and the notch and the home bar of a
  phone are left free.
- On a computer screen of 1280x800 the whole Controls card now fits beside the board of a clocked game (the list of
  moves is shorter there), and on the smallest phones held sideways (320 to 360 px high) the board, the clocks and the
  clock button fit on the screen with the header and the players made lower.
- The page is 144 KB (87 KB before), about 14 KB more in the Windows package. It was not made smaller: what could be
  taken out without cutting comments or features is under 2 KB.
- Not tried on a real phone or tablet, nor with a real camera: checked in a desktop browser made to behave as a phone
  (sizes from 320 px, touch and mouse events, both themes), on the demo and on staged states.

## 0.10.1

**Leaning pieces no longer block the game.** Four recordings of real games (three blitz, one slow) showed the tracker
stuck for good, also in the slow game. The camera looks at the board from the side and low, so the head of a tall
piece reaches over the square behind it: an empty square then looks occupied, and the single-square limit refused
the right move (a bishop to b3 was the likeliest move at 100% and was refused because a3 looked white).

- The tracker now knows, from the calibration, which neighbours of a square lie below it in the picture (where a piece
  can lean over it from). An empty square next to such a piece may show its colour, for the limits on what may be left
  unexplained. With a depth camera this is only believed where the height picture sees the square empty (it sees the
  foot of a piece), and then it also counts for which move is the likeliest. With colour alone it is believed at a
  higher price and never changes which move is the likeliest, because colour cannot tell a lean from a piece of the
  same colour that has just come there.
- A depth camera that disagrees with the colour pictures about a leaning square is no longer thrown out because of it.
- When the tracker has been unsure for a second, the board has been still as long, one explanation is clear and the only
  odd squares are leaned over, it takes it and allows what those squares misfit from then on; the models do not learn the
  lean as the look of an empty square.
- `--replay` also prints the position the replay ends in, to compare with the last picture of a recording.

On the four recordings (replayed, not played live): the slow game is now followed to the end and its final position
matches the last picture; the longest blitz game recovers more moves; the two other blitz games are not better (moves
played faster than the tracker can follow, hands over the board). Not tried live.

## 0.10.0

**A chess clock, if you want one.** A tester asked for an integrated timer with a time limit per player, an extra
time, a button or the space bar to switch, and a pause.

- **New game**: the time for each player (minutes) and the extra time added after each move (seconds), with presets
  (1+0, 3+2, 5+0, 10+0, 15+10). Leave the minutes empty for a game without a clock, which works as before. The choice
  is remembered by the page. API: `minutes` and `extra_seconds` in `POST /api/game/start`.
- **The clocks count down on every page** (the player whose time runs is highlighted, red when little is left, tenths
  of a second below ten seconds). The controller has a big button and the space bar (`POST /api/game/clock`,
  `switch`): the first press starts the clock for the player to move, the next ones hand the time over at once, and
  after a pause it goes on. There is a pause button (`pause`, `resume`).
- **The camera and the clock agree.** A move that is seen adds the extra time to the player who made it and gives the
  time to the other one, whatever was pressed: so a clock that nobody presses follows the game by itself (a move seen
  before the first press starts the clock and costs no time), a press before the move is seen changes nothing more, and
  a press by mistake is mended by the next move. Moves seen during a pause change no time.
- **A time that runs out ends the game** (a thread, `clock`, looks about four times a second): the other player wins,
  "time forfeit", or the game is a draw when that player could not checkmate with any moves. Undo reopens such a
  game, like a game ended by checkmate.
- **Kept with the game**: each move keeps the time both players had left (`left` in the game file; old files and games
  without a clock are unchanged), so taking a move back, resuming a game after a restart and the replay all have the
  times. The PGN gets a `TimeControl` tag and `[%clk h:mm:ss]` next to the time spent on each move, and the
  termination "time forfeit".
- Tested with a new module of clock rules (`src/clock.rs`, 13 tests with made-up times), the engine (a time running out,
  a draw instead of a loss, moves keeping the time left), the API, and the demo source through the real server.

**Limits.** The time of a move that the camera sees is the time the board became still, a little after the move, and two
or three moves seen together are charged to the time before them: press the clock (or the space bar) when moving to avoid
both. Only tried with the demo source and in tests; no players at a real board yet.

## 0.9.0

**Fast games.** A tester reported that the program works at normal speed and "does not work at all" in blitz. Three
independent audits (Claude Sonnet, Opus and Fable) found the same causes, and a new simulation of whole games with a
hand that reaches, hovers, rests and leaves (`src/tracker/blitz_tests.rs`, 25 pictures a second on a fake clock)
reproduced them: at the old settings the tracker was lost at a pause of 0.2 s between the hands, and for good as soon
as a hand rested on a piece it had moved, even in a slow game. Processing speed was not the cause (0.4 to 0.8 ms per
picture, 30 to 70 ms for the three-move search).

- **The board is still after 0.2 s, not 0.5 s** (`stable_time` 0.35 to 0.2, `motion_span` 0.15 to 0.1; new option
  `--motion-span`). Recordings made before keep their own values.
- **A hand over a square is no longer taken for a piece.** A square that no content explains and that has changed since
  the last confirmed picture counts the same for every explanation, so that a hand that rests on a white piece (which
  looks more like an empty square) no longer makes "the piece moved away" the likeliest explanation. Such a square is
  not learned from, either: before, a move accepted while a hand rested on it taught the models the colour of the hand,
  and every correct explanation was then rejected for the rest of the game. When the unclear part is a hidden square
  the tracker waits for the hand to go and says nothing, instead of showing "unsure".
- **A tracker that is far behind finds its way back.** More than three moves behind used to be final (only a click
  helped). Now, once the board has been still for half a second while unsure, a beam search of up to eight moves
  looks for the sequence, and takes it only when it is much clearer than anything else, flagged as doubtful. The order
  of the moves of one side cannot be known from a board, so it may differ from the order played.
- **One move is not taken when two were played** and a pair explains the picture clearly better.
- Measured on the synthetic stress test (`examples/stress.rs`) the numbers do not get worse: better in three of six
  conditions (hard 96 to 97%, dark 91 to 93%, noisy 96 to 97%), 99 to 98% in drift (4 moves typed, was 2), catch-up
  unchanged at 94% (2 wrong, was 1).

**Limits.** With no moment at all between two hands (a move every second with the hands never off the board) the board
is never still, and the tracker only catches up in bursts, with the order of moves wrong. Only simulated hands (discs)
were tried: no real board, camera, shadow or sleeve. The covered-square threshold (12 nats) and the 0.2 s are untuned on
real pictures; `--stable-time` and `--motion-span` change the timing, and `--record` then `--replay` shows what a
change does to a real game.

## 0.8.0

The first full release since 0.6.0: it holds everything of the test builds 0.7.0 and 0.7.1 (the faster page, the
clocks, dragging pieces, the board turn, the undo that holds, the depth camera and its settings), and the result of an
audit for reliability and performance. Three independent reviewers (Claude Sonnet, Opus and Fable, each reading the
whole code on its own) were given the same brief; what two or three of them found alike was checked in the code and
fixed, and the claims about speed were measured before anything was changed for them.

**Fixed**
- **One picture that cannot be decoded closed the camera.** A truncated picture is routine on a busy USB bus; it ended
  the camera thread, and the engine then closed the camera, waited 2 s and opened it again: several seconds without
  pictures in the middle of a game. Such a picture is now skipped, and only ten in a row fail the camera (Windows and
  Linux). Decoded MJPEG pictures are also no longer copied once more each.
- **A game that could not be written was forgotten.** If the disk refused for a moment (full, or a file held by a
  scanner), the game was dropped and written again only at the next move. It is now kept and written again, after a
  pause that grows from 1 to 10 s, and the page shows the notice until it is written.
- **The depth camera read could block for ever** (Media Foundation has no time limit), so that ending the game, unticking
  the box or changing the settings never took effect. It is read on a thread of its own and waited for 5 s at most, as
  the colour camera is. Its shutdown also happened in the wrong order (Media Foundation before the objects were let go
  of); the source is now shut down properly.
- **A camera driver that hangs left one more stuck thread every few seconds.** A camera (and the depth camera) is not opened
  while the thread of the last opening is still there; the page says the driver did not answer.
- **Listing the cameras could take the depth away in the middle of a game.** The page lists them every 5 s on the Calibrate
  tab; a listing that missed the depth part for a moment (the camera renumbering on the USB bus) cleared what was found
  about the board. During a game the depth is kept; outside one the list is believed, and the depth thread is woken when
  it changes.
- **Two writers of one file shared one temporary file** (a calibration set by hand and one moved by the board following):
  they could mix their bytes or fail the rename. Each write has its own temporary file now, removed if the write fails.
- **A calibration file that cannot be used** (corners not numbers, all on one spot) left the program "calibrated" while the
  tracker had no squares; it is now ignored with a message, as an unreadable file already was.
- **Threads that could end for good on a panic**: reading a picture is guarded like the processing of it, the depth thread
  starts again, the saver goes on. A picture that panics every time prints one line, not 25 a second.
- **Recordings no longer grow without limit** while the tracker stays in one status other than "watching": after 20 s the
  pictures come at the slow pace of `--record-every`.
- Smaller: `--stable-time` (or the file of a recording) with an infinite or absurd value panicked; `/depth.jpg` of a
  picture with no reading panicked in debug builds; the accept loop spun at full speed on an error that did not go away.

**Looked at, measured, and not changed.** The reviewers estimated tens of milliseconds for the search over three moves
(under the engine lock), for the game being replayed on every page update, and for the height picture of the depth
camera. Measured in a release build on this PC: 4.6 ms at the most for a picture that needs the three-move search, 0.1 ms
for a game of 150 moves, 2.6 ms for a height picture at 848x480. Nothing to gain at those costs, and the search is the
heart of the detection: it was left alone. On the 32-bit Atom they would be several times more.

**Known, not fixed**
- A camera that goes away, or pictures that are late, just as a move is made can leave the tracker behind the game; it
  then explains the board with wrong sequences of three moves. The whole-game test of two cameras stalls in about one run
  in ten for what looks like the same reason (it also did before this version).
- The games list reads every game file each time it is asked (slow with thousands of games, on a small machine); the
  preview stream of the camera is compressed at the full rate of the camera; a picture stream whose page left is only
  ended when pictures flow again.
- The depth reader was rewritten (reader thread, time limit, shutdown) and checked on its failure paths and by tests, but
  **not on live pictures of the D415 for this release: it was unplugged**. The version before was.

## 0.7.1

- **Depth settings on the Calibrate tab** (under "Also use the depth of this camera"): the nearest and the farthest
  distance that count (what is beyond the board, screens and walls, no longer confuses the search for the board
  plane, and the picture shows what is kept), the size and the speed of the depth picture (the sizes and speeds that
  the camera says it has: 424x240 to 1280x720, 6 to 30 a second), and how much of the picture has depth. The distances
  work at once; another size or speed opens the camera again and the board has to be found again. Not during a game.
  Kept in `depth.json`. Not offered: the camera's own controls (laser power, exposure, presets), which need the
  maker's library or its extension unit.
- **Tests:** the test build is optimised (`[profile.test]`), so the whole suite takes 25 s instead of two to four minutes
  and no longer times out on a small CI machine. The tests that type moves by hand use a tracker that never decides on its
  own (the synthetic board shows by chance the position that a shuffle of knights returns to). The test of a camera that
  goes away ends it in the middle of a move's still pictures; the engine's wait for a silent camera is a setting
  (`EngineConfig::fresh`, 2 s). Seen in that test: a camera that goes away *just as a move is made* can leave the tracker
  behind, and it then explains the board with wrong sequences of three moves. Not fixed yet.

## 0.7.0

**A page that follows the game as it is played, and more pictures a second.**

- **Pictures per second.** Asking the camera for 25 or 30 pictures a second used to give 15: a picture was kept only
  once a whole interval had passed since the last kept one, so a 30 pictures a second camera lost every other picture.
  The pictures to keep now follow a schedule, and 25 give 25 (measured on the D415: 3, 10, 25 and 30 asked, 3.0, 10.3,
  25.1 and 29.4 delivered, at about 18% of one core at 25). The default is 25 on Windows (3 elsewhere, for small boxes),
  there is a slider on the Calibrate tab (kept in `rate.json`), and `--fps` still sets it.
- **The board is still for a time, not for a number of pictures.** `stable_time` (`--stable-time`, 0.35 s) says how long
  the board must have been still, and a picture is compared with the one 0.15 s before it (`motion_span`), so a hand
  that creeps in is noticed at 25 pictures a second too, and a move is noticed as soon at 25 as at 3. On the demo
  game, the delay from a move on the board to the page being told went from 1.33 s (3 a second) to 0.74 s (25).
- **Following a board that moved is off the camera thread.** Looking for the board (which takes a while on a small
  machine) and writing the moved calibration now happen on a thread of their own, so that the pictures that follow
  are not late or stale. The tracker itself still runs on the picture thread, under the engine's lock: on a clear game
  it takes about 0.4 ms a picture (3 ms at the most); the hard case of three moves between two pictures was not timed.
- **The pages are told first.** The game used to be written to the disk, with a flush, before the pages were told, on
  the thread that reads the camera: a slow disk (a synchronised or scanned folder) made the page late and the camera
  thread late. The pages are told at once and a thread of its own writes the game; the pages ask for a change of the
  event stream 15 ms after the last one instead of 80.
- **The clock starts with the first move**, which is at 0 (it used to start when the game was started on the page).
  The page shows how long each player has been thinking, and the clock of the player to move runs; the replay shows the
  same up to the move shown. In the PGN the first move has `[%emt 0:00:00]`.
- **Undo.** The board still shows the move that was taken back, so the tracker used to find it again, or to learn the
  look of the squares from it. After an Undo nothing is decided and nothing is learned until the board shows the
  position again (the tracker then goes on by itself) or the move that was played is entered; meanwhile the moves the
  picture suggests, except the one taken back, are offered. New status `undone`.
- **Moving the pieces by hand on the board.** Drag a piece to its square, or tap it and then the square: the squares it
  may go to are shown (`GET /api/game/legal`), and a pawn that reaches the last rank asks what it becomes. Typing a move
  still works.
- **A button turns the board** a quarter at a time (kept in the browser), on the live board and in the replay.
- **The highlight of the last move** also shows the rook after castling and all the moves that were seen in one picture
  (`together` on the move, in the game file).
- Recordings keep a picture every 150 ms at most while something is going on (`busy_every`), not all of the 25 a
  second.
- **Depth camera (Windows, optional).** A camera with a depth part, such as the Intel RealSense D415, shows up as one
  camera with a box "Also use the depth of this camera". The depth is read through Media Foundation, turned into a
  picture of how far everything stands above the board, and put together with the colour pictures: it tells squares
  with a piece from empty ones, whatever the light, and stands in where a hand hides a square from the colour camera.
  `--source demo --depth-camera` shows it without hardware. Tried on synthetic boards, and on live depth pictures with
  synthetic pieces added; **not yet on a real board with pieces**.

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
