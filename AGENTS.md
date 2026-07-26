# Agent Guidelines

This repository owns Shellglass's hybrid Windows capture producer:

- Foreground Windows Terminal and classic conhost windows use native render taps.
- Other foreground windows are reconstructed from xa11y accessibility snapshots.
- Presentation and layout heuristics belong here, not in xa11y. Change xa11y only
  when the platform provider itself is incorrectly omitting or corrupting data
  that the accessibility API actually exposes.

The main objective when fixing a display issue is not pixel imitation. Produce a
readable, representative spatial TUI from the data the selected capture path
actually provides, with a deterministic regression proving the fix.

## Non-negotiable constraints

1. Native foreground terminal capture always takes precedence over accessibility.
2. Live accessibility rendering must never use screenshots, OCR, input simulation,
   Win32 text scraping, or application-specific automation. Screenshots are
   development oracles only.
3. Preserve provider text verbatim. Do not rewrite source text merely to hide a
   provider quirk. In particular, preserve newlines, indentation, rich text, and
   strings such as x64dbg's HTML-like tags.
4. Keep traversal and rendering bounded. Do not remove depth, node, text-length,
   row, or column limits to fix one application.
5. Prefer structural rules based on roles, bounds, states, and child shape over
   application-name checks. A good fix should help the same control pattern in
   other applications.
6. Do not fabricate unavailable content. If the accessibility tree contains only
   chrome or a scrollbar, say that content is not exposed; do not infer it from a
   screenshot.
7. Privacy failures are fail-closed. Never publish identity, title, tree, screenshot,
   or focus details for a denied or unidentified application. Transient capture
   errors leave the last coherent frame untouched and may be logged locally.
8. Do not inject experimental native DLLs into the terminal hosting the agent.
   Private-ABI native tests run in the documented Windows Sandbox gates.

## First determine which capture path is broken

Do this before changing rendering code.

- **Windows Terminal/conhost foreground window:** investigate the native adapter,
  protocol, broker, or complete-frame model. Accessibility must not override it.
- **Any other GUI foreground window:** investigate `src/accessibility.rs` and the
  captured accessibility tree.
- **Wrong frame during a focus transition:** investigate `src/native_broker.rs`
  and foreground-generation tickets as well as the producer.

A screenshot containing missing terminal cells under a rapidly repainting TUI is
usually a native dirty-delta problem, not an accessibility layout problem. A GUI
window with present-but-overlapping text is usually a renderer problem. A GUI
window whose text is absent from `tree.json` is a provider limitation, not a
layout problem.

## Evidence to collect

### 1. Keep the user's screenshot

Treat it as the visual reference, not as renderer input. Record which regions are
wrong and what exact text should remain visible. Do not commit arbitrary clipboard
or ShareX paths.

### 2. Record dynamic stream failures when useful

Use a Shellglass recording for corruption, resize, focus, privacy, or transition
issues that cannot be represented by one static tree:

```powershell
cd C:\CodeBlocks\shellglass-wt-tap
cargo run -- serve --bind 127.0.0.1:8080 `
  --record-dir target\recordings\issue-name
```

A `.sgs` recording is a timestamped, self-contained Shellglass push transcript.
It preserves register data, wire messages, fonts, and images. Hub pushes record by
default when the hub is configured for recording; `push --no-record` opts out.
See the sibling `../shellglass/README.md`, section **Session recording**, for the
format and retrieval commands.

Recordings may contain terminal output, document text, and other sensitive data.
Keep them under `target/`; do not commit or publish them without explicit approval.
A recording does not contain an accessibility tree or development screenshot, so
capture a layout fixture too when diagnosing a GUI renderer issue.

When reproducing a transition, write down the exact sequence, for example:

```text
WT -> Total Commander -> quickbar -> taskbar -> Total Commander -> WT
```

Also note the stream dimensions and whether the foreground app was moved, resized,
scrolled, or unchanged.

### 3. Capture a deterministic accessibility fixture

For a non-terminal GUI issue, capture the stable window before editing code:

```powershell
cd C:\CodeBlocks\shellglass-wt-tap
cargo run -- capture-layout-fixture target\layout-fixtures\case-name `
  --delay-ms 3000
```

Start the command, then focus the target during the delay. The target must remain
the same stable foreground window long enough for identity rechecks. The command
honors `privacy.toml` and explicit privacy options and writes:

- `tree.json` — the exact versioned snapshot consumed by the renderer;
- `reference.png` — a window-only screenshot for human comparison;
- `render.txt` — the TUI generated by the same renderer used for streaming.

Never bypass a privacy block to get a fixture. Do not commit a fixture containing
private data without approval.

If the app is already open, use xa11y's app listing to identify the correct PID and
title before focusing it:

```powershell
cargo run --manifest-path ..\xa11y\xa11y\Cargo.toml --bin xa11y -- apps
```

Do not treat `target/layout-fixtures/.../render.txt` as authoritative after a code
change; replay the immutable tree through the current renderer.

### 4. Replay without reopening the application

```powershell
cargo run -- render-layout-fixture `
  target\layout-fixtures\case-name\tree.json `
  --output target\layout-fixtures\case-name\render-after.txt
```

Test constrained layouts too when the rule concerns wrapping or clipping:

```powershell
cargo run -- render-layout-fixture `
  target\layout-fixtures\case-name\tree.json `
  --cols 120 --rows 40 `
  --output target\layout-fixtures\case-name\render-120x40.txt
```

Preview and streaming share this renderer; do not build a second preview-only
implementation.

## Inspect the tree before proposing a fix

The decompiler, screenshot, and application widget model are not the accessibility
tree. Inspect actual roles, bounds, names, values, states, and children.

A small Python walker is often faster than guessing:

```powershell
@'
import json
p = r"target/layout-fixtures/case-name/tree.json"
r = json.load(open(p, encoding="utf-8"))["snapshot"]["root"]

def walk(n, depth=0):
    text = n.get("value") or n.get("name") or n.get("description") or ""
    print("  " * depth, n["role"], repr(text[:240]), n.get("bounds"), len(n["children"]))
    for child in n["children"]:
        walk(child, depth + 1)

walk(r)
'@ | python -
```

For large Chromium/Electron trees, print only relevant roles or text. Always answer
these questions:

1. Is the missing text present anywhere in `tree.json`?
2. Is it in `name`, `value`, `description`, or descendant text?
3. Is one semantic label duplicated on parent and descendants?
4. Are newlines real, or are padded spaces wrapping into blank rows?
5. Do item bounds cover a whole row, only painted text, or a clipped fragment?
6. Are controls ordered semantically, spatially, or both?
7. Are scrollbars or header controls interleaved before content rows?
8. Is the control a real table/tree/list, or a list whose children represent cells?
9. Is content virtualized onto a current-line field with a larger ancestor host?
10. Is the apparent content completely absent, leaving only chrome or scrollbars?

If the answer to question 1 is no, stop trying layout heuristics. Document the
provider limitation and render a bounded neutral marker such as
`⟦ content not exposed via accessibility ⟧` when appropriate.

## Common renderer failure patterns

Implement the narrowest structural correction that addresses the captured shape.
Useful patterns already handled in `src/accessibility.rs` include:

- parent/descendant duplicate labels;
- semantic inline flow for overlapping HTML/MSHTML text fragments;
- variable-height wrapped list items and packed-row cursor advancement;
- nested lists exposed either structurally or as newline-delimited names;
- tree rows whose bounds cover only painted text;
- table-like `ListItem` rows backed by `TextField` and `StaticText` children;
- header-overlapped clipped rows and interleaved scrollbar controls;
- multiline editors mapped to `TextField` instead of `TextArea`;
- Monaco values exposed on a current-line field inside a larger editor host;
- fixed-width editor lines padded with trailing spaces;
- multiline log histories positioned approximately from scrollbar range values;
- hidden/offscreen subtree pruning;
- bounds outside inferred table columns and negative source indents;
- decorative controls never overwriting semantic content.

Do not solve overlap by arbitrarily deleting text. Decide which semantic node owns
it. Do not solve truncation by allowing every item to overwrite adjacent panes.
Borrow only demonstrably unused space, stop at siblings or container boundaries,
and keep a gutter between panes.

When a provider exposes an action or meaningful state, retain its semantic role.
Presentation simplification must not change capture or privacy behavior.

## Add a committed regression fixture

After reviewing the temporary fixture:

1. Copy only `tree.json` and `reference.png` to:

   ```text
   tests/fixtures/accessibility/<case-name>/
   ```

2. Add the case and provider shape to
   `tests/fixtures/accessibility/README.md`.
3. Add a focused test in `src/accessibility.rs` using `include_str!` and
   `render_snapshot`.
4. Assert the exact behavior that was broken, not a broad snapshot dump.

Good assertions include:

- all expected rows occur in order and on consecutive lines;
- a continuation phrase remains present after wrapping;
- nested bullets are indented relative to their parent;
- columns occur left-to-right on the same row;
- a long register or function name is complete up to the real pane boundary;
- duplicate fragments do not occur;
- a clipped/offscreen row is absent;
- unavailable content gets the explicit neutral marker;
- text from an inaccessible or privacy-blocked app never appears.

The test should fail against the pre-fix renderer. Keep screenshots as human oracles;
do not pixel-compare them in tests.

## Dynamic and broker regressions

A layout fixture cannot prove focus-race, retained-terminal, stale-ticket, pause,
or native redraw behavior. Add focused tests in `src/native_broker.rs` or the
relevant protocol module for cases such as:

- terminal -> GUI -> terminal;
- terminal -> denied app;
- GUI -> taskbar/capture failure -> GUI;
- stale accessibility publication after foreground changes;
- returning to an unchanged WT source requiring `RequestFull`;
- partial native repaint overload requiring full reconciliation.

Accessibility capture errors must log locally and publish nothing, preserving the
last coherent frame. Privacy-blocked behavior is separate: it may keep a retained
terminal live but must reveal no blocked-app transition or metadata.

## Native Windows Terminal/conhost issues

Read `native/windows/README.md`, `docs/windows-render-taps.md`, and
`docs/windows-render-taps-audit.md` before editing private-ABI code.

Important distinctions:

- WT capture batches contain dirty-region deltas. Dropping/replacing deltas as if
  they were complete frames can permanently lose cells. Reconcile any lost delta
  sequence with a forced full repaint.
- Downstream Shellglass frames are complete model snapshots and may use latest-only
  publication; render callback batches may not make that assumption.
- Render callbacks must remain bounded, allocation-free where documented,
  nonblocking, and free of pipe/model/encoding work.
- Existing injected adapter DLLs remain loaded for the lifetime of
  `WindowsTerminal.exe`; rebuilding the DLL is not enough to update that process.

Compile native changes in a fresh build directory if an old CMake cache references
a moved checkout:

```powershell
cmake -S native/windows -B target/native-windows-agent -A x64
cmake --build target/native-windows-agent --config Release `
  --target shellglass-wt-adapter
```

Never validate by injecting into the active terminal that hosts the agent. Use the
documented disposable Windows Sandbox gates, for example:

```powershell
powershell -File native/windows/test-wt-sandbox.ps1 `
  -Pdb C:\symbols\Microsoft.Terminal.Control.pdb
```

Run lifecycle/overload variants when those paths changed. If a Sandbox gate was
not rerun, say so explicitly; a successful compile is not real-target verification.

## Verification checklist

From `C:\CodeBlocks\shellglass-wt-tap`:

```powershell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test --lib
cargo check --no-default-features --features accessibility
git diff --check -- src/accessibility.rs tests/fixtures/accessibility
```

Use a narrower `git diff --check` list when unrelated CRLF documentation produces
known noise, but always include every file changed for the issue.

A detached stream worker can own the named pipe used by one control-plane test.
Do not stop a user's live stream without permission. Check it first:

```powershell
cargo run -- stream status
```

When safe, stop it and run the complete suite:

```powershell
cargo run -- stream stop
cargo test --lib
```

If it cannot be stopped, run the remaining suite with the exact blocked test
skipped and report that limitation rather than claiming a complete pass:

```powershell
cargo test --lib -- `
  --skip windows_native::tests::detached_control_plane_pauses_reports_and_resumes
```

For provider/test-app changes, run the relevant xa11y integration checks from the
xa11y repository as required by its own `AGENTS.md`. Shellglass-only fixture and
presentation changes should not modify xa11y.

## Completion report

A display fix is complete only when the final response states:

- the provider shape and root cause;
- what structural rendering/capture rule changed;
- what data remains unavailable and why, if applicable;
- the committed fixture and focused regression added;
- exact formatting, lint, build, and test results;
- whether the live worker, WT process, native DLL, or Sandbox gate still needs a
  restart/rerun.

Keep temporary renders, recordings, native builds, and exploratory scripts under
`target/`. Commit only durable source, documentation, and reviewed fixtures.
