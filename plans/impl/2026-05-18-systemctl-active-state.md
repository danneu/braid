# Migrate systemd unit-state reads from `systemctl is-active` to `systemctl show -P ActiveState`

## Context

ADR 018 line 164 (`docs/decisions/018-systemd-lifecycle.md`) is categorical: **"Do NOT use `systemctl is-active`"**; the prescribed shape is `systemctl show -P ActiveState <unit>`. The rule's rationale is exit-code conflation (callers that key on exit code classify `activating`/`deactivating` as not-active and can deadlock when issuing `start`/`stop` inside a held-resource window).

Two callers in braid still use `CmdRequest::SystemctlIsActive`:

- `cli/src/doctor.rs:732-739` -- `read_braid_online_active_state`, doctor's UPS-safety check.
- `cli/src/tui/probe.rs:786-801` -- `probe_daemon_status`, TUI daemon state probe (used for `upsd.service` and `hddfancontrol-braid.service`).

Both parse `raw.stdout.trim()` and discard `exit_status`, so the literal safety hazard does not apply. But the contradiction with a load-bearing ADR rule forces every future reader to re-derive "doctor's use is actually safe because it reads stdout" -- a permanent cognitive tax. The pending review plan `plans/review/2026-04-28-rust-owned-pool-operation-lock.md:121` already proposes adding `SystemctlShowActiveState` and explicitly keeping `SystemctlIsActive` "for any caller that still wants the boolean check" -- but no caller wants the boolean check. That stance is a misjudgment we settle here.

Outcome: one canonical way to read systemd unit state in braid; ADR 018 line 164 applies uniformly with no exception. This implementation plan edits only code, tests, and fixture comments; it does not edit other plan files. Historical or pending plans that mention the old variant are out of scope for this refactor and must adapt to the code that exists when they are implemented.

## Behavioral guarantee

Migration is purely mechanical -- product behavior is unchanged:

- `is-active` stdout and `show -P ActiveState` emit the same vocabulary for every state braid classifies (`active`, `inactive`, `failed`, `activating`, `deactivating`, `reloading`, `refreshing`).
- Both existing callers ignore `exit_status` and switch on `stdout.trim()`; neither classifier (doctor's `classify_braid_online_active_state` or TUI's stdout match in `probe_daemon_status`) needs to change.
- No VM test asserts on doctor's internal systemctl invocation. `tests/module/braid-doctor-ups.py:79` uses `systemctl is-active --quiet braid-online.service` as a VM-state precondition independent of braid's CLI -- unaffected.

## Changes

### 1. Add the new `CmdRequest` variant and delete the old one -- `cli/src/cmd.rs`

- Replace the `SystemctlIsActive { unit: String }` variant at `cli/src/cmd.rs:317-322` with `SystemctlShowActiveState { unit: String }`. Update the doc comment: drop the "exits non-zero (3)" wording (no longer relevant); state that the variant reads the `ActiveState` property and emits one line on stdout.
- Replace the `to_argv` arm at `cli/src/cmd.rs:1100-1103` with one that produces `["show", "-P", "ActiveState", unit]`.
- Add an explicit argv-shape row to the `browse_read_only_command_variants_generate_expected_argv` table at `cli/src/cmd.rs:1586` (the table currently has no `SystemctlIsActive` row -- this is the only place that pins the argv shape):

  ```rust
  (
      CmdRequest::SystemctlShowActiveState {
          unit: "braid-online.service".into(),
      },
      "systemctl",
      vec!["show", "-P", "ActiveState", "braid-online.service"],
  ),
  ```

### 2. Migrate doctor -- `cli/src/doctor.rs`

- `read_braid_online_active_state` at `cli/src/doctor.rs:732-739`: change `CmdRequest::SystemctlIsActive { unit: "braid-online.service".into() }` to `CmdRequest::SystemctlShowActiveState { unit: "braid-online.service".into() }`. Body otherwise unchanged (still reads `raw.stdout.trim()`).
- `classify_braid_online_active_state` at `cli/src/doctor.rs:724-730`: no change.
- Test sites at `cli/src/doctor.rs:2747, 2786, 2821, 2855, 2891, 2932`: replace the `CmdRequest::SystemctlIsActive { ... }` literal in each `with_output` call with `CmdRequest::SystemctlShowActiveState { ... }`.

### 3. Migrate TUI probe + model -- `cli/src/tui/probe.rs`, `cli/src/tui/model.rs`

Production code:

- `probe_daemon_status` at `cli/src/tui/probe.rs:786-801`: change the `CmdRequest::SystemctlIsActive` literal to `CmdRequest::SystemctlShowActiveState`. The stdout `match` on words at lines 794-800 is unchanged.
- Doc comment at `cli/src/tui/probe.rs:782-785`: rewrite to describe `show -P ActiveState` semantics (emits one line, exits 0 for known units, callers parse the word).
- Doc comment at `cli/src/tui/probe.rs:442` (`probe_fan_for_tui`, "Daemon liveness via `systemctl is-active` ..."): replace `systemctl is-active` with `systemctl show -P ActiveState`.
- Doc comment at `cli/src/tui/probe.rs:727` (`UPS_DAEMON_UNIT`, "`systemctl is-active upsd.service` distinguishes ..."): replace with `systemctl show -P ActiveState upsd.service`.
- Doc comments on `DaemonStatus` enum at `cli/src/tui/model.rs:95` ("as reported by `systemctl is-active`") and `cli/src/tui/model.rs:104` ("Output from `systemctl is-active` didn't match ..."): replace both `systemctl is-active` mentions with `systemctl show -P ActiveState`.

Tests in `cli/src/tui/probe.rs`:

- Swap the `CmdRequest::SystemctlIsActive` variant literals to `SystemctlShowActiveState` at `:2688`, `:2736`, `:2796`.
- Update the `RawCommandOutput.cmd` string literals at `:2682` (inline `raw` helper inside `probe_daemon_status_parses_all_states`), `:2740` and `:2800` (`mock_with_upsc_and_unit` and the invocation-failure fallback test) from `"systemctl is-active ..."` to `"systemctl show -P ActiveState ..."` -- keep the unit name suffix where present.
- Rewrite the test preamble at `cli/src/tui/probe.rs:2669-2676` (the comment above `probe_daemon_status_parses_all_states`): the current "Why" justifies defending against `is-active`'s non-zero-exit trap; after migration `show -P ActiveState` exits 0 for known units, so the "Why" becomes "callers still parse stdout regardless of exit code, defending against any future systemctl exit-code change." The fixture table at `:2691-2701` may keep its mixed exit-code rows as defensive coverage -- callers ignore exit_status either way, and removing the rows would weaken the contract that state classification is stdout-only.

### 4. Update the doctor test fixture helper -- `cli/src/test_fixtures/doctor.rs`, `cli/src/test_fixtures.rs`

- Rename `systemctl_is_active_output` at `cli/src/test_fixtures/doctor.rs:222-236` to `systemctl_show_active_state_output`.
- Update the `cmd` field literal from `"systemctl is-active braid-online.service"` to `"systemctl show -P ActiveState braid-online.service"`.
- Drop the `exit_status` branching (the `match state` block at lines 231-234) and set `exit_status: 0` unconditionally -- `show -P` exits 0 for any known unit regardless of state.
- Update the helper's doc comment at `cli/src/test_fixtures/doctor.rs:219-221` to reflect the new variant name.
- Update every call site of the renamed helper in `cli/src/doctor.rs` (the six test sites above all call `systemctl_is_active_output(...)`).
- Update the `cli/src/test_fixtures.rs` re-export from `systemctl_is_active_output` to `systemctl_show_active_state_output`.

### 5. ADR cross-reference -- no edit needed

ADR 018 line 164 already prescribes `systemctl show -P ActiveState`. After this refactor the rule is honored without exception; no ADR text changes.

## Files modified

- `cli/src/cmd.rs` (variant definition, `to_argv` mapping, new argv-shape row in `browse_read_only_command_variants_generate_expected_argv`).
- `cli/src/doctor.rs` (one production call site, six test call sites).
- `cli/src/tui/probe.rs` (one production call site, three doc comments, three test call sites, three `RawCommandOutput.cmd` strings, one test preamble).
- `cli/src/tui/model.rs` (two `DaemonStatus` enum doc comments).
- `cli/src/test_fixtures/doctor.rs` (helper rename + body simplification).
- `cli/src/test_fixtures.rs` (renamed fixture helper re-export).

No NixOS module, VM test, README, or other plan/doc file needs to change.

## Verification

Run in order; all must pass:

1. `just test-rust` -- the new argv-shape row in `browse_read_only_command_variants_generate_expected_argv` (section 1) pins `SystemctlShowActiveState`'s mapping; the doctor tests (six) and TUI probe tests (three) exercise the migrated call sites and the renamed fixture helper.
2. `just test-parsers` -- canary the parser surface against live tool output in VMs. No parser is touched, so this is regression coverage only.
3. `just test-vm braid-doctor-ups` -- exercises `check_braid_online_active_when_mounted` end-to-end. The VM test's `systemctl is-active --quiet` precondition at `tests/module/braid-doctor-ups.py:79` is independent of braid's CLI and continues to work; the doctor assertions at lines 85-87 still hold because product behavior is unchanged.

After verification, run the targeted search gate -- it must return zero hits:

```sh
rg 'SystemctlIsActive|systemctl is-active' cli/ modules/
```

This proves the CLI surface (production, tests, fixtures, doc comments) and the NixOS module are fully migrated.

Out of scope for this gate -- references that legitimately survive and are not searched:

- `docs/decisions/018-systemd-lifecycle.md:164` -- the ADR prescription naming `systemctl is-active` as the forbidden form. Must stay.
- `tests/module/*.py` -- VM-side scripts invoke `systemctl` directly inside the test VM; unrelated to the CLI's `CmdRequest` surface.
- `plans/` -- plan files are not implementation targets for this refactor. This plan file mentions both strings in the Context section by design; historical and pending plans may also mention the old command until their own revision or implementation cycle.
