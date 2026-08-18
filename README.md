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

The adapter remains loaded for the lifetime of `WindowsTerminal.exe`. The launcher
automatically configures/builds stale or missing native artifacts while running WT
processes have no adapter loaded. If an adapter update is needed after injection,
it reports the affected PIDs and asks the operator to exit those processes. See
[`native/windows/README.md`](native/windows/README.md) and
[`docs/windows-render-taps.md`](docs/windows-render-taps.md).

## Local accessibility preview

The standalone preview renders the same reconstructed `Frame` into the invoking
terminal's alternate screen, without starting shellglass HTTP/push transport:

```powershell
cargo run --no-default-features --features accessibility -- preview
```

This build contains the terminal preview without shellglass serve/push transport.

Build only the standalone accessibility viewer, without the shellglass serve or
push transports:

```console
cargo build --release --no-default-features --features accessibility
```

Run the resulting native executable on macOS or Linux:

```console
./target/release/shellglass-wt-tap preview
```

On Windows:

```powershell
target\release\shellglass-wt-tap.exe preview
```

Build separately on each target operating system; these executables are not
portable across platforms. macOS must grant Accessibility permission to the
executable or launching terminal. Linux requires its desktop accessibility
service to be available.
Press Ctrl-C to restore the terminal. Initially the preview may reconstruct its
own terminal because launching it gives that window focus; switch to the GUI
window you want to inspect. The preview detects terminal resizes and regenerates
the canvas at the full available size.

To keep previewing a window while another application has focus, first list the
top-level accessibility windows:

```console
./target/release/shellglass-wt-tap preview --list-windows
```

The tab-separated output includes PID, active state, application name, and window
title. Filters can narrow both the listing and the live preview. For example,
find IDA candidates and then select one unique window:

```console
./target/release/shellglass-wt-tap preview --list-windows --app-name-prefix IDA
./target/release/shellglass-wt-tap preview --pid 12345 --window-title-prefix "mydb.i64"
```

`--pid`, `--app-name-prefix`, and `--window-title-prefix` are case-sensitive and
may be combined. A live selector must match exactly one top-level window; the
preview refuses to guess when several match. Without a selector, preview continues
to follow the foreground window. On Windows, use the `.exe` path shown above.
Window listing and selection honor the same accessibility privacy policy as live
capture, so denied applications are not identified or listed.

Capture controls remain available:

```powershell
cargo run --no-default-features --features accessibility -- preview `
  --a11y-depth 16 --a11y-max-nodes 2000
```

The default snapshot interval is 300 ms. Streaming defaults to a 200×60 grid,
large enough to make useful use of a typical 1080p viewer. `--a11y-cols` and
`--a11y-rows` override the bounded grid for `serve`, `push`, and `stream start`;
preview dimensions come from its terminal. `--a11y-interval-ms`, `--a11y-depth`, and
`--a11y-max-nodes` apply to both.

### Zed's experimental accessibility tree

As of Zed 1.12.0, its GPUI AccessKit adapter is compiled in but disabled by
default. Zed constructs an explicitly inaccessible application unless
`ZED_EXPERIMENTAL_A11Y=1` is present in **Zed's** environment at process startup.
A UIA query or event subscription cannot activate an already-running inaccessible
instance. Fully exit every Zed process, then launch it from a PowerShell session
with the switch enabled:

```powershell
$env:ZED_EXPERIMENTAL_A11Y = "1"
zed
```

For future processes, the variable can instead be stored in the user environment;
Zed still needs to be restarted:

```powershell
[Environment]::SetEnvironmentVariable("ZED_EXPERIMENTAL_A11Y", "1", "User")
```

This is an upstream experimental switch and may change. In Zed 1.12.0 it enables
UIA activation and exposes title/status controls plus named `Left dock` and
`Editor` panes, but the project entries and editor text are not yet included in
the tree. Shellglass marks those large panes as
`⟦ content not exposed ⟧` rather than reconstructing pixels absent from the
provider. Setting the variable on Shellglass itself has no effect on Zed.

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
streaming never invokes screenshot capture. Committed x64dbg CPU, Total Commander, and Zed fixtures under
`tests/fixtures/accessibility/` exercise dense register/table/list rows, menu
packing, collision precedence, truncation, and explicitly unavailable panes.
See that directory's README for the refresh workflow.

The xa11y AccessKit, Qt, GTK, WinForms, WPF, and Cocoa test applications provide
repeatable native control layouts. The curated reports under
`../xa11y-table/captures/` additionally provide cross-platform table-cell bounds
and roles. Renderer tests assert relative placement and table row/column geometry
from those semantics; screenshots remain human oracles for shortcomings that are
intentionally too subjective for pixel-exact assertions.
