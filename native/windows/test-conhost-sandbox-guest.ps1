$ErrorActionPreference='Stop'
$work='C:\work';$resultPath="$work\result.json";$passed=$false;$detail='not started'
$server=$classic=$classicHost=$null
function Wait-Snapshot([string]$needle,[int]$seconds=20){
 $deadline=[DateTime]::UtcNow.AddSeconds($seconds);$raw=''
 do{
  Start-Sleep -Milliseconds 200
  try{$client=New-Object Net.WebClient;try{$raw=[Text.Encoding]::UTF8.GetString($client.DownloadData('http://127.0.0.1:18086/snapshot'))}finally{$client.Dispose()}}catch{$raw=''}
 }while($raw-notmatch[regex]::Escape($needle)-and[DateTime]::UtcNow-lt$deadline)
 if($raw-notmatch[regex]::Escape($needle)){throw "snapshot did not contain $needle"};return $raw
}
try{
 $server=Start-Process -PassThru "$work\shellglass.exe" -ArgumentList @('serve','--bind','127.0.0.1:18086') -RedirectStandardOutput "$work\server.out" -RedirectStandardError "$work\server.err"
 Start-Sleep 1;$server.Refresh()
 if($server.HasExited){throw "shellglass server exited $($server.ExitCode)"}

 $startup='Registry::HKEY_CURRENT_USER\Console\%%Startup';$legacy='{B23D10C0-E52E-411E-9D5B-C09FDF709C7D}'
 New-Item $startup -Force|Out-Null
 Set-ItemProperty $startup DelegationConsole $legacy;Set-ItemProperty $startup DelegationTerminal $legacy
 $before=@(Get-Process conhost -ErrorAction SilentlyContinue|ForEach-Object Id)
 $classic=Start-Process "$work\shellglass-conhost-client-fixture.exe" -PassThru -ArgumentList 'CLASSIC'
 Start-Sleep 1
 $classicHost=Get-Process conhost|Where-Object{$before-notcontains$_.Id}|Sort-Object StartTime|Select-Object -Last 1
 if(-not$classicHost){throw 'classic conhost was not created'}
 & "$work\shellglass-inject.exe" $classicHost.Id "$work\shellglass-conhost-adapter.dll"
 if($LASTEXITCODE){throw 'classic conhost injection failed'}
 Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class FocusWindow {
 public delegate bool Callback(IntPtr hwnd, IntPtr value);
 [DllImport("user32.dll")] static extern bool EnumWindows(Callback callback, IntPtr value);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
 [DllImport("user32.dll")] static extern bool ShowWindow(IntPtr hwnd, int command);
 [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
 public static bool ForPid(uint wanted) {
  bool found=false;
  EnumWindows((hwnd,value)=>{ uint pid; GetWindowThreadProcessId(hwnd,out pid); if(pid==wanted){ShowWindow(hwnd,5);found=SetForegroundWindow(hwnd);return false;}return true;},IntPtr.Zero);
  return found;
 }
}
'@
 Start-Sleep 4
 if(-not[FocusWindow]::ForPid($classic.Id)){throw 'could not foreground classic console'}
 $alternate=Wait-Snapshot 'SHELLGLASS_CLASSIC_ALT_SCREEN' 25
 $alternate|Set-Content "$work\snapshot-classic-alt.json" -Encoding utf8
 if($alternate-match'SHELLGLASS_CLASSIC_CONHOST_OK'){throw 'classic alternate screen leaked the main buffer'}
 $raw=Wait-Snapshot 'SHELLGLASS_CLASSIC_CONHOST_OK' 10
 $raw|Set-Content "$work\snapshot-classic.json" -Encoding utf8
 $snapshot=$raw|ConvertFrom-Json
 if($snapshot.w-ne90-or$snapshot.h-ne28){throw "classic dimensions were $($snapshot.w)x$($snapshot.h)"}
 if($snapshot.t-notmatch'SHELLGLASS_CLASSIC_TITLE'){throw 'classic title was not captured'}
 $unicode='UNICODE_FIDELITY: '+[char]0x6f22+[char]0x5b57+' e'+[char]0x0301+' '+[char]::ConvertFromUtf32(0x1f600)
 if(-not$raw.Contains($unicode)){throw 'classic UTF-16 grapheme/wide-cell fidelity missing'}
 if($raw-notmatch'249,241,165'-or$raw-notmatch'0,55,218'){throw 'classic resolved color style was not captured'}
 $metricDeadline=[DateTime]::UtcNow.AddSeconds(8);$metrics=''
 do{Start-Sleep -Milliseconds 200;$metrics=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue}while($metrics-notmatch'conhost render callback p95<=(\d+)us'-and[DateTime]::UtcNow-lt$metricDeadline)
 if($metrics-notmatch'conhost render callback p95<=(\d+)us'){throw 'conhost callback performance sample did not reach 120 frames'}
 $p95=[int]$Matches[1];if($p95-gt1000){throw "conhost callback p95 exceeded 1ms: ${p95}us"}
 $classic.Refresh();$classicHost.Refresh()
 if($classic.HasExited-or$classicHost.HasExited-or-not$classicHost.Responding){throw 'classic console was not responsive after capture'}

 Stop-Process $classic.Id -Force;Start-Sleep 2
 $faultDir="$work\fault";New-Item $faultDir -ItemType Directory -Force|Out-Null
 Copy-Item "$work\shellglass-conhost-fault-adapter.dll" "$faultDir\shellglass-conhost-adapter.dll" -Force
 Copy-Item "$work\shellglass-conhost-adapter.sgnp" "$faultDir\shellglass-conhost-adapter.sgnp" -Force
 $before=@(Get-Process conhost -ErrorAction SilentlyContinue|ForEach-Object Id)
 $classic=Start-Process "$work\shellglass-conhost-client-fixture.exe" -PassThru -ArgumentList 'CLASSIC'
 Start-Sleep 1
 $classicHost=Get-Process conhost|Where-Object{$before-notcontains$_.Id}|Sort-Object StartTime|Select-Object -Last 1
 if(-not$classicHost){throw 'fault-containment conhost was not created'}
 & "$work\shellglass-inject.exe" $classicHost.Id "$faultDir\shellglass-conhost-adapter.dll"
 if($LASTEXITCODE){throw 'fault-containment conhost injection failed'}
 Start-Sleep 4
 if(-not[FocusWindow]::ForPid($classic.Id)){throw 'could not foreground fault-containment conhost'}
 $faultDeadline=[DateTime]::UtcNow.AddSeconds(15);$faultLog=''
 do{Start-Sleep -Milliseconds 200;$faultLog=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue}while($faultLog-notmatch'native diagnostic 212: conhost capture provider disabled after an internal callback fault'-and[DateTime]::UtcNow-lt$faultDeadline)
 if($faultLog-notmatch'native diagnostic 212: conhost capture provider disabled after an internal callback fault'){throw 'conhost callback fault was not removed/diagnosed by the worker'}
 $classicHost.Refresh();if($classicHost.HasExited-or-not$classicHost.Responding){throw 'conhost became unresponsive after callback fault'}
 $passed=$true;$detail="classic conhost Unicode/text/style/cursor/title/resize/alternate-screen/callback-fault capture passed with callback p95<=${p95}us; headless ConPTY remains fail closed on this family"
}catch{$detail=($_|Out-String)}finally{
 @{passed=$passed;detail=$detail}|ConvertTo-Json|Set-Content $resultPath -Encoding utf8
 foreach($process in @($classic,$classicHost,$server)){if($process-and-not$process.HasExited){Stop-Process $process.Id -Force -ErrorAction SilentlyContinue}}
}
