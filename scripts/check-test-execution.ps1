# Run a cargo test command and fail unless it actually executed tests.
#
# PowerShell port of scripts/check-test-execution.sh; see that file for the
# full rationale. `cargo test` reports "test result: ok. 0 passed; 0 failed;
# ...; 12 filtered out" with exit status 0, which proves nothing, so assert on
# the executed count (passed + failed) instead of on the exit status:
#
#     scripts/check-test-execution.ps1 -Min 40 -- cargo test --release
#     scripts/check-test-execution.ps1 -SelfTest
#
# -Min is a floor, not an equality. `filtered out` and `ignored` never count.
[CmdletBinding(PositionalBinding = $false)]
param(
    [int]$Min = -1,
    [switch]$SelfTest,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Command
)

$ErrorActionPreference = 'Stop'

function Show-Usage {
    [Console]::Error.WriteLine('usage: check-test-execution.ps1 -Min N -- <command...>')
    [Console]::Error.WriteLine('       check-test-execution.ps1 -SelfTest')
    exit 2
}

# Sum of the executed tests reported by every `test result:` line.
function Get-ExecutedCount {
    param([string[]]$Lines)
    $total = 0
    foreach ($line in $Lines) {
        if ($line -match '^test result:') {
            foreach ($m in [regex]::Matches($line, '(\d+) (passed|failed)\b')) {
                $total += [int]$m.Groups[1].Value
            }
        }
    }
    return $total
}

function Invoke-SelfTest {
    $script:failures = 0
    function Check([string]$Label, [int]$Expected, [string]$Sample) {
        $actual = Get-ExecutedCount ($Sample -split "`r?`n")
        if ($actual -ne $Expected) {
            [Console]::Error.WriteLine("self-test FAILED: ${Label}: expected $Expected, counted $actual")
            $script:failures++
        }
    }

    Check 'filtered-out run counts as zero' 0 `
        'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s'
    Check 'a real run counts its tests' 12 `
        'test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.26s'
    Check 'failures count as executed, ignores do not' 2 `
        'test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 11 filtered out; finished in 5.06s'
    Check 'an all-ignored suite counts as zero' 0 `
        'test result: ok. 0 passed; 0 failed; 12 ignored; 0 measured; 0 filtered out; finished in 0.00s'
    Check 'multi-binary runs add up' 17 (
        "test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.26s`n" +
        'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.10s')
    Check 'no test result line at all counts as zero' 0 'error: could not compile'

    if ($script:failures -ne 0) {
        [Console]::Error.WriteLine("check-test-execution.ps1 self-test: $($script:failures) failure(s)")
        exit 1
    }
    Write-Host 'check-test-execution.ps1 self-test: ok'
}

if ($SelfTest) {
    Invoke-SelfTest
    exit 0
}

if ($Min -lt 0) { Show-Usage }
$cmd = @($Command)
if ($cmd.Count -gt 0 -and $cmd[0] -eq '--') { $cmd = $cmd[1..($cmd.Count - 1)] }
if ($cmd.Count -eq 0) { Show-Usage }

$captured = New-Object System.Collections.Generic.List[string]
$exe = $cmd[0]
$rest = @()
if ($cmd.Count -gt 1) { $rest = $cmd[1..($cmd.Count - 1)] }

# Do not let stderr from the native command become a terminating error.
$ErrorActionPreference = 'Continue'
& $exe @rest 2>&1 | ForEach-Object {
    $text = $_.ToString()
    $captured.Add($text)
    Write-Host $text
}
$status = $LASTEXITCODE
$ErrorActionPreference = 'Stop'

if ($status -ne 0) {
    [Console]::Error.WriteLine("check-test-execution.ps1: command failed with status $status")
    exit $status
}

$executed = Get-ExecutedCount $captured.ToArray()
if ($executed -lt $Min) {
    [Console]::Error.WriteLine("check-test-execution.ps1: only $executed test(s) executed, expected at least $Min")
    [Console]::Error.WriteLine('check-test-execution.ps1: a green run that executed nothing is not a pass')
    exit 1
}
Write-Host "check-test-execution.ps1: $executed test(s) executed (floor $Min)"
