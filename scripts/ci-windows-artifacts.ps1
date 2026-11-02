# PR artifacts only: build the maintained Windows release recipe without
# publishing or installing outside the isolated runner checkout.
#
#   scripts\ci-windows-artifacts.ps1                              # x64 (default)
#   scripts\ci-windows-artifacts.ps1 -Target aarch64-pc-windows-msvc
#
# The target must be the native host architecture of the runner: the script
# executes the built aplexer.exe and runs the Python suites against it, so it
# is a native build-and-smoke, never a cross-compile proof.
[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target = 'x86_64-pc-windows-msvc'
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$outDir = Join-Path (Get-Location) 'windows-artifacts'
New-Item -ItemType Directory -Path $outDir | Out-Null
$receipts = [System.Collections.Generic.List[object]]::new()
function Invoke-Recorded {
    param([string]$Name, [string]$Exe, [string[]]$Arguments)
    $start = [DateTime]::UtcNow.ToString('o')
    $log = Join-Path $outDir "$Name.log"
    & $Exe @Arguments 2>&1 | Tee-Object -FilePath $log | Out-Host
    $code = $LASTEXITCODE
    $receipts.Add(@{name=$Name; argv=@($Exe)+$Arguments; cwd=(Get-Location).Path;
        started=$start; finished=[DateTime]::UtcNow.ToString('o'); exit=$code; log="$Name.log"})
    $receipts | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 (Join-Path $outDir 'commands.json')
    if ($code -ne 0) { throw "$Name exited $code" }
}
Invoke-Recorded 'rustc' 'rustc' @('-vV')
Invoke-Recorded 'cargo' 'cargo' @('-V')
Invoke-Recorded 'python' 'python' @('--version')
$head = (& git rev-parse HEAD).Trim()
$tree = (& git rev-parse 'HEAD^{tree}').Trim()
Invoke-Recorded 'source-archive' 'git' @('archive', '--format=tar', '--output=windows-artifacts/source.tar', $head)
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if ($Target -eq 'x86_64-pc-windows-msvc') {
    $compiler = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe' | Select-Object -Last 1
}
else {
    # Native ARM64-hosted cl.exe first (windows-11-arm ships the ARM64 build
    # tools); the x64-hosted ARM64 cross-compiler as fallback (runs under
    # emulation but still emits aarch64 objects). Provenance records whichever
    # cl.exe actually produced the artifacts.
    $compiler = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.ARM64 -find 'VC\Tools\MSVC\*\bin\HostARM64\arm64\cl.exe' | Select-Object -Last 1
    if (-not $compiler) {
        $compiler = & $vswhere -latest -products '*' -find 'VC\Tools\MSVC\*\bin\Hostx64\arm64\cl.exe' | Select-Object -Last 1
    }
}
if (-not $compiler) { throw 'MSVC compiler provenance unavailable' }
$linker = Join-Path (Split-Path $compiler) 'link.exe'
$linkerEnv = 'CARGO_TARGET_' + ($Target.ToUpperInvariant() -replace '-', '_') + '_LINKER'
Set-Item "env:$linkerEnv" $linker
Invoke-Recorded 'build-tools' 'python' @('-m', 'pip', 'install', '--disable-pip-version-check', 'maturin==1.14.1', 'pytest==9.0.2')
Invoke-Recorded 'native-exe' 'cargo' @('build', '--locked', '--release', '--bins', '--target', $Target)
$exe = (Resolve-Path "target/$Target/release/aplexer.exe").Path
Invoke-Recorded 'exe-version' $exe @('--version')
New-Item -ItemType Directory (Join-Path $outDir 'native') | Out-Null
Copy-Item $exe (Join-Path $outDir 'native/aplexer.exe')
$platform = if ($Target -like 'x86_64*') { 'windows-amd64' } else { 'windows-arm64' }
Invoke-Recorded 'cli-wheel' 'python' @('scripts/build-wheels.py', '--platform', $platform, '--binaries-dir', (Join-Path $outDir 'native'), '--output-dir', (Join-Path $outDir 'cli-dist'))
Push-Location python
try {
    Invoke-Recorded 'client-wheel' 'python' @('-m', 'maturin', 'build', '--release', '--locked', '--target', $Target, '--out', (Join-Path $outDir 'client-dist'))
} finally { Pop-Location }
Invoke-Recorded 'cold-venv' 'python' @('-m', 'venv', 'wheel-test')
$py = (Resolve-Path 'wheel-test/Scripts/python.exe').Path
$cli = @(Get-ChildItem (Join-Path $outDir 'cli-dist') -Filter *.whl)
$client = @(Get-ChildItem (Join-Path $outDir 'client-dist') -Filter *.whl)
if ($cli.Count -ne 1 -or $client.Count -ne 1) { throw 'Expected exact wheel pair' }
Invoke-Recorded 'cold-wheel-install' $py @('-m', 'pip', 'install', '--no-index', '--find-links', (Join-Path $outDir 'client-dist'), $cli[0].FullName)
Invoke-Recorded 'cold-pytest-install' $py @('-m', 'pip', 'install', '--disable-pip-version-check', 'pytest==9.0.2')
$env:APLEXER_RUN_IN_PLACE = '1'
$env:APLEXER_WORKER = $exe
Invoke-Recorded 'cold-cli-version' (Resolve-Path 'wheel-test/Scripts/aplexer.exe').Path @('--version')
Invoke-Recorded 'cold-alias-version' (Resolve-Path 'wheel-test/Scripts/a.exe').Path @('--version')
Invoke-Recorded 'cold-native-controls' $py @('-m', 'pytest', '-q', 'python/tests')
Copy-Item 'wheel-test/Lib/site-packages/aplexer/_native.pyd' (Join-Path $outDir 'native/_native.pyd')
@{head=$head; tree=$tree; run=$env:GITHUB_RUN_ID; attempt=$env:GITHUB_RUN_ATTEMPT;
    runner=$env:ImageVersion; os=[Environment]::OSVersion.VersionString;
    target=$Target; platform=$platform; status='PASS';
    worker=@{path=$exe; sha256=(Get-FileHash $exe -Algorithm SHA256).Hash.ToLower(); selection='APLEXER_WORKER for cold-installed native client controls'};
    cl=@{path=$compiler; version=(Get-Item $compiler).VersionInfo.FileVersion; sha256=(Get-FileHash $compiler -Algorithm SHA256).Hash.ToLower()};
    linker=@{path=$linker; version=(Get-Item $linker).VersionInfo.FileVersion; sha256=(Get-FileHash $linker -Algorithm SHA256).Hash.ToLower()};
    cargoLockSha256=(Get-FileHash Cargo.lock -Algorithm SHA256).Hash.ToLower();
    source='Fresh native pair built from this exact HEAD; no inherited payload'} |
    ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 (Join-Path $outDir 'provenance.json')
$hashes = @{}
Get-ChildItem $outDir -Recurse -File | ForEach-Object {
    $hashes[[IO.Path]::GetRelativePath($outDir, $_.FullName)] = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower()
}
$hashes | ConvertTo-Json -Depth 10 | Set-Content -Encoding utf8 (Join-Path $outDir 'SHA256.json')
