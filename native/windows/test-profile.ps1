$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$release = Join-Path $root 'target/native-windows/Release'
$tool = Join-Path $release 'shellglass-profile.exe'
$dll = Join-Path $release 'shellglass-profile-fixture.dll'
$pdb = Join-Path $release 'shellglass-profile-fixture.pdb'
$conhostDll = Join-Path $release 'shellglass-conhost-profile-fixture.dll'
$conhostPdb = Join-Path $release 'shellglass-conhost-profile-fixture.pdb'
$out = Join-Path $env:TEMP 'shellglass-profile-fixture.sgnp'
$conhostOut = Join-Path $env:TEMP 'shellglass-conhost-profile-fixture.sgnp'
$bad = Join-Path $env:TEMP 'shellglass-profile-must-not-exist.sgnp'
Remove-Item $out,$conhostOut,$bad,"$out.report.json","$conhostOut.report.json","$bad.report.json" -Force -ErrorAction SilentlyContinue

& $tool $dll wt_fixture $out $pdb | Out-Host
if ($LASTEXITCODE -ne 0) { throw "valid exact profile generation failed ($LASTEXITCODE)" }
$bytes = [IO.File]::ReadAllBytes($out)
if ($bytes.Length -lt 100 -or [Text.Encoding]::ASCII.GetString($bytes,0,4) -ne 'SGNP') {
    throw 'generated profile has an invalid envelope'
}
$goodReport = Get-Content "$out.report.json" -Raw | ConvertFrom-Json
if ($goodReport.status -ne 'compatible' -or $goodReport.family -ne 'wt_fixture') {
    throw 'successful compatibility report is incorrect'
}

& $tool $conhostDll conhost_fixture $conhostOut $conhostPdb | Out-Host
if ($LASTEXITCODE -ne 0) { throw "valid conhost profile generation failed ($LASTEXITCODE)" }
$conhostReport = Get-Content "$conhostOut.report.json" -Raw | ConvertFrom-Json
if ($conhostReport.status -ne 'compatible' -or $conhostReport.family -ne 'conhost_fixture') {
    throw 'successful conhost compatibility report is incorrect'
}

[IO.File]::WriteAllBytes($bad, [byte[]](1,2,3))
# The same exact PE/PDB cannot masquerade as another ABI family: conhost requires
# PaintFrame, which this WT fixture intentionally lacks. Missing required symbols
# must remain a loud nonzero failure with no profile artifact.
$ErrorActionPreference = 'Continue'
& $tool $dll conhost_fixture $bad $pdb 2>$null | Out-Null
$badExit = $LASTEXITCODE
$ErrorActionPreference = 'Stop'
if ($badExit -eq 0 -or (Test-Path $bad)) {
    throw 'unknown/incomplete ABI family did not fail closed'
}
$badReport = Get-Content "$bad.report.json" -Raw | ConvertFrom-Json
if ($badReport.status -ne 'incompatible' -or $badReport.detail -notmatch 'PaintFrame') {
    throw 'failed compatibility report did not name its blocker'
}
Write-Host 'native WT/conhost PE/PDB profile success + fail-closed checks: OK'
