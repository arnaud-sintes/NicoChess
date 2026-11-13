@echo off
setlocal
title NicoChess cameras
cd /d "%~dp0"

rem Shows the camera (number 0 unless you give another one) with its picture formats and controls.
set CAMERA=%~1
if "%CAMERA%"=="" set CAMERA=0

"%~dp0nicochess.exe" --probe --device %CAMERA%

echo.
echo To look at another camera:  List cameras.bat 1
pause
