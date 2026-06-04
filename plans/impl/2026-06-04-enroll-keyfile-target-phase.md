# Plan: replace `recheck: bool` with a phase enum in enroll keyfile validation

## Context

`validate_generated_keyfile_target` (`cli/src/enroll_key_file.rs`) is the
target-safety gate for `braid enroll --generate`: the keyfile's directory must
exist, be a directory, and be a mount point before braid writes key material
into it (otherwise a generated key could land on the host root fs). It runs
twice:

- in `plan_enroll`, during pre-passphrase planning, and
- in `EnrollPlan::execute` as a load-bearing TOCTOU re-check at the **mutation
  boundary**, immediately before `generate_key_file` (introduced in `de935a44`;
  the ordering is pinned by `cmd_generate_mountpoint_revoked_between_plan_and_write`).

Both passes run the identical check but need different mount-point failure
wording, because the caller's knowledge differs:

- **plan**: `keyfile directory is not a mount point: <dir> -- mount the USB
  device there before running braid enroll --generate` (operator never mounted it)
- **recheck**: `keyfile directory <dir> was a mount point at plan time but is no
  longer mounted -- ... remount and re-run braid enroll --generate` (it
  regressed mid-run: hot-unplug / automount timeout)

Today that choice is a `recheck: bool` parameter. The bool **gates no logic** --
it only selects the message -- so it reads as boolean blindness at the call
sites (`..., true` / `..., false` say nothing on their own), and the function
carries no `///` explaining the two-phase contract (violating braid's Doc
Comments rule). A review finding proposed pushing the message out to the callers
via a structured error; that is rejected here because it splits the two
contrasting messages ~150 lines apart and forces a reader to visit both callers
to see both -- the opposite of the intended readability win.

## Approach

Replace the boolean with a named 2-variant phase enum and document the helper.
Keep both messages co-located in the helper as an exhaustive `match`. **No
behavior change**: the two message strings are reproduced verbatim. (The plan
also tightens the execute-time test so the recheck message becomes byte-pinned
like the plan message already is -- see step 4.)

1. **Add a private enum** next to the existing `EnrollmentPlanMode`, mirroring
   its derives and doc style (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`,
   enum-level `///` + variant `///`s) but **not** its visibility -- this enum is
   used only by the private helper and its two in-file callers, so it stays
   module-private (`EnrollmentPlanMode` is `pub(crate)` only because `add.rs` and
   `replace.rs` reference it):

   ```rust
   /// Which validation pass is checking the generated-keyfile target.
   /// The mount-point requirement is identical across passes; only the
   /// failure wording differs, because the caller's prior knowledge differs.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   enum KeyfileTargetPhase {
       /// First check, in `plan_enroll`: no prior validation. A failure
       /// means the operator never mounted the USB device.
       Plan,
       /// Mutation-boundary re-check in `EnrollPlan::execute`, run right
       /// before `generate_key_file`. The plan already proved the target
       /// was mounted, so a failure here means it regressed mid-run
       /// (hot-unplug / automount timeout) -- the load-bearing TOCTOU guard.
       Recheck,
   }
   ```

2. **Change the helper signature** `recheck: bool` -> `phase: KeyfileTargetPhase`
   in `validate_generated_keyfile_target`. Replace the `if recheck { ... } else
   { ... }` message selection with `match phase { KeyfileTargetPhase::Recheck =>
   ..., KeyfileTargetPhase::Plan => ... }` (message strings unchanged). Add a `///` to the function capturing why it
   exists: the shared target-safety gate, run at both plan time and the mutation
   boundary, with the mount-point failure framed per phase.

3. **Update the two call sites:**
   - `plan_enroll` (the `params.generate` branch) -> `KeyfileTargetPhase::Plan`
   - `EnrollPlan::execute` (before `generate_key_file`) -> `KeyfileTargetPhase::Recheck`

4. **Tighten the execute-time test.**
   `cmd_generate_mountpoint_revoked_between_plan_and_write` currently asserts only
   `err.to_string().contains("was a mount point at plan time but is no longer
   mounted")`, leaving the message suffix unpinned. Change it to assert exact
   `err.to_string()` equality against the full recheck message:

   ```
   keyfile directory <dir> was a mount point at plan time but is no longer mounted -- the USB device may have been unmounted or disconnected during enrollment; remount and re-run braid enroll --generate
   ```

   Exact equality holds because the recheck `?` (`enroll_key_file.rs#EnrollPlan::execute`)
   returns the bare `EnrollKeyFileError::Validation` *before* `apply_enrollment`,
   so no `partial_generate_recovery_message` wrapping applies and
   `cmd_enroll_key_file` passes it through unchanged. This closes the
   suffix/remediation wording-drift gap and brings the recheck message to the
   same byte-pinned bar as the plan message. Leave that test's existing
   `MountpointCheck` ordering assertions untouched.

## Why this shape (braid-specific)

- **Mutation Safety Heuristics** (AGENTS.md): the primitive safety check belongs
  inside the helper fronting the unsafe op. The enum keeps the check *and* both
  legitimate failure framings in that owning layer, instead of leaking framing to
  callers (the finding's direction). Keeping the enum module-private reinforces
  this -- the phase distinction is an internal detail of this one helper, not a
  crate-wide concept.
- **House style**: mirrors the file's own `EnrollmentPlanMode` dispatch enum, so
  it reads as braid. The exhaustive `match` is future-proof -- a third phase
  fails to compile until handled.
- **Doc Comments rule**: satisfies the rule the function currently violates, and
  the doc comment is the natural home for the TOCTOU / mutation-boundary
  rationale that was causing reviewer confusion.

## Critical files

- `cli/src/enroll_key_file.rs` -- the only file changed: new enum, helper
  signature + `match`, function `///`, two call sites, and the step-4 test
  tightening (the test lives in this file's `#[cfg(test)]` module).

## Reuse

- `EnrollmentPlanMode` (`cli/src/enroll_key_file.rs#EnrollmentPlanMode`) -- copy
  its derives and enum/variant doc form. Do **not** copy its `pub(crate)`
  visibility: that exists for the cross-module callers in `add.rs`/`replace.rs`,
  which `KeyfileTargetPhase` has no equivalent of, so the new enum is private.

## Out of scope / explicitly rejected

- The finding's structured-error "caller formats the message" approach -- splits
  the two messages across callers; rejected.
- Doc-comment-only (keep the bool) -- leaves the boolean-blindness smell the
  finding correctly identified.
- No new tests and no message-string changes. One existing assertion is tightened
  (substring -> exact equality, step 4); that strengthens the regression net
  without changing behavior.

## Verification

Run `just test-rust`. The correctness signal:

- The **plan-message** tests stay green with no edits -- they already pin that
  string, so an unchanged pass proves the plan wording is untouched:
  - exact equality: `generate_rejects_plain_directory_before_luks_discovery`
  - substring: `generate_dry_run_rejects_plain_directory_without_key_creation`
- The **recheck-message** test `cmd_generate_mountpoint_revoked_between_plan_and_write`
  is tightened to exact equality (step 4) and must pass against the verbatim
  recheck string -- proving both that the wording is untouched and that the
  previously-unpinned suffix is now covered. That test's `MountpointCheck`
  ordering assertions are unchanged and must still pass.

No VM tests needed: this is a Rust-internal refactor with no tool-output parsing,
no systemd/module surface, and no behavior change.
