# Runs the 32-bit program in the Debian i386 container with a real webcam of this PC.
# The webcam must first be handed to WSL (see PLAN.md section 11): usbipd-win, then
# `usbipd attach --wsl`, then `sudo modprobe uvcvideo` in a WSL window.
#
#   .\scripts\test-camera-docker.ps1 -Probe                       what the camera offers
#   .\scripts\test-camera-docker.ps1                              run the program, page on http://localhost:8080
#   .\scripts\test-camera-docker.ps1 -Options "--width 1280 --height 720 --fps 5"
#   .\scripts\test-camera-docker.ps1 -Device /dev/video2 -Probe
#
# Needs dist\nicochess (run .\scripts\build-target.ps1 first) and Docker Desktop running.

param(
    [string]$Device = '/dev/video0',
    [string]$Options = '--width 640 --height 480 --fps 3',
    [int]$Port = 8080,
    [switch]$Probe
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot\..").Path
if (-not (Test-Path "$root\dist\nicochess")) { throw 'dist\nicochess is missing: run .\scripts\build-target.ps1 first' }

$common = @('run', '--rm', '--platform', 'linux/386', '--device', "${Device}:${Device}", '-v', "$root\dist:/app:ro")

# Is the camera visible inside a container?
# (Docker prints an error when the device does not exist: that is expected here.)
$previous = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
docker @common nicochess-build ls -l $Device 2>&1 | Out-Null
$visible = ($LASTEXITCODE -eq 0)
$ErrorActionPreference = $previous
if (-not $visible) {
    Write-Host "The container cannot see $Device." -ForegroundColor Yellow
    Write-Host @"

The webcam has to be handed to WSL first (once per boot or unplug):
  1. Install usbipd-win (administrator PowerShell):   winget install --exact dorssel.usbipd-win
  2. List the USB devices and find the webcam:        usbipd list
  3. Share it (administrator, once):                  usbipd bind --busid <BUSID>
  4. Attach it to WSL:                                usbipd attach --wsl --busid <BUSID>
  5. Load the driver, in a WSL window (wsl):          sudo modprobe uvcvideo ; ls /dev/video*
Then run this script again. While attached, Windows programs cannot use the webcam;
give it back with:                                    usbipd detach --busid <BUSID>
"@
    exit 1
}

if ($Probe) {
    docker @common nicochess-build /app/nicochess --probe --device $Device
    exit $LASTEXITCODE
}

Write-Host "Page: http://localhost:$Port   (Ctrl+C to stop; games are lost when the container stops)"
$run = $common + @('-p', "${Port}:8080", 'nicochess-build', '/app/nicochess', '--device', $Device) + ($Options -split '\s+')
docker @run
