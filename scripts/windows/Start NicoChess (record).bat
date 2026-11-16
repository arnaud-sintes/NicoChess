@echo off
setlocal
title NicoChess (recording)
cd /d "%~dp0"

rem The same as "Start NicoChess.bat", and every game is recorded for debugging: a folder named after the start
rem (date and time) in %LOCALAPPDATA%\NicoChess\recordings, with the pictures the program saw, what it decided
rem and what the players did. Zip such a folder to send it. Camera number: Start NicoChess (record).bat 1
set CAMERA=%~1
if "%CAMERA%"=="" set CAMERA=0

set DATA=%LOCALAPPDATA%\NicoChess
if not exist "%DATA%" mkdir "%DATA%"
echo Games and calibration are kept in %DATA%
echo Recordings of the games are kept in %DATA%\recordings
echo.

"%~dp0nicochess.exe" --device %CAMERA% --width 1280 --height 720 --fps 5 --open --data-dir "%DATA%\games" --calibration "%DATA%\calibration.json" --record --record-dir "%DATA%\recordings"

echo.
echo NicoChess stopped. If it did not start, the message above says why.
pause
