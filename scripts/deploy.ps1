# Builds the 32-bit program and installs it on a target that is reachable over SSH,
# then restarts the service. For when the network is available (otherwise use the USB key).
#
#   .\scripts\deploy.ps1 -Target nico@192.168.1.50
#   .\scripts\deploy.ps1 -Target nico@192.168.1.50 -SkipBuild
#
# The user on the target needs sudo, and install.sh must have been run once.

param(
    [Parameter(Mandatory = $true)][string]$Target,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path "$PSScriptRoot\..").Path

if (-not $SkipBuild) {
    & "$PSScriptRoot\build-target.ps1"
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}
if (-not (Test-Path "$root\dist\nicochess")) { throw 'dist\nicochess is missing' }

scp "$root\dist\nicochess" "${Target}:/tmp/nicochess.new"
if ($LASTEXITCODE -ne 0) { throw 'scp failed' }

ssh -t $Target "sudo install -m 755 /tmp/nicochess.new /opt/nicochess/nicochess && rm /tmp/nicochess.new && sudo systemctl restart nicochess && sleep 2 && systemctl --no-pager --lines=5 status nicochess"
if ($LASTEXITCODE -ne 0) { throw 'installing on the target failed' }
