param(
    [string]$ExpectedVersion='1.24.11911.0',
    [ValidateRange(30,300)][int]$StressSeconds=30,
    [switch]$Persistent,
    [switch]$IncludeOperator
)
# Runs the aggregate fidelity/performance gate and deterministic lifecycle gate
# sequentially inside one Sandbox boot. In persistent mode it waits for a
# host-written rerun.request after each result, allowing fixes to be copied into
# the existing guest instead of restarting Hyper-V.
$ErrorActionPreference='Stop'
$work='C:\work'
$combined="$work\combined-result.json"
$request="$work\rerun.request"

function Read-ChildResult([string]$name) {
    $path="$work\result.json"
    if(-not(Test-Path $path)){throw "$name gate returned without result.json"}
    $value=Get-Content $path -Raw|ConvertFrom-Json
    Copy-Item $path "$work\$name-result.json" -Force
    return $value
}
function Save-IfPresent([string]$path,[string]$name) {
    if(Test-Path $path){Copy-Item $path "$work\$name" -Force}
}
function Run-Gates {
    Remove-Item $combined,"$work\result.json" -Force -ErrorAction SilentlyContinue
    try {
        # A persistent keeper must isolate Add-Type declarations and native test
        # state on every rerun. Child PowerShell processes provide a fresh CLR
        # runspace without rebooting the Sandbox.
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$work\aggregate.ps1" -ExpectedVersion $ExpectedVersion -StressSeconds $StressSeconds
        $aggregate=Read-ChildResult 'aggregate'
        Save-IfPresent "$work\server.err" 'aggregate-server.err'
        Save-IfPresent "$work\server-restart.err" 'aggregate-server-restart.err'
        Save-IfPresent "$work\performance.json" 'aggregate-performance.json'
        Save-IfPresent "$work\overload.json" 'aggregate-overload.json'
        if(-not$aggregate.passed){throw "aggregate gate failed: $($aggregate.detail)"}

        Remove-Item "$work\result.json" -Force
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$work\lifecycle.ps1" -ExpectedVersion $ExpectedVersion
        $lifecycle=Read-ChildResult 'lifecycle'
        Save-IfPresent "$work\server.err" 'lifecycle-server.err'
        if(-not$lifecycle.passed){throw "lifecycle gate failed: $($lifecycle.detail)"}

        $operatorDetail='not requested'
        if($IncludeOperator){
            Remove-Item "$work\operator-result.json" -Force -ErrorAction SilentlyContinue
            & powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$work\operator.ps1"
            if(-not(Test-Path "$work\operator-result.json")){throw 'operator launcher gate returned without operator-result.json'}
            $operator=Get-Content "$work\operator-result.json" -Raw|ConvertFrom-Json
            if(-not$operator.passed){throw "operator launcher gate failed: $($operator.detail)"}
            $operatorDetail=$operator.detail
        }

        $detail="aggregate: $($aggregate.detail); lifecycle: $($lifecycle.detail)"
        if($IncludeOperator){$detail+="; operator: $operatorDetail"}
        @{passed=$true;detail=$detail}|ConvertTo-Json|Set-Content $combined -Encoding utf8
    } catch {
        @{passed=$false;detail=($_|Out-String)}|ConvertTo-Json|Set-Content $combined -Encoding utf8
    }
}

do {
    Remove-Item $request -Force -ErrorAction SilentlyContinue
    Run-Gates
    if(-not$Persistent){break}
    while(-not(Test-Path $request)){Start-Sleep -Milliseconds 250}
} while($true)
