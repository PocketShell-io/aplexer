# PR artifacts only: build the maintained Windows release recipe without
# publishing or installing outside the isolated runner checkout.
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
$compiler = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe' | Select-Object -Last 1
if (-not $compiler) { throw 'MSVC compiler provenance unavailable' }
$linker = Join-Path (Split-Path $compiler) 'link.exe'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $linker
Invoke-Recorded 'build-tools' 'python' @('-m', 'pip', 'install', '--disable-pip-version-check', 'maturin==1.14.1', 'pytest==9.0.2')
Invoke-Recorded 'native-exe' 'cargo' @('build', '--locked', '--release', '--bins', '--target', 'x86_64-pc-windows-msvc')
$exe = (Resolve-Path 'target/x86_64-pc-windows-msvc/release/aplexer.exe').Path
Invoke-Recorded 'exe-version' $exe @('--version')
New-Item -ItemType Directory (Join-Path $outDir 'native') | Out-Null
Copy-Item $exe (Join-Path $outDir 'native/aplexer.exe')
Invoke-Recorded 'cli-wheel' 'python' @('scripts/build-wheels.py', '--platform', 'windows-amd64', '--binaries-dir', (Join-Path $outDir 'native'), '--output-dir', (Join-Path $outDir 'cli-dist'))
Push-Location python
try {
    Invoke-Recorded 'client-wheel' 'python' @('-m', 'maturin', 'build', '--release', '--locked', '--target', 'x86_64-pc-windows-msvc', '--out', (Join-Path $outDir 'client-dist'))
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
    target='x86_64-pc-windows-msvc'; status='PASS';
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
