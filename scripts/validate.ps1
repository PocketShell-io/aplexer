# Local/CI gate for Windows: PowerShell port of scripts/validate.sh.
#
#   scripts/validate.ps1
#
# The executed-test floor comes from the WINDOWS_MIN_TESTS environment
# variable (or -MinTests). The default of 40 is a deliberately conservative
# placeholder: it MUST be measured against a real Windows run and raised to
# sit well under the actual executed count, like the Linux floors.
[CmdletBinding()]
param(
    [int]$MinTests = $(if ($env:WINDOWS_MIN_TESTS) { [int]$env:WINDOWS_MIN_TESTS } else { 40 })
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) {
        [Console]::Error.WriteLine("$Exe $($Arguments -join ' ') failed with status $LASTEXITCODE")
        exit $LASTEXITCODE
    }
}

Write-Host '==> repository hygiene'
foreach ($f in 'Cargo.toml', 'README.md') {
    if (-not (Test-Path -PathType Leaf $f)) { throw "missing $f" }
}
if (-not (Test-Path -PathType Container 'src')) { throw 'missing src' }
$junk = Get-ChildItem -Recurse -Force -File -ErrorAction SilentlyContinue |
    Where-Object {
        $_.FullName -notmatch '[\\/](target|\.git|\.venv|node_modules)[\\/]' -and
        ($_.Name -like '*.pyc' -or $_.Name -eq '.DS_Store')
    } | Select-Object -First 1
if ($junk) { throw "stray file: $($junk.FullName)" }

Write-Host '==> Rust formatting, lints, and tests'
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    [Console]::Error.WriteLine('cargo is required')
    exit 127
}
Invoke-Native cargo @('fmt', '--all', '--', '--check')
Invoke-Native cargo @('clippy', '--all-targets', '--', '-D', 'warnings')

# `cargo test` exits 0 for a run that executed nothing, so go through the
# executed-count guard. The floor is a collapse detector, not a ratchet.
# The startup-test-hooks suites are Linux-only (cgroup/containment) and are
# not run here.
& "$PSScriptRoot/check-test-execution.ps1" -SelfTest
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& "$PSScriptRoot/check-test-execution.ps1" -Min $MinTests -- cargo test --all-targets
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Pick a Python interpreter for the plain-script steps.
$py = $null
foreach ($candidate in 'python', 'py') {
    if (Get-Command $candidate -ErrorAction SilentlyContinue) {
        & $candidate -c 'import sys' 2>$null
        if ($LASTEXITCODE -eq 0) { $py = $candidate; break }
    }
}
$haveUv = [bool](Get-Command uv -ErrorAction SilentlyContinue)
if (-not $py -and -not $haveUv) {
    [Console]::Error.WriteLine('python or uv is required')
    exit 1
}
function Invoke-Python {
    param([string[]]$Arguments)
    if ($py) { Invoke-Native $py $Arguments }
    else { Invoke-Native uv (@('run', '--frozen', 'python') + $Arguments) }
}

Write-Host '==> Coordination package generation'
Invoke-Python @('-B', 'tests/coordination_packages.py')

# Python suites must actually run; a missing runner is a hard failure.
function Invoke-PythonSuite {
    param([string]$Dir)
    Write-Host "==> Python syntax and tests ($Dir)"
    Invoke-Python @('-m', 'compileall', '-q', $Dir)
    $havePytest = $false
    if ($py) {
        & $py -c 'import pytest' 2>$null
        $havePytest = ($LASTEXITCODE -eq 0)
    }
    Push-Location $Dir
    try {
        if ($havePytest) {
            Invoke-Native $py @('-m', 'pytest', '-q')
        }
        elseif ($haveUv -and (Test-Path -PathType Leaf 'uv.lock')) {
            Invoke-Native uv @('run', '--frozen', '--with', 'pytest', 'python', '-m', 'pytest', '-q')
        }
        else {
            [Console]::Error.WriteLine("pytest is not installed; $Dir tests were not run")
            [Console]::Error.WriteLine('install pytest or uv so a missing Python suite cannot pass silently')
            exit 1
        }
    }
    finally { Pop-Location }
}

if (Test-Path -PathType Container 'python') { Invoke-PythonSuite 'python' }
if (Test-Path -PathType Container 'python-cli') { Invoke-PythonSuite 'python-cli' }
