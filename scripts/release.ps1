<#
.SYNOPSIS
  Builds every package and publishes them as a GitHub release (tag vX.Y.Z).

.DESCRIPTION
  The version is the one in Cargo.toml ([workspace.package]). To release: change it there, add a
  section "## X.Y.Z" to CHANGELOG.md, commit, push, then run this script. It refuses to run on a
  tree with uncommitted changes, when the commit is not pushed, or when the version is already
  released, so that every release is exactly one commit that anyone can check out.

  It builds, in this order (Docker Desktop must be running):
    scripts\build-target.ps1    dist\nicochess                    -> nicochess-linux-i386.tar.gz
    scripts\build-android.ps1   dist\nicochess-debug-apk.zip      (the APK, zipped, with install notes)
    scripts\package-windows.ps1 dist\NicoChess-windows.zip
  and publishes these three with SHA256SUMS.txt. Under "latest" they are always the newest version.

.EXAMPLE
  .\scripts\release.ps1                # build and publish
  .\scripts\release.ps1 -NoPublish     # build everything and stop before publishing
  .\scripts\release.ps1 -SkipBuild     # publish what is in dist\ (it must be newer than the last commit)
#>
param([switch]$NoPublish, [switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"
$root = (Resolve-Path "$PSScriptRoot\..").Path
$dist = Join-Path $root 'dist'
$version = Get-NicoVersion
$tag = "v$version"
Push-Location $root
try {
    # ---- The tree must be exactly a pushed commit.
    if (git status --porcelain) { throw 'there are uncommitted changes: commit them first (a release is one commit)' }
    if ((git rev-parse --abbrev-ref HEAD) -ne 'main') { throw 'release from the main branch' }
    git fetch --quiet origin main --tags
    $head = (git rev-parse HEAD)
    if ($head -ne (git rev-parse origin/main)) { throw 'the commit is not pushed (or origin has more): push first' }
    if (git tag --list $tag) { throw "version $version is already released ($tag exists): change the version in Cargo.toml" }
    $notes = Get-Content -Raw (Join-Path $root 'CHANGELOG.md') -ErrorAction SilentlyContinue
    if (-not $notes -or $notes -notmatch "(?m)^## $([regex]::Escape($version))\b") {
        throw "CHANGELOG.md has no section '## $version'"
    }

    # ---- Build.
    $started = Get-Date
    if (-not $SkipBuild) {
        & "$PSScriptRoot\build-target.ps1"
        & "$PSScriptRoot\build-android.ps1"
        & "$PSScriptRoot\package-windows.ps1"
    }

    # ---- The files to publish. They must be from this build, not left over from an older one.
    $linux = Join-Path $dist 'nicochess'
    $tarball = Join-Path $dist 'nicochess-linux-i386.tar.gz'
    if (Test-Path $tarball) { Remove-Item -Force $tarball }
    tar -czf $tarball -C $dist nicochess
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
    $assets = @(
        (Join-Path $dist 'NicoChess-windows.zip'),
        (Join-Path $dist 'nicochess-debug-apk.zip'),
        $tarball
    )
    foreach ($asset in $assets) {
        if (-not (Test-Path $asset)) { throw "missing $asset" }
        if (-not $SkipBuild -and (Get-Item $asset).LastWriteTime -lt $started) { throw "$asset was not rebuilt" }
    }
    $sums = Join-Path $dist 'SHA256SUMS.txt'
    ($assets | ForEach-Object {
        '{0}  {1}' -f (Get-FileHash $_ -Algorithm SHA256).Hash.ToLower(), (Split-Path -Leaf $_)
    }) -join "`n" | Set-Content -Path $sums -Encoding ASCII
    $assets += $sums

    # The built program must say the version being released.
    if (-not $SkipBuild) {
        $reported = & (Join-Path $root 'target\release\nicochess.exe') --version
        if ($reported -notmatch [regex]::Escape($version)) { throw "nicochess.exe reports '$reported', not $version" }
        if ($reported -match 'dirty') { throw "nicochess.exe reports '$reported': the tree was not clean" }
    }

    # ---- Release notes: the section of this version in CHANGELOG.md.
    $section = [regex]::Match($notes, "(?ms)^## $([regex]::Escape($version))\b[^\n]*\n(.*?)(?=^## |\z)").Groups[1].Value.Trim()
    $body = @"
$section

---
Packages (built from commit ``$(git rev-parse --short=8 HEAD)``):
- ``NicoChess-windows.zip``: unzip, double-click "Start NicoChess.bat". Nothing to install.
- ``nicochess-debug-apk.zip``: the Android app (a debug build), zipped with install notes.
- ``nicochess-linux-i386.tar.gz``: the program for the 32-bit Linux target (Debian 12 i386).
- ``SHA256SUMS.txt``: checksums of the three.
"@
    $notesFile = Join-Path $dist 'release-notes.md'
    $body | Set-Content -Path $notesFile -Encoding UTF8

    Write-Host ''
    Write-Host "Release $tag from commit $head"
    $assets | ForEach-Object { Write-Host ('  {0}  ({1:N1} MB)' -f (Split-Path -Leaf $_), ((Get-Item $_).Length / 1MB)) }
    if ($NoPublish) { Write-Host 'Not published (-NoPublish).'; return }

    # ---- Publish. The tag is created on GitHub, on exactly this commit.
    gh release create $tag @assets --target $head --title "NicoChess $version" --notes-file $notesFile --latest
    if ($LASTEXITCODE -ne 0) { throw 'gh release create failed' }
    git fetch --quiet --tags origin
    Write-Host ''
    Write-Host (gh release view $tag --json url --jq .url)
} finally {
    Pop-Location
}
