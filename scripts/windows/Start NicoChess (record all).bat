@echo off
setlocal
title NicoChess (recording every picture)
cd /d "%~dp0"

rem The same as "Start NicoChess (record).bat", but every picture the program looks at is kept (25 a second), also
rem while the board is quiet. For a game that must be followed again picture by picture, a blitz game for example.
rem This takes several megabytes a second on the disk: delete the recordings you no longer need.
rem Camera number: Start NicoChess (record all).bat 1
set CAMERA=%~1
if "%CAMERA%"=="" set CAMERA=0

set DATA=%LOCALAPPDATA%\NicoChess
if not exist "%DATA%" mkdir "%DATA%"
echo Games and calibration are kept in %DATA%
echo Every picture of the games is kept in %DATA%\recordings (several megabytes a second)
echo.

"%~dp0nicochess.exe" --device %CAMERA% --width 1280 --height 720 --fps 25 --open --data-dir "%DATA%\games" --calibration "%DATA%\calibration.json" --record-all --record-dir "%DATA%\recordings"

echo.
echo NicoChess stopped. If it did not start, the message above says why.
pause
