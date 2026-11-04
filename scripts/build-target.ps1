# Builds the 32-bit Linux binary for the target (Debian 12 i386) in a container.
# Needs Docker Desktop running. Output: dist\nicochess
#
#   .\scripts\build-target.ps1

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot\..").Path
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force $dist | Out-Null

# The first build creates the image (about 1 GB, a few minutes); later builds reuse it.
docker build --platform linux/386 -t nicochess-build -f "$root\docker\build.Dockerfile" "$root\docker"
if ($LASTEXITCODE -ne 0) { throw 'docker build failed' }

# The source is mounted read-only. Dependencies and build output live in named
# volumes, so rebuilds are incremental and nothing is written into the project.
docker run --rm --platform linux/386 `
    -v "${root}:/src:ro" `
    -v nicochess-cargo-registry:/opt/cargo/registry `
    -v nicochess-target:/build/target `
    -v "${dist}:/out" `
    nicochess-build `
    linux32 sh -c 'cargo build --release --locked --target-dir /build/target && cp /build/target/release/nicochess /out/nicochess && file /out/nicochess && ldd --version | head -1'
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

Write-Host "Built: $dist\nicochess"
