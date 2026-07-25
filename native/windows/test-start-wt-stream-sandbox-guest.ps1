# Runs only inside an already-open Windows Sandbox as WDAGUtilityAccount.
# It exercises the first-party operator launcher itself, not just its component
# steps. Never run this script against the host development terminal.
$ErrorActionPreference='Stop'
$work='C:\work'
$result="$work\operator-result.json"
$server=$null
$wt=$null
$passed=$false
$detail=''
Remove-Item $result -Force -ErrorAction SilentlyContinue
try {
    Get-Process 'WindowsTerminal','shellglass','shellglass-wt-tap' -ErrorAction SilentlyContinue|Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep 2
    $output=& "$work\native\windows\start-wt-stream.ps1" -Bind '127.0.0.1:18091' -NewTab `
        -PreparedProfile "$work\prepared.sgnp" 2>&1
    if($LASTEXITCODE){throw "operator launcher exited $LASTEXITCODE`: $output"}
    $serverInfo=Get-CimInstance Win32_Process|Where-Object{$_.Name-eq'shellglass-wt-tap.exe'-and$_.CommandLine-match'serve.*127.0.0.1:18091'}|Select-Object -First 1
    if(-not$serverInfo){throw "operator launcher did not leave its local mirror running: $output"}
    $server=Get-Process -Id $serverInfo.ProcessId -ErrorAction Stop
    $deadline=[DateTime]::UtcNow.AddSeconds(20)
    do{Start-Sleep -Milliseconds 250;$wt=Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Where-Object MainWindowHandle -ne 0|Select-Object -First 1}while(-not$wt-and[DateTime]::UtcNow-lt$deadline)
    if(-not$wt){throw 'operator launcher did not start or locate Windows Terminal'}
    $modules=@($wt.Modules|Where-Object ModuleName -ieq 'shellglass-wt-adapter.dll')
    if($modules.Count-ne1){throw "operator launcher loaded $($modules.Count) adapter modules, expected one"}
    $keys=New-Object -ComObject WScript.Shell
    if(-not$keys.AppActivate($wt.Id)){throw 'could not activate operator-launched WT'}
    Set-Clipboard 'Write-Output OPERATOR_LAUNCH_E2E_OK'
    $keys.SendKeys('+{INSERT}');$keys.SendKeys('{ENTER}')
    $deadline=[DateTime]::UtcNow.AddSeconds(20);$raw=''
    do{
        Start-Sleep -Milliseconds 250
        if($server.HasExited){throw "operator-launched server exited: $(Get-Content $env:TEMP\shellglass-wt-serve.err -Raw)"}
        try{$raw=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18091/snapshot').Content}catch{$raw=''}
    }while($raw-notmatch'OPERATOR_LAUNCH_E2E_OK'-and[DateTime]::UtcNow-lt$deadline)
    if($raw-notmatch'OPERATOR_LAUNCH_E2E_OK'){throw 'operator launch did not publish the fresh WT tab end-to-end'}
    $passed=$true
    $detail='start-wt-stream.ps1 profiled, served, injected, opened a fresh tab, and published it end-to-end'
}catch{$detail=$_|Out-String}
finally{
    @{passed=$passed;detail=$detail}|ConvertTo-Json|Set-Content $result -Encoding utf8
    Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Stop-Process -Force -ErrorAction SilentlyContinue
    if($server-and-not$server.HasExited){Stop-Process $server.Id -Force -ErrorAction SilentlyContinue}
}
