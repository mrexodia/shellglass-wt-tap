param(
    [Parameter(Mandatory=$true)][string]$Pdb,
    [string]$Version = '1.24.11911.0',
    [ValidateSet('wt_1_24','wt_1_24_11321')][string]$Family = 'wt_1_24',
    [string]$PackagePath = '',
    [int]$TimeoutSeconds = 300
)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$sandboxExe = "$env:SystemRoot\System32\WindowsSandbox.exe"
if (-not (Test-Path $sandboxExe)) { throw 'Windows Sandbox is unavailable; never run lifecycle injection against the active terminal host' }
if (-not (Test-Path $Pdb)) { throw "matching Microsoft.Terminal.Control.pdb not found: $Pdb" }

$package = Get-AppxPackage Microsoft.WindowsTerminal
if ($PackagePath) {
    $packageSource = (Resolve-Path $PackagePath).Path
} else {
    if (-not $package -or $package.Version -ne $Version) {
        throw "this verified family requires installed Microsoft.WindowsTerminal $Version or -PackagePath"
    }
    $packageSource = $package.InstallLocation
}
$native = Join-Path $root 'target/native-windows/Release'
$tap = Join-Path $root 'target/debug/shellglass-wt-tap.exe'
foreach ($file in @("$native/shellglass-profile.exe", "$native/shellglass-wt-adapter.dll", "$native/shellglass-inject.exe", "$native/shellglass-wt-fixture.exe", $tap)) {
    if (-not (Test-Path $file)) { throw "build artifact missing: $file" }
}

$work = Join-Path $root "target/wt-lifecycle-sandbox-e2e-$PID"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item $work -ItemType Directory | Out-Null
Copy-Item -LiteralPath $packageSource -Destination (Join-Path $work 'TerminalPackage') -Recurse
Copy-Item "$native/shellglass-wt-adapter.dll","$native/shellglass-inject.exe","$native/shellglass-wt-fixture.exe" -Destination $work
Copy-Item $tap (Join-Path $work 'shellglass.exe')
Copy-Item "$env:SystemRoot\System32\MSVCP140.dll","$env:SystemRoot\System32\VCRUNTIME140.dll","$env:SystemRoot\System32\VCRUNTIME140_1.dll" -Destination $work
Copy-Item (Join-Path $PSScriptRoot 'test-wt-lifecycle-sandbox-guest.ps1') (Join-Path $work 'guest.ps1')

$download = Join-Path $work 'download'
winget download --id Microsoft.WindowsTerminal --version $Version --architecture x64 `
    --download-directory $download --accept-source-agreements --accept-package-agreements | Out-Host
if ($LASTEXITCODE) { throw "winget dependency download failed ($LASTEXITCODE)" }
$dependencies = Get-ChildItem (Join-Path $download 'Dependencies') -Filter '*.msix' -ErrorAction Stop
if (-not $dependencies) { throw 'winget did not provide the Microsoft.UI.Xaml dependency' }
New-Item (Join-Path $work 'Dependencies') -ItemType Directory | Out-Null
$dependencies | Copy-Item -Destination (Join-Path $work 'Dependencies')

$module = Join-Path $packageSource 'Microsoft.Terminal.Control.dll'
$profile = Join-Path $work 'shellglass-wt-adapter.sgnp'
& "$native/shellglass-profile.exe" $module $Family $profile (Resolve-Path $Pdb).Path
if ($LASTEXITCODE -or -not (Test-Path $profile)) { throw 'stock WT failed the fail-closed ABI profile gate' }

$escaped = [System.Security.SecurityElement]::Escape($work)
$config = Join-Path $work 'test.wsb'
@"
<Configuration>
  <MappedFolders><MappedFolder><HostFolder>$escaped</HostFolder><SandboxFolder>C:\work</SandboxFolder><ReadOnly>false</ReadOnly></MappedFolder></MappedFolders>
  <!-- Use WARP: repeated Sandbox vGPU runs have triggered host bugcheck 0x119. -->
  <Networking>Default</Networking><VGpu>Disable</VGpu>
  <LogonCommand><Command>powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\work\guest.ps1 -ExpectedVersion $Version</Command></LogonCommand>
</Configuration>
"@ | Set-Content $config -Encoding utf8

$launcher = Start-Process -PassThru $sandboxExe -ArgumentList $config
$resultPath = Join-Path $work 'result.json'
$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
try {
    while (-not (Test-Path $resultPath) -and [DateTime]::UtcNow -lt $deadline -and -not $launcher.HasExited) { Start-Sleep 2 }
    if (-not (Test-Path $resultPath)) {
        if (Test-Path (Join-Path $work 'guest-started.txt')) { throw 'isolated WT lifecycle guest started but timed out' }
        throw 'Windows Sandbox never started the lifecycle guest command before timeout'
    }
    Start-Sleep 1
    $result = Get-Content $resultPath -Raw | ConvertFrom-Json
    if (-not $result.passed) { throw "isolated WT lifecycle test failed: $($result.detail)" }
    Write-Host "real stock Windows Terminal lifecycle E2E: OK - $($result.detail)"
} finally {
    Get-Process WindowsSandboxClient -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if (-not $launcher.HasExited) { Stop-Process $launcher.Id -Force -ErrorAction SilentlyContinue }
}
exit 0
