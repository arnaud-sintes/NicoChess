# NicoChess

Monitor a real chessboard with a USB webcam and track the game automatically.

## Goal

An old webcam on a tripod looks at the board at about 45° vertical tilt and about 20° lateral offset.
Software on a PC captures frames and works out the position and the moves played.
Implementation language: Rust if possible.

The target machine runs headless. A USB Wi-Fi dongle will expose its own access point, so a phone or tablet can join it, open a simple HTTP page, start a game and see the board state.

## Documents

- [Design](docs/design.md): feasibility, approach, pipeline, Rust stack, milestones.
- [Plan](PLAN.md): what to install and configure on the target, and how.
- [Target hardware](docs/target-hardware.md): INEOS 600 (Atom N270) constraints, Linux choice, checks to run.

## Status

Discussion and design only, no code yet.
Next step: scaffold the Rust project (headless capture, calibration, warp preview over HTTP).
