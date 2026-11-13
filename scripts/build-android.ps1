# Builds the Android app (an APK) in a container. Needs Docker Desktop running.
# Output: dist\nicochess-debug.apk
#
#   .\scripts\build-android.ps1
#   .\scripts\build-android.ps1 -Abis arm64-v8a      # only 64-bit ARM: faster
#
# The first run creates the image (about 3 GB: JDK, Android SDK and NDK, Rust) and
# takes a while; later runs reuse it and the caches below.

param([string]$Abis = 'arm64-v8a armeabi-v7a')

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
    -v "${root}:/src:ro" `
    -v nicochess-gradle:/root/.gradle `
    -v nicochess-android-keystore:/root/.android `
    -v nicochess-cargo-registry-android:/opt/cargo/registry `
    -v nicochess-android-target:/work/target `
    -v "${dist}:/out" `
    nicochess-android `
    bash /src/docker/android-build.sh
if ($LASTEXITCODE -ne 0) { throw 'android build failed' }

Write-Host "Built: $dist\nicochess-debug.apk"
