@echo off
powershell.exe -ExecutionPolicy Bypass -File ".\native\windows\start-wt-stream.ps1" -Hub "https://sl0p.foo/hub" -Key "%SHELLGLASS_KEY%"
echo Open a new terminal to not lose the hooks
pause