# Design

## Feasibility

The project is realistic and an old webcam is probably enough.
The condition is to track the game instead of recognizing pieces from scratch on every frame.

## Webcam adequacy

- 720p is comfortable, 640×480 is marginal.
- With the board filling the frame at 720p, a square is about 60 px at the near side and about 40 px at the far side.
- That is enough to tell empty from occupied and white from black.
- A 45° tilt with a 20° lateral offset is fine, because a homography from the 4 board corners corrects the board plane.

## Real difficulties

- Tall pieces (king, queen) hide the square behind them.
- The player's hand occludes the board during a move.
- Glare on a glossy board, and auto-exposure or white-balance drift.
- Telling piece types apart (bishop vs pawn) from a low angle with a cheap sensor.

## Recommended approach

Do not ask "what piece is on each square?".
Ask "which legal move explains what changed?".

1. Start from the known initial position, or a FEN entered by the user.
2. Per square, detect only empty / white piece / black piece.
3. Wait until the image is stable (no hand, a few identical frames), then compare with the previous stable state.
4. Score every legal move from the current position against the observed occupancy and color changes, and pick the best match.
5. Castling, en passant and captures fall out naturally from the legal-move set.
6. Piece types come from the game state, not from vision.
7. If the tracker loses sync, the user re-enters the position or corrects it manually.
8. Promotion is the only ambiguity; default to queen with a prompt.

A full ML piece classifier (CNN or YOLO, as in the chesscog research project) is possible later.
It needs a labeled dataset for this board, lighting and angle.

## Pipeline

1. Capture frames, locking auto-exposure, white balance and focus if the driver allows it.
2. Calibrate once (fixed tripod): click the 4 board corners, compute the homography, save it.
3. Warp to a top-down 8×8 grid.
4. Sample each square from its center-to-base region, because tall pieces lean into neighbors in the warped image.
5. Compute per-square features against a reference image of the empty board, then classify color.
6. Apply a stability gate: large frame difference means a hand is present, then debounce.
7. Infer the move with a rules library.
8. Output: board display and PGN, optionally feed an engine.

Auto-detecting the corners of the board is done from the 49 inner corners of the squares (saddle points), not from the outline of the board, so pieces on the board do not stop it as long as about half of those corners stay visible. Manual 4-click calibration stays available when it fails.

## Rust stack

| Need | Crate |
|---|---|
| Camera capture | Linux: `v4l` (V4L2), used directly. Windows: `nokhwa` (Media Foundation), on a camera thread of its own, with frames decoded in the program. Android: the app captures with Camera2 and pushes the pictures in (`src/pushed.rs`) |
| Image processing and homography | `imageproc` + `image` (pure Rust), or `opencv` (needs an OpenCV install) |
| Chess rules, legal moves, FEN/PGN | `shakmaty` |
| UI and preview | The program's own small synchronous web server (`src/http.rs`, thread per connection) on the target machine. `tiny_http` and `axum` were considered; ready-made small servers buffer the event streams, and `axum` is heavier for a single-core Atom |
| Optional ML later | `ort` (ONNX Runtime) or `tract` |

`imageproc` provides `Projection::from_control_points` and `warp`, so the pure-Rust route covers the homography.
Pure Rust was enough, including automatic corner detection (`src/boarddetect.rs`), so `opencv` was never added.

## Setup tips

- Use diffuse, constant lighting and a matte board, since glare is the main enemy.
- Choose pieces with strong color contrast against the squares.
- A higher camera makes occupancy easier but piece shapes harder, and 45° is a good compromise.
- Keep camera and board fixed. During a game the calibration follows a small shift (a knock to the camera), see "Stabilization" in requirements.md; after a larger move, calibrate again.

## Milestones

1. Live camera preview (`v4l`).
2. Click 4 corners, warp, and display the top-down grid.
3. Occupancy and color detection with an overlay.
4. Stability gate and legal-move matching with `shakmaty`.
5. Polish: error recovery, manual correction, PGN export.
