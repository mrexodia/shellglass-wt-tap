# Windows native render-tap components

This directory contains the native-side build/test tooling for
[`docs/windows-render-taps.md`](../../docs/windows-render-taps.md).

## Build

Use an x64 Native Tools prompt (Visual Studio 2022):

```powershell
cmake -S native/windows -B target/native-windows -A x64
cmake --build target/native-windows --config Release
cmake -S native/windows -B target/native-windows-arm64 -A ARM64
cmake --build target/native-windows-arm64 --config Release
cargo build --release
# Intentional real-WT operator launch (never used by automated tests):
powershell -File native/windows/start-wt-stream.ps1 -Bind 127.0.0.1:8080
# Or: ... -Hub https://hub -Key <secret> [-Pdb C:\symbols\Microsoft.Terminal.Control.pdb]
# Push launch replaces any existing detached stream and runs the current Rust
# source through cargo run --locked --release before injection/reuse.
powershell -File native/windows/test-profile.ps1
powershell -File native/windows/test-e2e.ps1
# Real-target tests: injection occurs only inside Windows Sandbox.
# Preferred: aggregate + lifecycle in one persistent Sandbox boot. If a gate
# needs adjustment, refresh/rerun it in that same guest instead of rebooting it.
powershell -File native/windows/test-wt-sandbox.ps1 -IncludeLifecycle -KeepSandboxOpen `
  -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
powershell -File native/windows/test-wt-sandbox-reuse.ps1 `
  -Work target\wt-sandbox-e2e-<host-pid>
# The gates can still be run independently when debugging one stage:
powershell -File native/windows/test-wt-sandbox.ps1 -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
powershell -File native/windows/test-wt-lifecycle-sandbox.ps1 -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
# Optional sustained leak/overload gate (30 seconds is the default):
powershell -File native/windows/test-wt-sandbox.ps1 -StressSeconds 120 `
  -TimeoutSeconds 650 -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
# Both gates accept a pre-extracted second signed package/profile family:
powershell -File native/windows/test-wt-sandbox.ps1 `
  -Version 1.24.11321.0 -Family wt_1_24_11321 `
  -PackagePath C:\packages\Terminal-1.24.11321-x64 -Pdb C:\symbols\11321\Microsoft.Terminal.Control.pdb
powershell -File native/windows/test-conhost-sandbox.ps1 -Pdb C:\symbols\conhost.pdb
```

The end-to-end test starts `shellglass-wt-tap serve`, runs the independently
compiled `shellglass-native-mock.exe`, asserts its C++-encoded frame arrives at
`GET /snapshot`, serves a C++-encoded content-addressed PNG, opens the real
read-only SSH/ANSI viewer and asserts the same native marker, records both text
and image reference, kills/restarts the broker while keeping the adapter alive, and
asserts a reconnect registration plus fresh full frame, verifies serve recording,
then moves the still-running adapter to `push` and restarts its hub to verify the
client re-register/full reconnect path (including the hub image endpoint before
and after restart). This covers:

```text
native process -> versioned named pipe -> security check -> strict decoder
 -> source selection -> model::Grid -> diff::Live -> HTTP/recording/push/hub
```

The exact little-endian payload layouts and limits are frozen in
[`protocol.md`](protocol.md). The mock is a protocol fixture, not an injection
adapter. `test-profile.ps1`
uses separately built WT and conhost symbol fixtures to exercise successful DIA
profile emission, then requests an incompatible ABI family and asserts that it
fails with no artifact.
Both x64 and ARM64 broker/profile/mock tools are compile-gated in CI; the x64
mock runs end-to-end. The production `shellglass-wt-adapter.dll`, exact-family
`shellglass-conhost-adapter.dll`, explicit `shellglass-inject.exe`, and deterministic
payload fixtures build on x64.
Pass `-IncludeLifecycle` to execute both real-target gates sequentially in one
Sandbox boot; child results and logs are preserved separately. Add
`-KeepSandboxOpen` to leave a request-waiting guest alive, then use
`test-wt-sandbox-reuse.ps1 -Work <mapped-work-tree>` to copy updated scripts and
binaries into it and rerun without restarting Hyper-V. This is preferred over
repeated guest restarts because Sandbox/Hyper-V churn can destabilize the
development desktop. WT Sandbox configurations force software rendering: a
vGPU-backed run caused a host `VIDEO_SCHEDULER_INTERNAL_ERROR` (0x119), so these
gates must not opt back into host GPU virtualization. `-IncludeOperator` adds the
first-party launcher gate after aggregate and lifecycle coverage.

`test-wt-sandbox.ps1` installs the exact signed WT package into a disposable
Windows Sandbox, profiles it, injects only the sandbox process, and verifies real
text, resolved styles/underline/link/conceal/blink, search/selection overlays,
grouped sixel image blobs that persist across partial dirty paints, DECSCUSR
cursor, title, resize, rapid away-and-back resize generation coherence,
alternate-screen entry/exit, tab/pane/multi-window and rapid-focus switching, verified high-integrity WT/broker authorization, WT-owned
history through both wheel and the real UI Automation scrollbar while output
continues at the unseen live bottom, continued updates while Notepad owns the
foreground, resize/reflow while still scrolled, broker
restart/re-registration, callback-fault disable/removal through a test-only
adapter build, and separate 120-callback p95 intervals at 80x24, 240x80, and
320x100. CPU/private-memory thresholds are enforced. Its sustained full-screen
stage stalls the broker reader while keeping the pipe connected, requires a
nonzero bounded-drop count followed by full-state reconciliation, and performs a real tab-focus round trip while the
worker is blocked to prove target hooks do not share its IPC lock. It then
hard-restarts the broker and requires a fresh full. The same already-injected WT
is handed to the production detached push worker; the gate verifies prompt
editing, pause/freeze, resume/full, status, stop, and a real hub snapshot.
`-StressSeconds 120` produced 121 seconds of active capture with 1.7 MiB
private-memory growth and 357 deliberately dropped intermediate frames.
`test-wt-lifecycle-sandbox.ps1` is the deterministic pane lifecycle gate: it
closes a repainting split during real callbacks, then hooks both named windows
before a real pane is detached, reattached, and closed; both exact release
profiles pass. **Never substitute the active terminal PID for
these isolated tests:** a private-ABI defect can terminate every agent sharing
that terminal.
`test-conhost-sandbox.ps1` likewise profiles the Sandbox system family and verifies
classic conhost text, resolved colors, cursor, title, resize, alternate-screen
entry/exit, callback-fault disable/removal, responsiveness, and a 120-frame
callback-p95 gate (250 us in the latest recorded run).

## Symbol profiles

`profile_tool.cpp` builds as `shellglass-profile.exe`. It reads one exact PE, asks
DIA to load that PE's RSDS-identified PDB (honoring `_NT_SYMBOL_PATH`, or an explicit
matching PDB argument), checks PDB function type information to distinguish overloads, and for the
both verified `wt_1_24*` release families also verify `Cluster`, `CursorOptions`, `TextAttribute`,
`RenderFrameInfo`, `IRenderData`, and `IRenderEngine` sizes/member/vtable offsets,
and writes an `.sgnp`
profile containing:

- architecture and image size;
- SHA-256 of the target module;
- PDB GUID and age;
- ABI family;
- exact executable-section RVAs and 16 expected prologue bytes; and
- a SHA-256 integrity trailer.

Every attempt also writes `<output>.report.json`, including failed attempts. The
report names the module, ABI family, compatibility status, and exact missing or
ambiguous requirement instead of silently guessing after an upgrade.

Example:

```powershell
$env:_NT_SYMBOL_PATH='srv*C:\symbols*https://msdl.microsoft.com/download/symbols'
target\native-windows\Release\shellglass-profile.exe `
  'C:\path\Microsoft.Terminal.Control.dll' wt_1_24 wt_1_24.sgnp `
  'C:\symbols\matching-Microsoft.Terminal.Control.pdb'
```

Generation deliberately fails if any required exact function or ABI fact is absent
or ambiguous. There is no signature-scan fallback. Place the generated profile beside
`shellglass-wt-adapter.dll` with the same stem (`shellglass-wt-adapter.sgnp`). The
`start-wt-stream.ps1` is the first-party operator path for the verified personal
x64 deployment: it automatically configures/builds stale or missing native
artifacts when no running WT process has the adapter loaded, fail-closed profiles
the installed package, starts local serve or detached push, and injects every
visible WT process. A normal uninjected WT does not block that first build. If a
stale process-lifetime adapter is already loaded, the launcher reports its PID and
requires that process to exit before CMake can relink it safely.
Existing controls recover lazily on their first post-injection focus gain/loss
or authoritative `OwningHwnd` assignment using exact PDB-verified `_renderer`,
`_pData`, and `_owningHwnd` offsets; they are never scanned or guessed. The owner
setter path covers a default-terminal handoff whose first pane predates injection
but receives its real window only afterward.
Pass `-NewTab` only as a fallback to force a transition immediately. The actual
script is exercised by `test-start-wt-stream-sandbox-guest.ps1`; the latest
persistent-Sandbox result reached the browser snapshot end-to-end. The
lower-level adapter is also suitable for process-startup injection by an
operator-controlled facility. `shellglass-inject.exe` remains the explicit
low-level integration tool; automated gates use it only inside Windows Sandbox.

## Current compatibility

The x64 adapter is production-profiled and real-target verified against stock
Windows Terminal `1.24.11911.0` (`wt_1_24`) and `1.24.11321.0`
(`wt_1_24_11321`). Their matching Microsoft PDBs expose the exact
ControlCore lifecycle/focus/owner hooks and renderer attachment/redraw methods. The
adapter validates the module hash, RSDS identity, profile integrity, executable
prologues, and family-pinned type/vtable contract before patching. Render callbacks
copy into fixed-capacity batches only. Because WT paints dirty-region deltas, an
occupied queue is never replaced by a newer partial update; after a drop, the worker
rejects the incomplete queued sequence and schedules a full repaint. All pipe I/O, UTF conversion, model assembly, and encoding remain on its worker. Dormant,
disconnected, and closed sources release their large batch/model buffers on the
worker after callback quiescence. Unknown hashes and changed type layouts install
no hooks.

The x64 `conhost_10_0_19045` family is also production-profiled and real-target
verified for classic conhost on system image `10.0.19041.7548`. It attaches a
bounded old-generation `IRenderEngine` through the profile-pinned renderer and is
explicitly disabled for `--headless`: that generation's ConPTY passthrough path does
not submit application text to renderer fan-out, so publishing its blank repaint
would violate fidelity. The newer package OpenConsole path cannot supply a modern
replacement family either: its matching source constructs `Renderer` only when
`!IsInVtIoMode()`, and headless ConPTY output instead goes directly through
`VtIo::Writer`. Supporting it would require a separately designed outgoing-VT
parser backend rather than a render tap.

The remaining compatibility gaps are explicit:

- no ARM64 WT or conhost hook/trampoline family yet (the broker, injector, mock,
  profile tool, and fixtures already compile for ARM64);
- no production headless-ConPTY family; the tested generation does not drive its
  renderer fan-out for application text and therefore remains fail closed;
- WT fidelity now includes grouped image slices served as content-addressed PNG
  blobs, OSC 8 hyperlink URIs, underline colors/styles, conceal/blink, search
  highlights, and selection-background overlays. Unit coverage proves arbitrary
  grapheme preservation; the classic-conhost Sandbox fixture proves Han,
  combining-mark, emoji, and wide-cell transport with direct `WriteConsoleW`, and
  the stock-WT fixture independently proves Han/wide-cell transport from raw UTF-8.

The mock remains protocol evidence rather than real-target evidence; the isolated
stock-WT sandbox gate is the real x64 WT proof. No signature-scan fallback exists.
