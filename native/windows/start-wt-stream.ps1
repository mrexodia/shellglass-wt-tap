[CmdletBinding(DefaultParameterSetName='Local')]
param(
    [Parameter(Mandatory=$true,ParameterSetName='Push')][string]$Hub,
    [Parameter(Mandatory=$true,ParameterSetName='Push')][string]$Key,
    [Parameter(ParameterSetName='Local')][string]$Bind='127.0.0.1:8080',
    [string]$Pdb='',
    [string]$PreparedProfile='',
    [switch]$NewTab,
    [switch]$PrepareOnly
)
# Operator convenience launcher for the two verified x64 WT families. This is
# intentionally never called by automated tests: it injects the user's real WT.
# Existing controls recover lazily on their first post-injection focus gain/loss.
# -NewTab remains available as a convenient way to force a transition.
$ErrorActionPreference='Stop'
$root=(Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if([Runtime.InteropServices.RuntimeInformation]::OSArchitecture-ne[Runtime.InteropServices.Architecture]::X64){
    throw 'the verified personal-deployment adapters require x64 Windows'
}
$package=Get-AppxPackage Microsoft.WindowsTerminal|Sort-Object Version -Descending|Select-Object -First 1
if(-not$package){throw 'Microsoft.WindowsTerminal is not installed'}
$family=switch([string]$package.Version){
    '1.24.11911.0' {'wt_1_24';break}
    '1.24.11321.0' {'wt_1_24_11321';break}
    default {throw "WT $($package.Version) is unknown and will not be injected; only 1.24.11911.0 and 1.24.11321.0 are verified"}
}
$native=Join-Path $root 'target/native-windows/Release'
$profileTool=Join-Path $native 'shellglass-profile.exe'
$adapter=Join-Path $native 'shellglass-wt-adapter.dll'
$injector=Join-Path $native 'shellglass-inject.exe'
$tap=@((Join-Path $root 'target/release/shellglass-wt-tap.exe'),(Join-Path $root 'target/debug/shellglass-wt-tap.exe'))|Where-Object{Test-Path $_}|Select-Object -First 1
foreach($artifact in @($profileTool,$adapter,$injector,$tap)){
    if(-not$artifact-or-not(Test-Path $artifact)){throw "required build artifact is missing: $artifact`nBuild with: cmake --build target/native-windows --config Release; cargo build --release"}
}
$module=Join-Path $package.InstallLocation 'Microsoft.Terminal.Control.dll'
$profile=Join-Path $native 'shellglass-wt-adapter.sgnp'
$matched=$null
if($PreparedProfile){
    $prepared=(Resolve-Path $PreparedProfile -ErrorAction Stop).Path
    if($prepared -ne $profile){Copy-Item $prepared $profile -Force}
    $matched="prepared profile $prepared (the adapter will reverify it in-target)"
}else{
    $candidates=@()
    if($Pdb){$candidates+=@(Resolve-Path $Pdb -ErrorAction Stop).Path}
    else{
        $repoPdb=Join-Path $root 'target/symbol-research/Microsoft.Terminal.Control.pdb'
        $parentPdb=Join-Path $root '../shellglass/target/symbol-research/Microsoft.Terminal.Control.pdb'
        if(Test-Path $repoPdb){$candidates+=(Resolve-Path $repoPdb).Path}
        if(Test-Path $parentPdb){$candidates+=(Resolve-Path $parentPdb).Path}
        if(Test-Path 'C:\Symbols\Microsoft.Terminal.Control.pdb'){
            $candidates+=@(Get-ChildItem 'C:\Symbols\Microsoft.Terminal.Control.pdb' -Filter 'Microsoft.Terminal.Control.pdb' -Recurse -File -ErrorAction SilentlyContinue|ForEach-Object FullName)
        }
    }
    $candidates=@($candidates|Select-Object -Unique)
    if(-not$candidates){throw 'matching WT PDB not found; pass -Pdb <Microsoft.Terminal.Control.pdb>'}
    foreach($candidate in $candidates){
        $profileOut=Join-Path $env:TEMP 'shellglass-profile.out';$profileErr=Join-Path $env:TEMP 'shellglass-profile.err'
        Remove-Item $profileOut,$profileErr -Force -ErrorAction SilentlyContinue
        $profileProcess=Start-Process -PassThru -Wait -WindowStyle Hidden $profileTool `
            -ArgumentList @("`"$module`"",$family,"`"$profile`"","`"$candidate`"") `
            -RedirectStandardOutput $profileOut -RedirectStandardError $profileErr
        if($profileProcess.ExitCode -eq 0 -and (Test-Path $profile)){$matched=$candidate;break}
    }
    if(-not$matched){throw "none of the candidate PDBs matched WT $($package.Version); pass its exact Microsoft.Terminal.Control.pdb"}
}
Write-Host "Verified WT $($package.Version) as $family using $matched"
if($PrepareOnly){
    Write-Host "Prepared fail-closed profile: $profile"
    return
}

$server=$null
$startedWorker=$false
if($PSCmdlet.ParameterSetName-eq'Push'){
    $statusOut=Join-Path $env:TEMP 'shellglass-stream-status.out';$statusErr=Join-Path $env:TEMP 'shellglass-stream-status.err'
    Remove-Item $statusOut,$statusErr -Force -ErrorAction SilentlyContinue
    $statusProcess=Start-Process -PassThru -Wait -WindowStyle Hidden $tap -ArgumentList @('stream','status') `
        -RedirectStandardOutput $statusOut -RedirectStandardError $statusErr
    if($statusProcess.ExitCode -ne 0){
        # Clap accepts SHELLGLASS_KEY directly; avoid placing the long-lived
        # capability in either the launcher or detached worker command line.
        $priorKey=[Environment]::GetEnvironmentVariable('SHELLGLASS_KEY','Process')
        try{
            [Environment]::SetEnvironmentVariable('SHELLGLASS_KEY',$Key,'Process')
            & $tap stream start --hub $Hub
            if($LASTEXITCODE){throw 'detached stream worker did not start'}
        }finally{
            [Environment]::SetEnvironmentVariable('SHELLGLASS_KEY',$priorKey,'Process')
        }
        $startedWorker=$true
    }else{Write-Host 'A detached stream worker is already running; reusing it.'}
}else{
    $server=Start-Process -PassThru -WindowStyle Hidden $tap `
        -ArgumentList @('serve','--bind',$Bind) `
        -RedirectStandardOutput (Join-Path $env:TEMP 'shellglass-wt-serve.out') `
        -RedirectStandardError (Join-Path $env:TEMP 'shellglass-wt-serve.err')
    Start-Sleep 1
    if($server.HasExited){
        $server.WaitForExit();Start-Sleep -Milliseconds 100
        $serverErr=Get-Content (Join-Path $env:TEMP 'shellglass-wt-serve.err') -Raw -ErrorAction SilentlyContinue
        $serverOut=Get-Content (Join-Path $env:TEMP 'shellglass-wt-serve.out') -Raw -ErrorAction SilentlyContinue
        throw "local shellglass server exited $($server.ExitCode): $serverErr$serverOut"
    }
}

try {
    $targets=@(Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Where-Object MainWindowHandle -ne 0)
    if(-not$targets){
        Start-Process explorer.exe "shell:AppsFolder\$($package.PackageFamilyName)!App"
        $deadline=[DateTime]::UtcNow.AddSeconds(15)
        do{
            Start-Sleep -Milliseconds 250
            $targets=@(Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Where-Object MainWindowHandle -ne 0)
        }while(-not$targets-and[DateTime]::UtcNow-lt$deadline)
        if(-not$targets){throw 'Windows Terminal did not create a visible target process'}
    }
    foreach($target in $targets){
        $target.Refresh()
        if($target.HasExited){continue}
        $loaded=@($target.Modules|Where-Object ModuleName -ieq 'shellglass-wt-adapter.dll')
        if(-not$loaded){
            & $injector $target.Id $adapter
            if($LASTEXITCODE){
                $target.Refresh()
                if($target.HasExited){continue}
                throw "injection failed for WT PID $($target.Id); run this script at an equivalent integrity level"
            }
        }
    }
    if($NewTab){
        # The registered execution alias asks the existing WT process to create a
        # new control. Direct WindowsApps execution is denied on ordinary hosts.
        $wtAlias=Get-Command wt.exe -ErrorAction SilentlyContinue
        if($wtAlias){
            Start-Process $wtAlias.Source -ArgumentList @('-w','0','new-tab')
        }else{
            # The alias can be administratively disabled. App activation still
            # creates a post-injection control (possibly in a new WT window).
            Start-Process explorer.exe "shell:AppsFolder\$($package.PackageFamilyName)!App"
        }
    }
    if($PSCmdlet.ParameterSetName-eq'Push'){
        Start-Sleep 2
        & $tap stream status
        Write-Host 'Streaming is active. Use: shellglass-wt-tap stream pause|resume|stop|status'
    }else{
        Write-Host "Local WT mirror: http://$Bind/  (server PID $($server.Id))"
        Write-Host "Stop it with: Stop-Process -Id $($server.Id)"
        if(-not$NewTab){Write-Host 'Switch to each existing tab once to recover it lazily.'}
    }
} catch {
    if($server-and-not$server.HasExited){Stop-Process $server.Id -Force -ErrorAction SilentlyContinue}
    if($startedWorker){
        try{Start-Process -Wait -WindowStyle Hidden $tap -ArgumentList @('stream','stop')}catch{}
    }
    throw
}
