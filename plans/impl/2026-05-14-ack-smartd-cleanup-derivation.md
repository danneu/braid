# Plan: collapse duplicated smartd-cleanup derivation in ack

## Context

`cli/src/ack.rs` currently derives the same three values in two arms of
`cmd_ack_impl`:

1. `causes: &[AlertCause]` -- borrowed slice of the entry-snapshotted latch
2. `latch_had_smartd` -- `causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert))`
3. `remove_smartd` -- `smartd_active || latch_had_smartd`

Both derivations live at `cli/src/ack.rs:42-46` + `:83-84` (mounted arm)
and `:108-111` + `:151-152` (offline arm). They are byte-identical.

The duplication was introduced this week across two refactors:

- `5298407 fix(cli): snapshot smartd state during ack` -- added
  `latch_had_smartd`/`remove_smartd` to both arms when the cleanup helper
  gained the `remove_smartd: bool` parameter.
- `82a553f refactor(cli): collapse ack latch cause access` -- added the
  parallel `causes` slice access to both arms. The commit message says it
  was kept "without threading borrows across the offline handoff or
  changing ack behavior" -- an intentionally tight refactor that left the
  next step open.

The risk is drift: the SmartdAlert-cleanup-exception policy (clear the
flag even if it arrived after entry, when a SmartdAlert cause was already
latched at entry) is the same decision in both arms. A future edit that
adjusts the policy (filter out a new cause variant, add a new
"active for ack" condition, change the snapshot semantics) has to be
applied identically in two places or the mounted and offline branches
will silently disagree. The existing test pair
(`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag` at
`cli/src/ack.rs:546`, `ack_offline_with_smartd_latch_cleans_mid_probe_smartd_flag`
at `:520`) was written to defend exactly this invariant and already pins
both arms, so drift would be caught -- but only if both tests stay
present and both keep covering the same property.

Outcome: derive `causes`, `latch_had_smartd`, and `remove_smartd` once
at entry in `cmd_ack_impl`, threaded into `ack_offline` as the bool
`remove_smartd` plus the `causes` slice. The SmartdAlert-cleanup policy
collapses to a single expression; behavior, tests, and error paths are
unchanged.

## Recommended approach

Single-file change in `cli/src/ack.rs`. No public API change -- both
`cmd_ack_impl` and `ack_offline` are private to the module.

### Step 1: Compute the derived values once in `cmd_ack_impl`

After the existing entry snapshot of `latch_state`, `latch_corrupt`, and
`smartd_active` (`cli/src/ack.rs:34-46`), keep the `causes` slice access
and add the two derivations next to it:

```rust
let causes: &[AlertCause] = latch_state
    .as_ref()
    .map(|s| s.causes.as_slice())
    .unwrap_or(&[]);
let smartd_active = alert::smartd_alert_active(paths);
let latch_had_smartd = causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert));
let remove_smartd = smartd_active || latch_had_smartd;
```

This is semantics-preserving: `latch_had_smartd` is a pure function of
`causes`, which was already entry-snapshotted; `remove_smartd` is a pure
function of `smartd_active` (already entry-snapshotted) and
`latch_had_smartd`. Both new lines sit alongside the rest of the entry
snapshot, which matches the existing comment block at
`cli/src/ack.rs:26-33` ("Snapshot the gating inputs ... before probing
the pool").

In the mounted branch, drop the now-redundant local derivations at
`cli/src/ack.rs:83-84` and pass `remove_smartd` directly to
`cleanup_alert_files_and_beeper`.

### Step 2: Change `ack_offline`'s signature

Replace `latch_state: Option<AlertState>` with `causes: &[AlertCause]`,
and add `remove_smartd: bool`. The full signature becomes:

```rust
fn ack_offline(
    causes: &[AlertCause],
    latch_corrupt: bool,
    smartd_active: bool,
    remove_smartd: bool,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
```

Inside the body:

- Drop the `causes` derivation at `cli/src/ack.rs:108-111` -- it is now
  the parameter.
- Drop the `latch_had_smartd`/`remove_smartd` derivation at
  `cli/src/ack.rs:151-152` -- `remove_smartd` is now the parameter.
- All four existing uses of `causes` (`has_alert` at `:113`,
  `BtrfsDeviceErrors` refusal at `:123-128`, `missing_devids` extraction
  at `:135-141`, the `SmartdAlert` check that is removed in this step)
  continue to work against the borrowed slice with no changes other than
  losing the SmartdAlert check.
- `smartd_active` stays as a parameter because it is still needed for the
  `has_alert` gate at `:113`.

Lifetime: `latch_state` lives in `cmd_ack_impl` and outlives the
`ack_offline` call, so borrowing `causes` across the call is safe.
`latch_state` is no longer needed inside `ack_offline` and can be dropped
from the call site at `cli/src/ack.rs:55-62`.

### Step 3: Update the cleanup helper docstring

The docstring on `cleanup_alert_files_and_beeper` at
`cli/src/ack.rs:160-177` currently says "Callers compute `remove_smartd`
as `smartd_active || latch_had_smartd` from inputs snapshotted at entry."
After this refactor there is one computation site (in `cmd_ack_impl`),
not two callers each computing it. Tighten the wording to reflect the
single derivation site -- one short sentence, no expansion.

### Step 4: Update the stale mounted-test preamble

The "Why it exists" line in the preamble for
`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag` at
`cli/src/ack.rs:540` currently reads:

> The mounted path computes its own cleanup decision; this pins the
> same crash-recovery exception as the offline path.

After this refactor the mounted path no longer computes its own
decision -- both call sites read the single `remove_smartd` value
derived once in `cmd_ack_impl`. Rewrite that line to say the mounted
branch must apply the shared entry-snapshotted SmartdAlert cleanup
decision, mirroring the offline preamble's "must not regress to
`remove_smartd = smartd_active`" framing at `cli/src/ack.rs:513-515`.
Leave the Intent and Scenario lines and the test body unchanged.

Sibling preambles to leave as-is:

- `cli/src/ack.rs:484` ("Offline ack has a separate cleanup call site
  from the mounted path; both must honor the same snapshot-scoped
  smartd rule") -- still accurate. Each branch still invokes
  `cleanup_alert_files_and_beeper` from its own call site; only the
  bool argument is derived once.
- `cli/src/ack.rs:831` ("cmd_ack_impl and ack_offline have separate
  cleanup call sites") -- still accurate for the same reason.

### What does not change

- `cmd_ack` public signature, `AckError` variants, all printed messages,
  all file-system effects.
- Cleanup short-circuiting on the first non-NotFound IO error.
- Test bodies. The mounted/offline SmartdAlert-latch pair, the
  cleanup-failure pair, the corrupt-latch and foreign-fstype tests, and
  the no-alert no-op tests all go through `cmd_ack`/`cmd_ack_impl` and
  observe the same effects. One test preamble at `cli/src/ack.rs:540`
  is rewritten in Step 4; no other test text changes.

## Critical files

- `cli/src/ack.rs` -- the only file modified.
  - `cmd_ack_impl` (`:19-99`): add two derivations to the entry
    snapshot; drop the redundant locals at `:83-84`; update the
    `ack_offline` call site at `:55-62`.
  - `ack_offline` (`:101-158`): change signature; drop `causes`
    derivation at `:108-111`; drop `latch_had_smartd`/`remove_smartd`
    derivation at `:151-152`.
  - `cleanup_alert_files_and_beeper` (`:178-190`): docstring update
    only; body unchanged.
  - Test preamble at `:538-544`
    (`cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag`):
    rewrite the "Why it exists" line per Step 4; test body unchanged.

## Reused utilities

Nothing new. The refactor relies entirely on existing types and
functions:

- `crate::alert::AlertCause`, `crate::alert::AlertState` -- already
  imported at `cli/src/ack.rs:1-3`.
- `crate::alert::smartd_alert_active`, `crate::alert::load_alert_latch`,
  `crate::alert::remove_smartd_alert_flag`,
  `crate::alert::remove_alert_latch`,
  `crate::alert::remove_alert_latch_corrupt`,
  `crate::alert::load_acked_stats_fallible`,
  `crate::alert::save_acked_stats` -- already used by this file.

## Verification

- `just test-rust` -- covers all the in-file unit tests, including the
  invariant pair this refactor unifies:
    - `cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag`
      (`cli/src/ack.rs:546`)
    - `ack_offline_with_smartd_latch_cleans_mid_probe_smartd_flag`
      (`cli/src/ack.rs:520`)
  and the cleanup-failure pair that exercises the CleanupFailed branch in
  both arms:
    - `cmd_ack_returns_cleanup_failed_when_remove_smartd_alert_errors_after_baseline_saved`
      (`cli/src/ack.rs:588`)
    - `ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`
      (`cli/src/ack.rs:841`)
- `cargo check -p braid-cli` (already covered by `just test-rust`) --
  surfaces any lifetime or signature issue from changing `ack_offline`'s
  parameters.

No NixOS VM tests are needed: the change is internal to a private helper
and observable behavior is unchanged. Both mounted and offline ack
flows are covered end-to-end by the Rust unit tests above.
