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

Another camera: "List cameras.bat" shows what Windows offers; then start with its
number, for example:  Start NicoChess.bat 1

From a phone or tablet
----------------------
The black window prints an address such as http://192.168.1.20:8080 : open it on a
device on the same network. The first page to connect controls the game, the others
watch. The first time, Windows asks whether NicoChess may use the network: allow it
on private networks, or other devices cannot connect.

Where things are kept
---------------------
%LOCALAPPDATA%\NicoChess : the games (games\), the calibration and the picture
settings. To remove NicoChess, delete this folder and the folder it came in.

If it does not start
--------------------
- "forbidden by its access permissions" or "address already in use": the port is
  reserved or taken (Docker Desktop and Hyper-V reserve ranges of ports). NicoChess
  tries other ports by itself; the address it picked is printed in the black window.
- Black or frozen picture: another program (Teams, Zoom, the Camera app) may be
  using the camera. Close it, then restart NicoChess.
- The wrong camera opens: use the camera number, see above.

Everything you can set is listed by:  nicochess.exe --help
