param(
    [Parameter(Mandatory=$true)][string]$Work,
    [string]$NativeBuildDir='',
    [int]$TimeoutSeconds=760,
    [switch]$CloseOnSuccess
)
# Re-runs the combined WT gates in a Sandbox left alive by
# test-wt-sandbox.ps1 -IncludeLifecycle -KeepSandboxOpen. Updated scripts and
# binaries are copied through the existing writable mapping; Hyper-V is not
# restarted.
$ErrorActionPreference='Stop'
$work=(Resolve-Path $Work).Path
$root=(Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if(-not(Get-Process WindowsSandboxClient -ErrorAction SilentlyContinue)){
    throw 'no reusable Windows Sandbox is running'
}
if(-not(Test-Path (Join-Path $work 'guest.ps1'))-or-not(Test-Path (Join-Path $work 'TerminalPackage'))){
    throw "not a persistent shellglass WT work tree: $work"
}
$native=if($NativeBuildDir){(Resolve-Path $NativeBuildDir).Path}else{Join-Path $root 'target/native-windows/Release'}
Copy-Item (Join-Path $PSScriptRoot 'test-wt-sandbox-guest.ps1') (Join-Path $work 'aggregate-body.ps1') -Force
Copy-Item (Join-Path $PSScriptRoot 'test-wt-lifecycle-sandbox-guest.ps1') (Join-Path $work 'lifecycle-body.ps1') -Force
# Older persistent keepers dot-source aggregate.ps1/lifecycle.ps1 in one CLR
# runspace. Wrappers force fresh child runspaces so repeated Add-Type declarations
# and native test state cannot collide without rebooting the Sandbox.
'& powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\work\aggregate-body.ps1 @args'|Set-Content (Join-Path $work 'aggregate.ps1') -Encoding ascii
'& powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\work\lifecycle-body.ps1 @args'|Set-Content (Join-Path $work 'lifecycle.ps1') -Encoding ascii
Copy-Item "$native/shellglass-wt-adapter.dll","$native/shellglass-wt-fault-adapter.dll","$native/shellglass-inject.exe","$native/shellglass-wt-fixture.exe" -Destination $work -Force
Copy-Item (Join-Path $root 'target/debug/shellglass-wt-tap.exe') (Join-Path $work 'shellglass.exe') -Force
Copy-Item (Join-Path $root '../shellglass/target/debug/shellglass.exe') (Join-Path $work 'shellglass-stock.exe') -Force

$result=Join-Path $work 'combined-result.json'
Remove-Item $result -Force -ErrorAction SilentlyContinue
[DateTime]::UtcNow.ToString('O')|Set-Content (Join-Path $work 'rerun.request') -Encoding ascii
$deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
while(-not(Test-Path $result)-and[DateTime]::UtcNow-lt$deadline){Start-Sleep -Seconds 2}
if(-not(Test-Path $result)){
    $stage=Join-Path $work 'stage.txt'
    $last=if(Test-Path $stage){(Get-Content $stage -Raw).Trim()}else{'unknown'}
    throw "persistent Sandbox rerun timed out (last stage: $last)"
}
Start-Sleep 1
$value=Get-Content $result -Raw|ConvertFrom-Json
if(-not$value.passed){throw "persistent Sandbox WT gate failed: $($value.detail)"}
Write-Host "persistent Sandbox WT render-tap + lifecycle E2E: OK - $($value.detail)"
if($CloseOnSuccess){
    Get-Process WindowsSandboxClient -ErrorAction SilentlyContinue|Stop-Process -Force -ErrorAction SilentlyContinue
}
