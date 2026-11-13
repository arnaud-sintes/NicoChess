# Builds the Android app (an APK) in a container. Needs Docker Desktop running.
# Output: dist\nicochess-debug.apk and dist\nicochess-debug-apk.zip (the same, zipped with install notes)
#
#   .\scripts\build-android.ps1
#   .\scripts\build-android.ps1 -Abis arm64-v8a      # only 64-bit ARM: faster
#
# The first run creates the image (about 3 GB: JDK, Android SDK and NDK, Rust) and
# takes a while; later runs reuse it and the caches below.

param([string]$Abis = 'arm64-v8a armeabi-v7a')

. "$PSScriptRoot\common.ps1"
$commit = Get-NicoCommit
$version = Get-NicoVersion

$root = (Resolve-Path "$PSScriptRoot\..").Path
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

# Docker writes its progress to stderr, which PowerShell would take for an error.
$ErrorActionPreference = 'Continue'

docker build -t nicochess-android -f "$root\docker\android.Dockerfile" "$root\docker"
if ($LASTEXITCODE -ne 0) { throw 'docker build failed' }

# Dependencies and build output live in named volumes: rebuilds are incremental. The
# debug signing key lives in one too: with a new key each time, Android would refuse to
# update the installed app (it would have to be uninstalled first).
$env:MSYS_NO_PATHCONV = '1'
docker run --rm `
    -e "ABIS=$Abis" `
    -e "NICOCHESS_COMMIT=$commit" `
    -v "${root}:/src:ro" `
    -v nicochess-gradle:/root/.gradle `
    -v nicochess-android-keystore:/root/.android `
    -v nicochess-cargo-registry-android:/opt/cargo/registry `
    -v nicochess-android-target:/work/target `
    -v "${dist}:/out" `
    nicochess-android `
    bash /src/docker/android-build.sh
if ($LASTEXITCODE -ne 0) { throw 'android build failed' }

# The APK in a zip, with a few lines on how to install it: mail programs and some download pages
# refuse to carry an .apk, and a zip also says which version it is.
$zip = Join-Path $dist 'nicochess-debug-apk.zip'
$readme = Join-Path $dist 'INSTALL.txt'
@"
NicoChess $version for Android (debug build, commit $commit)

1. Copy nicochess-debug.apk to the phone (unzip it first).
2. Open it from a file manager. Android asks to allow installing apps from this source: allow it.
3. Open NicoChess. It asks for the camera, and for notifications on Android 13 and later.

Updating: install the new APK over the old one. If Android says the app conflicts with an existing
one, it was signed with another key: uninstall the old NicoChess once, then install this one.
The debug key is kept in the Docker volume nicochess-android-keystore on the build machine.
"@ | Set-Content -Path $readme -Encoding ASCII
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $dist 'nicochess-debug.apk'), $readme -DestinationPath $zip
Remove-Item -Force $readme

Write-Host "Built: $dist\nicochess-debug.apk (version $version, commit $commit)"
Write-Host "Zip:   $zip"
