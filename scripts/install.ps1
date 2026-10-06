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
    # Windows PowerShell 5.1 turns any native-command stderr text (cargo prints
    # warnings there) into a terminating error under 'Stop'; judge the build by
    # its exit code instead.
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        cargo build --release --bin aplexer
        $buildExit = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
        Pop-Location
    }
    if ($buildExit -ne 0) { exit $buildExit }
    $src = Join-Path $root 'target\release\aplexer.exe'
}
if (-not (Test-Path -PathType Leaf $src)) { throw "binary not found: $src" }

New-Item -ItemType Directory -Force $BinDir | Out-Null

# Atomic replace that works while sessions are running. A running .exe cannot
# be overwritten or deleted, but it CAN be renamed: so the old file is moved
# aside to `<name>.old-<pid>` (a name no process can already hold open) and the
# new one takes its place. Leftovers from earlier installs are deleted
# best-effort at the start of each run (still-running ones just stay until the
# last session using them exits). New copies are staged next to the target
# first, so a concurrently starting client never executes a half-written file.
function Remove-OldCopies([string]$To) {
    $dir = Split-Path -Parent $To
    $leaf = Split-Path -Leaf $To
    Get-ChildItem -LiteralPath $dir -Filter "$leaf.old-*" -Force -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
    Get-ChildItem -LiteralPath $dir -Filter '.aplexer.*.tmp' -Force -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
}

function Install-File([string]$From, [string]$To) {
    Remove-OldCopies $To
    $tmp = Join-Path (Split-Path -Parent $To) ('.aplexer.' + [guid]::NewGuid().ToString('N') + '.tmp')
    Copy-Item -LiteralPath $From -Destination $tmp -Force
    try {
        Move-Item -LiteralPath $tmp -Destination $To -Force
    }
    catch {
        # Target is in use ("Access denied"): rename it aside, then retry.
        $old = "$To.old-$PID"
        try {
            if (Test-Path -LiteralPath $To) { Move-Item -LiteralPath $To -Destination $old -Force }
            Move-Item -LiteralPath $tmp -Destination $To -Force
        }
        catch {
            Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
            # Put the original back rather than leave the install without a binary.
            if ((Test-Path -LiteralPath $old) -and -not (Test-Path -LiteralPath $To)) {
                Move-Item -LiteralPath $old -Destination $To -Force
            }
            throw
        }
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
