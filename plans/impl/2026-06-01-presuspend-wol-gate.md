# Pre-suspend Wake-on-LAN gate for autosuspend

Date: 2026-06-01
Status: Implementation plan
(Conventional name: `2026-06-01-presuspend-wol-gate.md` -- written to the
plan-mode-assigned path.)

## Context

The doctor `wake_on_lan` check that landed in `c1ef5f2` verifies runtime WoL
state, but only **on demand**: it helps only if the operator runs
`sudo braid doctor`. The impl plan's Follow Up asked for the durable version --
something that re-checks WoL every suspend cycle and refuses to let the NAS
sleep into an unreachable state.

The real failure it closes: `braid.autoSuspend` makes WoL mandatory at build
time (`wolInterface` assertion), but the live NIC can still report `Wake-on: d`
-- BIOS ErP/Deep Sleep, a driver default, or a driver that resets WoL on resume
(RTL8125; see `docs/guides/power-management.md`). When that happens, autosuspend
sleeps the box after `idleTime` and it is unreachable until someone physically
presses the power button.

**Scope decision (settled with the user):** implement **only the autosuspend
gate (Layer 1)** -- a hidden `braid wol-ready` command wired as an
`ExternalCommand` activity check, mirroring `braid idle`. The invariant:

> `braid.autoSuspend` will not automatically suspend the NAS unless
> `braid.autoSuspend.wolInterface` currently reports `Wake-on: g`.

This matches braid's ownership boundary: braid owns and configures autosuspend,
so it guarantees *that* path is safe, while leaving manual `systemctl suspend`
available as an admin escape hatch (testing, maintenance, deliberately
suspending a physically-accessible machine).

**Explicitly deferred (see Out of scope):** the universal `sleep.target` systemd
gate that would also block manual suspend, and re-arming WoL on resume.

This scope is also the cheapest: it adds **no new systemd unit** (so ADR-018 is
untouched) and **no new pinned tool, parser, or fixture** (ethtool was pinned
and its `Wake-on:` parser classified last commit). It only adds a CLI command,
one autosuspend check, tests, and docs.

## Design

### 1. Shared WoL classifier (no drift with doctor)

Extract the pure ethtool-output classification into a new `cli/src/wol.rs` so
`doctor` and the new command cannot diverge -- the same pattern `ExclusiveOp`
uses across `idle.rs` and `preflight.rs`.

- Move `ethtool_field` and `wol_modes_parseable` from `doctor.rs` into `wol.rs`
  as `pub(crate)`.
- Add a pure classifier:
  ```rust
  /// Tri-state WoL readiness derived from one `ethtool <iface>` invocation.
  /// Shared by the doctor check and the autosuspend `wol-ready` gate so the
  /// two cannot disagree about what counts as "magic-packet armed".
  pub(crate) enum WolReadiness {
      Armed { active: String },
      Disabled { active: String, supports: String }, // supports g, active lacks g
      Unsupported { supports: String },              // supports lacks g
      Unparseable,                                    // exit 0 but no parseable lines
      QueryFailed { exit: i32, detail: String },      // ethtool non-zero
  }
  pub(crate) fn classify_wol(stdout: &str, stderr: &str, exit: i32) -> WolReadiness
  ```
- `doctor::summarize_wol(iface, stdout, stderr, exit) -> CheckResult` keeps its
  **exact current signature and messages** -- it becomes a thin
  `match classify_wol(...)` that renders the same strings. This keeps every
  existing doctor WoL unit test (`cli/src/doctor.rs` tests ~5528-5701) green
  unchanged, and `check_wake_on_lan` keeps its own `runner.run` + spawn-error
  arm as-is.

Reuse: `CmdRequest::EthtoolShow { interface }` (already exists),
`config.auto_suspend()` -> `AutoSuspend { wol_interface }`
(`cli/src/config.rs:45-49,103-105`).

### 2. Hidden `braid wol-ready` command

In `wol.rs`, alongside the classifier (mirrors `idle.rs` housing both
`cmd_idle` and `IdleResult`):
```rust
/// Autosuspend gate: confirms the configured WoL interface is armed before
/// braid's automatic suspend path is allowed to proceed. Hidden, exit-code
/// driven, fed by the shared `classify_wol`.
pub fn cmd_wol_ready<R: CommandRunner>(runner: &R, auto: Option<&AutoSuspend>) -> WolReadyOutcome
pub enum WolReadyOutcome { Armed, NotReady(String), SetupError(String) }
```
- `auto == None` -> `SetupError` (autosuspend not configured; defensive --
  the check only exists when `autoSuspend.enable`).
- run `EthtoolShow`; spawn `Err` -> `NotReady` ("cannot verify"); on `Ok`,
  `classify_wol`: `Armed` -> `Armed`, everything else -> `NotReady(reason)`.

`cli/src/main.rs`:
- Add to `Commands` (mirror the hidden scrub commands at lines 62-71):
  ```rust
  /// Internal: autosuspend WoL gate. exit 0 = Wake-on: g armed,
  /// exit 1 = not armed / unverifiable, exit 2 = setup/config error.
  /// Hidden from `braid --help`.
  #[command(hide = true)]
  WolReady,
  ```
- Add `WolReady` to the `LockPolicy::None` group (lines 176-184) -- read-only,
  no pool lock, no `Filesystem`.
- Dispatch arm near `Idle` (~786): `load_config_or_exit(path, 2)`, then map
  `cmd_wol_ready(&RealRunner, config.auto_suspend())`:
  `Armed` -> print + `exit(0)`; `NotReady(r)` -> print + `exit(1)`;
  `SetupError(r)` -> print + `exit(2)`.
- Register `pub mod wol;` in `cli/src/lib.rs`.

Exit-code contract (matches `braid idle`, ADR-016): `0` armed, `1` not
armed/unverifiable, `2` setup error.

### 3. Autosuspend `BraidWol` check

In `modules/braid/auto-suspend.nix`, add a sibling to `BraidPool` (lines 77-87)
inside the `checks` mkMerge, using the identical fail-closed inversion idiom:
```nix
# Block autosuspend-initiated sleep unless the configured NIC reports
# Wake-on: g. Inverted like BraidPool: `braid wol-ready` exit 0 (armed)
# -> `!` -> 1 -> autosuspend allows suspend; any non-zero (not armed,
# unverifiable, setup error, or `timeout` overrun) -> `!` -> 0 -> activity
# -> block suspend. Fail-closed per docs/design/decisions/016-auto-suspend.md.
# `timeout` lives inside bash so its overrun result is inverted by `!`.
BraidWol = {
  class = "ExternalCommand";
  command = "${pkgs.bash}/bin/bash -c '! ${pkgs.coreutils}/bin/timeout -k 2 10 ${braidWrapped}/bin/braid wol-ready'";
};
```
No `cli.nix` change: `auto_suspend.wol_interface` is already emitted into
`config.json`, and `braidWrapped`'s toolPath already includes
`cfg.packages.ethtool` (both landed last commit), so `wol-ready` resolves the
pinned ethtool through the wrapper. The check is intentionally
pool-state-independent (no `ConditionPathIsMountPoint`): an *offline* pool that
autosuspends still needs WoL to be wakeable.

## Files to modify

- `cli/src/wol.rs` (new) -- `WolReadiness`, `classify_wol`, `ethtool_field`,
  `wol_modes_parseable`, `WolReadyOutcome`, `cmd_wol_ready`, unit tests.
- `cli/src/doctor.rs` -- `summarize_wol` becomes a thin map over
  `wol::classify_wol`; drop the two moved helpers (import from `wol`).
- `cli/src/main.rs` -- `WolReady` command variant, `LockPolicy::None` arm,
  dispatch arm.
- `cli/src/lib.rs` -- `pub mod wol;`.
- `modules/braid/auto-suspend.nix` -- `BraidWol` check.
- `tests/module/braid-auto-suspend.{nix,py}` -- see Testing.
- Docs -- see Docs.

Per AGENTS.md "Doc Comments": each new `pub(crate)`/`pub` item in `wol.rs` gets
a `///` justifying-why comment.

## Testing

Same constraint as the doctor check: VM virtio NICs cannot do real WoL, so the
supported seam is the **package-override fake ethtool** that already exists in
`tests/module/braid-auto-suspend.nix` (a `writeShellScriptBin "ethtool"` that
prints `Wake-on: g`/`d` from `/tmp/braid-wol-mode`).

1. **Unit (`cli/src/wol.rs`)** -- primary, behavioral, structure-insensitive.
   `classify_wol` per branch: `Wake-on: g` and multi-flag `Wake-on: ug` ->
   `Armed`; `Wake-on: d` + `Supports: pumbg` -> `Disabled`; `Supports: d` ->
   `Unsupported`; exit 0 + garbage -> `Unparseable`; non-zero -> `QueryFailed`.
   `cmd_wol_ready` via `MockRunner`: armed -> `Armed`; disabled/unsupported/
   unparseable/non-zero/spawn-error -> `NotReady`; `auto = None` ->
   `SetupError`. Each test gets the Intent/Why/Scenario preamble.
   The multi-flag `ug` case is the regression guard against a naive
   `grep 'Wake-on: g'` reimplementation (substring traps on the `Supports`
   line and misses `ug`).
2. **VM (`braid-auto-suspend.py`)** -- extend the existing config-inspection
   subtests (which already assert `[check.BraidPool]` and the
   `timeout -k 2 10` + store-path shape):
   - assert `[check.BraidWol]` exists, references `braid wol-ready`, uses
     `/nix/store/` paths, `bin/timeout -k 2 10`, and `bin/bash`;
   - using the fake ethtool: `printf d > /tmp/braid-wol-mode` then assert
     `braid wol-ready` (through the wrapper) exits non-zero; `g` -> exit 0;
   - mirror the existing BraidPool hang-stub overrun test for the `BraidWol`
     command string (substitute a hang stub for `braid wol-ready`, assert the
     inner `timeout` fires and `!` inverts to 0 -- block -- before the outer
     watchdog).
3. No new fixtures (ethtool is hand-authored / no-live-capture; the parser is
   unchanged, only relocated).

## Docs

- `docs/design/decisions/016-auto-suspend.md` -- document the `BraidWol`
  `ExternalCommand` check beside the `braid idle` one, state the invariant
  above, extend the exit-code bridge table for `wol-ready`, and note explicitly
  that this gates **only** the autosuspend path (ownership boundary) and that
  the universal `sleep.target` gate was considered and deferred.
- `docs/guides/power-management.md` -- note that with `autoSuspend` enabled,
  braid now blocks idle auto-suspend when WoL is not armed (so "my NAS won't
  sleep" -> run `sudo braid doctor`).
- No ADR-010 / principles / AGENTS.md parser-contract changes: ethtool is
  already a pinned, classified tool; `wol-ready` reuses the same parser.
- No command-reference page: `wol-ready` is hidden, like the scrub internals.

## Out of scope (tracked follow-ups)

- **Universal `sleep.target` gate.** A `RequiredBy=sleep.target` +
  `Before=sleep.target` oneshot running `braid wol-ready` *does* reliably abort
  any suspend (verified: logind starts `suspend.target` with job mode
  `replace-irreversibly`, which honors dependencies; `systemd-suspend.service`
  `Requires=sleep.target`). Deferred because it broadens the claim from "braid
  won't auto-suspend unsafely" to "this machine may not suspend at all," which
  is a bigger ownership claim and more surprising operationally.
- **Re-arm WoL on resume.** A `post` system-sleep hook running
  `ethtool -s <iface> wol g` would fix reset-on-resume drivers (RTL8125) so the
  box keeps sleeping. Deferred: it introduces runtime NIC mutation (a new class
  of behavior). Consequence to document: with the gate alone, a reset-on-resume
  NIC keeps the box awake after the first wake (visible + diagnosable via
  `braid doctor`) rather than stranding it -- the safe, degraded direction.

## Verification

- `just test-rust` -- new `wol.rs` unit tests pass; doctor WoL tests still green.
- `just test-vm braid-auto-suspend` -- focused VM run for the new `BraidWol`
  assertions and the `wol-ready` exit codes through the wrapper.
- Blast radius is the autosuspend module + CLI dispatch; a focused run is
  appropriate. Hand back to the user for a full-suite rerun rather than running
  it autonomously.

## Follow Up

- Decide whether to add a universal `sleep.target` gate for manual suspend; `docs/design/decisions/016-auto-suspend.md` currently records that broader ownership claim as deferred.
- Decide whether to re-arm WoL on resume with a system-sleep hook; `docs/design/decisions/016-auto-suspend.md` currently records the gate-only behavior as the safe degraded path.
