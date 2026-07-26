# shellglass-wt-tap

Hybrid foreground-window capture provider for the sibling
[`shellglass`](../shellglass/) library. Supported terminal windows use the
private-ABI Windows Terminal/conhost render taps; every other foreground window
is reconstructed from its [`xa11y`](../xa11y/) accessibility tree. All capture,
source-selection, and detached-control behavior lives here; shellglass receives
only ordinary `Frame`s through `SourceSession`.

Supported x64 Windows Terminal families currently include exact releases
`1.24.11911.0` and `1.24.11321.0`. Unknown hashes, RSDS identities, prologues, or
PDB-verified layouts fail closed. Existing controls are recovered lazily on their
first post-injection focus transition or authoritative `OwningHwnd` assignment
through exact `_renderer`, `_pData`, and `_owningHwnd` offsets—never heap or
signature scanning.

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

# Hub push: stops any prior detached stream, then builds/starts the current
# source with `cargo run --locked --release`.
.\native\windows\start-wt-stream.ps1 `
  -Hub https://hub -Key <secret> `
  -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
```

Existing tabs recover when they next gain or lose focus; pass `-NewTab` only to
force an immediate transition. Native terminal frames always take precedence.
When the foreground HWND has no matching native source, a bounded xa11y worker
projects element bounds into an aspect-preserving spatial terminal canvas and
reconstructs controls from roles, names, values, focus, selection, and state.
Small dialogs retain approximately CSS-pixel scale instead of stretching across
the full stream, while oversized windows shrink to fit. This is semantic-only:
live capture never takes or consults screenshots. If usable bounds are absent,
the renderer falls back to its diagnostic tree view. A capture that races a
terminal focus change is rejected under the
broker lock, so it cannot overwrite the native frame. Transient accessibility
failures—such as taskbar focus, popup quickbars, or providers switching roots—are
logged but leave the last coherent frame untouched. `--foreground-only`
disables accessibility reconstruction and stops outside known terminals.

Accessibility data can contain document text, messages, and form values. Keep the
default loopback bind or put authenticated transport in front of a public bind.

### Accessibility privacy blacklist

Discord, Discord Canary, and Discord PTB are denied by default. Keep persistent
additions in a TOML file:

```toml
# privacy.toml
[privacy]
deny_apps = ["Slack", "Signal.exe"]
```

A `privacy.toml` in the process working directory is loaded automatically, so the
normal commands need no additional option:

```powershell
cargo run -- serve
.\native\windows\start-wt-stream.ps1
```

Use `--a11y-config PATH` (or launcher parameter `-A11yConfig PATH`) to select a
different file explicitly. An explicit missing or invalid file is an error; an
absent default `./privacy.toml` is normal.

Names are exact and case-insensitive; `.exe` is optional. Repeatable
`--a11y-deny-app` options add temporary entries, and
`SHELLGLASS_A11Y_CONFIG` can provide the config path for service launchers. The
worker checks the accessibility application name and foreground process
executable before traversing its window. While a denied application is active,
a previously selected native terminal remains subscribed and continues streaming;
otherwise Shellglass leaves the previous frame untouched. The denied app's name,
title, tree, and focus transition never enter the stream. If process
identity cannot be established, capture fails closed instead of streaming the
unidentified window.

The adapter remains loaded for the lifetime of `WindowsTerminal.exe`; rebuilding
or retesting a new DLL requires fully exiting that process. See
[`native/windows/README.md`](native/windows/README.md) and
[`docs/windows-render-taps.md`](docs/windows-render-taps.md).

## Local accessibility preview

The standalone preview renders the same reconstructed `Frame` into the invoking
terminal's alternate screen, without starting shellglass HTTP/push transport:

```powershell
cargo run --no-default-features --features accessibility -- preview
```

This build contains the terminal preview without shellglass serve/push transport.
Press Ctrl-C to restore the terminal. Initially the preview may reconstruct its
own terminal because launching it gives that window focus; switch to the GUI
window you want to inspect. The preview detects terminal resizes and regenerates
the canvas at the full available size. Capture controls remain available:

```powershell
cargo run --no-default-features --features accessibility -- preview `
  --a11y-depth 16 --a11y-max-nodes 2000
```

The default snapshot interval is 300 ms. Streaming defaults to a 200×60 grid,
large enough to make useful use of a typical 1080p viewer. `--a11y-cols` and
`--a11y-rows` override the bounded grid for `serve`, `push`, and `stream start`;
preview dimensions come from its terminal. `--a11y-interval-ms`, `--a11y-depth`, and
`--a11y-max-nodes` apply to both.

### Developing the spatial renderer

Screenshots are development oracles, not live renderer inputs. Capture a test
application's window screenshot and xa11y tree at the same size, keep them as a
paired fixture, and compare the generated terminal frame with the screenshot
side by side. Runtime streaming remains accessibility-only.

Capture an exact pair with the development command, then focus the target during
the delay:

```powershell
cargo run -- capture-layout-fixture target\layout-fixtures\my-case --delay-ms 3000
```

It writes `tree.json`, `reference.png`, and the current `render.txt`, but live
streaming never invokes screenshot capture. Committed x64dbg CPU and Total
Commander fixtures under `tests/fixtures/accessibility/` now exercise dense
register/table/list rows, menu packing, collision precedence, and truncation.
See that directory's README for the refresh workflow.

The xa11y AccessKit, Qt, GTK, WinForms, WPF, and Cocoa test applications provide
repeatable native control layouts. The curated reports under
`../xa11y-table/captures/` additionally provide cross-platform table-cell bounds
and roles. Renderer tests assert relative placement and table row/column geometry
from those semantics; screenshots remain human oracles for shortcomings that are
intentionally too subjective for pixel-exact assertions.
