param(
    [Parameter(Mandatory=$true)][string]$Pdb,
    [int]$TimeoutSeconds = 180
)
$ErrorActionPreference='Stop'
$root=(Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$sandboxExe="$env:SystemRoot\System32\WindowsSandbox.exe"
if(-not(Test-Path $sandboxExe)){throw 'Windows Sandbox is unavailable; conhost startup injection must remain isolated'}
if(-not(Test-Path $Pdb)){throw "matching conhost.pdb not found: $Pdb"}
$native=Join-Path $root 'target/native-windows/Release'
$shellglass=Join-Path $root 'target/debug/shellglass-wt-tap.exe'
foreach($file in @("$native/shellglass-profile.exe","$native/shellglass-conhost-adapter.dll","$native/shellglass-conhost-fault-adapter.dll","$native/shellglass-inject.exe","$native/shellglass-conhost-client-fixture.exe",$shellglass)){
 if(-not(Test-Path $file)){throw "build artifact missing: $file"}
}
$work=Join-Path $root "target/conhost-sandbox-e2e-$PID"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item $work -ItemType Directory|Out-Null
Copy-Item "$native/shellglass-conhost-adapter.dll","$native/shellglass-conhost-fault-adapter.dll","$native/shellglass-inject.exe","$native/shellglass-conhost-client-fixture.exe" -Destination $work
Copy-Item $shellglass (Join-Path $work 'shellglass.exe')
Copy-Item "$env:SystemRoot\System32\MSVCP140.dll","$env:SystemRoot\System32\VCRUNTIME140.dll","$env:SystemRoot\System32\VCRUNTIME140_1.dll" -Destination $work
Copy-Item (Join-Path $PSScriptRoot 'test-conhost-sandbox-guest.ps1') (Join-Path $work 'guest.ps1')
$profile=Join-Path $work 'shellglass-conhost-adapter.sgnp'
& "$native/shellglass-profile.exe" "$env:SystemRoot\System32\conhost.exe" conhost_10_0_19045 $profile (Resolve-Path $Pdb).Path
if($LASTEXITCODE-or-not(Test-Path $profile)){throw 'system conhost failed the fail-closed ABI profile gate'}
$escaped=[Security.SecurityElement]::Escape($work)
$config=Join-Path $work 'test.wsb'
@"
<Configuration>
 <MappedFolders><MappedFolder><HostFolder>$escaped</HostFolder><SandboxFolder>C:\work</SandboxFolder><ReadOnly>false</ReadOnly></MappedFolder></MappedFolders>
 <Networking>Disable</Networking><VGpu>Disable</VGpu>
 <LogonCommand><Command>powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\work\guest.ps1</Command></LogonCommand>
</Configuration>
"@|Set-Content $config -Encoding utf8
$launcher=Start-Process -PassThru $sandboxExe -ArgumentList $config
$resultPath=Join-Path $work 'result.json';$deadline=[DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
try{
 while(-not(Test-Path $resultPath)-and[DateTime]::UtcNow-lt$deadline-and-not$launcher.HasExited){Start-Sleep 2}
 if(-not(Test-Path $resultPath)){throw 'isolated conhost test timed out without a result'}
 Start-Sleep 1;$result=Get-Content $resultPath -Raw|ConvertFrom-Json
 if(-not$result.passed){throw "isolated conhost test failed: $($result.detail)"}
 Write-Host "real conhost render-tap E2E: OK - $($result.detail)"
}finally{
 Get-Process WindowsSandboxClient -ErrorAction SilentlyContinue|Stop-Process -Force -ErrorAction SilentlyContinue
 if(-not$launcher.HasExited){Stop-Process $launcher.Id -Force -ErrorAction SilentlyContinue}
}
