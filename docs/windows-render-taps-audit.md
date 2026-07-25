# Windows render-tap completion audit (x64 personal deployment)

This checklist maps the requirements in `windows-render-taps.md` to implementation
and verification surfaces. It is intentionally conservative: a mock proves the
protocol path, not private-ABI fidelity, and an x64 compile does not prove a real
target. The accepted personal-deployment scope is the current x64 Windows Terminal
hook path. The user explicitly excluded ARM64 and headless ConPTY; their fail-closed
states are retained but are not completion blockers.

## Phase checklist

| Requirement | Artifact / verification surface | State |
|---|---|---|
| Generic parser-independent source session | `src/source.rs`; `source::tests::synthetic_external_source_is_frame_compatible_and_latest_only` | complete |
| Preserve PTY source behavior | `src/pty.rs`, `src/cli.rs`, `src/client.rs`; workspace test suite | complete |
| Serve, push, recording, SSH, and images remain source-agnostic | `native/windows/test-e2e.ps1`: native C++ mock → HTTP, PNG endpoint, real SSH, recording, push/hub and hub restart | complete |
| Bounded versioned native protocol | `src/native_protocol.rs`; `native/windows/protocol.md`; malformed/overflow/UTF-8/wide/style/link/image tests | complete |
| Registry, generations, precedence, ambiguity, resize validation | `src/native_broker.rs` unit tests | complete |
| Secure per-session Windows pipes and foreground tracking | `src/windows_native.rs`; Windows unit/E2E tests; real elevated-token Sandbox assertion | complete for tested x64 environment |
| Detached start/pause/resume/stop/status | `src/cli.rs`, `src/windows_native.rs`; unit test plus real WT detached push/pause/freeze/resume/hub snapshot stage | complete |
| Broker restart and newest-frame behavior | `test-e2e.ps1`; WT stalled-reader and hard-restart stages | complete |
| WT text MVP | `native/windows/wt_adapter.cpp`; exact profiles for 1.24.11911.0 and 1.24.11321.0 | complete for two exact x64 releases |
| Last active WT remains live under non-terminal foreground | real Notepad foreground stage while output advances; `snapshot-nonterminal-foreground-live.json` | complete on primary release |
| WT visible history, including output arriving while scrolled | `test-wt-sandbox-guest.ps1`; `snapshot-scrolled-during-output.json` | complete on both exact releases |
| WT resize/reflow while scrolled | `test-wt-sandbox-guest.ps1`; `snapshot-scrolled-resized.json` | complete on both exact WT releases |
| WT rapid resize transaction coherence | synchronized `RESIZE_COHERENCE` fixture; 20 away/back-to-identical-size cycles reject mixed-generation frames | complete on primary release |
| WT wheel and scrollbar history navigation | real wheel stage plus writable UI Automation `ScrollBar`/`RangeValue`; `snapshot-scrollbar-history.json` | complete on primary release |
| WT alternate-screen entry/exit | fixture DECSET/DECRST 1049; `snapshot-alternate.json`, `snapshot-main-after-alternate.json` | complete on primary release |
| Local prompt editing remains functional | detached pause stage types a wrong final character, backspaces, corrects it, then resumes capture | complete on primary release |
| Classic conhost MVP | `native/windows/conhost_adapter.cpp`; `test-conhost-sandbox.ps1` | complete for `conhost_10_0_19045` x64 |
| Headless ConPTY text capture | system family real test receives no application text; current package source creates `Renderer` only outside `IsInVtIoMode()` | explicitly excluded by user; correctly fail closed |
| WT/conhost dedup and ambiguity | broker precedence/ambiguity tests | complete at broker policy level; real backing-conhost coexistence cannot be exercised by the fail-closed headless family |
| Tabs, split panes, rapid focus, multiple windows | aggregate WT gate plus dedicated lifecycle gate | complete on both exact WT releases |
| Pane detach, reattach, close | `test-wt-lifecycle-sandbox.ps1`: two already-hooked named windows; split closes during sustained callbacks | complete on both exact WT releases; latest expanded primary gate passed |
| Elevated target authorization | mandatory-label RID assertions in aggregate WT gate | complete in Windows Sandbox |
| Hyperlinks, underline colors/styles | WT fixture and snapshot assertions | complete |
| Selection/search overlays | WT fixture and snapshot assertions | complete |
| Conceal/blink/cursor parity | WT fixture and snapshot assertions, including DECSCUSR vertical-bar wire style | complete for represented shared-model fields |
| Image slice grouping and image blobs | WT real sixel assertion; dirty-region persistence; C++ mock PNG through serve/record/push/hub | complete |
| Line-rendition decision | documented closest-cell normalization in `windows-render-taps.md` | complete; deliberate limitation |
| DIA profile generation and compatibility reports | `profile_tool.cpp`, `test-profile.ps1` | complete |
| Fail closed on unknown PE/PDB/prologue/layout | profile tests and adapter startup verification; no signature scan | complete |
| Multiple exact ABI profiles | two WT release profiles and one system-conhost profile | complete for supported x64 targets |
| Callback fault containment | test-only WT/conhost fault DLLs and real callback diagnostics 202/212 | complete |
| No target callback IPC/allocation/model conversion | fixed-capacity callback batches; worker-side transport/model/image encoding | complete by implementation and stalled-reader responsiveness gate |
| Bounded newest-wins overload | atomic replacement queue and nonzero dropped-frame diagnostics under suspended broker reader | complete |
| Dormant/closed-source memory release | callback-quiescence handoff and worker-side batch/model reclamation | complete; aggregate memory gate exercises many source switches |
| Size performance matrix | 80×24, 240×80, 320×100 interval p95, CPU and private-memory thresholds | complete on both exact WT releases |
| Sustained output/leak check | configurable `-StressSeconds`; successful 121-second primary run | complete for primary exact release |
| Broker absence and slow reader | suspended-reader drop test plus hard process restart/full-frame recovery | complete |
| Push/hub reconnect | native mock E2E; real WT detached worker publishes through a local hub | complete |
| First-party operator startup | `start-wt-stream.ps1`; `test-start-wt-stream-sandbox-guest.ps1` | complete: actual script profiled/served/injected/opened a fresh tab/published it in the persistent Sandbox |
| ARM64 runtime hooks | explicitly excluded by user; tooling/fixtures still compile | not a completion blocker |

## Scenario evidence boundaries

- `mock_adapter.cpp` proves only protocol and shared-pipeline interoperability.
- `test-wt-sandbox.ps1 -IncludeLifecycle -KeepSandboxOpen` is the real WT
  private-ABI/fidelity/lifecycle evidence. `test-wt-sandbox-reuse.ps1` refreshes
  and reruns it in the same guest. Injection occurs only inside Windows Sandbox.
  The latest expanded primary artifact is
  `target/wt-sandbox-e2e-23224/combined-result.json`; it passed alternate screen,
  wheel and scrollbar history, prompt editing, detached push/pause/resume,
  close-during-render, and all prior gates.
- `test-conhost-sandbox.ps1` is the real classic-conhost evidence. Its successful
  result explicitly asserts that the tested headless family remains fail closed.
- Unit tests prove malformed-input rejection and policy edge cases but do not
  substitute for a target renderer callback.

## Completion audit

The user explicitly accepted WT hooks as the complete personal-deployment scope
and waived headless ConPTY; ARM64 was already waived. Current post-change gates
are green:

- Rust fmt/clippy/181 unit tests plus the vendored vt100 suite;
- all Rust feature subsets;
- viewer type-check, 28 tests, and fresh committed dist;
- x64 and retained ARM64 compile gates;
- native profile success and fail-closed tests;
- native mock serve/SSH/image/recording/broker-restart/push/hub-reconnect E2E;
- both exact WT release aggregates;
- latest expanded current-machine WT aggregate plus lifecycle in one persistent
  Sandbox (`target/wt-sandbox-e2e-23224/combined-result.json`);
- classic conhost (extra, outside the accepted WT-only scope); and
- the non-injecting `start-wt-stream.ps1 -PrepareOnly` host setup gate against
  the installed `1.24.11911.0` package and matching PDB; and
- the actual first-party operator script inside the persistent Sandbox, where it
  started local serve, launched and injected WT, opened a fresh tab, and published
  `OPERATOR_LAUNCH_E2E_OK` (`operator-result.json`: passed).

No requirement remains missing within the accepted x64 WT scope. Unsupported
versions, ARM64, and headless ConPTY continue to fail closed rather than weakening
ABI or fidelity guarantees.
