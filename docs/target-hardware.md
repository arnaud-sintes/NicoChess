# Target hardware

Target machine: INEOS 600 mini PC, Intel Atom N270 1.6 GHz.
Product page: https://www.zoneindustrie.com/actualite/INEOS-600-un-nouveau-mini-PC-pour-l-integration-7570.html
The page was not opened during the discussion; the analysis relies on the CPU stated by the user.
RAM and storage of the actual unit are still to be verified.

## Constraints

- The N270 is 32-bit only (no x86-64).
- Windows 11 is impossible and Windows 10 32-bit is past end of support.
- Rust targets `i686-*`, which is well supported, but every dependency must exist in 32-bit form.
- Single core, in-order, roughly 5 to 10 times slower than a modern laptop core.
- RAM is probably 1 to 2 GB (chipset cap is 2 GB).
- Graphics are GMA 945 class, so `egui`/`eframe` and other GL UIs will likely not work well.
- USB 2.0 only, which is fine for a webcam.

## Verdict

Enough if the design is event-driven and lean.

- 640×480 or 720p at 2 to 5 fps is plenty, since chess moves are slow.
- Warp only the board region to a small grid (for example 256×256 or 400×400), not the full frame.
- Legal-move matching with `shakmaty` is trivial in CPU terms.
- The stability gate means full analysis runs only when the board settles.

## Things to avoid on this machine

- ML piece classification (ONNX, YOLO).
- MJPEG decoding at 720p, the most expensive step; prefer YUYV or a lower resolution.
- Building on the device; cross-compile from the main PC.
- `opencv`; use pure-Rust `imageproc`.
- A local GUI; run headless and expose a small web page (`tiny_http`) or terminal output, and view it from another machine.

## Operating system

Linux is better than Windows: lower overhead, good V4L2 camera access (the `v4l` crate), easy headless use over SSH.
The distribution must still ship 32-bit x86.

1. Debian 12 "Bookworm" (i386), recommended.
   - One of the last mainstream releases with a full 32-bit installer and kernel.
   - Install without a desktop, SSH only.
   - Should be covered by LTS security updates until 2028 (to verify on debian.org).
   - Debian 13 dropped i386 as a full architecture, so do not pick it.
2. Alpine Linux (x86), lean alternative.
   - Very small footprint and pairs well with the `i686-unknown-linux-musl` Rust target for a static binary.
   - More setup work.
3. antiX or another light Debian-based 32-bit distro, acceptable but a desktop is not needed.

Avoid Ubuntu (dropped 32-bit installs) and any heavy desktop environment.

## Checks to run on the device (live USB)

```
lscpu                          # confirm 32-bit only (no "lm" flag), core count
free -h                        # actual RAM
lsusb                          # webcam detected
v4l2-ctl --list-formats-ext    # resolutions and formats (YUYV vs MJPEG)
v4l2-ctl --list-ctrls          # can exposure, white balance and focus be locked
```

`--list-formats-ext` matters most.
If the camera only offers MJPEG at 720p, decoding cost on the Atom may cap the frame rate, and 640×480 would be the better choice.

## Webcam

Camera on offer: Logitech C170 (about 15 years old).
Specs below come from the Logitech support page (French, "Logitech Webcam C170 Caractéristiques techniques", https://support.logi.com/hc/fr/articles/360023305614-Logitech-Webcam-C170-Caract%C3%A9ristiques-techniques).
They are not yet verified on the device (`v4l2-ctl --list-formats-ext`).

- Basic VGA webcam, fixed focus, UVC class (works with V4L2), USB 2.0.
- Sensor: "Réelle = 640x480, interpolée = 1,3 MP". The real resolution is 640×480.
- Video modes (4:3 SD): 320×240, 640×480, 1024×768. 16:9 modes: 320×180 and 360p.
- Frame rate: 30 fps is stated for 640×480 only; none is given for the other modes.
- 1024×768 is above the sensor resolution, so it is upscaled and adds no detail. It only costs bandwidth and CPU. Use 640×480.
- Field of view 58° diagonal (about 48° horizontal in 4:3), focal length 2.3 mm, plastic lens.
  For a board about 45 cm wide, the camera would be roughly 50 to 60 cm away to fill the frame, before the tilt and lateral offset. This is a calculation, not from the page.
- The 4:3 shape fits a chessboard better than 16:9.
- Fixed focus is an advantage here: nothing to lock, no focus hunting.
- 640×480 is marginal in `design.md`. With the board filling the frame, estimated squares are about 30 px (near) and 20 px (far). This is a scaling estimate from the 720p figures, not a measurement.
- Weak points: sensor noise in dim light, aggressive auto-exposure and white balance (may not be lockable), black piece on a dark square.

Mitigations:

1. Mount the camera close so the board fills the frame.
2. Use constant diffuse light and a matte board.
3. Use pieces with strong colour contrast against the squares.
4. Keep the stability gate and legal-move scoring, which can absorb a few misread squares per move.

Plan: use the C170 as a proof-of-concept camera and build milestones 1 to 3 with it.
After milestone 3 (occupancy overlay), judge whether detection holds up in the real lighting.

Camera fallback: if detection is unreliable, switch to a 720p camera such as the Logitech C270 (fixed focus, UVC, 1280×720).
No design change is needed, only a re-calibration of the 4 corners.

## Build targets to set up

- `i686-unknown-linux-gnu` for Debian.
- `i686-unknown-linux-musl` for Alpine or a static binary.
- First milestone on the target: a headless capture-and-warp tool serving its preview over HTTP.
