@echo off
setlocal
title NicoChess
cd /d "%~dp0"

rem Camera number: 0 is the first camera of this PC. To use another one:
rem     Start NicoChess.bat 1
rem "List cameras.bat" shows what Windows offers.
set CAMERA=%~1
if "%CAMERA%"=="" set CAMERA=0

set DATA=%LOCALAPPDATA%\NicoChess
if not exist "%DATA%" mkdir "%DATA%"
echo Games and calibration are kept in %DATA%
echo.

"%~dp0nicochess.exe" --device %CAMERA% --width 1280 --height 720 --fps 5 --open --data-dir "%DATA%\games" --calibration "%DATA%\calibration.json"

echo.
echo NicoChess stopped. If it did not start, the message above says why.
pause
