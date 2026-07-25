param(
    [Parameter(Mandatory=$true)][string]$Pdb,
    [string]$Version = '1.24.11911.0',
    [ValidateSet('wt_1_24','wt_1_24_11321')][string]$Family = 'wt_1_24',
    [string]$PackagePath = '',
    [string]$NativeBuildDir = '',
    [string]$TapPath = '',
    [ValidateRange(30,300)][int]$StressSeconds = 30,
    [switch]$IncludeLifecycle,
    [switch]$IncludeOperator,
    [switch]$KeepSandboxOpen,
    [int]$TimeoutSeconds = 480
)
$ErrorActionPreference = 'Stop'
if($KeepSandboxOpen-and-not$IncludeLifecycle){throw '-KeepSandboxOpen requires -IncludeLifecycle'}
if($IncludeOperator-and-not$IncludeLifecycle){throw '-IncludeOperator requires -IncludeLifecycle'}
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$sandboxExe = "$env:SystemRoot\System32\WindowsSandbox.exe"
if (-not (Test-Path $sandboxExe)) { throw 'Windows Sandbox is unavailable; never run this test against the active terminal host' }
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
$native = if($NativeBuildDir){(Resolve-Path $NativeBuildDir).Path}else{Join-Path $root 'target/native-windows/Release'}
$tap = if($TapPath){(Resolve-Path $TapPath).Path}else{Join-Path $root 'target/debug/shellglass-wt-tap.exe'}
$stockShellglass = Join-Path $root '../shellglass/target/debug/shellglass.exe'
foreach ($file in @("$native/shellglass-profile.exe", "$native/shellglass-wt-adapter.dll", "$native/shellglass-wt-fault-adapter.dll", "$native/shellglass-inject.exe", "$native/shellglass-wt-fixture.exe", $tap, $stockShellglass)) {
    if (-not (Test-Path $file)) { throw "build artifact missing: $file" }
}

$work = Join-Path $root "target/wt-sandbox-e2e-$PID"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item $work -ItemType Directory | Out-Null
Copy-Item -LiteralPath $packageSource -Destination (Join-Path $work 'TerminalPackage') -Recurse
Copy-Item "$native/shellglass-wt-adapter.dll","$native/shellglass-wt-fault-adapter.dll","$native/shellglass-inject.exe","$native/shellglass-wt-fixture.exe" -Destination $work
Copy-Item $tap (Join-Path $work 'shellglass.exe')
Copy-Item $stockShellglass (Join-Path $work 'shellglass-stock.exe')
Copy-Item "$env:SystemRoot\System32\MSVCP140.dll","$env:SystemRoot\System32\VCRUNTIME140.dll","$env:SystemRoot\System32\VCRUNTIME140_1.dll" -Destination $work
if($IncludeLifecycle){
    Copy-Item (Join-Path $PSScriptRoot 'test-wt-sandbox-guest.ps1') (Join-Path $work 'aggregate.ps1')
    Copy-Item (Join-Path $PSScriptRoot 'test-wt-lifecycle-sandbox-guest.ps1') (Join-Path $work 'lifecycle.ps1')
    Copy-Item (Join-Path $PSScriptRoot 'test-wt-combined-sandbox-guest.ps1') (Join-Path $work 'guest.ps1')
    if($IncludeOperator){
        Copy-Item (Join-Path $PSScriptRoot 'test-start-wt-stream-sandbox-guest.ps1') (Join-Path $work 'operator.ps1')
        New-Item (Join-Path $work 'native/windows') -ItemType Directory -Force|Out-Null
        Copy-Item (Join-Path $PSScriptRoot 'start-wt-stream.ps1') (Join-Path $work 'native/windows/start-wt-stream.ps1')
        New-Item (Join-Path $work 'target/native-windows/Release') -ItemType Directory -Force|Out-Null
        $operatorNative=Join-Path $work 'target/native-windows/Release'
        Copy-Item "$native/shellglass-profile.exe","$native/shellglass-wt-adapter.dll","$native/shellglass-inject.exe" -Destination $operatorNative
        $operatorBin=Join-Path $work 'target/debug'
        New-Item $operatorBin -ItemType Directory -Force|Out-Null
        Copy-Item $tap (Join-Path $operatorBin 'shellglass-wt-tap.exe')
        # The clean Sandbox lacks the desktop VC runtime. Unlike the root-level
        # aggregate copies, launcher artifacts live in nested build directories,
        # so put the runtime beside each executable/DLL as Windows expects.
        $vcRuntime=@("$env:SystemRoot\System32\MSVCP140.dll","$env:SystemRoot\System32\VCRUNTIME140.dll","$env:SystemRoot\System32\VCRUNTIME140_1.dll")
        Copy-Item $vcRuntime -Destination $operatorNative
        Copy-Item $vcRuntime -Destination $operatorBin
    }
}else{
    Copy-Item (Join-Path $PSScriptRoot 'test-wt-sandbox-guest.ps1') (Join-Path $work 'guest.ps1')
}

# The clean Sandbox image may not carry Microsoft.UI.Xaml 2.8. Winget provides
# the signed framework dependency; the WT package itself is copied byte-for-byte
# from the exact host package matched by the profile.
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
if($IncludeOperator){Copy-Item $profile (Join-Path $work 'prepared.sgnp') -Force}

$escaped = [System.Security.SecurityElement]::Escape($work)
$persistentArg=if($KeepSandboxOpen){' -Persistent'}else{''}
$operatorArg=if($IncludeOperator){' -IncludeOperator'}else{''}
$config = Join-Path $work 'test.wsb'
@"
<Configuration>
  <MappedFolders><MappedFolder><HostFolder>$escaped</HostFolder><SandboxFolder>C:\work</SandboxFolder><ReadOnly>false</ReadOnly></MappedFolder></MappedFolders>
  <!-- Host vGPU virtualization has caused VIDEO_SCHEDULER_INTERNAL_ERROR
       bugchecks under repeated WT Sandbox runs. WARP is sufficient for these
       capture semantics and keeps this disposable-target gate off the host GPU. -->
  <Networking>Default</Networking><VGpu>Disable</VGpu>
  <LogonCommand><Command>powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\work\guest.ps1 -ExpectedVersion $Version -StressSeconds $StressSeconds$persistentArg$operatorArg</Command></LogonCommand>
</Configuration>
"@ | Set-Content $config -Encoding utf8

# This is the sole launch boundary: all injection happens inside the disposable
# VM. A target crash can produce a dump but cannot terminate this terminal or
# any other agent sharing it.
$launcher = Start-Process -PassThru $sandboxExe -ArgumentList $config
$resultPath = Join-Path $work $(if($IncludeLifecycle){'combined-result.json'}else{'result.json'})
$deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
try {
    while (-not (Test-Path $resultPath) -and [DateTime]::UtcNow -lt $deadline -and -not $launcher.HasExited) {
        Start-Sleep 2
    }
    if (-not (Test-Path $resultPath)) {
        if (Test-Path (Join-Path $work 'guest-started.txt')) {
            $stagePath=Join-Path $work 'stage.txt'
            $lastStage=if(Test-Path $stagePath){(Get-Content $stagePath -Raw).Trim()}else{'unknown'}
            throw "isolated WT guest started but timed out without a result (last stage: $lastStage)"
        }
        throw 'Windows Sandbox never started the mapped guest command before timeout'
    }
    Start-Sleep 1
    $result = Get-Content $resultPath -Raw | ConvertFrom-Json
    if (-not $result.passed) { throw "isolated WT test failed: $($result.detail)" }
    $label=if($IncludeOperator){'render-tap + lifecycle + operator launcher'}elseif($IncludeLifecycle){'render-tap + lifecycle'}else{'render-tap'}
    Write-Host "real stock Windows Terminal ${label} E2E: OK - $($result.detail)"
} finally {
    if($KeepSandboxOpen){
        Write-Host "Sandbox kept open for reuse; mapped work tree: $work"
    }else{
        Get-Process WindowsSandboxClient -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        if (-not $launcher.HasExited) { Stop-Process $launcher.Id -Force -ErrorAction SilentlyContinue }
    }
}
exit 0
