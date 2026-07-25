param([string]$ExpectedVersion='1.24.11911.0',[ValidateRange(30,300)][int]$StressSeconds=30)
# Runs only inside Windows Sandbox. The host harness maps an isolated work tree
# at C:\work; no process on the host is opened or injected.
$ErrorActionPreference = 'Stop'
$work = 'C:\work'
$resultPath = "$work\result.json"
$server = $null
$wt = $null
$passed = $false
$detail = ''
$stage = 'startup'
$serverSuspended = $false
$hub = $null
$detachedStarted = $false
Remove-Item $resultPath -Force -ErrorAction SilentlyContinue
[DateTime]::UtcNow.ToString('O') | Set-Content "$work\guest-started.txt" -Encoding ascii

function Snapshot-Text([string]$raw) {
    $builder = New-Object Text.StringBuilder
    foreach ($row in ($raw | ConvertFrom-Json).d) {
        $skipWideContinuation = $false
        foreach ($cell in $row) {
            $value = $cell
            while ($value -is [Array]) { $value = if ($value.Count) { $value[0] } else { 0 } }
            if ($skipWideContinuation -and $value -is [ValueType] -and [int64]$value -eq 0) { $skipWideContinuation=$false;continue }
            $skipWideContinuation=$false
            if ($value -is [string]) { [void]$builder.Append($value) }
            elseif ($value -is [ValueType] -and [int64]$value -ne 0) { [void]$builder.Append([char]::ConvertFromUtf32([int]$value)) }
            else { [void]$builder.Append(' ') }
            if ($cell -is [Array]) { foreach($part in $cell) { if($part.w-eq1){$skipWideContinuation=$true} } }
        }
        [void]$builder.Append("`n")
    }
    return $builder.ToString()
}

function Invoke-Shellglass([string[]]$arguments,[string]$name,[int]$seconds=10) {
    $stdout="$work\control-$name.out";$stderr="$work\control-$name.err"
    Remove-Item $stdout,$stderr -Force -ErrorAction SilentlyContinue
    $process=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" -ArgumentList $arguments `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    if(-not$process.WaitForExit($seconds*1000)){
        Stop-Process $process.Id -Force -ErrorAction SilentlyContinue
        throw "shellglass $name command exceeded ${seconds}s"
    }
    $process.WaitForExit();$process.Refresh()
    $output=if(Test-Path $stdout){Get-Content $stdout -Raw}else{''}
    $errorText=if(Test-Path $stderr){Get-Content $stderr -Raw}else{''}
    if($process.ExitCode){throw "shellglass $name command exited $($process.ExitCode): $errorText"}
    return $output
}

function Wait-Snapshot([string]$needle, [int]$seconds = 20) {
    $script:stage|Set-Content "$work\stage.txt" -Encoding ascii
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    $raw = ''
    do {
        Start-Sleep -Milliseconds 200
        if ($script:server.HasExited) { throw "shellglass exited: $(Get-Content $work\server.err -Raw)" }
        if ($script:wt.HasExited) { throw "Windows Terminal exited before capture" }
        $script:wt.Refresh()
        if (-not $script:wt.Responding) { throw 'Windows Terminal stopped responding' }
        try {
            $client = New-Object Net.WebClient
            try { $raw = [Text.Encoding]::UTF8.GetString($client.DownloadData('http://127.0.0.1:18083/snapshot')) }
            finally { $client.Dispose() }
        } catch { $raw = '' }
    } while ($raw -notmatch $needle -and [DateTime]::UtcNow -lt $deadline)
    if ($raw -notmatch $needle) { $raw|Set-Content "$work\failed-snapshot.json" -Encoding utf8;throw "snapshot did not contain $needle during $script:stage" }
    return $raw
}

try {
    New-Item "$work\dumps" -ItemType Directory -Force | Out-Null
    New-Item 'HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\WindowsTerminal.exe' -Force | Out-Null
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\WindowsTerminal.exe' DumpFolder "$work\dumps"
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\WindowsTerminal.exe' DumpType 2 -Type DWord
    New-Item 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' -Force | Out-Null
    Set-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock' AllowDevelopmentWithoutDevLicense 1 -Type DWord

    Get-AppxPackage Microsoft.WindowsTerminal -ErrorAction SilentlyContinue | Remove-AppxPackage -ErrorAction SilentlyContinue
    if(-not(Get-AppxPackage Microsoft.UI.Xaml.2.8 -ErrorAction SilentlyContinue)){
        Add-AppxPackage -Path (Get-ChildItem "$work\Dependencies\*.msix" | Select-Object -First 1 -ExpandProperty FullName)
    }
    $loose = Join-Path $env:LOCALAPPDATA 'Packages\ShellglassTerminalUnderTest'
    Copy-Item "$work\TerminalPackage" $loose -Recurse -Force
    Remove-Item "$loose\AppxBlockMap.xml","$loose\AppxSignature.p7x","$loose\AppxMetadata" -Recurse -Force -ErrorAction SilentlyContinue
    Add-AppxPackage -Register "$loose\AppxManifest.xml"
    $package = Get-AppxPackage Microsoft.WindowsTerminal
    if ($package.Version -ne $ExpectedVersion) { throw "wrong WT package version $($package.Version), expected $ExpectedVersion" }
    $state=Join-Path $env:LOCALAPPDATA "Packages\$($package.PackageFamilyName)\LocalState"
    New-Item $state -ItemType Directory -Force|Out-Null
    '{"confirmCloseAllTabs":false,"profiles":{"defaults":{"font":{"face":"Cascadia Mono","size":6},"scrollbarState":"always"}},"actions":[{"command":{"action":"movePane","window":"new"},"keys":"ctrl+shift+d"},{"command":{"action":"renameWindow","name":"shellglass-main"},"keys":"ctrl+shift+r"},{"command":{"action":"movePane","window":"shellglass-main","index":0},"keys":"ctrl+shift+a"},{"command":{"action":"moveFocus","direction":"previousInOrder"},"keys":"ctrl+shift+b"},{"command":{"action":"moveFocus","direction":"right"},"keys":"ctrl+shift+g"},{"command":{"action":"adjustFontSize","delta":8},"keys":"ctrl+shift+i"},{"command":{"action":"adjustFontSize","delta":-2},"keys":"ctrl+shift+j"},{"command":"resetFontSize","keys":"ctrl+shift+k"},{"command":{"action":"adjustFontSize","delta":-4},"keys":"ctrl+shift+l"},{"command":{"action":"newTab","commandline":"C:\\work\\shellglass-wt-fixture.exe FOURTH_WINDOW_MARKER"},"keys":"ctrl+shift+m"},{"command":"closePane","keys":"ctrl+shift+q"},{"command":{"action":"splitPane","split":"right","commandline":"C:\\work\\shellglass-wt-fixture.exe THIRD_PANE_MARKER"},"keys":"ctrl+shift+s"}]}'|Set-Content (Join-Path $state 'settings.json') -Encoding utf8

    $server = Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" `
        -ArgumentList @('serve','--bind','127.0.0.1:18083') `
        -RedirectStandardOutput "$work\server.out" -RedirectStandardError "$work\server.err"
    Start-Sleep 2
    Start-Process explorer.exe 'shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App'
    Start-Sleep 8
    $wt = Get-Process WindowsTerminal -ErrorAction Stop | Sort-Object StartTime | Select-Object -Last 1
    if (-not $wt.Responding) { throw 'fresh WT window is not responding' }
    $initialWindow = $wt.MainWindowHandle

    # This base tab deliberately predates injection. After hooks are installed,
    # switching back to it must recover its exact ControlCore/Renderer through
    # the PDB-pinned private member offsets rather than requiring a new tab.
    $keys = New-Object -ComObject WScript.Shell
    if(-not$keys.AppActivate($wt.Id)){throw 'could not foreground pre-injection WT tab'}
    Set-Clipboard 'Write-Output PREEXISTING_TAB_LAZY_RECOVERY'
    $keys.SendKeys('+{INSERT}');$keys.SendKeys('{ENTER}');Start-Sleep 1

    & "$work\shellglass-inject.exe" $wt.Id "$work\shellglass-wt-adapter.dll"
    if ($LASTEXITCODE) { throw "injector exited $LASTEXITCODE" }
    Start-Sleep 2
    if (($wt.Modules | Where-Object ModuleName -eq 'shellglass-wt-adapter.dll').Count -ne 1) {
        throw 'adapter module did not load exactly once'
    }

    # Injection occurs after the base tab only to prove that every captured core
    # is created after verified hooks are installed. Production startup injection
    # observes all cores from process startup.
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class Chord {
 [DllImport("user32.dll")] public static extern void keybd_event(byte key, byte scan, uint flags, UIntPtr extra);
 public static void Press(params byte[] keys) {
  foreach (var k in keys) keybd_event(k,0,0,UIntPtr.Zero);
  for (var i=keys.Length-1;i>=0;--i) keybd_event(keys[i],0,2,UIntPtr.Zero);
 }
}
'@
    if (-not $keys.AppActivate($wt.Id)) { throw 'could not foreground isolated WT' }
    [Chord]::Press(0x11,0x10,0x52)
    Start-Sleep 1
    [Chord]::Press(0x11,0x10,0x54)
    Start-Sleep 4
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe FIRST_TAB_MARKER'
    $keys.SendKeys('+{INSERT}')
    $keys.SendKeys('{ENTER}')

    $raw = Wait-Snapshot 'SHELLGLASS_REAL_WT_TAP_7F3A'
    if ($raw -notmatch 'UNICODE_FIDELITY') { $raw = Wait-Snapshot 'UNICODE_FIDELITY' }
    $unicode = 'UNICODE_FIDELITY: '+[char]0x6f22+[char]0x5b57+' '+[char]0x754c
    if (-not (Snapshot-Text $raw).Contains($unicode)) { throw 'stock-WT UTF-8 wide-cell fidelity missing' }
    $raw | Set-Content "$work\snapshot.json" -Encoding utf8
    $snapshot = $raw | ConvertFrom-Json
    if ($snapshot.w -lt 20 -or $snapshot.h -lt 10) { throw "invalid viewport $($snapshot.w)x$($snapshot.h)" }
    if (-not $snapshot.t) { throw 'WT title was not captured' }
    if ($null -eq $snapshot.p) { throw 'WT cursor was not captured' }
    if ($snapshot.q -ne 6) { throw "WT DECSCUSR vertical-bar cursor style was not captured: $($snapshot.q)" }
    if ($raw -notmatch '231,72,86') { throw 'resolved stock-WT red foreground style was not captured' }
    if ($raw -notmatch 'UNDERLINE_COLOR_FIDELITY' -or $raw -notmatch '"u":3' -or $raw -notmatch '"k":\[0,255,0\]') {
        throw 'underline style/color fidelity was not captured'
    }
    if ($raw -notmatch 'LINK_FIDELITY' -or $raw -notmatch 'https://example.com/shellglass') {
        throw 'OSC 8 hyperlink text/URI fidelity was not captured'
    }
    if ($raw -notmatch 'CONCEAL_BLINK_FIDELITY' -or $raw -notmatch '"o":1' -or $raw -notmatch '"x":1') {
        throw 'conceal/blink fidelity was not captured'
    }
    if (-not $snapshot.i -or $snapshot.i.Count -ne 1 -or -not $snapshot.i[0].k -or $snapshot.i[0].h -le 1) {
        throw 'WT sixel image slices were not grouped into one multi-row placement'
    }
    $image=(New-Object Net.WebClient).DownloadData("http://127.0.0.1:18083/images/$($snapshot.i[0].k)")
    if ($image.Length -lt 8 -or $image[0] -ne 0x89 -or $image[1] -ne 0x50 -or $image[2] -ne 0x4e -or $image[3] -ne 0x47) {
        throw 'WT grouped image slices did not produce a served PNG blob'
    }
    $metricDeadline=[DateTime]::UtcNow.AddSeconds(8); $metrics=''
    do {
        Start-Sleep -Milliseconds 200
        $metrics=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue
    } while ($metrics -notmatch 'render callback p95<=(\d+)us' -and [DateTime]::UtcNow -lt $metricDeadline)
    if ($metrics -notmatch 'render callback p95<=(\d+)us') { throw 'WT callback performance sample did not reach 120 frames' }
    $p95=[int]$Matches[1]
    if ($p95 -gt 1000) { throw "WT render callback p95 exceeded 1ms: ${p95}us" }

    # Return to the tab that was fully initialized before injection. Its first
    # post-hook focus transition must lazily attach and publish a complete frame.
    [Chord]::Press(0x11,0x10,0x09)
    $stage='preexisting-tab-lazy-recovery';$preexisting=Wait-Snapshot 'PREEXISTING_TAB_LAZY_RECOVERY' 20
    $preexisting|Set-Content "$work\snapshot-preexisting-recovered.json" -Encoding utf8
    [Chord]::Press(0x11,0x09)
    $stage='post-recovery-return';[void](Wait-Snapshot 'SHELLGLASS_REAL_WT_TAP_7F3A' 15)

    $searched=''
    1..3 | ForEach-Object {
        if ($searched -match '255,255,0') { return }
        [void]$keys.AppActivate($wt.Id)
        $keys.SendKeys('{ESC}')
        $keys.SendKeys('^+f')
        Start-Sleep 2
        $keys.SendKeys('SCROLL')
        Start-Sleep 1
        $keys.SendKeys('{ENTER}')
        $searchDeadline=[DateTime]::UtcNow.AddSeconds(4)
        do {
            Start-Sleep -Milliseconds 200
            $searched=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content
        } while ($searched -notmatch '255,255,0' -and [DateTime]::UtcNow -lt $searchDeadline)
    }
    $searched | Set-Content "$work\snapshot-search.json" -Encoding utf8
    if ($searched -notmatch '255,255,0') { throw 'WT search-highlight overlay was not resolved into the frame' }
    $keys.SendKeys('{ESC}')

    # WT owns scrollback. A render tap must mirror its scrolled viewport rather
    # than the live ConPTY bottom represented by a conhost tap.
    [void]$keys.AppActivate($wt.Id)
    Add-Type @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class Wheel {
 public delegate bool EnumCallback(IntPtr hwnd,IntPtr value);
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left,Top,Right,Bottom; }
 [DllImport("user32.dll")] public static extern bool GetWindowRect(System.IntPtr h,out RECT r);
 [DllImport("user32.dll")] public static extern bool SetCursorPos(int x,int y);
 [DllImport("user32.dll")] public static extern bool MoveWindow(System.IntPtr h,int x,int y,int w,int hgt,bool repaint);
 [DllImport("user32.dll")] public static extern bool ShowWindow(System.IntPtr h,int command);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(System.IntPtr h);
 [DllImport("user32.dll")] public static extern System.IntPtr GetForegroundWindow();
 [DllImport("user32.dll")] public static extern bool IsWindow(System.IntPtr h);
 [DllImport("user32.dll")] static extern bool IsWindowVisible(System.IntPtr h);
 [DllImport("user32.dll")] static extern bool EnumWindows(EnumCallback callback,IntPtr value);
 [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hwnd,out uint pid);
 [DllImport("user32.dll")] public static extern void mouse_event(uint f,uint x,uint y,int data,System.UIntPtr extra);
 public static IntPtr[] ForPid(uint wanted) {
  var result=new List<IntPtr>();
  EnumWindows((hwnd,value)=>{uint pid;GetWindowThreadProcessId(hwnd,out pid);if(pid==wanted&&IsWindowVisible(hwnd))result.Add(hwnd);return true;},IntPtr.Zero);
  return result.ToArray();
 }
}
'@
    $rect=New-Object Wheel+RECT
    [void][Wheel]::GetWindowRect($wt.MainWindowHandle,[ref]$rect)
    [void][Wheel]::MoveWindow($wt.MainWindowHandle,$rect.Left,$rect.Top,850,550,$true)
    $resizeDeadline=[DateTime]::UtcNow.AddSeconds(10); $resized=$null
    do {
        Start-Sleep -Milliseconds 200
        $resized=((Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content | ConvertFrom-Json)
    } while ($resized.w -eq $snapshot.w -and $resized.h -eq $snapshot.h -and [DateTime]::UtcNow -lt $resizeDeadline)
    if ($resized.w -eq $snapshot.w -and $resized.h -eq $snapshot.h) { throw 'real WT resize did not update native frame dimensions' }
    [void][Wheel]::GetWindowRect($wt.MainWindowHandle,[ref]$rect)
    [void][Wheel]::SetCursorPos($rect.Left+200,$rect.Bottom-60)
    1..2 | ForEach-Object {
        [Wheel]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero)
        [Wheel]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero)
        Start-Sleep -Milliseconds 80
    }
    $selectionDeadline=[DateTime]::UtcNow.AddSeconds(5); $selection=''
    do {
        Start-Sleep -Milliseconds 200
        $selection=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content
    } while ($selection -notmatch '"g":\[255,255,255\]' -and [DateTime]::UtcNow -lt $selectionDeadline)
    $selection | Set-Content "$work\snapshot-selection.json" -Encoding utf8
    if ($selection -notmatch '"g":\[255,255,255\]') { throw 'WT selection overlay was not resolved into the frame' }
    # Click once to clear, then start a fixture that keeps writing while WT is
    # scrolled into its private history. This distinguishes the WT tap from a
    # ConPTY live-bottom view rather than testing only a static scrollback.
    [Wheel]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero); [Wheel]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero)
    [Chord]::Press(0x11,0x43);Start-Sleep 1
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe SCROLL_LIVE_MARKER live'
    [Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
    $stage='live-output-start';[void](Wait-Snapshot 'LIVE_OUTPUT_005' 20)

    # A GUI application taking foreground must not unsubscribe the last active
    # terminal. Keep the fixture writing while Notepad owns foreground and prove
    # that the mirrored live-bottom marker advances before returning to WT.
    $beforeAway=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content
    $beforeMatches=[regex]::Matches($beforeAway,'LIVE_OUTPUT_(\d{3})')
    $beforeNumber=($beforeMatches|ForEach-Object{[int]$_.Groups[1].Value}|Measure-Object -Maximum).Maximum
    $notepad=Start-Process notepad.exe -PassThru
    $awayDeadline=[DateTime]::UtcNow.AddSeconds(10)
    do{Start-Sleep -Milliseconds 100;$notepad.Refresh()}while($notepad.MainWindowHandle-eq0-and[DateTime]::UtcNow-lt$awayDeadline)
    if($notepad.MainWindowHandle-eq0){throw 'non-terminal foreground fixture did not create a window'}
    [void][Wheel]::SetForegroundWindow($notepad.MainWindowHandle)
    Start-Sleep 2
    $awayRaw=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content
    $awayRaw|Set-Content "$work\snapshot-nonterminal-foreground-live.json" -Encoding utf8
    $awayMatches=[regex]::Matches($awayRaw,'LIVE_OUTPUT_(\d{3})')
    $awayNumber=($awayMatches|ForEach-Object{[int]$_.Groups[1].Value}|Measure-Object -Maximum).Maximum
    if($null-eq$beforeNumber-or$null-eq$awayNumber-or$awayNumber-le$beforeNumber){
        throw "last active terminal stopped while a non-terminal window was foreground (${beforeNumber} -> ${awayNumber})"
    }
    Stop-Process $notepad.Id -Force -ErrorAction SilentlyContinue
    [void][Wheel]::SetForegroundWindow($wt.MainWindowHandle);Start-Sleep 1

    [void][Wheel]::GetWindowRect($wt.MainWindowHandle,[ref]$rect)
    [void][Wheel]::SetCursorPos([int](($rect.Left+$rect.Right)/2),[int](($rect.Top+$rect.Bottom)/2))
    1..50 | ForEach-Object { [Wheel]::mouse_event(0x0800,0,0,120,[UIntPtr]::Zero) }
    Start-Sleep 1
    try { (Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content | Set-Content "$work\snapshot-scroll-attempt.json" -Encoding utf8 } catch {}
    $stage='live-output-scrolled';$scrolled = Wait-Snapshot 'SCROLL_00'
    if ($scrolled -match 'LIVE_OUTPUT_') { throw 'scrolled WT viewport exposed the ConPTY live bottom' }
    $scrolled | Set-Content "$work\snapshot-scrolled.json" -Encoding utf8
    Start-Sleep 2
    $stage='live-output-still-scrolled';$scrolledDuringOutput=Wait-Snapshot 'SCROLL_00'
    if($scrolledDuringOutput-match'LIVE_OUTPUT_'){throw 'new output snapped the mirrored WT viewport to live bottom'}
    $scrolledDuringOutput|Set-Content "$work\snapshot-scrolled-during-output.json" -Encoding utf8

    # Reflow the window while it remains in history. The native dimensions must
    # change, but the mirrored picture must remain historical while output keeps
    # arriving at the unseen live bottom.
    $beforeHistoryResize=$scrolledDuringOutput|ConvertFrom-Json
    [void][Wheel]::GetWindowRect($wt.MainWindowHandle,[ref]$rect)
    [void][Wheel]::MoveWindow($wt.MainWindowHandle,$rect.Left,$rect.Top,($rect.Right-$rect.Left)+120,($rect.Bottom-$rect.Top)+60,$true)
    $historyResizeDeadline=[DateTime]::UtcNow.AddSeconds(10);$historyResized='';$historyPicture=$null
    do{
        Start-Sleep -Milliseconds 200
        $historyResized=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18083/snapshot').Content
        $historyPicture=$historyResized|ConvertFrom-Json
    }while($historyPicture.w-eq$beforeHistoryResize.w-and$historyPicture.h-eq$beforeHistoryResize.h-and[DateTime]::UtcNow-lt$historyResizeDeadline)
    if($historyPicture.w-eq$beforeHistoryResize.w-and$historyPicture.h-eq$beforeHistoryResize.h){throw 'WT resize while scrolled did not update native dimensions'}
    if($historyResized-notmatch'SCROLL_'-or$historyResized-match'LIVE_OUTPUT_'){
        $historyResized|Set-Content "$work\snapshot-scrolled-resized-failed.json" -Encoding utf8
        throw 'WT resize/reflow while scrolled lost the historical viewport'
    }
    $historyResized|Set-Content "$work\snapshot-scrolled-resized.json" -Encoding utf8
    [void][Wheel]::GetWindowRect($wt.MainWindowHandle,[ref]$rect)
    [void][Wheel]::SetCursorPos([int](($rect.Left+$rect.Right)/2),[int](($rect.Top+$rect.Bottom)/2))
    1..50 | ForEach-Object { [Wheel]::mouse_event(0x0800,0,0,-120,[UIntPtr]::Zero) }
    $stage='live-output-bottom';[void](Wait-Snapshot 'LIVE_OUTPUT_' 15)

    # Exercise WT's actual UI Automation scrollbar separately from the wheel
    # path above. RangeValue is the writable accessibility surface backed by
    # the visible ScrollBar control, avoiding pixel-coordinate assumptions.
    Add-Type -AssemblyName UIAutomationClient
    $automationRoot=[System.Windows.Automation.AutomationElement]::FromHandle($wt.MainWindowHandle)
    $scrollbarCondition=New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::ScrollBar)
    $scrollbar=$automationRoot.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$scrollbarCondition)
    if(-not$scrollbar){throw 'WT accessibility tree did not expose its visible scrollbar'}
    $scrollRange=[System.Windows.Automation.RangeValuePattern]$scrollbar.GetCurrentPattern([System.Windows.Automation.RangeValuePattern]::Pattern)
    if(-not$scrollRange-or$scrollRange.Current.IsReadOnly){throw 'WT scrollbar did not expose writable RangeValue'}
    $scrollRange.SetValue($scrollRange.Current.Minimum)
    $stage='scrollbar-history';$scrollbarHistory=Wait-Snapshot 'SCROLL_' 10
    $scrollbarHistory|Set-Content "$work\snapshot-scrollbar-history.json" -Encoding utf8
    if($scrollbarHistory-match'LIVE_OUTPUT_'){throw 'WT scrollbar did not leave the live-bottom viewport'}
    $scrollRange.SetValue($scrollRange.Current.Maximum)
    $stage='scrollbar-bottom';[void](Wait-Snapshot 'LIVE_OUTPUT_' 10)

    # Exercise the terminal's real alternate screen independently of the later
    # overload loop. The mirror must show the alternate contents while active,
    # then return to the preserved main buffer when DECSET 1049 is cleared.
    [Chord]::Press(0x11,0x43);Start-Sleep 1
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe ALT_SCREEN_RUN alt'
    [Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
    $stage='alternate-screen-enter';$alternate=Wait-Snapshot 'ALT_SCREEN_FIDELITY' 20
    $alternate|Set-Content "$work\snapshot-alternate.json" -Encoding utf8
    if($alternate-match'WT_MAIN_BEFORE_ALT'){throw 'WT alternate frame retained hidden main-buffer contents'}
    $stage='alternate-screen-exit';$mainAfterAlternate=Wait-Snapshot 'WT_MAIN_AFTER_ALT' 10
    $mainAfterAlternate|Set-Content "$work\snapshot-main-after-alternate.json" -Encoding utf8
    if($mainAfterAlternate-match'ALT_SCREEN_FIDELITY'){throw 'WT main buffer retained alternate-screen contents after exit'}

    # Keep one known-live source selected while measuring exact viewport sizes;
    # repeated diagnostics are emitted every 120 completed captures.
    [Chord]::Press(0x11,0x43);Start-Sleep 2
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class PerfWindow {
 [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left,Top,Right,Bottom; }
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd,out RECT rect);
 [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr hwnd,int x,int y,int width,int height,bool repaint);
}
'@
    $perfHwnd=$wt.MainWindowHandle
    function Current-Snapshot {
        $client=New-Object Net.WebClient
        try{return [Text.Encoding]::UTF8.GetString($client.DownloadData('http://127.0.0.1:18083/snapshot'))|ConvertFrom-Json}
        finally{$client.Dispose()}
    }
    function Assert-Resize-Coherent($picture) {
        $header=[string]$picture.d[0][0][0]
        if($header-notmatch'^RESIZE_COHERENCE_([A-Z])_'){return $false}
        $fill=$Matches[1]
        for($row=1;$row-lt$picture.h;$row++){
            $text=[string]$picture.d[$row][0][0]
            if($text-match"[^$fill ]"){throw "resize frame mixed viewport generations in row ${row}: expected ${fill}"}
        }
        return $true
    }

    # A synchronized fixture repaints every cell with one generation marker.
    # Rapidly move away from and back to exactly the same pixel dimensions before
    # sampling. Cell dimensions alone cannot reject an old queued batch in this
    # case; the adapter's viewport generation must do so, and every accepted
    # frame must contain only one marker.
    [Chord]::Press(0x11,0x43);Start-Sleep 1
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe RESIZE_COHERENCE resize'
    [Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
    $stage='resize-coherence-start';$coherenceDeadline=[DateTime]::UtcNow.AddSeconds(8);$coherentSamples=0
    do{Start-Sleep -Milliseconds 100;$candidate=Current-Snapshot;if(Assert-Resize-Coherent $candidate){$coherentSamples++}}while($coherentSamples-lt1-and[DateTime]::UtcNow-lt$coherenceDeadline)
    if($coherentSamples-lt1){throw 'resize coherence fixture never reached the mirror'}
    $base=New-Object PerfWindow+RECT;[void][PerfWindow]::GetWindowRect($perfHwnd,[ref]$base)
    $baseWidth=$base.Right-$base.Left;$baseHeight=$base.Bottom-$base.Top
    $stage='resize-coherence-roundtrip'
    1..20|ForEach-Object{
        [void][PerfWindow]::MoveWindow($perfHwnd,$base.Left,$base.Top,$baseWidth-140,$baseHeight-70,$true)
        Start-Sleep -Milliseconds 15
        [void][PerfWindow]::MoveWindow($perfHwnd,$base.Left,$base.Top,$baseWidth+170,$baseHeight+90,$true)
        Start-Sleep -Milliseconds 15
        [void][PerfWindow]::MoveWindow($perfHwnd,$base.Left,$base.Top,$baseWidth,$baseHeight,$true)
        Start-Sleep -Milliseconds 40
        $candidate=Current-Snapshot
        if(Assert-Resize-Coherent $candidate){$coherentSamples++}
    }
    if($coherentSamples-lt10){throw "too few coherent resize samples: ${coherentSamples}"}
    $fixtureExitDeadline=[DateTime]::UtcNow.AddSeconds(10)
    do{Start-Sleep -Milliseconds 200;$candidate=Current-Snapshot;$fixtureActive=([string]$candidate.d[0][0][0])-match'^RESIZE_COHERENCE_'}while($fixtureActive-and[DateTime]::UtcNow-lt$fixtureExitDeadline)
    if($fixtureActive){throw 'resize coherence fixture did not restore the main screen'}

    function Resize-Grid([int]$targetCols,[int]$targetRows) {
        for($attempt=0;$attempt-lt12;$attempt++){
            $picture=Current-Snapshot
            if($picture.w-eq$targetCols-and$picture.h-eq$targetRows){return}
            $box=New-Object PerfWindow+RECT;[void][PerfWindow]::GetWindowRect($perfHwnd,[ref]$box)
            $pixelWidth=$box.Right-$box.Left;$pixelHeight=$box.Bottom-$box.Top
            $cellWidth=[Math]::Max(1.0,($pixelWidth-16.0)/[Math]::Max(1,$picture.w))
            $cellHeight=[Math]::Max(1.0,($pixelHeight-48.0)/[Math]::Max(1,$picture.h))
            $colDelta=$targetCols-$picture.w;$rowDelta=$targetRows-$picture.h
            $widthAdjust=if([Math]::Abs($colDelta)-eq1){2*[Math]::Sign($colDelta)}else{$colDelta*$cellWidth}
            $heightAdjust=if([Math]::Abs($rowDelta)-eq1){2*[Math]::Sign($rowDelta)}else{$rowDelta*$cellHeight}
            [void][PerfWindow]::MoveWindow($perfHwnd,$box.Left,$box.Top,[int][Math]::Max(180,$pixelWidth+$widthAdjust),[int][Math]::Max(140,$pixelHeight+$heightAdjust),$true)
            Start-Sleep -Milliseconds 500
        }
        $actual=Current-Snapshot;throw "could not resize grid to ${targetCols}x${targetRows}; got $($actual.w)x$($actual.h)"
    }
    $performance=@();$logical=[Environment]::ProcessorCount
    foreach($size in @(@(80,24),@(240,80),@(320,100))){
        $cols=$size[0];$rows=$size[1]
        [Chord]::Press(0x11,0x10,0x4b);Start-Sleep 1
        if($cols-eq80){[Chord]::Press(0x11,0x10,0x49);Start-Sleep 1}
        elseif($cols-eq240){[Chord]::Press(0x11,0x10,0x4a);Start-Sleep 1}
        elseif($cols-eq320){[Chord]::Press(0x11,0x10,0x4c);Start-Sleep 1}
        $stage="performance-resize-${cols}x${rows}";$stage|Set-Content "$work\stage.txt" -Encoding ascii
        Resize-Grid $cols $rows
        $diagnosticsBefore=([regex]::Matches((Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue),'WT render callback p95<=')).Count
        $wt.Refresh();$cpuBefore=$wt.TotalProcessorTime.TotalMilliseconds;$memoryBefore=$wt.PrivateMemorySize64;$clock=[Diagnostics.Stopwatch]::StartNew()
        Set-Clipboard "C:\work\shellglass-wt-fixture.exe PERF_${cols}x${rows}";[Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
        $stage="performance-${cols}x${rows}";[void](Wait-Snapshot "PERF_${cols}x${rows}" 30)
        $diagnosticDeadline=[DateTime]::UtcNow.AddSeconds(10);$performanceLog=''
        do{Start-Sleep -Milliseconds 200;$performanceLog=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue;$diagnosticsNow=([regex]::Matches($performanceLog,'WT render callback p95<=')).Count}while($diagnosticsNow-lt$diagnosticsBefore+2-and[DateTime]::UtcNow-lt$diagnosticDeadline)
        if($diagnosticsNow-lt$diagnosticsBefore+2){throw "fewer than two callback intervals were measured at ${cols}x${rows}"}
        $matches=[regex]::Matches($performanceLog,'WT render callback p95<=(\d+)us');$sizeP95=[int]$matches[$matches.Count-1].Groups[1].Value
        if($sizeP95-gt1000){throw "callback p95 exceeded 1ms at ${cols}x${rows}: ${sizeP95}us"}
        $wt.Refresh();$clock.Stop();$cpuMs=$wt.TotalProcessorTime.TotalMilliseconds-$cpuBefore
        $sample=[pscustomobject]@{cols=$cols;rows=$rows;p95_us=$sizeP95;cpu_percent=[Math]::Round(100*$cpuMs/[Math]::Max(1,$clock.Elapsed.TotalMilliseconds*$logical),2);private_bytes=$wt.PrivateMemorySize64;active_private_delta=$wt.PrivateMemorySize64-$memoryBefore}
        if($sample.cpu_percent-gt5){throw "WT CPU exceeded 5% of machine capacity at ${cols}x${rows}: $($sample.cpu_percent)%"}
        if($sample.private_bytes-gt512MB){throw "WT private memory exceeded 512 MiB at ${cols}x${rows}: $($sample.private_bytes)"}
        if($sample.active_private_delta-gt16MB){throw "WT active capture grew private memory by more than 16 MiB at ${cols}x${rows}: $($sample.active_private_delta)"}
        $performance+=$sample
        [Chord]::Press(0x11,0x43);Start-Sleep 2
    }
    $performance|ConvertTo-Json|Set-Content "$work\performance.json" -Encoding utf8
    [Chord]::Press(0x11,0x10,0x4b);Start-Sleep 1
    $box=New-Object PerfWindow+RECT;[void][PerfWindow]::GetWindowRect($perfHwnd,[ref]$box);[void][PerfWindow]::MoveWindow($perfHwnd,$box.Left,$box.Top,850,550,$true);Start-Sleep 2
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe FIRST_TAB_MARKER';[Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
    $stage='performance-restore';[void](Wait-Snapshot 'FIRST_TAB_MARKER' 30)


    # Two post-hook tabs produce distinct ControlCore/Renderer sources. Focus
    # transitions must switch the broker deterministically and force full frames.
    [void]$keys.AppActivate($wt.Id)
    [Chord]::Press(0x11,0x10,0x54)
    Start-Sleep 3
    Set-Clipboard 'C:\work\shellglass-wt-fixture.exe SECOND_TAB_MARKER'
    $keys.SendKeys('+{INSERT}')
    $keys.SendKeys('{ENTER}')
    $secondTab=Wait-Snapshot 'SECOND_TAB_MARKER'
    $secondTab | Set-Content "$work\snapshot-second-tab.json" -Encoding utf8
    [Chord]::Press(0x11,0x10,0x09)
    $stage='tab-return';$firstAgain = Wait-Snapshot 'FIRST_TAB_MARKER'
    $firstAgain | Set-Content "$work\snapshot-first-again.json" -Encoding utf8
    if ($firstAgain -match 'SECOND_TAB_MARKER') { throw 'tab focus switch retained the wrong WT source' }

    # Exercise rapid tab focus transitions, then prove that a second top-level
    # window receives its own source and that foregrounding the first window
    # restores the original source rather than retaining the newer one.
    1..8 | ForEach-Object {
        [Chord]::Press(0x11,0x09)
        Start-Sleep -Milliseconds 80
        [Chord]::Press(0x11,0x10,0x09)
        Start-Sleep -Milliseconds 80
    }
    $stage='rapid-focus';$afterRapid = Wait-Snapshot 'FIRST_TAB_MARKER'
    if ($afterRapid -match 'SECOND_TAB_MARKER') { throw 'rapid tab focus transitions retained the wrong source' }

    $beforeFourthWindows=@(Get-Process WindowsTerminal -ErrorAction Stop|ForEach-Object{[Wheel]::ForPid($_.Id)}|ForEach-Object{[int64]$_})
    [Chord]::Press(0x11,0x10,0x4e)
    $fourthDeadline=[DateTime]::UtcNow.AddSeconds(20);$fourthWindow=[IntPtr]::Zero;$fourthProcess=$null
    do{
        Start-Sleep -Milliseconds 250
        foreach($process in (Get-Process WindowsTerminal -ErrorAction SilentlyContinue)){
            foreach($handle in [Wheel]::ForPid($process.Id)){if($beforeFourthWindows -notcontains ([int64]$handle)){$fourthWindow=$handle;$fourthProcess=$process;break}}
            if($fourthWindow-ne[IntPtr]::Zero){break}
        }
    }while($fourthWindow-eq[IntPtr]::Zero-and[DateTime]::UtcNow-lt$fourthDeadline)
    if($fourthWindow-eq[IntPtr]::Zero-or-not$fourthProcess){throw 'new-window chord did not create another WT window'}
    if($fourthProcess.Id-ne$wt.Id){
        & "$work\shellglass-inject.exe" $fourthProcess.Id "$work\shellglass-wt-adapter.dll"
        if($LASTEXITCODE){throw 'second WT process injection failed'}
    }
    [void][Wheel]::ShowWindow($fourthWindow,5)
    if(-not[Wheel]::SetForegroundWindow($fourthWindow)){throw 'could not foreground new WT window'}
    [Chord]::Press(0x11,0x10,0x4d)
    [void](Wait-Snapshot 'FOURTH_WINDOW_MARKER')
    [Chord]::Press(0x12,0x73)
    $fourthCloseDeadline=[DateTime]::UtcNow.AddSeconds(10)
    while([Wheel]::IsWindow($fourthWindow)-and[DateTime]::UtcNow-lt$fourthCloseDeadline){Start-Sleep -Milliseconds 200}
    if([Wheel]::IsWindow($fourthWindow)){throw 'second WT window did not close'}
    if($fourthProcess.Id-ne$wt.Id){$fourthProcess.Refresh();if(-not$fourthProcess.HasExited){Stop-Process $fourthProcess.Id -Force};Start-Sleep 2}
    $wt.Refresh();$initialWindow=$wt.MainWindowHandle
    if($initialWindow-eq[IntPtr]::Zero-or-not[Wheel]::IsWindow($initialWindow)){throw 'original WT window disappeared after multi-window close'}
    [void][Wheel]::ShowWindow($initialWindow,5)
    if (-not [Wheel]::SetForegroundWindow($initialWindow)) { throw 'could not restore the first WT window' }
    $restoreRect=New-Object Wheel+RECT;[void][Wheel]::GetWindowRect($initialWindow,[ref]$restoreRect)
    [void][Wheel]::SetCursorPos([int](($restoreRect.Left+$restoreRect.Right)/2),[int](($restoreRect.Top+$restoreRect.Bottom)/2))
    [Wheel]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero);[Wheel]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero)
    $foregroundDeadline=[DateTime]::UtcNow.AddSeconds(5)
    while([Wheel]::GetForegroundWindow()-ne$initialWindow-and[DateTime]::UtcNow-lt$foregroundDeadline){Start-Sleep -Milliseconds 100}
    if([Wheel]::GetForegroundWindow()-ne$initialWindow){throw 'first WT HWND did not become foreground'}
    [Chord]::Press(0x11,0x09);Start-Sleep -Milliseconds 300
    [Chord]::Press(0x11,0x10,0x09)
    $stage='multi-window-return';$firstWindowAgain = Wait-Snapshot 'FIRST_TAB_MARKER'
    if ($firstWindowAgain -match 'FOURTH_WINDOW_MARKER') { throw 'top-level window focus retained the wrong WT source' }

    # Keep the pipe connected but suspend its reader under sustained 320x100
    # synchronized updates. The worker may block; the render callback must not.
    # Its capacity-one slot must replace stale batches. A separate hard broker
    # restart below verifies disconnect dormancy and full re-registration.
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class BrokerPause {
 [DllImport("ntdll.dll")] public static extern int NtSuspendProcess(IntPtr process);
 [DllImport("ntdll.dll")] public static extern int NtResumeProcess(IntPtr process);
}
'@
    [Chord]::Press(0x11,0x43);Start-Sleep 1
    Set-Clipboard "C:\work\shellglass-wt-fixture.exe OVERLOAD_MARKER $StressSeconds"
    [Chord]::Press(0x11,0x10,0x56);[Chord]::Press(0x0d)
    $stage='overload-start';[void](Wait-Snapshot 'OVERLOAD_BEGIN' 20)
    $wt.Refresh();$overloadCpuBefore=$wt.TotalProcessorTime.TotalMilliseconds;$overloadMemoryBefore=$wt.PrivateMemorySize64;$overloadClock=[Diagnostics.Stopwatch]::StartNew()
    if($overloadMemoryBefore-gt160MB){throw "dormant/closed WT sources retained too much private memory before overload: $overloadMemoryBefore"}
    if([BrokerPause]::NtSuspendProcess($server.Handle)-ne0){throw 'could not suspend the broker reader for overload testing'}
    $serverSuspended=$true
    Start-Sleep 5
    # Focus hooks share registry state with the worker, but must never wait
    # behind its blocked pipe write. A tab round-trip while the reader is frozen
    # catches accidental lock coupling on the target UI thread.
    [Chord]::Press(0x11,0x09);Start-Sleep -Milliseconds 500
    [Chord]::Press(0x11,0x10,0x09);Start-Sleep 7
    $wt.Refresh()
    if ($wt.HasExited -or -not $wt.Responding) { throw 'WT became unresponsive during focus changes with the broker reader stalled' }
    if([BrokerPause]::NtResumeProcess($server.Handle)-ne0){throw 'could not resume the broker reader after overload testing'}
    $serverSuspended=$false
    $stage='slow-broker-catchup';[void](Wait-Snapshot 'OVERLOAD_COMPLETE' ($StressSeconds+15))
    $dropDeadline=[DateTime]::UtcNow.AddSeconds(10);$overloadLog='';$dropped=0
    do{
        Start-Sleep -Milliseconds 200
        $overloadLog=Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue
        $dropMatches=[regex]::Matches($overloadLog,'WT render callback p95<=\d+us max=\d+us count=\d+ sample=\d+ dropped=(\d+)')
        if($dropMatches.Count){$dropped=($dropMatches|ForEach-Object{[int64]$_.Groups[1].Value}|Measure-Object -Maximum).Maximum}
    }while($dropped-le0-and[DateTime]::UtcNow-lt$dropDeadline)
    if($dropped-le0){throw 'stalled-broker overload did not report replacement of stale capture frames'}
    $wt.Refresh();$overloadClock.Stop();$overloadCpuMs=$wt.TotalProcessorTime.TotalMilliseconds-$overloadCpuBefore
    $overloadCpu=[Math]::Round(100*$overloadCpuMs/[Math]::Max(1,$overloadClock.Elapsed.TotalMilliseconds*$logical),2)
    $overloadDelta=$wt.PrivateMemorySize64-$overloadMemoryBefore
    if($overloadCpu-gt10){throw "sustained capture exceeded 10% of machine CPU capacity: ${overloadCpu}%"}
    if($wt.PrivateMemorySize64-gt512MB-or$overloadDelta-gt64MB){throw "sustained capture exceeded memory bound: private=$($wt.PrivateMemorySize64) delta=$overloadDelta"}
    [pscustomobject]@{seconds=[Math]::Round($overloadClock.Elapsed.TotalSeconds,2);cpu_percent=$overloadCpu;private_bytes=$wt.PrivateMemorySize64;private_delta=$overloadDelta;dropped_frames=$dropped}|ConvertTo-Json|Set-Content "$work\overload.json" -Encoding utf8

    Stop-Process $server.Id -Force
    Start-Sleep 2
    $wt.Refresh()
    if($wt.HasExited-or-not$wt.Responding){throw 'WT became unresponsive while the broker was absent'}
    $server=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" `
        -ArgumentList @('serve','--bind','127.0.0.1:18083') `
        -RedirectStandardOutput "$work\server-restart.out" -RedirectStandardError "$work\server-restart.err"
    Start-Sleep 2
    $stage='broker-restart';[void](Wait-Snapshot 'OVERLOAD_COMPLETE' 20)

    # Windows Sandbox runs this stock WT and its broker elevated. Prove that
    # coverage from mandatory-label RIDs rather than assuming elevation from the
    # launcher, and require an equivalently authorized broker token.
    Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class TokenIntegrity {
 [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
 [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr process,uint access,out IntPtr token);
 [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,IntPtr data,int length,out int needed);
 [DllImport("advapi32.dll")] static extern IntPtr GetSidSubAuthorityCount(IntPtr sid);
 [DllImport("advapi32.dll")] static extern IntPtr GetSidSubAuthority(IntPtr sid,uint index);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
 public static int Rid(uint pid) {
  var process=OpenProcess(0x1000,false,pid); if(process==IntPtr.Zero)throw new System.ComponentModel.Win32Exception();
  IntPtr token; if(!OpenProcessToken(process,8,out token)){CloseHandle(process);throw new System.ComponentModel.Win32Exception();}
  int needed;GetTokenInformation(token,25,IntPtr.Zero,0,out needed);var data=Marshal.AllocHGlobal(needed);
  try { if(!GetTokenInformation(token,25,data,needed,out needed))throw new System.ComponentModel.Win32Exception();
   var sid=Marshal.ReadIntPtr(data);var count=Marshal.ReadByte(GetSidSubAuthorityCount(sid));return Marshal.ReadInt32(GetSidSubAuthority(sid,(uint)(count-1)));
  } finally {Marshal.FreeHGlobal(data);CloseHandle(token);CloseHandle(process);}
 }
}
'@
    $normalRid=[TokenIntegrity]::Rid($wt.Id);$brokerRid=[TokenIntegrity]::Rid($server.Id)
    if($normalRid-lt0x3000-or$brokerRid-lt$normalRid){throw "high-integrity coverage was not real/equivalent: WT=$normalRid broker=$brokerRid"}

    # Switch the already-injected live WT process from foreground serve to the
    # production detached stream control plane. This is a real target -> native
    # pipe -> detached push -> hub snapshot gate, including pause/freeze and a
    # fresh full frame after resume. No terminal process or Sandbox is restarted.
    Stop-Process $server.Id -Force;Start-Sleep 2
    $detachedKey='sandbox-detached-native-key'
    $detachedId=(& "$work\shellglass-stock.exe" print-id --key $detachedKey).Trim()
    if($LASTEXITCODE-or-not$detachedId){throw 'could not derive detached-stream session id'}
    $hub=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass-stock.exe" `
        -ArgumentList @('hub','--bind','127.0.0.1:18084','--allow',"${detachedId}:native-detached") `
        -RedirectStandardOutput "$work\hub.out" -RedirectStandardError "$work\hub.err"
    Start-Sleep 2
    $stage='detached-start';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    $streamStart=Invoke-Shellglass @('stream','start','--hub','http://127.0.0.1:18084','--key',$detachedKey) 'start'
    $detachedStarted=$true
    $stage='detached-first-frame';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    $detachedDeadline=[DateTime]::UtcNow.AddSeconds(20);$detachedRaw=''
    do{
        Start-Sleep -Milliseconds 200
        if($hub.HasExited){throw "detached test hub exited: $(Get-Content $work\hub.err -Raw)"}
        try{$detachedRaw=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18084/s/native-detached/snapshot').Content}catch{$detachedRaw=''}
    }while($detachedRaw-notmatch'OVERLOAD_COMPLETE'-and[DateTime]::UtcNow-lt$detachedDeadline)
    if($detachedRaw-notmatch'OVERLOAD_COMPLETE'){throw 'detached stream did not publish the existing real WT source through the hub'}
    $stage='detached-pause';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    $pauseResult=Invoke-Shellglass @('stream','pause') 'pause'
    [void]$keys.AppActivate($wt.Id);[Chord]::Press(0x11,0x43);Start-Sleep 1
    # Type and correct the final character rather than pasting this command, so
    # the gate also proves ordinary prompt editing remains locally functional.
    $keys.SendKeys('Write-Output DETACHED_RESUME_MARKEX');$keys.SendKeys('{BACKSPACE}');$keys.SendKeys('R');$keys.SendKeys('{ENTER}');Start-Sleep 2
    $pausedRaw=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18084/s/native-detached/snapshot').Content
    if($pausedRaw-match'DETACHED_RESUME_MARKER'){throw 'paused detached stream did not freeze the published frame'}
    $stage='detached-resume';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    $resumeResult=Invoke-Shellglass @('stream','resume') 'resume'
    $resumeDeadline=[DateTime]::UtcNow.AddSeconds(15);$resumedRaw=''
    do{Start-Sleep -Milliseconds 200;$resumedRaw=(Invoke-WebRequest -UseBasicParsing 'http://127.0.0.1:18084/s/native-detached/snapshot').Content}while($resumedRaw-notmatch'DETACHED_RESUME_MARKER'-and[DateTime]::UtcNow-lt$resumeDeadline)
    if($resumedRaw-notmatch'DETACHED_RESUME_MARKER'){throw 'detached stream resume did not request and publish a fresh full frame'}
    $stage='detached-status';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    $statusResult=Invoke-Shellglass @('stream','status') 'status'
    if($statusResult-notmatch'^shellglass-wt-tap stream: streaming; sources=[1-9]'){throw "detached stream status failed: $statusResult"}
    $stage='detached-stop';$stage|Set-Content "$work\stage.txt" -Encoding ascii
    [void](Invoke-Shellglass @('stream','stop') 'stop')
    $detachedStopped=$false
    for($attempt=0;$attempt-lt50;$attempt++){
        $probe=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" -ArgumentList @('stream','status')
        if($probe.WaitForExit(1000)-and$probe.ExitCode){$detachedStopped=$true;break}
        if(-not$probe.HasExited){Stop-Process $probe.Id -Force -ErrorAction SilentlyContinue}
        Start-Sleep -Milliseconds 100
    }
    if(-not$detachedStopped){throw 'detached stream worker did not release its control pipe after stop'}
    $detachedStarted=$false
    Stop-Process $hub.Id -Force;$hub=$null;Start-Sleep 2
    $server=Start-Process -PassThru -WindowStyle Hidden "$work\shellglass.exe" `
        -ArgumentList @('serve','--bind','127.0.0.1:18083') `
        -RedirectStandardOutput "$work\server-fault.out" -RedirectStandardError "$work\server-fault.err"
    Start-Sleep 3

    # Load a test-only build into a fresh stock process. Its first real
    # PaintBufferLine callback takes the same production disable path used by an
    # internal callback failure: atomics on the render thread, then diagnostic
    # and source removal on the worker.
    Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Stop-Process -Force
    $goneDeadline=[DateTime]::UtcNow.AddSeconds(10)
    while((Get-Process WindowsTerminal -ErrorAction SilentlyContinue)-and[DateTime]::UtcNow-lt$goneDeadline){Start-Sleep -Milliseconds 200}
    $faultDir="$work\fault";New-Item $faultDir -ItemType Directory -Force|Out-Null
    Copy-Item "$work\shellglass-wt-fault-adapter.dll" "$faultDir\shellglass-wt-adapter.dll" -Force
    Copy-Item "$work\shellglass-wt-adapter.sgnp" "$faultDir\shellglass-wt-adapter.sgnp" -Force
    Start-Process explorer.exe 'shell:AppsFolder\Microsoft.WindowsTerminal_8wekyb3d8bbwe!App'
    $faultDeadline=[DateTime]::UtcNow.AddSeconds(20);$wt=$null
    do{Start-Sleep -Milliseconds 250;$wt=Get-Process WindowsTerminal -ErrorAction SilentlyContinue|Where-Object{$_.MainWindowHandle-ne0}|Select-Object -First 1}while(-not$wt-and[DateTime]::UtcNow-lt$faultDeadline)
    if(-not$wt){throw 'fault-containment WT did not start'}
    & "$work\shellglass-inject.exe" $wt.Id "$faultDir\shellglass-wt-adapter.dll"
    if($LASTEXITCODE){throw 'fault-containment adapter injection failed'}
    if(-not$keys.AppActivate($wt.Id)){throw 'could not foreground fault-containment WT'}
    [Chord]::Press(0x11,0x10,0x54)
    $faultDeadline=[DateTime]::UtcNow.AddSeconds(15);$faultLog=''
    do{Start-Sleep -Milliseconds 200;$faultLog=Get-Content "$work\server-fault.err" -Raw -ErrorAction SilentlyContinue}while($faultLog-notmatch'native diagnostic 202: WT capture provider disabled after an internal callback fault'-and[DateTime]::UtcNow-lt$faultDeadline)
    if($faultLog-notmatch'native diagnostic 202: WT capture provider disabled after an internal callback fault'){throw 'callback fault diagnostic was not emitted off the render thread'}
    $wt.Refresh()
    if ($wt.HasExited -or -not $wt.Responding) { throw 'WT was not responsive after capture' }
    $brokerLogs=(Get-Content "$work\server.err" -Raw -ErrorAction SilentlyContinue)+(Get-Content "$work\server-restart.err" -Raw -ErrorAction SilentlyContinue)+(Get-Content "$work\server-fault.err" -Raw -ErrorAction SilentlyContinue)
    if($brokerLogs-match'native adapter disconnected'){throw "native adapter disconnected during a nominal gate: $($Matches[0])"}
    $passed = $true
    $detail = "stock WT fidelity/resize/rapid-resize-coherence/alternate-screen/sticky-last-terminal/live-output-scrollback-reflow/tab/multi-window/elevated/rapid-focus/broker-restart/detached-push-pause-resume/newest-wins-overload/callback-fault capture passed with render callback p95<=${p95}us"
} catch {
    $detail = ($_ | Out-String)
    if ($wt -and -not $wt.HasExited) {
        $wt.Refresh()
        if (-not $wt.Responding) {
            & rundll32.exe C:\Windows\System32\comsvcs.dll, MiniDump $wt.Id "$work\dumps\WindowsTerminal-hang.dmp" full 2>$null
        }
    }
} finally {
    if($serverSuspended-and$server-and-not$server.HasExited){[void][BrokerPause]::NtResumeProcess($server.Handle)}
    if($detachedStarted){try{[void](Invoke-Shellglass @('stream','stop') 'cleanup-stop' 3)}catch{}}
    if($hub-and-not$hub.HasExited){Stop-Process $hub.Id -Force -ErrorAction SilentlyContinue}
    @{ passed = $passed; detail = $detail } | ConvertTo-Json | Set-Content $resultPath -Encoding utf8
    Get-Process WindowsTerminal -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if ($server -and -not $server.HasExited) { Stop-Process $server.Id -Force }
}
