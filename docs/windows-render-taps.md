# Windows terminal render taps

Status: complete for the accepted personal x64 Windows Terminal deployment. Phases 1–3 and the WT portions of phases 5–7 are implemented and real-target verified for two exact x64 releases; `start-wt-stream.ps1` is the operator launch path. Classic-conhost Phase 4 is also implemented for one family as extra coverage. The user explicitly excluded headless ConPTY and ARM64 runtime hooks; both remain fail closed, while ARM64 tooling stays compile-gated.

Implementation evidence:

- The parent shellglass library's `src/source.rs` provides `SourceSession`,
  `FramePublisher`, and no-op external status handling; this project consumes
  that generic boundary without compiling shellglass's PTY/parser backend.
- Phase 2: `src/native_protocol.rs`, `src/native_broker.rs`, and
  `src/windows_native.rs` implement the bounded binary decoder, generations,
  source registry/precedence, image store, WinEvent foreground selection, secure
  named pipe, handshake timeouts, and newest-frame publication. The data pipe
  permits sandboxed same-user conhosts while the control pipe requires exact
  integrity. `native/windows/mock_adapter.cpp` plus `test-e2e.ps1` verify native
  process → pipe → `Grid` → HTTP snapshot/content-addressed image/read-only
  SSH/recording, adapter recovery across a broker restart, and native push plus
  image recovery across a hub restart.
- Detached control plane: `stream start|pause|resume|stop|status` is implemented;
  pause unsubscribes and freezes, resume selects and forces a fresh full.
- Operator launch: `native/windows/start-wt-stream.ps1` selects only a verified
  installed x64 family, generates the exact fail-closed profile, starts local
  serve or detached push, and injects running WT processes. Push reruns stop the
  prior detached worker first, then execute the current Rust source through
  `cargo run --locked --release` so a locked stale binary is never reused. Existing tabs attach
  lazily on their first post-injection focus gain/loss or authoritative owner-HWND
  assignment through exact PDB-verified member offsets; this includes the first
  pane of a default-terminal handoff. `-NewTab` is an explicit fallback, never scanning.
- Compatibility tooling: `native/windows/profile_tool.cpp` verifies PE/PDB
  identity through DIA, type-qualified symbols, executable RVAs/prologues, and
  the family-pinned renderer type/member/vtable ABI, then emits an integrity-tagged
  profile plus compatibility JSON report. It fails closed on missing/ambiguous
  facts and deletes stale output. x64 and ARM64 native tools are compile-gated.
- Phase 3: `native/windows/wt_adapter.cpp` implements exact x64 profiles for
  stock WT 1.24.11911.0 (`wt_1_24`) and 1.24.11321.0
  (`wt_1_24_11321`). Both pass the same isolated real-target gate. Its render
  engine is dormant until subscribed,
  uses fixed-capacity lock-free batches that preserve ordered dirty-region deltas
  and reconcile overload drops with a full repaint, reclaims
  the large fixed batches whenever a source becomes dormant, and does all
  IPC/model/encoding work on a worker. The real `test-wt-sandbox.ps1` gate
  verifies Unicode/wide-cell text, resolved styles, DECSCUSR cursor shape, title,
  resize, rapid away-and-back resize coherence (with a viewport-generation
  layout-generation seqlock rejecting queued old-layout batches even when
  dimensions match again, without invalidating ordinary scroll/image batches),
  alternate-screen entry/exit, WT-owned scrolled viewport while output continues
  at the unseen live bottom, wheel and actual UI Automation scrollbar navigation,
  resize/reflow while still scrolled, tabs, search and selection overlays,
  hyperlinks, conceal/blink, and dirty-region-persistent grouped multi-row sixel
  slices (including served PNG bytes) end-to-end inside a disposable VM. It
  separately gates 120-callback intervals at 80x24, 240x80, and 320x100 with a
  1 ms p95 ceiling (250 us in the latest recorded runs), target CPU/private-memory
  limits, and a sustained full-screen run. Stalling the broker reader proves
  nonzero bounded drops followed by full-state reconciliation; a real tab-focus
  round trip during the stall proves focus/lifecycle hooks do not wait behind worker IPC. A hard broker restart then
  proves dormant disconnect behavior and a fresh full registration. A 121-second
  run recorded 357 dropped intermediate frames and only 1.7 MiB private-memory
  growth. The latest expanded gate also switches the already-injected WT process
  from foreground serve to the production detached worker, proves local prompt
  editing while paused, hub-side freeze, resume with a fresh full frame, status,
  and clean stop.
- The accepted WT portions of phases 5–7 are complete. As extra coverage,
  `native/windows/conhost_adapter.cpp` implements
  the exact x64 `conhost_10_0_19045` classic-console family; its isolated Sandbox
  gate verifies Unicode grapheme/wide-cell text, resolved colors, cursor, title,
  resize, alternate-screen entry/exit, responsiveness, and
  a 120-frame callback p95 at 1 ms (250 us in the latest recorded run).
  The adapter rejects `--headless` because this system generation's ConPTY
  passthrough path does not submit application text to renderer fan-out. The
  newer package `OpenConsole.exe` path on this machine has the same fundamental
  ceiling: its corresponding `ConsoleAllocateConsole` source creates `Renderer`
  only when `!gci.IsInVtIoMode()`, while ConPTY output is handled by
  `VtIo::Writer`. There is therefore no renderer object to tap in headless mode.
  Capturing that path would require the separately scoped outgoing-VT/parser
  backend mentioned in section 8.3, not another verified render-tap ABI family. WT now emits grouped image-slice PNG
  blobs, hyperlink/underline fidelity, conceal/blink, search highlights, and
  selection backgrounds; the aggregate sandbox exercises tab/multi-window source
  focus and rapid transitions. The dedicated `test-wt-lifecycle-sandbox.ps1`
  gate first closes a split pane during sustained full-screen callbacks and
  verifies split-pane focus/close, then deterministically creates two
  already-hooked named windows, detaches a pane to the destination, reattaches it
  to the main window, and closes it; both exact WT releases pass. The gates also verify
  high-integrity WT/broker authorization, real broker restart/re-registration,
  callback-fault disable/removal through a test-only build, and explicit CPU,
  memory, callback-p95, and overload-drop bounds. The tested headless ConPTY
  generation still does not drive conhost's exposed renderer fan-out. ARM64
  tooling compiles, but runtime hooks are not part of this x64 deployment. No
  signature-scan fallback has been added.

This document describes a Windows capture backend for shellglass that observes already-running terminal sessions without launching a command in a shellglass-owned PTY. It has two native providers:

1. a **Windows Terminal render tap**, injected into the stock Windows Terminal process; and
2. a **conhost render tap**, injected into each console host process, including headless ConPTY hosts.

Both providers feed shellglass's existing structured `Frame::Screen(Grid)` pipeline. A graphics/window-capture backend is intentionally out of scope.

The design assumes an external facility can inject a shellglass capture DLL at process startup. It does not require patching or replacing Windows Terminal or conhost on disk.

## 1. Goals

The system should:

- let the user begin and end streaming without starting a replacement shell or wrapping a command;
- leave input, output, terminal scrollback, resizing, tabs, panes, and normal shell lifetime under the original terminal application's control;
- discover all eligible terminal sessions from process startup, while doing negligible work until one is selected;
- follow the active terminal window and focused pane automatically;
- publish the currently visible Windows Terminal viewport, including a viewport that the user has scrolled into history;
- provide a generic fallback for ConPTY and classic console sessions where no terminal-host-specific tap exists;
- preserve shellglass's cell model, browser copy/search behavior, diff protocol, hub, recording, and SSH viewer;
- perform no network I/O and no blocking IPC on a target process's render thread;
- fail closed on an unknown binary or incompatible internal ABI instead of risking the target process.

## 2. Non-goals

This design does not attempt to:

- capture arbitrary GUI windows or desktop pixels;
- remotely control or inject input into a terminal;
- expose a terminal's complete scrollback history as a shellglass history API;
- make conhost know a downstream terminal application's private scroll position;
- promise one injected binary will remain ABI-compatible with every future Windows Terminal or Windows release;
- replace the existing PTY source on Unix or remove the current explicit-command mode;
- mirror post-processing that exists only as pixels, such as a custom pixel shader.

## 3. The two terminal states

For a Windows Terminal pane backed by ConPTY, there are two distinct terminal models:

```text
command-line application
        |
        | Win32 console calls and/or VT output
        v
headless conhost / ConPTY
        |  authoritative ConPTY live screen
        |  outgoing VT stream
        v
Windows Terminal Terminal/TextBuffer
        |  terminal-owned history and viewport
        v
Windows Terminal renderer
        |  focused/unfocused appearance, selection, cursor, images
        v
visible pane
```

This distinction determines what each tap can promise.

### 3.1 Windows Terminal tap

The Windows Terminal tap is downstream of ConPTY and observes the state Windows Terminal is actually rendering. It can mirror:

- the focused pane;
- the selected tab;
- Windows Terminal's current viewport into its own scrollback;
- terminal-side reflow;
- resolved profile appearance;
- terminal selection and search overlays;
- cursor presentation; and
- image slices known to the renderer.

This is the preferred provider for Windows Terminal.

### 3.2 conhost tap

The conhost tap observes conhost's own live screen. A headless ConPTY conhost knows the negotiated row/column count and its current console buffer, but it does not know which rows a downstream terminal is showing from that terminal's private history.

Consequently, a conhost-backed stream can remain live while the local user scrolls backward in Windows Terminal, but remote viewers continue seeing the live ConPTY screen. Local scrolling is not impaired; it simply is not mirrored.

For a classic conhost window, conhost is itself the terminal host and its viewport is the displayed viewport, so this limitation does not apply in the same way.

## 4. Provider capability matrix

This is the intended provider contract. In the currently supported
`conhost_10_0_19045` family the entire headless-conhost column is unavailable and
fails closed, because application text bypasses renderer fan-out; the classic column
is the implemented conhost surface.

| Capability | Windows Terminal tap | headless conhost tap | classic conhost tap |
|---|---:|---:|---:|
| Discovers an existing session from startup | yes | yes | yes |
| Knows logical columns and rows | yes | yes | yes |
| Leaves local input and scrollback unchanged | yes | yes | yes |
| Mirrors a user-scrolled viewport | yes | no | yes |
| Knows active WT tab and split pane | yes | indirect only | n/a |
| Knows owning top-level HWND | yes | normally yes, via reparent signal | yes |
| Sees final WT profile colors/font | yes | no | n/a |
| Covers terminals other than WT that use ConPTY | no | yes | n/a |
| Covers legacy Win32 console writes | through WT's model | yes | yes |
| Mirrors post-pixel-shader output | no | no | no |
| Requires target-version ABI support | yes | yes | yes |

The broker prefers the Windows Terminal provider whenever it covers a foreground pane. The conhost provider is a generic fallback, not a second simultaneous view of the same pane.

## 5. System architecture

```text
+----------------------------- target processes -----------------------------+
|                                                                            |
|  WindowsTerminal.exe                 conhost.exe                            |
|  +----------------------+            +----------------------+               |
|  | injected WT adapter  |            | injected conhost    |               |
|  | + capture engines    |            | adapter + engine    |               |
|  +----------+-----------+            +----------+-----------+               |
|             | newest-frame queue                 |                           |
|             +--------------------+---------------+                           |
+----------------------------------|-------------------------------------------+
                                   | local versioned named-pipe IPC
                                   v
                         +---------------------+
                         | shellglass broker   |
                         | - source registry   |
                         | - foreground policy |
                         | - active selection  |
                         | - frame validation  |
                         +----------+----------+
                                    | watch::Receiver<Arc<Frame>>
                                    v
             existing diff::Live / push WebSocket / recording / SSH / SSE
```

There are three process roles:

- **native adapters** run in terminal processes and translate private renderer ABIs into a small shellglass capture protocol;
- the **broker** runs out of process, chooses one active source, validates untrusted IPC, and converts native frames into `model::Grid`; and
- the existing **serve or push pipeline** publishes those frames.

The injected modules never connect to a hub and never contain session keys.

## 6. Common render-tap model

Both native providers should use the target's renderer contract rather than intercepting `WriteConsole`, `ReadFile`, or every VT write. The Windows console code is already designed to fan one render frame out to multiple `IRenderEngine` implementations.

Relevant source interfaces in the checked-out terminal repository include:

- `src/renderer/inc/IRenderEngine.hpp`;
- `src/renderer/inc/IRenderData.hpp`;
- `src/renderer/base/renderer.cpp`;
- `src/cascadia/TerminalControl/ControlCore.cpp`; and
- `src/host/renderData.cpp`.

The analyzed `conhost.exe` also contains `Renderer::AddRenderEngine`, `Renderer::_PaintFrameForEngine`, `RenderData::GetViewport`, `RenderData::GetTextBuffer`, and the expected paint methods. Exact interfaces differ by Windows generation, so these names describe a logical contract, not one permanent binary ABI.

### 6.1 Dormant attachment

An engine is attached when a terminal control or conhost renderer is initialized, but starts dormant. While dormant it may record a dirty bit and current dimensions, but `StartPaint` returns `S_FALSE` and it does not build frames.

When the broker subscribes:

1. set `enabled = true` atomically;
2. acquire the target terminal's own render-data/console lock;
3. call the target renderer's full-redraw operation;
4. release the lock; and
5. publish the first completed full frame.

This provides an on-demand snapshot without continuously parsing every inactive session.

### 6.2 Full viewport first

The initial implementation should repaint and rebuild the full viewport whenever the capture engine is dirty. It should not initially reproduce the target renderer's scroll-copy and dirty-rectangle algorithms.

Full viewport rebuilding has useful properties:

- subscription always starts from complete state;
- resize and source switching naturally produce complete state;
- missed invalidations cannot leave stale cells;
- target-version differences in scroll optimization do not affect correctness; and
- shellglass already computes compact network deltas from complete `Frame`s.

A normal terminal viewport is small enough for this to be practical. Dirty-region reconstruction can be introduced later only if measurement proves it necessary.

### 6.3 Frame lifecycle

A capture engine uses this state machine:

```text
Dormant
  | subscribe + full redraw
  v
EnabledDirty
  | StartPaint
  v
Building
  | EndPaint: freeze local snapshot
  v
Ready
  | Present: nonblocking enqueue
  +---------------------------> EnabledClean

any invalidation while enabled -> EnabledDirty
unsubscribe                  -> Dormant
renderer teardown            -> Dead
```

`EndPaint` is normally called while the target's terminal lock is held. It must finish copying all borrowed target data. `Present` is normally called after that lock is released and should only perform a nonblocking swap/enqueue.

### 6.4 Newest-frame backpressure

Each engine owns a capacity-one completed-frame slot. Producing frame N+1 replaces unconsumed frame N. A dedicated adapter worker drains this slot, serializes it, and writes to the broker.

The render thread must never:

- wait for named-pipe availability;
- wait for network state;
- compress images;
- resolve symbols;
- allocate without a hard bound; or
- block behind an older frame.

This matches shellglass's existing newest-screen behavior: intermediate visual states may be coalesced, but the most recent complete state is retained.

## 7. Windows Terminal render tap

### 7.1 Target module

The primary target is `Microsoft.Terminal.Control.dll` loaded by the stock `WindowsTerminal.exe`. Renderer-base code is linked into this module in current Windows Terminal builds.

The adapter waits for module load using `LdrRegisterDllNotification` or an equivalent loader notification installed after injection. It must not assume the control module is loaded when the injection entry point runs.

### 7.2 Required symbols

The adapter profile should resolve exact decorated overloads for at least:

- `implementation::ControlCore::Initialize(float,float,float)`;
- `implementation::ControlCore::~ControlCore()`;
- `implementation::ControlCore::GotFocus()`;
- `implementation::ControlCore::LostFocus()`;
- `implementation::ControlCore::OwningHwnd(uint64_t)`;
- `implementation::ControlCore::GetRenderer()` or `GetRenderData()`;
- `Renderer::GetRenderData()`;
- `Renderer::AddRenderEngine(IRenderEngine*)`;
- `Renderer::RemoveRenderEngine(IRenderEngine*)`; and
- `Renderer::TriggerRedrawAll(...)`.

Optional fallback/diagnostic hooks include:

- `ControlCore::_focusChanged(bool)`;
- `ControlCore::WindowVisibilityChanged(bool)`; and
- `TerminalPage::_activePaneChanged(...)` in `TerminalApp.dll`.

`GotFocus` should be tested first. Hooking `TerminalPage` adds another module and more WinRT ABI surface and is unnecessary if control focus transitions cover tab and pane changes reliably.

### 7.3 Attaching to a control

The `ControlCore::Initialize` detour calls the original first. On successful initialization it creates a registry entry:

```text
WtSource {
    source_id,
    process_id,
    core_pointer,
    renderer_pointer,
    render_data_pointer,
    owning_hwnd,
    capture_engine,
    initialized,
    focused,
    visible,
    subscribed,
    generation
}
```

It then adds the dormant capture engine under the terminal lock. The conceptual sequence is:

```cpp
auto ok = original_initialize(core, width, height, scale);
if (ok && !registry.contains(core)) {
    auto* renderer = resolved_get_renderer(core);
    auto* data = resolved_renderer_get_render_data(renderer);
    auto* engine = create_engine_for_this_abi(...);

    data->LockConsole();
    resolved_add_render_engine(renderer, engine);
    data->UnlockConsole();

    registry.add(core, renderer, data, engine);
}
return ok;
```

Production code must use scope guards and must not let exceptions cross the detour boundary.

### 7.4 Active pane and window

Hooking `ControlCore::OwningHwnd(uint64_t)` establishes the `ControlCore* -> HWND` mapping. Hooking `GotFocus` marks that core as the most recently focused source for the HWND. `LostFocus` records loss of focus but does not immediately discard the mapping, because bringing a browser to the foreground should not forget which terminal pane was last active.

The broker separately tracks the foreground top-level window using an out-of-context `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ...)`.

The selected WT source is:

```text
foreground HWND
    -> registered WT process/window
    -> most recently focused live ControlCore for that HWND
```

When a different tab or split pane gets focus, the broker switches subscriptions and the new engine performs a full redraw.

### 7.5 Scrolling semantics

Windows Terminal's `Terminal`/`TextBuffer` and renderer own the host-side viewport. A mouse-wheel or scrollbar movement invalidates the renderer and causes the capture engine to repaint the historical rows currently visible. This is why the WT provider satisfies the strict “show what I am looking at” behavior.

Output that arrives while the user is scrolled backward follows Windows Terminal's own viewport policy. The tap mirrors the resulting rendered viewport rather than imposing a shellglass policy.

### 7.6 Appearance semantics

The engine receives the styling selected by Windows Terminal's render path. The normalized frame should prefer resolved RGB colors rather than palette indices, because the browser does not have Windows Terminal's profile palette.

The adapter should capture, where exposed by the target ABI:

- resolved foreground and background;
- bold/intense and faint;
- italic;
- blink;
- conceal;
- reverse video;
- underline style and color;
- strikethrough;
- hyperlink identifiers and URIs;
- cursor position, visibility, type, and color;
- selection/search overlays;
- title;
- line rendition; and
- image slices.

A custom pixel shader is not represented in this semantic contract and is not mirrored.

### 7.7 Teardown

The `ControlCore` destructor detour marks the source dead before calling the original destructor. Under the terminal lock it removes the engine if the renderer is still usable.

The engine must not be freed immediately after removal. A `Present` call for an already-started frame can occur outside the terminal lock. Safe options are:

- process-lifetime engine allocation, acceptable because closed tabs are bounded in ordinary use; or
- epoch/deferred reclamation after the worker and render callbacks have both acknowledged the source generation is quiescent.

The first implementation should use process-lifetime allocation and eliminate lifetime risk. The registry entry can become a tombstone and release large frame buffers immediately.

## 8. conhost render tap

### 8.1 Coverage

Inject into every `conhost.exe` selected by the injector policy, including headless instances started with ConPTY arguments. Each headless ConPTY normally has its own conhost process, naturally providing one source per console session.

This provider covers:

- classic console applications using Win32 console APIs;
- VT applications hosted through ConPTY;
- Windows Terminal panes backed by ConPTY;
- other terminal emulators that use the Windows pseudoconsole API; and
- console sessions for which no richer terminal-host adapter exists.

It does not cover a terminal connection implemented entirely in a terminal process without conhost/ConPTY.

### 8.2 Dimensions and owner identity

Conhost does know the logical pseudoconsole dimensions.

Initial dimensions are supplied on the headless conhost command line. Later resizes follow this path:

```text
terminal host
  -> ResizePseudoConsole / ConptyResizePseudoConsole
  -> PTY signal pipe: ResizeWindow
  -> PtySignalInputThread::_DoResizeWindow
  -> conhost SCREEN_INFORMATION resize
```

The target's `RenderData::GetViewport()` returns the viewport conhost intends to render.

The terminal host can also send a parent HWND:

```text
ConptyReparentPseudoConsole
  -> PTY signal pipe: SetParent
  -> PtySignalInputThread::_DoSetWindowParent
```

The adapter hooks the signal handlers or the resulting state mutation to report:

- current columns and rows;
- owner HWND;
- show/hide state; and
- resize generation.

For classic conhost, the real console HWND is used directly.

### 8.3 Renderer attachment

Conhost versions differ in how the VT output engine is connected. In the analyzed shipped binary, the renderer contains `Renderer::AddRenderEngine`, a VT render engine, GDI-related engines, and the same general fan-out architecture. In newer source generations some ConPTY output mechanics have moved through `VtIo::Writer` and terminal-output connection abstractions.

Therefore, conhost support is implemented as an ABI-family adapter, not one fixed list of RVAs.

For a family with renderer fan-out, the preferred attachment sequence is:

1. detour `Renderer::AddRenderEngine` or renderer initialization;
2. observe the first native engine being attached;
3. attach one dormant shellglass engine using the original `AddRenderEngine`, guarded against recursion;
4. record `Renderer*` and `RenderData*`;
5. request a full redraw when subscribed; and
6. detach or tombstone during renderer teardown.

A guarded `AddRenderEngine` detour is useful because it avoids relying on the private offset of a global renderer pointer:

```cpp
void add_engine_hook(Renderer* renderer, IRenderEngine* native) {
    original_add(renderer, native);
    if (!tls_inside_attach && !registry.has_renderer(renderer)) {
        tls_inside_attach = true;
        original_add(renderer, create_dormant_capture_engine(renderer));
        tls_inside_attach = false;
    }
}
```

The exact signature in the analyzed `conhost.c` includes additional source-location/error parameters. The adapter must use the matching PDB/type information and exact calling convention rather than this simplified example.

If a Windows generation no longer exposes a suitable render-engine fan-out for headless ConPTY, that generation is unsupported by the render-tap provider until a version-specific equivalent attachment point is implemented. This applies to both tested x64 paths on the deployment machine: the system conhost's real callback gate receives no application text, and current package OpenConsole source does not construct `Renderer` at all when `IsInVtIoMode()`. An outgoing-VT byte tap is a separate possible backend, but it is not the render-tap design specified here.

### 8.4 Headless scrollback semantics

The conhost `SCREEN_INFORMATION` contains a viewport, buffer, and virtual-bottom logic. That viewport is the screen conhost is presenting to its output connection. It is not Windows Terminal's later view into Windows Terminal's own history.

No ConPTY resize, owner, visibility, or input message communicates “the user is currently looking 200 rows back in Windows Terminal.” Consequently:

- the conhost engine remains at the live ConPTY screen;
- Windows Terminal can remain scrolled backward without interference;
- remote viewers see live output rather than the locally displayed historical rows; and
- switching back to the bottom requires no shellglass action.

This behavior must be explicit in CLI/help text for a conhost fallback.

### 8.5 Active-session inference

An owner HWND identifies the terminal window but not a split pane when several ConPTY sessions share that HWND. ConPTY setup enables focus event mode, and terminal focus transitions travel through the ConPTY input side. A conhost adapter can report the most recent focus-in/focus-out state as an indirect active-session signal.

This is less authoritative than the WT `ControlCore` focus state and must be treated as a hint. The broker selection order is:

1. use a terminal-host-native active source if registered for the foreground HWND;
2. otherwise use the live conhost source for that HWND with the most recent focus-in event;
3. otherwise retain the last unambiguous conhost source for that HWND; and
4. if ambiguity remains, freeze rather than oscillating between panes.

The focus inference needs an integration test for tab switches, split-pane switches, application focus loss, and terminal window activation.

### 8.6 conhost appearance semantics

Headless conhost is upstream of the terminal profile. It may know color indices and console-side resolved colors, but it does not know the final Windows Terminal font, palette, acrylic/background treatment, or unfocused appearance.

The normalized frame should preserve semantic attributes and color indices when that is more truthful than exporting conhost's internal fallback RGB palette. Shellglass's configured browser font remains the presentation font for a conhost source.

For classic conhost, conhost's own render data is much closer to final appearance, although the browser still renders glyphs independently.

## 9. Normalizing render callbacks into `model::Grid`

Native adapters should not serialize C++ renderer objects over IPC. They copy callback data into a version-independent native frame, which the broker converts into Rust `model` types.

### 9.1 Cells

For each cluster:

```text
text       = cluster's UTF-16 converted strictly to UTF-8
columns    = renderer-provided occupied cell count
wide       = columns == 2
style      = current normalized style
```

A multi-codepoint grapheme remains one `StyledCell.text`. A wide glyph occupies one stored cell with `wide = true`; its continuation column is not stored, matching current `Grid` conventions.

Zero-column combining clusters are appended to the preceding cell when the target renderer presents them that way. Invalid UTF-16 is replaced deterministically and counted in diagnostics.

### 9.2 Styles

Normalize styles into the fields shellglass already supports:

- `fg`, `bg`;
- `bold`, `dim`, `italic`;
- underline style 0–5;
- `strike`, `concealed`, `blink`;
- underline color;
- `inverse`;
- hyperlink table ID; and
- `wide`.

Where a provider has already resolved reverse video or selection into RGB colors, it should not also set flags that cause the browser to apply the transformation a second time. Each adapter profile must document whether callback colors are pre- or post-transformation.

### 9.3 Coordinates and line rendition

Paint coordinates are viewport-relative or convertible through the current viewport supplied by the target ABI. Adapters clamp every write to the announced frame bounds.

Double-width and double-height lines cannot be represented fully by the current `Grid`. The first implementation should either:

- flatten renderer coordinates into ordinary cells where the target callback has already done so; or
- mark the frame with an unsupported-feature diagnostic and render the closest cell representation.

The implemented `wt_1_24` family takes the first option: it records WT's
viewport-relative logical clusters and column widths and deliberately normalizes
`PrepareLineTransform` to the ordinary `Grid`. Text/copy coordinates remain exact,
while double-height/double-width visual scaling is the documented closest-cell
limitation. A future row-metadata extension can represent line rendition, but it
should be designed for both native taps and the vendored VT parser rather than added
only for Windows.

### 9.4 Cursor

Map the target cursor to:

- `Grid::cursor`;
- `Grid::cursor_style` values 0–6; and
- actual visibility after viewport clipping.

The existing model has no cursor color or width. Those values are retained in native diagnostics but omitted until the shared model gains support.

The adapter should report semantic cursor visibility rather than emitting every blink phase if doing so would duplicate the browser's own cursor blinking. This policy must match the existing shellglass viewer behavior and be verified visually.

### 9.5 Title and links

The current target title maps to `Grid::title`. Hyperlink IDs are remapped to compact frame-local `u32` IDs, and only URIs referenced by visible cells are placed in `Grid::links`.

URIs remain subject to the viewer's existing allowlist. Native target strings are length-capped before allocation or IPC.

### 9.6 Selection and search overlays

There are two viable representations:

- resolve overlays into each visible cell's final RGB foreground/background; or
- extend the shellglass model with explicit selection/search spans.

The first implementation should resolve overlays into cells because it requires no wire change and most closely mirrors the painted view. It means remote browser selection is independent of the operator's native selection, which is acceptable: the operator's highlight is pixels/styles in the stream, while the viewer's own selection still uses ghost rows.

### 9.7 Images

Modern renderer ABIs may expose `PaintImageSlice`. Slices are copied during the callback and never retained by pointer. A worker outside the render thread groups compatible adjacent slices into a raster, encodes PNG, computes the existing shellglass content key, and emits `ImagePlacement` plus `ImageBlob`.

Until grouping is implemented, the provider advertises `images = false` and omits slices. It must not generate one independently encoded PNG per row.

Pending image work must not delay text frames. The existing shellglass zombie/held-image behavior can be reused after native placements are expressed in the shared model.

## 10. Synchronization and frame pacing

### 10.1 Target locks

The target renderer owns synchronization around render callbacks. Adapter code must not recursively acquire the same console lock from inside a paint callback.

External actions such as attach, detach, and forced redraw acquire the target's lock through the resolved `IRenderData` interface or an exact target helper. Lock acquisition is never attempted from `DllMain`.

### 10.2 Frame rate

The target renderer determines when a visual frame exists. The capture adapter adds a transport cap of approximately 30 frames per second by coalescing completed frames in its newest-frame queue; it does not sleep the target render thread.

A source switch and first subscription bypass the interval once so the initial full frame is immediate.

### 10.3 Synchronized updates

A renderer tap observes the target's presentation boundary, so it should not need shellglass's VT-level DEC 2026 `SyncGate`. The target decides when to paint after synchronized output. The normalized source marks every published frame as already presentation-gated.

The PTY source retains its existing parser-side synchronization behavior.

## 11. Symbol and ABI management

### 11.1 No signature-scanning-first policy

Each target PE contains a CodeView/RSDS PDB identity. A shellglass helper resolves symbols before injection:

1. hash the target module;
2. read its PDB GUID and age;
3. fetch the matching PDB from the configured Microsoft symbol server/cache;
4. use DIA to resolve exact decorated symbols and type information;
5. select a compiled ABI-family adapter;
6. write a signed or integrity-protected local profile containing RVAs and expected bytes; and
7. pass that profile to the injected module.

The injected module verifies:

- target module hash;
- image size and architecture;
- PDB GUID/age from the PE;
- every RVA is inside the expected executable section; and
- expected prologue bytes match before detouring.

Any mismatch disables that provider.

As a feasibility check, the installed stock Windows Terminal build examined during this design exposed a downloadable, unstripped `Microsoft.Terminal.Control.pdb` containing the required internal `ControlCore` and `Renderer` symbols. This is evidence for the approach, not a promise that every future build publishes equivalent symbols.

### 11.2 ABI families

`IRenderEngine`, `IRenderData`, `Renderer`, `TextAttribute`, cursor options, and image types are private ABIs. Profiles are grouped by compatible interface family, for example:

```text
native/windows/
  common/                 normalized frame and IPC code
  wt_1_24/                matching renderer ABI shim
  wt_1_25/                another shim if ABI changed
  conhost_10_0_x/         matching system conhost ABI shim
  conhost_10_0_y/
```

A profile may share a compiled shim when PDB type inspection proves the virtual layout and callback structures are identical. Matching names alone are insufficient.

### 11.3 Calling and runtime rules

Native shims must:

- use the target architecture and MSVC ABI;
- use exact decorated overloads and calling conventions;
- enable Control Flow Guard;
- avoid passing STL ownership across module boundaries;
- avoid freeing memory allocated by the target and vice versa;
- avoid C++ exceptions across any hook or virtual callback;
- treat all target pointers as borrowed;
- copy spans and string views before returning;
- install no hooks while holding loader lock; and
- restore or abandon hooks safely during process shutdown.

ARM64 and x64 are separate adapters and detour implementations.

## 12. Native IPC protocol

The injected module connects as a client to a per-logon-session broker pipe created by shellglass. The protocol is binary, little-endian, length-prefixed, and independently versioned from the shellglass hub wire format.

### 12.1 Envelope

```text
u32 magic          "SGNT"
u16 protocol       native capture protocol version
u16 message_type
u32 payload_length
u64 process_nonce
u64 sequence
[payload]
```

The broker rejects unknown versions, oversized payloads, invalid enum values, impossible grid dimensions, malformed UTF-8, and sequence regressions.

### 12.2 Adapter-to-broker messages

```text
HELLO {
    provider: windows_terminal | conhost,
    pid,
    architecture,
    module_hashes,
    adapter_family,
    capabilities
}

SOURCE_ADDED {
    source_id,
    generation,
    owner_hwnd,
    rows,
    cols,
    title,
    provider_flags
}

SOURCE_UPDATED {
    source_id,
    generation,
    owner_hwnd?,
    rows?, cols?,
    focused?, visible?,
    title?
}

SOURCE_REMOVED {
    source_id,
    generation
}

FRAME {
    source_id,
    generation,
    frame_sequence,
    rows,
    cols,
    style_table,
    link_table,
    row/cell data,
    cursor,
    title,
    image placements
}

IMAGE_BLOB {
    source_id,
    generation,
    content_key,
    mime,
    bytes
}

DIAGNOSTIC {
    source_id?,
    bounded code,
    bounded text
}
```

### 12.3 Broker-to-adapter messages

```text
SUBSCRIBE {
    source_id,
    generation,
    desired_max_fps
}

UNSUBSCRIBE {
    source_id,
    generation
}

REQUEST_FULL {
    source_id,
    generation
}

PING
SHUTDOWN
```

A stale generation never reactivates a reused pointer/source ID.

### 12.4 Pipe behavior

The adapter reconnects with bounded backoff if the broker is absent. While disconnected all engines remain dormant. Starting shellglass later therefore activates already-registered injected adapters without requiring terminal restart, provided the injection agent maintains a small discovery/control channel or the adapter retries the broker pipe.

The pipe worker owns serialization. The render thread only swaps a refcounted/native frame buffer into the worker queue.

## 13. Broker source selection

### 13.1 Foreground policy

Default policy:

- if the foreground HWND has a native terminal-host source, subscribe to its focused pane;
- otherwise, if it has an unambiguous conhost source, subscribe to that source;
- when a different terminal HWND or WT tab becomes active, switch to its selected source;
- treat focus metadata as a level but advance selection order only on a false-to-true edge, so a delayed repeated `true` from the old tab cannot steal selection back;
- when a non-terminal window becomes foreground, keep the last selected terminal live;
- never downgrade a retained WT pane to its backing conhost merely because the foreground HWND is unrelated; and
- when the selected source dies, fall back only to a visible source that has previously been focused.

This makes the stream represent the operator's last active terminal while they use
Discord, a browser, the desktop, or the shellglass viewer itself. Explicit
`stream pause` remains the privacy boundary. `--foreground-only` restores strict
visible-view behavior by unsubscribing whenever no known terminal HWND is foreground.

### 13.2 Provider precedence and deduplication

For a WT pane, both a WT source and its backing conhost may be registered. The broker does not publish both. Precedence is:

```text
Windows Terminal native source > conhost source
```

Initial deduplication can use owner HWND, focus timing, process metadata, and dimensions. A later WT adapter can expose a stronger ConPTY/session identity if needed. Ambiguous conhost sources are retained in the registry but not auto-selected.

### 13.3 Source switches

Every source switch forces a shellglass full frame, even when the old and new dimensions match. This resets:

- cell matrix;
- cursor;
- title;
- links;
- images; and
- provider presentation metadata.

The source switch itself is not exposed as HTML. Viewer continuity remains the existing full/delta protocol.

## 14. Shellglass integration

### 14.1 Generic source session

Shellglass now exposes a parser-agnostic source session while preserving its existing PTY implementation:

```rust
pub struct SourceSession {
    pub frames: watch::Receiver<Arc<Frame>>,
    pub sink_status: Arc<dyn SinkStatus>,
}

pub trait SinkStatus: Send + Sync {
    fn hub_down(&self, _reason: &str) {}
    fn hub_up(&self) {}
}
```

Implementations:

- `PtySource`: current command/PTY/parser behavior and terminal pause notifier;
- `WindowsNativeSource`: broker frames and a no-op sink-status implementation.

A native source must never pause, clear, repaint, or write into the user's terminal when the hub disconnects.

The shellglass CLI delegates to the public API, its client startup closure returns
`SourceSession`, and `server`/`diff::Live` remain frame-oriented.

### 14.2 CLI shape

Proposed foreground commands:

```text
shellglass-wt-tap serve
shellglass-wt-tap push https://hub --key ...
```

To avoid occupying the shell whose pane is being observed, add a detached control plane:

```text
shellglass-wt-tap stream start --hub https://hub --key ...
shellglass-wt-tap stream pause
shellglass-wt-tap stream resume
shellglass-wt-tap stream stop
shellglass-wt-tap stream status
```

`stream start` launches or contacts a per-user broker and returns immediately. The user's prompt continues normally in the same terminal.

The existing trailing command remains the default for `--source pty` and for non-Windows platforms.

### 14.3 Presentation metadata

The current register payload fixes browser fonts and render configuration for a session. Native sources can switch between panes with different Windows Terminal fonts. The first implementation may deliberately use shellglass's configured browser font for every native source while preserving content and RGB color fidelity.

Exact per-pane font switching requires a later protocol feature for presentation metadata and dynamic font assets. It is not required to prove the render taps.

## 15. Failure behavior

### 15.1 Unknown target build

If symbols, hash, expected bytes, or ABI family do not match:

- install no hooks for that provider;
- report one bounded diagnostic to the broker/injection controller;
- allow the target process to run normally; and
- permit another provider, such as conhost, to cover the session.

### 15.2 Capture-engine callback failure

Every callback is `noexcept` in effect. On an internal adapter error:

- atomically disable that engine;
- return success to the target renderer where doing so is safe;
- drop the incomplete frame;
- queue a diagnostic outside the render path; and
- do not repeatedly retry inside the same process unless the broker explicitly requests it.

A capture failure must not take down the real terminal renderer.

### 15.3 Broker or hub outage

A broker outage makes adapters dormant after their pipe queues fill/drop. A hub outage affects only shellglass's external client. Neither condition changes the terminal process or command-line application.

On reconnect:

1. sources re-register;
2. the broker selects the active source;
3. it sends `REQUEST_FULL`; and
4. the normal shellglass client re-registers and sends a full frame before deltas.

### 15.4 Process shutdown

Do not perform blocking detach work in `DllMain(DLL_PROCESS_DETACH)`. Normal object/renderer destructor hooks mark sources dead. At final process teardown, leaked process-lifetime engine objects are reclaimed by the OS.

## 16. Security and trust boundaries

Although injection is assumed available, captured terminal contents remain sensitive.

- The native pipe is restricted to the current user and logon session.
- Elevated terminal processes require an equivalently authorized broker; the system must not accidentally bridge integrity levels.
- The adapter accepts only subscription/control messages, never arbitrary target-process calls or terminal input.
- The broker treats native frame data as untrusted and applies dimensions, allocation, string, image, and message-size limits.
- File- or shared-memory-referencing image protocols are never dereferenced by the broker merely because a terminal cell mentions them.
- The injected module contains no hub key and performs no Internet requests.
- A global pause must stop subscriptions and freeze or replace the published frame promptly.

## 17. Implementation phases

### Phase 1: shellglass source boundary

- Introduce `SourceSession` and no-op native sink status.
- Keep PTY behavior and tests unchanged.
- Add a synthetic native-frame source test.
- Verify serve, push reconnect, recording, SSH, and image stores remain backend-agnostic.

### Phase 2: native broker and protocol

- Implement source registry, named-pipe transport, message caps, generations, and foreground tracking.
- Implement a mock adapter process.
- Exercise source add/remove, focus switches, resize, malformed messages, broker restart, and newest-frame dropping.

### Phase 3: Windows Terminal text MVP

- Resolve and verify the installed WT symbols.
- Hook `ControlCore::Initialize`, focus, owner HWND, and destruction.
- Attach a dormant engine.
- Capture full-frame text, RGB foreground/background, basic attributes, cursor, title, and resize.
- Verify the operator-scrolled WT viewport is mirrored.

### Phase 4: conhost text MVP

- Build the first conhost ABI adapter for the analyzed system binary.
- Hook renderer attachment/teardown and PTY resize/owner signals.
- Capture full-frame text, attributes, cursor, title, and alternate screen.
- Demonstrate fallback with a second ConPTY terminal host and with classic conhost.
- Document and test live-bottom behavior while WT is scrolled backward.

### Phase 5: active-source hardening

- Test multiple WT windows, tabs, split panes, elevated windows, pane moves, terminal detach/reattach, and rapid focus changes.
- Add conhost focus inference.
- Deduplicate WT and backing-conhost sources.
- Fail closed on ambiguous ownership.

### Phase 6: fidelity

- Hyperlinks and underline colors.
- Selection/search overlay resolution.
- Image-slice grouping and existing shellglass image blobs.
- Conceal/blink/cursor parity.
- Line-rendition model decision.
- Optional dynamic presentation/font metadata.

### Phase 7: compatibility automation

- Build symbol-profile tooling around DIA.
- Record PDB GUID/age, type-layout fingerprints, RVAs, and prologue hashes.
- Add x64 and ARM64 adapter CI where binaries are available.
- Generate a compatibility report rather than silently guessing after upgrades.

## 18. Verification plan

### 18.1 Unit tests

- Native frame decoder rejects overflow, malformed UTF-8, invalid wide cells, duplicate style IDs, out-of-range coordinates, and excessive image sizes.
- Native-to-`Grid` conversion preserves graphemes, wide cells, links, colors, cursor, title, and blank cells.
- Source generations prevent stale frame publication.
- Newest-frame queue always retains the latest complete frame.
- Provider precedence consistently chooses WT over conhost.

### 18.2 Injected adapter tests

For each supported binary hash:

- symbol profile matches PDB and PE identity;
- every detour prologue matches expected bytes;
- attaching a dormant engine causes no frame work;
- subscription forces exactly one initial complete frame;
- detach/close during rendering does not use freed memory;
- broker absence does not slow target rendering; and
- callback faults disable only the capture provider.

### 18.3 Windows Terminal scenarios

- normal shell output and prompt editing;
- alternate-screen TUI entry and exit;
- mouse-wheel and scrollbar history navigation;
- output arriving while scrolled backward;
- resize and reflow while scrolled;
- tab switch;
- split-pane focus switch;
- multiple top-level WT windows;
- selection, search, hyperlinks, and cursor styles;
- profile color and focus appearance changes;
- pane close during a frame; and
- move/detach pane between windows.

The decisive scrollback assertion is: the WT provider's remote frame equals the rows visibly presented by WT, not the current ConPTY bottom.

### 18.4 conhost scenarios

- classic `cmd.exe` conhost;
- headless ConPTY with Win32 console writes;
- headless ConPTY with VT output;
- initial and repeated resize signals;
- main and alternate screen;
- owner HWND and visibility updates;
- several ConPTY panes sharing one owner HWND;
- terminal focus-in/out inference; and
- downstream WT scrolling while output continues.

The decisive headless assertion is: local WT history navigation remains unaffected, while the conhost source intentionally remains on the live screen.

### 18.5 Performance measurements

Measure target-process impact with capture dormant and active:

- render-frame latency;
- time under terminal lock;
- CPU at 80x24, 240x80, and 320x100;
- memory per registered and subscribed source;
- high-rate full-screen TUI output;
- sixel/image output;
- broker disconnected; and
- slow hub/network.

No target render callback may block on broker or hub throughput. The expected overload behavior is dropped intermediate capture frames, never a slower local terminal.

## 19. Decisions and rationale

1. **Use renderer fan-out, not global console API hooks.** The renderer supplies coherent visual frames and handles both Win32 and VT application behavior.
2. **Prefer WT for WT panes.** Only WT knows its private history viewport and active pane authoritatively.
3. **Retain conhost as a generic fallback.** It covers ConPTY hosts broadly and gives a useful live-session view without changing local workflows.
4. **Attach early but remain dormant.** Startup injection makes every source discoverable; dormant engines avoid continuously parsing or copying inactive sessions.
5. **Rebuild full viewports first.** Correctness and version tolerance matter more than local dirty-rectangle optimization.
6. **Keep IPC and network off render threads.** Target responsiveness is a hard invariant.
7. **Version private ABIs explicitly.** PDBs and hashes select known adapters; unknown builds are not guessed.
8. **Keep shellglass's existing frame pipeline.** Native capture replaces only the producer, preserving the mature diff, hub, recording, and viewer layers.

## 20. Recommended first deliverable

The first end-to-end milestone should support one known stock Windows Terminal build and produce text-only frames from the focused pane:

```text
startup injection
  -> symbol-verified WT hooks
  -> dormant render engine per ControlCore
  -> broker selects foreground/focused core
  -> full viewport frame
  -> shellglass Grid
  -> existing push/hub/viewer
```

It must demonstrate all three workflow properties before expanding to conhost:

1. `shellglass-wt-tap stream start` returns control to the existing shell;
2. ordinary commands, TUIs, tabs, panes, and local scrollback continue unchanged; and
3. scrolling backward in Windows Terminal changes the remote view to the same historical rows.

The conhost render tap then becomes the second provider, using the same native protocol and broker while deliberately providing live-bottom semantics for headless ConPTY sessions.
