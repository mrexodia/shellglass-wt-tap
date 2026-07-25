# shellglass-wt-tap

Private-ABI Windows Terminal capture provider for the sibling
[`shellglass`](../shellglass/) library. All injector, DIA/PDB, named-pipe, source-selection,
and detached-control behavior lives here; shellglass receives only ordinary
`Frame`s through `SourceSession`.

Supported x64 Windows Terminal families currently include exact releases
`1.24.11911.0` and `1.24.11321.0`. Unknown hashes, RSDS identities, prologues, or
PDB-verified layouts fail closed. Existing controls are recovered lazily on their
first post-injection focus transition through exact `_renderer`, `_pData`, and
`_owningHwnd` offsets—never heap or signature scanning.

## Build

```powershell
cargo build
cmake -S native\windows -B target\native-windows -A x64
cmake --build target\native-windows --config Release
```

## Run

```powershell
# Local viewer
.\native\windows\start-wt-stream.ps1 `
  -Bind 127.0.0.1:8080 `
  -Pdb C:\symbols\Microsoft.Terminal.Control.pdb

# Hub push
.\native\windows\start-wt-stream.ps1 `
  -Hub https://hub -Key <secret> `
  -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
```

Existing tabs recover when they next gain or lose focus; pass `-NewTab` only to
force an immediate transition. Switching to a non-terminal application keeps the
last active terminal live; direct `serve`, `push`, and `stream start` commands can
opt into strict foreground-only capture with `--foreground-only`. The adapter remains loaded for the lifetime of
`WindowsTerminal.exe`; rebuilding or retesting a new DLL requires fully exiting
that process. See
[`native/windows/README.md`](native/windows/README.md) and
[`docs/windows-render-taps.md`](docs/windows-render-taps.md).
