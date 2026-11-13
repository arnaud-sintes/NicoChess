# Shared by the build and release scripts (dot-source it: . "$PSScriptRoot\common.ps1").

# The version of NicoChess: the one in Cargo.toml, under [workspace.package].
function Get-NicoVersion {
    $line = Select-String -Path (Join-Path $PSScriptRoot '..\Cargo.toml') -Pattern '^version = "(\d+\.\d+\.\d+)"' | Select-Object -First 1
    if (-not $line) { throw 'no version = "x.y.z" in Cargo.toml' }
    return $line.Matches[0].Groups[1].Value
}

# The commit being built, short, with "-dirty" when there are uncommitted changes ("" if git says nothing).
# The Docker builds cannot run git, so the scripts give it to them (NICOCHESS_COMMIT, see build.rs).
function Get-NicoCommit {
    $root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    $hash = (git -C $root rev-parse --short=8 HEAD 2>$null)
    if ($LASTEXITCODE -ne 0 -or -not $hash) { return '' }
    $changes = (git -C $root status --porcelain 2>$null)
    if ($changes) { return "$hash-dirty" }
    return $hash
}
