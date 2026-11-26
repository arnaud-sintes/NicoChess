# Recording as video: measurements and the choice of parameters

`--record` and `--record-all` write the pictures of a game as one H.264 video per camera when an `ffmpeg` program is
found (`src/video.rs`, ENVIRONMENT.md "Recording as video"). This page keeps the measurements that led to the
parameters, so that they can be checked or redone (new camera, new codec, a slower PC).

Where the numbers come from: real recordings of 2026-11-24 (1280x720 camera, a depth camera, NicoChess 0.11.1), the
portable `ffmpeg` n8.1 GPL build of BtbN (checksum checked), on a PC with a 12th generation Core i7-12850HX (16 cores,
24 threads). The speeds below are for that PC: **nothing was measured on the 32-bit Atom or on a phone**, which is why
the recorder is a feature of the PC and only starts a program that is not there elsewhere.

## Size and speed (600 pictures of the camera from the middle of a game, 1280x720, 24 s of video at 25 a second)

The pictures were read from the recording (JPEG, quality 92) and given to `ffmpeg` raw (`rgb24`), as the program does.
JPEG is 246.8 KB a picture for the camera and 82.8 KB for the height picture of the depth camera.

| Encoder | KB a picture | Times faster than real time | CPU seconds for 24 s of video |
|---|---|---|---|
| JPEG q92 (what was written before) | 246.8 | n/a | n/a |
| x264 veryfast crf 18 | 48.5 | 14.5 | 24.5 |
| **x264 veryfast crf 20** | **31.0** | 16.1 | 22.1 |
| x264 veryfast crf 23 | 11.6 | 17.6 | 16.0 |
| x264 medium crf 20 | 32.8 | 5.7 | 63.9 |
| x264 slow crf 20 | 33.5 | 3.0 | 111 |
| x264 veryfast crf 20, `-tune stillimage` | 34.6 | 13.7 | 25.5 |
| **x264 veryfast crf 20, `-threads 2`** | 31.0 | **4.9** | **13.9 (0.58 of a core)** |
| x264 medium crf 20, `-threads 2` | 32.7 | 1.3 (too slow) | 39.0 |
| x265 fast crf 22 | 11.4 | 3.2 | 67 |
| SVT-AV1 preset 10 crf 30 | 6.7 | 14.6 | 15.1 |
| FFV1 lossless | 751 | 5.6 | 31.8 |
| x264 lossless | 1080 | 4.2 | 82 |

Lossless is not an option: it is three to four times *bigger* than the JPEG, because the noise of the camera does not
compress. `-tune stillimage` makes it bigger, not smaller (the noise again). The slower presets cost three to eight
times the CPU for no gain in size. The depth view is smaller still: x264 veryfast gives 16.4 KB at crf 18, 12.5 at 20
and 8.4 at 23, against 82.8 as JPEG and 139 for FFV1.

## Does the tracker decide the same on the pictures that went through the codec?

Two replays of a recording whose pictures were encoded and decoded again, against the replay of the original:

| Recording | Setting (camera / depth) | What differs from the replay of the original |
|---|---|---|
| 13-36-23, 27 plies, a clean game | crf 20 / unchanged | nothing (the whole report is the same) |
| 13-36-23 | crf 23 / unchanged | nothing |
| 13-21-41, 51 plies, hands over the board, 26 detections | crf 23 / crf 23 | one certainty: 79% instead of 80% on a doubtful move |
| 13-21-41 | crf 20 / crf 18 | the same one point |
| 13-21-41 | crf 28 / crf 28 | **a castling that the original replay had missed is found, but two detections at the end lead to positions the game never had, and the replay ends in another position**: too much |

So 18 to 23 keep the decisions, and 28 does not. The keyframes of `tools/groundtruth` on the clean game: 26 for the
original and for crf 20 (24 of them at the same still stretches), 28 for crf 23. On the board squares, against the
original pictures: mean error 2.2 to 2.7 grey levels, PSNR 35 to 37 dB, the mean colour of a square shifts by 0.6 grey
level (3.5 at most), and the flicker of that mean from one picture to the next is not higher than in the original
(1.2 to 1.3 against 1.39). A crop of crf 23 reads as well as one of the original.

## What was chosen, and why

- x264, preset `veryfast`, `-threads 2`, `-pix_fmt yuv420p`, a key picture every 50 pictures, Matroska.
  Fast enough with two threads (about five times faster than real time, about half a core), so that the tracker keeps
  its cores; `ffmpeg` runs at a lower priority on Windows.
- Camera: `-crf 23` (11.6 KB a picture, 21 times smaller than JPEG). The tracker decided the same as on the JPEG
  pictures on both recordings; the keyframes of `tools/groundtruth` number 28 instead of 26 on the clean game, and a
  crop still reads as well as the original. `-crf 20` (31 KB a picture, 8 times smaller than JPEG) gave the keyframes
  closest to the originals: use it by changing `CAMERA_CRF` if the look of the pictures matters more than disk.
- Depth view: `-crf 23` (8 KB a picture, 10 times smaller).
- Whole recordings: the game 13-36-23 (2271 camera pictures, 122 s) is about 550 MB of camera pictures as JPEG, 82 MB as
  video at crf 20 (35 MB at crf 23). The game 13-21-41 (4009 + 729 pictures, 206 s) is 1.0 GB as JPEG, and 134 MB at
  crf 20 / 18 (54 MB at crf 23).

## What these numbers do not tell

- The pictures that were encoded were JPEG at quality 92 already (what the recordings hold), not the camera's own
  pictures, which are a little cleaner: the video of the live program should be slightly smaller than the figures here.
- No live game, no camera and no Atom: the cost at 25 pictures a second on a small box is not known. The encoder is
  optional and only started on request.
- Only two recordings, both from one camera and one board.
