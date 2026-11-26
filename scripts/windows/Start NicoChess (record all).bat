@echo off
setlocal
title NicoChess (recording every picture)
cd /d "%~dp0"

rem The same as "Start NicoChess (record).bat", but every picture each camera gives is kept (25 a second), also
rem while the board is quiet and whether the program looks at it or not. For a game that must be followed again
rem picture by picture, a blitz game for example. As JPEG files this takes several megabytes a second on the disk
rem (much less with an ffmpeg.exe next to nicochess.exe): delete the recordings you no longer need.
rem Camera number: Start NicoChess (record all).bat 1
set CAMERA=%~1
if "%CAMERA%"=="" set CAMERA=0

set DATA=%LOCALAPPDATA%\NicoChess
if not exist "%DATA%" mkdir "%DATA%"
echo Games and calibration are kept in %DATA%
echo Every picture of the games is kept in %DATA%\recordings
rem With ffmpeg.exe next to nicochess.exe the pictures are kept as one small video for each camera.
if exist "%~dp0ffmpeg.exe" echo The pictures are recorded as video with the ffmpeg.exe next to nicochess.exe
if not exist "%~dp0ffmpeg.exe" echo No ffmpeg.exe next to nicochess.exe: the pictures are kept as JPEG files, several megabytes a second.
if not exist "%~dp0ffmpeg.exe" echo To record video, put ffmpeg.exe next to nicochess.exe (see README.txt).
echo.

"%~dp0nicochess.exe" --device %CAMERA% --width 1280 --height 720 --fps 25 --open --data-dir "%DATA%\games" --calibration "%DATA%\calibration.json" --record-all --record-dir "%DATA%\recordings"

echo.
echo NicoChess stopped. If it did not start, the message above says why.
pause
