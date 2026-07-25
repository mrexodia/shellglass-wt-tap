$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$tap = Join-Path $root 'target/debug/shellglass-wt-tap.exe'
$stockShellglass = Join-Path $root '../shellglass/target/debug/shellglass.exe'
$mock = Join-Path $root 'target/native-windows/Release/shellglass-native-mock.exe'
$stdout = Join-Path $env:TEMP 'shellglass-native-e2e.out'
$stderr = Join-Path $env:TEMP 'shellglass-native-e2e.err'
$recordDir = Join-Path $env:TEMP "shellglass-native-e2e-record-$PID"
$port = 18081
$hubPort = 18082
$sshPort = 18085
$sshHostKey = Join-Path $env:TEMP "shellglass-native-e2e-hostkey-$PID"
$key = 'shellglass-native-e2e-key'
$imageKey = 'f11fb145fb56636723b20f30e40aaac672e9de2c9677de363551d82668cbd5cd'
$id = (& $stockShellglass print-id --key $key).Trim()
Remove-Item $recordDir -Recurse -Force -ErrorAction SilentlyContinue
New-Item $recordDir -ItemType Directory | Out-Null

function Start-NativeServer {
    Start-Process -PassThru -WindowStyle Hidden -FilePath $tap `
        -ArgumentList @('serve', '--bind', "127.0.0.1:$port", '--record-dir', $recordDir,
                        '--ssh-bind', "127.0.0.1:$sshPort", '--ssh-host-key', $sshHostKey, '--ssh-motd-delay', '0') `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
}

function Start-TestHub {
    Start-Process -PassThru -WindowStyle Hidden -FilePath $stockShellglass `
        -ArgumentList @('hub', '--bind', "127.0.0.1:$hubPort", '--allow', $id) `
        -RedirectStandardOutput (Join-Path $env:TEMP 'shellglass-native-hub.out') `
        -RedirectStandardError (Join-Path $env:TEMP 'shellglass-native-hub.err')
}

function Assert-Png([string]$url) {
    $client = New-Object Net.WebClient
    try { $bytes = $client.DownloadData($url) } finally { $client.Dispose() }
    if ($bytes.Length -lt 8 -or $bytes[0] -ne 0x89 -or $bytes[1] -ne 0x50 -or
        $bytes[2] -ne 0x4e -or $bytes[3] -ne 0x47) {
        throw "native image endpoint did not return PNG bytes: $url"
    }
}

function Wait-HubSnapshot($hub, [int]$seconds = 15) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    $snapshot = ''
    do {
        Start-Sleep -Milliseconds 150
        if ($hub.HasExited) { throw "test hub exited with $($hub.ExitCode)" }
        try { $snapshot = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$hubPort/s/$id/snapshot").Content }
        catch { $snapshot = '' }
    } while ($snapshot -notmatch 'shellglass native mock adapter' -and [DateTime]::UtcNow -lt $deadline)
    if ($snapshot -notmatch 'shellglass native mock adapter') {
        throw "native push did not reach/recover through hub: $snapshot"
    }
}

function Read-NativeSsh {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = "$env:SystemRoot\System32\OpenSSH\ssh.exe"
    $info.Arguments = "-tt -o BatchMode=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -p $sshPort viewer@127.0.0.1"
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = New-Object Diagnostics.Process
    $process.StartInfo = $info
    if (-not $process.Start()) { throw 'could not start OpenSSH viewer fixture' }
    Start-Sleep 2
    $process.StandardInput.Write('q')
    $process.StandardInput.Close()
    if (-not $process.WaitForExit(5000)) {
        $process.Kill()
        throw 'native-source SSH viewer did not exit after q'
    }
    return $process.StandardOutput.ReadToEnd() + $process.StandardError.ReadToEnd()
}

function Wait-NativeSnapshot($server, [int]$seconds = 10) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    $snapshot = ''
    do {
        Start-Sleep -Milliseconds 100
        if ($server.HasExited) {
            throw "shellglass exited: $(Get-Content $stderr -Raw)"
        }
        try {
            $snapshot = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$port/snapshot").Content
        } catch {
            $snapshot = ''
        }
    } while ($snapshot -notmatch 'shellglass native mock adapter' -and [DateTime]::UtcNow -lt $deadline)
    if ($snapshot -notmatch 'shellglass native mock adapter') {
        throw "native frame did not reach HTTP snapshot: $snapshot"
    }
}

$server = $null
$adapter = $null
$hub = $null
$pusher = $null
try {
    $server = Start-NativeServer
    # Wait for HTTP readiness before starting the adapter, so a connection failure
    # cannot be confused with ordinary server startup.
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 100
        if ($server.HasExited) { throw "shellglass exited during startup: $(Get-Content $stderr -Raw)" }
        try { $ready = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$port/snapshot").StatusCode -eq 200 }
        catch { $ready = $false }
    } while (-not $ready -and [DateTime]::UtcNow -lt $deadline)
    if (-not $ready) { throw 'shellglass HTTP server did not become ready' }

    $adapter = Start-Process -PassThru -WindowStyle Hidden -FilePath $mock
    Wait-NativeSnapshot $server
    Assert-Png "http://127.0.0.1:$port/images/$imageKey"
    if ($adapter.HasExited) { throw "mock adapter exited with $($adapter.ExitCode)" }

    # SSH consumes the same generic frame receiver as HTTP. Exercise the actual
    # read-only ANSI transport rather than treating HTTP success as a proxy.
    $sshView = Read-NativeSsh
    if ($sshView -notmatch 'shellglass native mock adapter') {
        throw "native source frame did not reach the SSH viewer: $sshView"
    }

    # The same generic frame stream must feed serve-mode native recordings.
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    $recorded = ''
    do {
        Start-Sleep -Milliseconds 100
        $recorded = Get-ChildItem $recordDir -Filter '*.sgs' -ErrorAction SilentlyContinue |
            ForEach-Object { Get-Content $_.FullName -Raw } | Out-String
    } while ($recorded -notmatch 'shellglass native mock adapter' -and [DateTime]::UtcNow -lt $deadline)
    if ($recorded -notmatch 'shellglass native mock adapter' -or $recorded -notmatch $imageKey) {
        throw 'native source text/image frame did not enter the backend-agnostic recording pipeline'
    }

    # Kill/recreate the broker while the adapter remains alive. It must reconnect,
    # repeat registration, receive a fresh subscription, and send a complete frame.
    Stop-Process -Id $server.Id -Force
    $server.WaitForExit()
    $server = Start-NativeServer
    Wait-NativeSnapshot $server
    if ($adapter.HasExited) { throw "adapter exited instead of reconnecting ($($adapter.ExitCode))" }

    # Move the still-running adapter to a push client and verify both initial hub
    # delivery and the push client's own reconnect/full-frame behavior.
    Stop-Process -Id $server.Id -Force
    $server.WaitForExit()
    $server = $null
    $hub = Start-TestHub
    $pusher = Start-Process -PassThru -WindowStyle Hidden -FilePath $tap `
        -ArgumentList @('push', "http://127.0.0.1:$hubPort", '--key', $key) `
        -RedirectStandardOutput (Join-Path $env:TEMP 'shellglass-native-push.out') `
        -RedirectStandardError (Join-Path $env:TEMP 'shellglass-native-push.err')
    Wait-HubSnapshot $hub
    Assert-Png "http://127.0.0.1:$hubPort/s/$id/images/$imageKey"
    Stop-Process -Id $hub.Id -Force
    $hub.WaitForExit()
    $hub = Start-TestHub
    Wait-HubSnapshot $hub
    Assert-Png "http://127.0.0.1:$hubPort/s/$id/images/$imageKey"
    if ($pusher.HasExited) { throw "native pusher exited instead of reconnecting ($($pusher.ExitCode))" }

    Write-Host 'native mock -> serve/SSH/image/recording + broker restart + push/hub reconnect: OK'
} finally {
    if ($adapter -and -not $adapter.HasExited) { Stop-Process -Id $adapter.Id -Force }
    if ($pusher -and -not $pusher.HasExited) { Stop-Process -Id $pusher.Id -Force }
    if ($hub -and -not $hub.HasExited) { Stop-Process -Id $hub.Id -Force }
    if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force }
    Remove-Item $recordDir -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $sshHostKey,"$sshHostKey.pub" -Force -ErrorAction SilentlyContinue
}
