# Install the aplexer CLI on Windows: aplexer.exe plus an `a.exe` alias.
#
#   scripts\install.ps1                          # build --release, install to the default dir
#   scripts\install.ps1 -BinDir C:\tools\bin     # install somewhere else
#   scripts\install.ps1 -Bin PATH                # install an already-built aplexer.exe
#   scripts\install.ps1 -AddToPath               # also add the bin dir to the user PATH
#
# Default bin dir: %LOCALAPPDATA%\Programs\aplexer\bin. The alias is a second
# copy of the same file (symlinks need elevation or Developer Mode on
# Windows); re-running this script refreshes both together.
[CmdletBinding()]
param(
    [string]$Bin = '',
    [string]$BinDir = (Join-Path $env:LOCALAPPDATA 'Programs\aplexer\bin'),
    [switch]$AddToPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

if ($Bin) {
    $src = (Resolve-Path $Bin).Path
}
else {
    Push-Location $root
    try {
        cargo build --release --bin aplexer
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    finally { Pop-Location }
    $src = Join-Path $root 'target\release\aplexer.exe'
}
if (-not (Test-Path -PathType Leaf $src)) { throw "binary not found: $src" }

New-Item -ItemType Directory -Force $BinDir | Out-Null

# Atomic replace: copy next to the destination, then rename over it, so a
# concurrently starting client never executes a half-written file. A running
# .exe cannot be overwritten but can be renamed, so on failure move the old
# file aside (best-effort delete of the leftover) and retry.
function Install-File([string]$From, [string]$To) {
    $tmp = Join-Path (Split-Path -Parent $To) ('.aplexer.' + [guid]::NewGuid().ToString('N') + '.tmp')
    Copy-Item -LiteralPath $From -Destination $tmp -Force
    try {
        Move-Item -LiteralPath $tmp -Destination $To -Force
    }
    catch {
        $old = $To + '.old'
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $To) { Move-Item -LiteralPath $To -Destination $old -Force }
        Move-Item -LiteralPath $tmp -Destination $To -Force
        Remove-Item -LiteralPath $old -Force -ErrorAction SilentlyContinue
    }
}

Install-File $src (Join-Path $BinDir 'aplexer.exe')
Install-File $src (Join-Path $BinDir 'a.exe')

Write-Host "installed $src"
Write-Host "  as    $(Join-Path $BinDir 'aplexer.exe')"
Write-Host "  alias $(Join-Path $BinDir 'a.exe') (copy of aplexer.exe)"

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$entries = @()
if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }
$onPath = $entries | Where-Object { $_.TrimEnd('\') -ieq $BinDir.TrimEnd('\') }
if (-not $onPath) {
    if ($AddToPath) {
        $new = (@($entries) + $BinDir) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $new, 'User')
        Write-Host "added $BinDir to the user PATH; open a new terminal to pick it up"
    }
    else {
        Write-Host "note: $BinDir is not on your user PATH; re-run with -AddToPath to add it"
    }
}
