# Prepares the folder `usb\` to copy onto a USB key (FAT32) before going to the target:
# the 32-bit program, the installer, the offline Debian packages, the scripts and a README.
#
#   .\scripts\make-usb-bundle.ps1                 build everything
#   .\scripts\make-usb-bundle.ps1 -SkipBuild      reuse dist\nicochess
#   .\scripts\make-usb-bundle.ps1 -SkipDebs       reuse the packages already in usb\debs
#
# Needs Docker Desktop running (to build, and to download the packages for Debian 12 i386).

param(
    [switch]$SkipBuild,
    [switch]$SkipDebs
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot\..").Path
$bundle = Join-Path $root 'usb'
$source = Join-Path $root 'scripts\usb'

New-Item -ItemType Directory -Force $bundle, "$bundle\debs", "$bundle\docs" | Out-Null

if (-not $SkipBuild) {
    & "$PSScriptRoot\build-target.ps1"
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}
if (-not (Test-Path "$root\dist\nicochess")) { throw 'dist\nicochess is missing: run without -SkipBuild' }

Copy-Item "$root\dist\nicochess" $bundle -Force
foreach ($file in 'install.sh', 'setup-ap.sh', 'packages-base.txt', 'packages-wifi.txt', 'README.txt') {
    Copy-Item "$source\$file" $bundle -Force
}
Copy-Item "$root\scripts\target-setup.sh" $bundle -Force
Copy-Item "$root\PLAN.md", "$root\docs\requirements.md", "$root\docs\target-hardware.md", "$root\docs\design.md" "$bundle\docs" -Force

if (-not $SkipDebs) {
    # The packages are downloaded inside a Debian 12 i386 container: same release and
    # architecture as the target. The folder becomes a small apt repository.
    docker run --rm --platform linux/386 `
        -v "${bundle}\debs:/out" `
        -v "${source}:/scripts:ro" `
        i386/debian:12 sh /scripts/fetch-debs.sh
    if ($LASTEXITCODE -ne 0) { throw 'downloading the packages failed' }
}
if (-not (Test-Path "$bundle\debs\Packages")) { throw 'usb\debs\Packages is missing: run without -SkipDebs' }

# Checksums of everything, the packages included: the apt repository on the key is not signed
# (install.sh trusts it), so this is what catches a damaged copy.
$lines = Get-ChildItem $bundle -Recurse -File |
    Where-Object { $_.Name -ne 'SHA256SUMS' } |
    Sort-Object FullName |
    ForEach-Object {
        $relative = $_.FullName.Substring($bundle.Length + 1).Replace('\', '/')
        '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $relative
    }
[System.IO.File]::WriteAllText((Join-Path $bundle 'SHA256SUMS'), (($lines -join "`n") + "`n"))

$size = (Get-ChildItem $bundle -Recurse -File | Measure-Object Length -Sum).Sum / 1MB
$debs = (Get-ChildItem "$bundle\debs" -Filter *.deb | Measure-Object).Count
Write-Host ''
Write-Host ("Ready: {0}  ({1:N0} MB, {2} packages)" -f $bundle, $size, $debs)
Write-Host 'Copy the CONTENT of that folder to the root of a FAT32 USB key, then follow README.txt on the target.'
