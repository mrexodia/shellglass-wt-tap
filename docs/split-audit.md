# Shellglass / WT tap split completion audit

This checklist maps the accepted split objective to current artifacts and direct
evidence. A checked implementation item is not by itself a completion claim;
runtime gates are listed separately.

## Deliverables

| Requirement | Artifact / evidence | State |
|---|---|---|
| Independent sibling Cargo project using the shellglass library | `Cargo.toml`: its own `[workspace]`; `shellglass = { path = "../shellglass", default-features = false }` | implemented |
| Parser-independent source abstraction | shellglass `src/source.rs`: `SourceSession`, `SinkStatus` | implemented |
| Stable complete-frame publisher, newest-wins | `src/source.rs`: `FramePublisher`, `external_source`; `synthetic_external_source_is_frame_compatible_and_latest_only` | verified by root tests |
| Generic discontinuity marker | `model::Grid::source_epoch`; `diff::encode_delta` layout check; `source_epoch_switch_forces_full_even_at_same_layout` | verified by root tests |
| Public presentation/serve/push orchestration | shellglass `src/api.rs`: `Presentation`, `ServeOptions`, `PushOptions`, `serve`, `push`; source types re-exported | implemented |
| Delayed push source startup | `client::run` invokes the source factory only after a successful upgrade; `external_push_waits_for_upgrade_then_sends_stock_wire` proves refusal does not invoke it | verified by root tests |
| Synthetic third-party producer | `examples/external-source.rs`; `docs/library-api.md`; example was run with only `serve-api` and its `/snapshot` returned `external frame` | verified |
| HTTP snapshot and SSE through library API | `external_source_serves_stock_snapshot_and_sse` | verified by root tests |
| Synthetic push wire through library API | `external_push_waits_for_upgrade_then_sends_stock_wire` checks register followed by full frame | verified by root tests |
| CLI delegates rather than duplicating orchestration | `src/cli.rs` serve/push paths call `api::serve` / `api::push` | inspected |
| Existing PTY implementation remains | `SourceArgs::start` still calls `pty::start`; default `serve`/`push` features include the existing PTY stack; all PTY/parser tests pass | verified by root tests and feature matrix |
| External project does not compile PTY/parser stack | `shellglass-wt-tap` forwards `serve-api`/`push-api`; `cargo tree` contains none of `portable-pty`, `icy_sixel`, `signal-hook`, or `vt100` | verified |
| Hook-specific Rust moves out of shellglass | `src/{native_protocol,native_broker,windows_native}.rs`; corresponding shellglass files are absent | inspected |
| Native implementation and scripts move | `native/windows/`; shellglass has no `native/` tree | inspected |
| Hook CLI/control plane moves | `src/main.rs`: serve, push, and `stream start|pause|resume|stop|status`; shellglass CLI has no active-terminal/stream surface | inspected |
| Hook design/audit docs move | `docs/windows-render-taps*.md`; shellglass exposes only generic `docs/library-api.md` | inspected |
| Stock shellglass has no injector/private ABI/PDB/native-pipe code | shellglass repository search finds no adapter/injector/native protocol/Pipe/DIA references | verified |
| Ordinary unmodified hub | external mock E2E uses parent `shellglass.exe hub`; no WT-specific hub code exists | verified |
| Images, recording, SSH, broker reconnect, push/hub reconnect | `native/windows/test-e2e.ps1` completed: `native mock -> serve/SSH/image/recording + broker restart + push/hub reconnect: OK` | verified after split |
| Unknown builds/layouts fail closed | `native/windows/test-profile.ps1` completed all success + mismatch/stale-output checks; launcher exact-version switch; adapter exact profile validation | verified |
| x64 and retained ARM64 compile gates | x64 and ARM64 CMake Release builds completed in `target/` | verified |
| Operator launcher supports existing tabs | `start-wt-stream.ps1` defaults to lazy focus-transition recovery and uses `-NewTab` only on request; docs state DLLs are process-lifetime | implemented |
| Sandbox avoids host GPU virtualization | WT Sandbox scripts specify `<VGpu>Disable</VGpu>` after host bugcheck 0x119 from vGPU; both second-family aggregate/lifecycle and operator gates subsequently ran without a host crash | verified |

## Automated verification already completed after the split

- `cargo test --workspace`: 169 shellglass tests plus the vendored vt100 suite.
- `cargo test`: 14 broker/protocol/pipe tests.
- Shellglass and tap Clippy with all targets and `-D warnings`.
- Root feature matrix: `hub`, `serve`, `push`, `serve-api`, `push-api`,
  and no features, all with warnings denied.
- Viewer TypeScript type-check, 28 Node tests, rebuild, and committed-dist check.
- x64 native Release build and `test-profile.ps1`.
- ARM64 native compile gate.
- Native mock HTTP/SSH/image/recording/reconnect/push E2E.
- Primary WT aggregate+lifecycle in the persistent Sandbox using split artifacts
  copied byte-for-byte into the guest. Tested SHA-256 values:
  - adapter: `1ad85eae87d201db3b27627485d989463198372820a3118239e2df3a8ad4e035`;
  - tap executable: `055caa841754b53d43a0b32212c71f775c8b4f188cca8be8041046c1af905bf8`.
  Result: `target/wt-sandbox-e2e-23004/combined-result.json`; lazy pre-injection
  recovery proof: `snapshot-preexisting-recovered.json`.
- Exact WT `1.24.11321.0` aggregate and lifecycle under software rendering:
  `target/wt-sandbox-e2e-11040/{aggregate,lifecycle}-result.json`.
  Both passed, including lazy pre-injection recovery in
  `snapshot-preexisting-recovered.json`. The tested tap and adapter exactly match
  current builds: SHA-256 `0bb518436cb2eef094b291319e5619ac0802b4bb87676ad72898efdf423703a3`
  and `1ad85eae87d201db3b27627485d989463198372820a3118239e2df3a8ad4e035`.
- Migrated operator launcher under software rendering:
  `target/wt-sandbox-e2e-11040/operator-result.json` reports
  profiling, serve, injection, explicit fallback tab creation, and browser
  publication end-to-end.

## Uncovered completion gates

None. Final formatting, warning-denied checks, script parsing, diff hygiene, and
root/external boundary inspection were repeated after the runtime gates.
