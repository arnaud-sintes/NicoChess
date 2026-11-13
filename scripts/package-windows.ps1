<#
.SYNOPSIS
  Builds the Windows package: dist\NicoChess-windows\ and dist\NicoChess-windows.zip.

.DESCRIPTION
  The package holds nicochess.exe (the page is inside it, and the C runtime is linked in,
  so there is nothing to install), launchers for double-clicking, and a short README.
  Unzip it anywhere and double-click "Start NicoChess.bat".

.EXAMPLE
  .\scripts\package-windows.ps1
#>
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\common.ps1"
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
try {

# The program says which commit it was built from (build.rs reads this, or git).
$env:NICOCHESS_COMMIT = Get-NicoCommit
# The paths of this machine (the user name in them) must not end up in the program, where panic
# messages carry them. The compiler is told to write other names. These flags come from the
# environment, which replaces the ones of .cargo\config.toml: the static runtime is repeated here.
$separator = [char]0x1f
$env:CARGO_ENCODED_RUSTFLAGS = @(
    '-C', 'target-feature=+crt-static',
    "--remap-path-prefix=$env:USERPROFILE=/home",
    "--remap-path-prefix=$root=/nicochess"
) -join $separator
Write-Host 'Building the program (release)...'
try {
    cargo build --release --locked
} finally {
    Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
}
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$exe = Join-Path $root 'target\release\nicochess.exe'
if (-not (Test-Path $exe)) { throw "missing $exe" }

# The runtime must be inside the program: look for the DLL that would otherwise be needed.
$bytes = [System.IO.File]::ReadAllBytes($exe)
$text = [System.Text.Encoding]::ASCII.GetString($bytes)
if ($text -match '(?i)vcruntime\d+\.dll') {
    throw 'nicochess.exe still needs the Visual C++ runtime (VCRUNTIME DLL): check .cargo\config.toml'
}

$out = Join-Path $root 'dist\NicoChess-windows'
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item $exe (Join-Path $out 'nicochess.exe')

# Plain text files for Windows: ASCII with CRLF line ends.
foreach ($file in Get-ChildItem (Join-Path $root 'scripts\windows') -File) {
    $content = (Get-Content -Raw $file.FullName) -replace "`r?`n", "`r`n"
    [System.IO.File]::WriteAllText((Join-Path $out $file.Name), $content, [System.Text.Encoding]::ASCII)
}

# Which version this is, for the README.
$version = Get-NicoVersion
$commit = $env:NICOCHESS_COMMIT
Add-Content -Path (Join-Path $out 'README.txt') -Encoding ASCII -Value "`r`nVersion $version ($commit)"

$zip = Join-Path $root 'dist\NicoChess-windows.zip'
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path (Join-Path $out '*') -DestinationPath $zip

$size = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host ''
Write-Host "Package: $out"
Write-Host "Zip:     $zip ($size MB)"
Get-ChildItem $out | Format-Table Name, Length -AutoSize
} finally {
    Pop-Location
}
