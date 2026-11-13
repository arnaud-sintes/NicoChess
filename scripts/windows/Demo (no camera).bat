@echo off
setlocal
title NicoChess demo
cd /d "%~dp0"

rem Plays a whole game on a drawn board: to try the page without a camera or a board.
rem In the page: Calibrate, Open the camera, Auto-detect the board, then Live, Start game.
set DATA=%LOCALAPPDATA%\NicoChess\demo
if not exist "%DATA%" mkdir "%DATA%"

"%~dp0nicochess.exe" --source demo --open --data-dir "%DATA%\games" --calibration "%DATA%\calibration.json"

echo.
echo NicoChess stopped.
pause
