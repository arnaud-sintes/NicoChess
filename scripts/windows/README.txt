NicoChess for Windows
=====================

NicoChess watches a chessboard with a camera, works out the moves, records the
games and shows them on a web page (on this PC, a phone or a tablet).

Start
-----
1. Double-click "Start NicoChess.bat". A black window opens and the page opens in
   your browser. Leave the black window open: closing it stops NicoChess.
2. On the page: Calibrate, Open the camera, then "Auto-detect the board" (or click
   the 4 corners: a1, h1, h8, a8). Check that the green outline sits on the board.
3. Back on Live: Start game. Moves appear as they are played.

No camera at hand? "Demo (no camera).bat" plays a whole game on a drawn board.

Testing? Start with "Start NicoChess (record).bat" instead: it does the same and also
records every game for debugging (see Recordings below).

Another camera (a laptop with a built-in camera and a USB one, for example): on the page,
Calibrate > Camera, pick it in the list (not during a game). The choice is remembered
by the camera's name, so it is used again next time even if Windows numbers the
cameras differently. Changing the camera forgets the calibration of the board.
You can also start with a camera number:  Start NicoChess.bat 1  ("List cameras.bat"
shows the numbers); a number given this way beats the remembered choice.

From a phone or tablet
----------------------
The black window prints an address such as http://192.168.1.20:8080 : open it on a
device on the same network. The first page to connect controls the game, the others
watch. The first time, Windows asks whether NicoChess may use the network: allow it
on private networks, or other devices cannot connect.

Where things are kept
---------------------
%LOCALAPPDATA%\NicoChess : the games (games\), the calibration and the picture
settings, and the recordings (recordings\). To remove NicoChess, delete this folder and
the folder it came in.

Recordings
----------
"Start NicoChess (record).bat" keeps, for every game, a folder named after its start
(for example 2026-11-16_11-30-05) in %LOCALAPPDATA%\NicoChess\recordings. It holds the
pictures the program looked at, what it decided and what the players did (moves typed
by hand, undo, calibration). The developer can replay it to see why a move was missed.
The pictures show whatever the camera sees, so look at a few before sending a folder.
Nothing is sent anywhere by the program. Expect a few hundred MB per hour of play at
1280x720: delete the folders you no longer need. To send one, zip its folder.

"Start NicoChess (record all).bat" keeps every picture the program looks at (25 a second),
also while the board is quiet: for a fast game (blitz) that has to be followed again picture
by picture. It takes several megabytes a second on the disk, so use it for one game at a time
and delete the folder afterwards. If the disk cannot keep up, the program says how many
pictures were left out when the game ends.

Smaller recordings (optional): with ffmpeg.exe in this folder, next to nicochess.exe, the
pictures are kept as one video for each camera, 20 times smaller than JPEG files. ffmpeg is not
part of this package (about 170 MB): get a "win64-gpl" build, for example from
https://github.com/BtbN/FFmpeg-Builds/releases, check its checksum, and copy bin\ffmpeg.exe here.
The same ffmpeg.exe is needed to replay such a recording. Without it the program keeps JPEG files.

If it does not start
--------------------
- "forbidden by its access permissions" or "address already in use": the port is
  reserved or taken (Docker Desktop and Hyper-V reserve ranges of ports). NicoChess
  tries other ports by itself; the address it picked is printed in the black window.
- Black or frozen picture: another program (Teams, Zoom, the Camera app) may be
  using the camera. Close it, then restart NicoChess.
- The wrong camera opens: use the camera number, see above.

Everything you can set is listed by:  nicochess.exe --help
