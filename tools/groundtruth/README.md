# Ground truth from the pictures of a recording

Pilot of a way to read the real game from the pictures of a recording, independently of the tracker, so that the
tracker's decisions can be compared with it and re-tested after a change. Not part of the program: nothing here is
shipped, and `cargo build` does not build it (the two tools are Cargo examples, built on request).

The idea, in three steps (only the first two exist so far):

1. **Keyframes** (`keyframes`, no model, from the pictures only). Warp each picture of the first camera to the board seen
   from above with the calibration in force at that moment, find the stretches where the board is still, keep one picture
   per stretch when at least two squares differ from the last kept one.
2. **Reading** (a person, or a model that looks at the pictures, today a model asked in a session). For each keyframe:
   from the last confirmed position, which legal sequence of 1 to 4 moves does the picture show? `legal` lists the legal
   sequences and ranks them by the squares that changed; the picture decides. The result is the list of moves with the
   keyframe that shows each one: the ground truth of the game. A reading that fits no legal sequence is a doubt for a
   person, never a guess.
3. **Comparison** (not written yet). The ground truth against what the tracker decided (the `detect` lines of
   `events.jsonl`, or a fresh `--replay`): the first difference, moves missed, moves wrong, moves late, doubts that were
   unnecessary, each with the pictures. The same after every change to the tracker, until they match.

## Run

From the repository root (a recording is only read, never changed):

    cargo run --release --example keyframes -- "%LOCALAPPDATA%\NicoChess\recordings\2026-11-24_13-36-23" <output folder>
    cargo run --release --example legal -- "<fen>" --plies 4 --changed e2,e4,e5,e7 --top 5

`keyframes` writes into the output folder (keep it outside the repository):

- `kf000.jpg`, ...: the board from above, white at the bottom, a-file on the left, files and ranks written around it, 80
  pixels a square (`--px`). `--side` also writes `kf000_side.jpg`, the camera picture around the board as it was.
- `index.json`: for each keyframe the time, the source picture (file name in the recording), the tracker's status, how
  long the board had been still, the squares that changed since the previous keyframe (and the ten that changed most,
  with their numbers), the events between the two keyframes (`detect`, `unsure`, `manual`, ...), and the position the
  tracker believed in.
- `motion.csv`: time, tracker status and how much the board changed from the picture before, for every picture. It is
  what the stretches are cut with (`--still`, `--frames`); look at it when keyframes are missing or many.

What the numbers mean (pilot, one recording, 1280x720, contrast 1.55): the noise of the camera is 3 to 5 grey levels per
square; a hand over the board 9 and more; a black piece or a white piece on a dark square 25 to 100; a white piece on a
light square only 10 to 15, which is why a change of two squares at 9 is a keyframe and one square alone is not (a
reflection or a shadow).

The warp is the tracker's own (`Calibration` corners, a1 h1 h8 a8 in the pixels of the recorded picture; the recorded
pictures are already turned and adjusted), at 80 pixels a square instead of 50.

## What is known and not known

First numbers (recording 2026-11-24_13-36-23, the cleanest of the six: 2271 pictures of the first camera, 27 plies in the
saved game): 33 still stretches, 26 keyframes, so about one per ply or fewer (the moves made in a burst share one, which
a legal sequence of up to 4 moves then explains: `Be3 Bxe3 fxe3 O-O` came from one keyframe). The crops are clear
enough to read which squares changed and, for most pieces, what they are. The first 18 keyframes (start, then the first
21 plies) were read by a model in a session, each against the legal sequences from the previous position, and all 18
agreed with the saved game (14 with moves, 3 with no change, the start). That reading was not blind (the reader had
seen the saved moves), only one game was tried, and only its first 21 plies: it shows that the pictures are readable,
not yet how often a reading is right.

Things to know when using it:

- A move made while a hand stays over the board leaves no still stretch, so no keyframe: the next keyframe then
  explains several moves at once (with `--still 4 --frames 5` the last four plies of that recording, which were played
  without a pause, gave one keyframe; with the defaults they give two). A lower `--frames` or a higher `--still` finds
  shorter pauses, at the cost of more keyframes that show a hand.
- Keyframes that show no change (a light that moved, a hand resting beside the board) are normal; the reading says
  "no change".
- Piece types are read from the picture only when they are clear (rooks, kings, pawns and knights are; a bishop and a
  queen from above are less so). A piece of one colour replaced by another of the same colour on the same square (a
  recapture) changes no square's colour: only the picture and the rules show it.
- Moves that lead to the same position in a different order (`e3 e5 e4` and `e4 e5`) cannot be told apart by any
  picture: take the shortest and say so.
