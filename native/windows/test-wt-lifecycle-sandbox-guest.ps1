param([string]$ExpectedVersion='1.24.11911.0')
# Dedicated pane-move lifecycle gate. It creates both destination windows and
# installs hooks before moving content, avoiding races with post-creation
# injection while still exercising real detach, rehydrate, reattach, and close.
$ErrorActionPreference='Stop'
$work='C:\work'
$resultPath="$work\result.json"
$server=$null
$passed=$false
$detail=''
$stage='startup'
Remove-Item $resultPath -Force -ErrorAction SilentlyContinue
[DateTime]::UtcNow.ToString('O')|Set-Content "$work\guest-started.txt" -Encoding ascii

function Snapshot-Raw {
    try {
        $client=New-Object Net.WebClient
        try { return [Text.Encoding]::UTF8.GetString($client.DownloadData('http://127.0.0.1:18084/snapshot')) }
        finally { $client.Dispose() }
    } catch { return '' }
}
function Assert-WtResponsive {
    $processes=@(Get-Process WindowsTerminal -ErrorAction SilentlyContinue)
    if(-not$processes){throw "all WT processes exited during $script:stage"}
    foreach($process in $processes){$process.Refresh();if(-not$process.HasExited-and-not$process.Responding){throw "WT PID $($process.Id) stopped responding during $script:stage"}}
}
function Wait-Marker([string]$marker,[int]$seconds=20) {
    $deadline=[DateTime]::UtcNow.AddSeconds($seconds);$raw=''
    do {
        Start-Sleep -Milliseconds 200
        if($script:server.HasExited){throw "shellglass exited: $(Get-Content $work\server.err -Raw)"}
        Assert-WtResponsive
        $raw=Snapshot-Raw
    } while($raw-notmatch$marker-and[DateTime]::UtcNow-lt$deadline)
    if($raw-notmatch$marker){$raw|Set-Content "$work\failed-$($script:stage).json" -Encoding utf8;throw "snapshot did not contain $marker during $script:stage"}
    return $raw
}

try {
    New-Item 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -Force|Out-Null
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' AllowDevelopmentWithoutDevLicense 1 -Type DWord
    Get-AppxPackage Microsoft.WindowsTerminal -ErrorAction SilentlyContinue|Remove-AppxPackage -ErrorAction SilentlyContinue
    if(-not(Get-AppxPackage Microsoft.UI.Xaml.2.8 -ErrorAction SilentlyContinue)){
        Add-AppxPackage -Path (Get-ChildItem "$work\Dependencies\*.msix"|Select-Object -First 1 -ExpandProperty FullName)
    }
    $loose=Join-Path $env:LOCALAPPDATA 'Packages\ShellglassTerminalLifecycle'
    Copy-Item "$work\TerminalPackage" $loose -Recurse -Force
    Remove-Item "$loose\AppxBlockMap.xml","$loose\AppxSignature.p7x","$loose\AppxMetadata" -Recurse -Force -ErrorAction SilentlyContinue
    Add-AppxPackage -Register "$loose\AppxManifest.xml"
    $package=Get-AppxPackage Microsoft.WindowsTerminal
    if($package.Version-ne$ExpectedVersion){throw "wrong WT package version $($package.Version), expected $ExpectedVersion"}
    $state=Join-Path $env:LOCALAPPDATA "Packages\$($package.PackageFamilyName)\LocalState"
    New-Item $state -ItemType Directory -Force|Out-Null
    @'
{"confirmCloseAllTabs":false,"profiles":{"defaults":{"font":{"face":"Cascadia Mono","size":8}}},"actions":[{"command":{"action":"renameWindow","name":"shellglass-main"},"keys":"ctrl+shift+r"},{"command":{"action":"renameWindow","name":"shellglass-detached"},"keys":"ctrl+shift+e"},{"command":{"action":"movePane","window":"shellglass-detached","index":0},"keys":"ctrl+shift+d"},{"command":{"action":"movePane","window":"shellglass-main","index":0},"keys":"ctrl+shift+a"},{"command":{"action":"splitPane","split":"right","commandline":"C:\\work\\shellglass-wt-fixture.exe LIFECYCLE_SPLIT 30"},"keys":"ctrl+shift+s"},{"command":{"action":"moveFocus","direction":"previousInOrder"},"keys":"ctrl+shift+b"},{"command":{"action":"moveFocus","direction":"right"},"keys":"ctrl+shift+g"},{"command":"closePane","keys":"ctrl+shift+q"}]}
'@|Set-Content (Join-Path $state 'settings.json') -Encoding utf8

    $server=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" -ArgumentList @('serve','--bind','127.0.0.1:18084') -RedirectStandardOutput "$work\server.out" -RedirectStandardError "$work\server.err"
    Start-Sleep 2
    Start-Process explorer.exe 'shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App'
    Start-Sleep 8
    $mainProcess=Get-Process WindowsTerminal -ErrorAction Stop|Where-Object{$_.MainWindowHandle-ne0}|Select-Object -First 1
    if(-not$mainProcess){throw 'main WT window did not start'}

    Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class LifecycleWindow {
 public delegate bool EnumCallback(IntPtr hwnd,IntPtr value);
 [DllImport("user32.dll")] static extern bool EnumWindows(EnumCallback callback,IntPtr value);
 [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr hwnd);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd,out uint pid);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
 [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd,int command);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
 [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
 public static IntPtr[] ForPid(uint wanted) { var found=new List<IntPtr>(); EnumWindows((h,v)=>{uint p;GetWindowThreadProcessId(h,out p);if(p==wanted&&IsWindowVisible(h))found.Add(h);return true;},IntPtr.Zero);return found.ToArray(); }
}
public static class LifecycleChord {
 [DllImport("user32.dll")] static extern void keybd_event(byte key,byte scan,uint flags,UIntPtr extra);
 public static void Press(params byte[] keys){foreach(var k in keys)keybd_event(k,0,0,UIntPtr.Zero);for(var i=keys.Length-1;i>=0;--i)keybd_event(keys[i],0,2,UIntPtr.Zero);}
}
'@
    $keys=New-Object -ComObject WScript.Shell
    function Foreground([IntPtr]$hwnd) {
        [void][LifecycleWindow]::ShowWindow($hwnd,5)
        if(-not[LifecycleWindow]::SetForegroundWindow($hwnd)){throw "could not foreground HWND $hwnd during $script:stage"}
        $deadline=[DateTime]::UtcNow.AddSeconds(5)
        while([LifecycleWindow]::GetForegroundWindow()-ne$hwnd-and[DateTime]::UtcNow-lt$deadline){Start-Sleep -Milliseconds 100}
        if([LifecycleWindow]::GetForegroundWindow()-ne$hwnd){throw "HWND $hwnd did not become foreground during $script:stage"}
    }
    function Run-In-NewTab([string]$marker) {
        [LifecycleChord]::Press(0x11,0x10,0x54);Start-Sleep 2
        Set-Clipboard "C:\work\shellglass-wt-fixture.exe $marker"
        $keys.SendKeys('+{INSERT}');$keys.SendKeys('{ENTER}')
        [void](Wait-Marker $marker 20)
    }
    function Inject([Diagnostics.Process]$process) {
        & "$work\shellglass-inject.exe" $process.Id "$work\shellglass-wt-adapter.dll"
        if($LASTEXITCODE){throw "adapter injection failed for PID $($process.Id)"}
        Start-Sleep 2
        $fresh=Get-Process -Id $process.Id -ErrorAction Stop
        $adapterModules=@($fresh.Modules|Where-Object { $_.ModuleName -ieq 'shellglass-wt-adapter.dll' })
        if($adapterModules.Count -ne 1){throw "adapter did not load exactly once in PID $($process.Id); count=$($adapterModules.Count)"}
    }
    function Find-InPanes([IntPtr]$hwnd,[string]$marker) {
        Foreground $hwnd
        foreach($direction in @($null,@(0x11,0x10,0x47),@(0x11,0x10,0x42))){
            if($direction){[LifecycleChord]::Press($direction);Start-Sleep -Milliseconds 500}
            $raw=Snapshot-Raw
            if($raw-match$marker){return $raw}
        }
        return Wait-Marker $marker 15
    }

    Inject $mainProcess
    $mainHwnd=$mainProcess.MainWindowHandle
    Foreground $mainHwnd
    [LifecycleChord]::Press(0x11,0x10,0x52);Start-Sleep 1
    $stage='main-first';Run-In-NewTab 'LIFECYCLE_FIRST'
    # Create the keeper before the teardown stress. It remains in the already-
    # hooked main window while the FIRST tab's repainting split is closed.
    $stage='main-keeper';Run-In-NewTab 'LIFECYCLE_KEEP_MAIN'
    [LifecycleChord]::Press(0x11,0x12,0x32);$stage='main-first-before-split';[void](Wait-Marker 'LIFECYCLE_FIRST' 15)
    [LifecycleChord]::Press(0x11,0x10,0x53)
    $stage='split-right';[void](Wait-Marker 'LIFECYCLE_SPLIT' 25)
    [LifecycleChord]::Press(0x11,0x10,0x42)
    $stage='split-left';$left=Wait-Marker 'LIFECYCLE_FIRST' 15
    if($left-match'LIFECYCLE_SPLIT'){throw 'split-pane left focus retained the right source'}
    [LifecycleChord]::Press(0x11,0x10,0x47)
    $stage='split-right-return';[void](Wait-Marker 'LIFECYCLE_SPLIT' 15)
    [LifecycleChord]::Press(0x11,0x10,0x51)
    # The split fixture is in its sustained full-screen repaint loop here, so
    # closing it exercises ControlCore teardown concurrent with real callbacks.
    $stage='split-close-during-render';$afterSplit=Wait-Marker 'LIFECYCLE_FIRST' 15
    if($afterSplit-match'LIFECYCLE_SPLIT'){throw 'closed split pane remained published'}
    # Let WT finish its split-layout close animation before continuing.
    Start-Sleep 2;Foreground $mainHwnd
    [LifecycleChord]::Press(0x11,0x12,0x32);$stage='main-first-select';[void](Wait-Marker 'LIFECYCLE_FIRST' 15)

    $before=@(Get-Process WindowsTerminal -ErrorAction Stop|ForEach-Object{[LifecycleWindow]::ForPid($_.Id)}|ForEach-Object{[int64]$_})
    [LifecycleChord]::Press(0x11,0x10,0x4e)
    $deadline=[DateTime]::UtcNow.AddSeconds(20);$destinationHwnd=[IntPtr]::Zero;$destinationProcess=$null
    do {
        Start-Sleep -Milliseconds 250
        foreach($process in (Get-Process WindowsTerminal -ErrorAction SilentlyContinue)){
            foreach($handle in [LifecycleWindow]::ForPid($process.Id)){
                if($before-notcontains([int64]$handle)){$destinationHwnd=$handle;$destinationProcess=$process;break}
            }
            if($destinationHwnd-ne[IntPtr]::Zero){break}
        }
    } while($destinationHwnd-eq[IntPtr]::Zero-and[DateTime]::UtcNow-lt$deadline)
    if($destinationHwnd-eq[IntPtr]::Zero-or-not$destinationProcess){throw 'second WT window did not appear'}
    if($destinationProcess.Id-ne$mainProcess.Id){Inject $destinationProcess}
    Foreground $destinationHwnd
    $stage='destination-host';Run-In-NewTab 'LIFECYCLE_DESTINATION_HOST'
    [LifecycleChord]::Press(0x11,0x10,0x45);Start-Sleep 1

    Foreground $mainHwnd
    [LifecycleChord]::Press(0x11,0x12,0x32);$stage='pre-detach';[void](Wait-Marker 'LIFECYCLE_FIRST' 15)
    [LifecycleChord]::Press(0x11,0x10,0x44);Start-Sleep 3
    Foreground $destinationHwnd;[LifecycleChord]::Press(0x11,0x12,0x31);Start-Sleep 1
    $stage='detached';[void](Find-InPanes $destinationHwnd 'LIFECYCLE_FIRST')
    if(-not[LifecycleWindow]::IsWindow($mainHwnd)-or-not[LifecycleWindow]::IsWindow($destinationHwnd)){throw 'a host window died during pane detach'}

    # The moved pane is now focused in the named destination. Move it back into
    # tab zero of the already-hooked main window, then close it while captured.
    [LifecycleChord]::Press(0x11,0x10,0x41);Start-Sleep 3
    Foreground $mainHwnd;[LifecycleChord]::Press(0x11,0x12,0x31);Start-Sleep 1
    $stage='reattached';[void](Find-InPanes $mainHwnd 'LIFECYCLE_FIRST')
    [LifecycleChord]::Press(0x11,0x10,0x51);Start-Sleep 2
    [LifecycleChord]::Press(0x11,0x12,0x32)
    $stage='closed-moved-pane';[void](Wait-Marker 'LIFECYCLE_KEEP_MAIN' 20)
    if((Snapshot-Raw)-match'LIFECYCLE_FIRST'){throw 'closed reattached pane remained in the published frame'}

    Foreground $destinationHwnd
    [LifecycleChord]::Press(0x11,0x12,0x32)
    $stage='destination-survives';[void](Wait-Marker 'LIFECYCLE_DESTINATION_HOST' 20)
    Assert-WtResponsive
    $log=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue
    if($log-match'native adapter disconnected'){throw 'native adapter disconnected during pane lifecycle test'}
    $passed=$true
    $detail='split-pane focus/close plus detach to an already-hooked named WT window, reattach, and close passed with both host windows responsive and correct source selection'
} catch {
    $detail=($_|Out-String)
} finally {
    @{passed=$passed;detail=$detail}|ConvertTo-Json|Set-Content $resultPath -Encoding utf8
    Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Stop-Process -Force -ErrorAction SilentlyContinue
    if($server-and-not$server.HasExited){Stop-Process $server.Id -Force}
}
