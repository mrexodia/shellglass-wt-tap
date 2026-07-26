# Accessibility layout fixtures

Each fixture pairs the exact accessibility snapshot consumed by the spatial
renderer with a screenshot of the same stable foreground window:

- `tree.json` — versioned renderer input, including roles, states, text, and bounds.
- `reference.png` — development oracle for side-by-side visual review; it is never
  read by live streaming or by the renderer.

Capture or refresh a fixture from the repository root:

```powershell
cargo run -- capture-layout-fixture target\layout-fixtures\case-name --delay-ms 3000
```

After starting the command, focus the target during the delay. The command
checks the privacy policy, captures the accessibility snapshot, captures only
the stable foreground window bounds, rechecks window identity, and writes
`tree.json`, `reference.png`, and a plain `render.txt` preview. Copy reviewed
`tree.json` and `reference.png` into this directory.

Replay any tree after changing the renderer, without reopening the application:

```powershell
cargo run -- render-layout-fixture tests\fixtures\accessibility\x64dbg-cpu\tree.json `
  --output target\x64dbg-render.txt
```

Committed fixtures:

- `7zip-file-list` — a list-view table with header-overlapped clipped rows,
  interleaved scroll controls, and child controls representing each column.
- `chrome-striga` — Chromium article content with nested headings, links, and
  inline text runs whose union bounds overlap within each HTML paragraph.
- `chrome-x64dbg-commands` — variable-height FAQ list items whose wrapped
  continuation lines must not be overwritten by following items.
- `chrome-x64dbg-plugins` — nested HTML lists whose child bullets require
  separate indented rows rather than flattened parent labels.
- `chrome-huggingface-list` — variable-height HTML bullet items and compact
  evaluation-result rows from a Hugging Face model page.
- `chm-x64dbg` — legacy MSHTML prose fragments, navigation lists, and a native
  keyword-results pane whose items are not exposed through accessibility.
- `chm-x64dbg-contents` — text-width native tree rows and legacy semantic lists
  whose hierarchy is exposed through names and nested list structure.
- `dataexplorer` — a multiline code editor exposed by UIA as `TextField`
  rather than `TextArea`.
- `ida-pseudocode` — fixed-width editor lines padded with trailing spaces that
  must not wrap into visually blank rows.
- `vscode-screen-reader` — a Monaco editor whose complete multiline value is
  exposed on a current-line `TextField` with much smaller bounds than its host.
- `zed-experimental-a11y` — Zed 1.12.0 with `ZED_EXPERIMENTAL_A11Y=1`, exposing
  title/status controls and large named panes but no project entries or editor
  text.
- `x64dbg-cpu` — dense menus, disassembly, registers, flags, tables, and nested
  panes from x64dbg's CPU view.
- `x64dbg-log` — a multiline static-text log viewport whose accessibility value
  contains the complete newline-delimited log history.
- `visual-studio-2022` — a WPF IDE workspace with a table row positioned left
  of its inferred first column, exercising defensive coordinate handling.
- `total-commander` — adjacent menus/tabs and two dense file lists from Total
  Commander.
- `total-commander-lister` — a custom-painted text viewport that exposes only
  window chrome and a scrollbar, with no accessible document or text value.

Tests use the JSON snapshots for deterministic structural assertions. PNGs are
human references because native GUI pixels and semantic terminal output are not
meaningfully pixel-equal.
