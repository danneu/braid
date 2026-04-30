# Extract All-Disk Credential Verification Helper (Lean)

## Context

Today, the commands that trust one passphrase or keyfile across multiple LUKS
disks have inconsistent and incomplete preflight verification:

- **mount/unlock/recover**: verify only the first disk in `plan.to_unlock`,
  then loop and open every disk. A divergent slot-0 on disk2 lets disk1's
  mapper open before disk2's open fails, leaving a stranded
  `/dev/mapper/braid-*` mapper behind.
- **add**: verifies only `pool.devices.first()`, and only when a fresh-format
  candidate exists (`cli/src/add.rs:418-442`). A divergent slot-0 on a
  non-first live pool member is silently missed at preflight. An
  already-open `PresentLuks` candidate's slot-0 is never checked against the
  user-supplied passphrase, so an `add` can succeed against a candidate whose
  slot-0 differs from the rest of the pool, breaking the single-passphrase
  invariant for future mount/unlock.
- **replace**: verifies only `pool.devices.first()` for fresh replacements
  (`cli/src/replace.rs:188-212`) and the new disk only when it is closed
  `PresentLuks` (`:220-237`). Non-first live member slot-0 divergence is
  silently missed; an already-open new disk is never verified against its
  own slot-0.
- **enroll**: already verifies every candidate, but via two duplicated paths
  (`verify_first_candidate_passphrase` at `cli/src/enroll_key_file.rs:117`
  and an inline `if i > 0` block at `:208-219`).

Net effect: divergent non-first slot-0s are missed at preflight in
mount/unlock/add/replace, and `enroll`'s duplicated paths have drifted in
wording and emit seam.

**Intended outcome.** One shared helper that verifies a credential against
*every* relevant disk before any mutation or open, used by all five
callsites, with command-specific error wrapping kept at the call site. The
helper widens preflight coverage; it does not restructure existing
validation phases. Where the widened verify can race ahead of an existing
identity/label check on the same invocation (only `add` mixes these), we
accept that the credential error may now win over the identity error in
that mixed case -- today's identity-first ordering is an implementation
accident of the narrow first-disk verify, not a documented invariant.
Surfacing a divergent slot-0 (a latent integrity bug in the existing pool)
earlier is the more useful UX.

## Helper

New file `cli/src/credential_verify.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVerifyTarget {
    pub name: String,
    pub device: String,
}

#[derive(Debug, Clone, Copy)]
pub enum Credential<'a> {
    Passphrase(&'a str),
    KeyFile(&'a Path),
}

#[derive(Debug)]
pub enum CredentialVerifyError {
    Rejected { target: CredentialVerifyTarget },
    Luks { target: CredentialVerifyTarget, source: luks::LuksError },
}

pub fn verify_credential_for_targets<R: CommandRunner>(
    runner: &R,
    targets: &[CredentialVerifyTarget],
    credential: Credential<'_>,
    color_enabled: bool,
    mut emit: impl FnMut(&str),
) -> Result<(), CredentialVerifyError>;
```

One loop over `targets`. The credential `kind` for the wait line is
derived from the `Credential` variant (`Passphrase` -> `CredentialKind::Passphrase`,
`KeyFile` -> `CredentialKind::KeyFile`). For each target, in
caller-provided order:

1. Build the wait line via `credential_wait_line(kind, color_enabled, &target.name)`
   (`cli/src/status_tag.rs:66`); pass the rendered string to `emit`
   **before** dispatching the verify call (so the failing target's wait
   line is emitted even if the verify returns `Rejected`/`Err`).
2. Dispatch on the `credential` variant:
   - `Credential::Passphrase(p)` calls
     `luks::verify_passphrase(runner, &target.device, p)`
     (`cli/src/luks.rs:410`).
   - `Credential::KeyFile(path)` calls
     `luks::verify_key_file(runner, &target.device, path)`
     (`cli/src/luks.rs:625`).
3. `VerifyOutcome::Authenticated` -> continue.
4. `VerifyOutcome::Rejected` -> return
   `Err(Rejected { target: target.clone() })`. Stop; do not verify
   later targets.
5. `Err(LuksError)` -> return
   `Err(Luks { target: target.clone(), source: e })`.

`Credential<'a>` derives `Clone, Copy` so the enum can be inspected for
its kind once (to render the wait line) and then matched again inside
the loop body without consuming it. Empty target list returns `Ok(())`
and the emit closure is never invoked. The helper does not do
header-state diagnosis, command-specific error wrapping, or
post-success activation -- all that stays at the callsite.

## Callsite Migrations

### mount / unlock / recover

`cli/src/mount.rs:490` (`open_disks_with_passphrase`) and `cli/src/mount.rs:638`
(`OpenCredential::KeyFile` arm in `execute_unlock_and_mount`).

Today both arms verify only `plan.to_unlock[0]`, then loop and open every
disk. Change: build a `Vec<CredentialVerifyTarget>` from `plan.to_unlock`
(every entry, in order; `name` from the first tuple element, `device` from
`by_id.0`), call `verify_credential_for_targets` with the matching
`Credential` variant inside an explicit `match` (no blanket `?`),
then run the existing per-disk open loop (`cli/src/mount.rs:532-574`):

```rust
match verify_credential_for_targets(
    runner, &targets, Credential::Passphrase(&passphrase), // or Credential::KeyFile(kf)
    color_enabled, |line| eprint!("{line}"),
) {
    Ok(()) => {}
    Err(CredentialVerifyError::Rejected { target }) => {
        // Route through explain_open_failure -- see "Helper rejection" below.
    }
    Err(CredentialVerifyError::Luks { target, source: e @ LuksError::OpenFailed { .. } }) => {
        // Route through explain_open_failure -- see "Helper Luks error" below.
    }
    Err(CredentialVerifyError::Luks { source, .. }) => {
        return Err(MountError::Luks(source));
    }
}
```

**Do not implement `From<CredentialVerifyError> for MountError`** -- mount
needs header-state diagnosis on `Rejected` and `OpenFailed`, which a
blanket `From` cannot express. Same rule applies at every other
callsite: each maps `CredentialVerifyError` explicitly using its own
wording.

`explain_open_failure` (`cli/src/mount.rs:459`) stays, used at two layers:

- **Helper rejection.** On `CredentialVerifyError::Rejected { target }`,
  probe the failing target's header and route through
  `explain_open_failure`. The first-disk wording becomes per-target,
  with separate strings per credential variant:
  - Passphrase: `original_summary = "passphrase rejected on '{name}'"`,
    `ok_fallback = MountError::Failed("wrong passphrase (rejected by {name})")`.
  - Keyfile: `original_summary = "keyfile rejected on '{name}'"`,
    `ok_fallback = MountError::Failed("wrong keyfile (rejected by {name})")`.

  The credential variant under verification is known at the callsite
  (the arm matched on `OpenCredential`), so wiring the right wording is
  a per-arm decision -- not a method on `CredentialVerifyError`. **Do
  not** parameterize the wording by introspecting `Credential` inside
  the helper; keep the helper free of user-facing strings.
- **Helper Luks error.** On `CredentialVerifyError::Luks { target, source }`:
  - `source = LuksError::OpenFailed { .. }` -> probe header, route through
    `explain_open_failure` (mirrors today's
    `Err(e @ LuksError::OpenFailed { .. })` arm at `cli/src/mount.rs:501`,
    `:644`). A wiped/damaged header on a non-first verify must still
    surface restore/repair guidance.
  - any other `LuksError` variant -> `return Err(MountError::Luks(source))`
    directly. Mirrors today's `Err(e) => return Err(e.into())` arm. Spawn
    failures and command-shape errors must not be re-narrated as
    header-state recovery wording.
- **Post-verify open rejection.** The wording at `cli/src/mount.rs:543-551`
  ("verified passphrase on '{first_name}' but rejected here") changes to:

  ```
  cryptsetup open rejected on '{name}' even though the {credential} was just
  verified against every planned disk. The credential likely changed between
  preflight and open (race or external LUKS manipulation).
  ```

  Non-auth (`OpenFailed` other-than-2) and probe-failed branches keep their
  current wording -- those paths are not about credential validity.

The keyfile arm (currently inline at `cli/src/mount.rs:638-`) becomes
near-identical scaffolding to the passphrase arm; extract it into
`open_disks_with_key_file` for symmetry.

### add

`cli/src/add.rs:418-442` is today's `pool.devices.first()` block, gated on
`any_needs_format`.

Replace it with one widened verify call up front, before Pass 1
(`:444-482`). Pass 1 is **not restructured** -- `validate_braid_preconditions`
at `:457`, `ensure_luks_open` at `:461`, and `classify_braid_disk_fsid` at
`:466` all stay where they are.

**Targets.** Build a single ordered list:

1. Every entry in `self.pool.devices` -- map to
   `CredentialVerifyTarget { name: name_from_mapper(&d.mapper.0).unwrap_or(...).into(), device: d.underlying.clone() }`.
   `PoolDevice.underlying` (`cli/src/types.rs:106`) is the raw device path,
   so this drops today's `CryptsetupStatus { mapper }` round-trip.
2. Every `PresentLuks` candidate (regardless of `mapper_open`) -- map to
   `CredentialVerifyTarget { name: self.names[i].clone(), device: self.by_ids[i].0.clone() }`.
   `PresentNotLuks` candidates contribute no target (no slot-0 to verify).

Verifying an already-open candidate is safe: cryptsetup's
`--test-passphrase` path sets `activated_name = NULL`
(`reference/cryptsetup/src/cryptsetup.c:1774`), so the existing dm-crypt
mapper for the open device is untouched.

**Trigger.** The `:373` `steps.is_empty()` short-circuit fires first
and is independent of the helper -- already-in-pool-only invocations
(empty step list) skip passphrase read and the helper entirely (see
Position below). On the work-doing path, build the target list and
call the helper only when the post-`:373` target list is non-empty.
The pure fresh-format bootstrap case (no live pool, all candidates
`PresentNotLuks`) has a non-empty step list but an empty target list
-- no live members and no `PresentLuks` candidates contribute targets
-- so the helper is a no-op there and `confirm_new` (gated at `:410`)
remains the only credential safety net for bootstrap, matching today.

**Position.** Inside `AddPlan::execute`, at the same point as today's
first-disk verify (`cli/src/add.rs:418-442`) -- after notes rendering,
after the `:373` no-op short-circuit, after the confirmation prompt,
and after `read_passphrase_with` at `:411`. Unrelated validation
errors (name conflicts, missing journal dir, etc.) still surface in
the same order they do today (memory: preserve-error-precedence).

Crucially, the `:373` short-circuit (`if self.steps.is_empty()`) fires
**before** the verify, exactly as today. Already-in-pool-only invocations
that produce an empty step list still skip passphrase read and the
widened verify entirely -- preserving today's `no_journal_on_noop_add`
guarantee that no-op adds neither read the passphrase nor touch
cryptsetup. The widened verify only runs on non-empty execution plans.

**Error mapping.**

- `Rejected { target }` where `target.device` is a pool-member device:
  `AddError::Validation("passphrase does not match existing pool member '{target.name}'. All disks must use the same passphrase.")`.
- `Rejected { target }` where `target.device` is a candidate device:
  `AddError::Validation("passphrase rejected by candidate disk '{target.name}' ({target.device})")`.
- `Luks { source, .. }` -> `AddError::Luks(source)`.

Distinguishing pool-vs-candidate at the error site uses the (small) target
list itself: candidates start at index `self.pool.devices.len()`.

**Helper call.** Always `Credential::Passphrase(&passphrase)` --
`add` only handles passphrase credentials. Map errors explicitly,
distinguishing pool-member from candidate by target index (candidates
start at `self.pool.devices.len()`):

```rust
match verify_credential_for_targets(
    runner, &targets, Credential::Passphrase(&passphrase),
    color_enabled, |line| eprint!("{line}"),
) {
    Ok(()) => {}
    Err(CredentialVerifyError::Rejected { target }) => {
        let pool_len = self.pool.devices.len();
        let target_idx = targets.iter().position(|t| t == &target).unwrap();
        return Err(AddError::Validation(if target_idx < pool_len {
            format!(
                "passphrase does not match existing pool member '{}'. \
                 All disks must use the same passphrase.",
                target.name,
            )
        } else {
            format!(
                "passphrase rejected by candidate disk '{}' ({})",
                target.name, target.device,
            )
        }));
    }
    Err(CredentialVerifyError::Luks { source, .. }) => {
        return Err(AddError::Luks(source));
    }
}
```

The wait-line emit closure is `|line| eprint!("{line}")` (matches
today's `emit_credential_wait_line` direct call). **No
`From<CredentialVerifyError> for AddError`** -- the pool-vs-candidate
wording cannot be expressed in a blanket impl.

**Precedence consequence.** The widened verify runs at the start of
`AddPlan::execute`, after planning has already raised any foreign-label
error (`validate_braid_preconditions` in `compile_add_steps_multi:1007`,
all `PresentLuks` candidates) and any foreign-FSID/no-btrfs error on
already-open candidates (`classify_braid_disk_fsid` in
`compile_add_steps_multi:1011`, only `mapper_open: true`). The only
identity check downstream of `read_passphrase_with` is Pass 1's
`classify_braid_disk_fsid` on closed candidates (after
`ensure_luks_open`). On the narrow mixed case where a non-first pool
member has a divergent slot-0 *and* a closed braid-labeled candidate
has a foreign FSID, the credential error now wins over the candidate
FSID error. See the docs section below for the full rationale; the
test plan pins this shape with
`cmd_add_pool_member_credential_mismatch_wins_over_closed_candidate_foreign_fsid`.

### replace

`cli/src/replace.rs:188-212` (first-pool-member, gated on `PresentNotLuks`
new disk) and `:220-237` (new disk, gated on `mapper_open: false`).

Replace both with a single helper call. Build the target list once, in
this order:

1. **Anchors.**
   - If old disk is currently in `pool.devices` (live replace):
     `retained_members = pool.devices` minus the entry whose name is `old_name`.
     If `!retained_members.is_empty()`, anchors = retained_members.
     Otherwise (one-disk live pool), anchors = `[source]` (use the source
     as the only available credential anchor).
   - Else (missing replace): `retained_members = pool.devices`, anchors =
     `pool.devices`.

   Map each anchor to `CredentialVerifyTarget` via
   `name_from_mapper(&d.mapper.0)` and `d.underlying`.

   Excluding the source from anchors when other retained members exist
   is intentional: the source disk being replaced may itself have a
   divergent slot-0 (the legitimate use case for replacing it), and
   blocking its own replacement would defeat the point.

2. **New disk.** If `new_probed.state == PresentLuks { .. }` (any
   `mapper_open` state), append
   `CredentialVerifyTarget { name: new_name.clone(), device: new_by_id.0.clone() }`.
   Today's code skips `mapper_open: true`, but
   `build_replacement_membership` commits the new disk to membership, so
   it must be verified. `PresentNotLuks` new disks contribute no target.

**Position.** Same point as today's first-pool-member verify
(`cli/src/replace.rs:188`), before journal write and after pool/source
preflight reads.

**Helper call.** Always `Credential::Passphrase(&passphrase)` --
`replace` only handles passphrase credentials. Map errors explicitly,
distinguishing anchors from new-disk by target index (new disk, when
present, is the last entry). The wait-line emit closure preserves
`replace`'s existing stderr capture seam at `cli/src/replace.rs:444`
and the test that pins it
(`cmd_replace_renders_preserved_preflight_notes_on_old_equals_new_validation`
at `:4277`):

```rust
match verify_credential_for_targets(
    runner, &targets, Credential::Passphrase(&passphrase),
    color_enabled, |line| emit_replace_stderr(line),
) {
    Ok(()) => {}
    Err(CredentialVerifyError::Rejected { target }) => {
        let is_new_disk = new_disk_target.as_ref() == Some(&target);
        return Err(ReplaceError::Validation(if is_new_disk {
            format!(
                "passphrase rejected by new disk '{}' ({})",
                target.name, target.device,
            )
        } else {
            format!(
                "passphrase does not match existing pool member '{}'",
                target.name,
            )
        }));
    }
    Err(CredentialVerifyError::Luks { source, .. }) => {
        return Err(ReplaceError::Luks(source));
    }
}
```

**No `From<CredentialVerifyError> for ReplaceError`** -- the
anchor-vs-new-disk wording cannot be expressed in a blanket impl.

**Error mapping.**

- `Rejected { target }` where `target` is an anchor:
  `ReplaceError::Validation("passphrase does not match existing pool member '{target.name}'")`.
- `Rejected { target }` where `target` is the new disk:
  `ReplaceError::Validation("passphrase rejected by new disk '{target.name}' ({target.device})")`.
- `Luks { source, .. }` -> `ReplaceError::Luks(source)`.

### enroll

`cli/src/enroll_key_file.rs:117` (`verify_first_candidate_passphrase`),
`:173` (its call), `:186` (per-disk keyfile probe), `:208-219` (per-disk
non-first passphrase verify).

Replace both call sites with the shared helper, **preserving today's
per-iteration sequencing**: a slot-1 conflict on disk #1 must still report
before a wrong-passphrase on disk #2. Do **not** call the helper with a
full candidate list up front -- that reorders verifies ahead of slot-1
checks and changes which error wins on mixed failures.

All helper calls in `enroll` use `Credential::Passphrase(passphrase)` --
the per-disk keyfile probe in the loop body is a direct
`luks::verify_key_file` call, not via the helper, because its outcome
drives idempotent-skip logic that the generic helper does not model.

- **Up-front** (replaces `:173`): helper with `[first_candidate]`
  (single target). Same position and effect as today's
  `verify_first_candidate_passphrase`. Map errors with **first-candidate**
  wording:
  ```rust
  match verify_credential_for_targets(
      runner, &[first_candidate_target],
      Credential::Passphrase(passphrase),
      color_enabled, |line| eprint!("{line}"),
  ) {
      Ok(()) => {}
      Err(CredentialVerifyError::Rejected { target }) => {
          return Err(EnrollKeyFileError::Validation(format!(
              "wrong passphrase (verified against {})",
              target.name,
          )));
      }
      Err(CredentialVerifyError::Luks { source, .. }) => {
          return Err(EnrollKeyFileError::Luks(source));
      }
  }
  ```
- **Per-disk loop body** for each `(i, candidate)`:
  1. **ExistingKeyfile mode only:** `luks::verify_key_file` (direct
     call, not via the helper). If `Authenticated`, push
     `AlreadyEnrolled` and `continue` (existing, unchanged).
     `GenerateNew` skips this entirely.
  2. If `i > 0`: helper with `[candidate]` (single target). The first
     candidate is already covered by the up-front call -- repeating it
     would issue a duplicate cryptsetup call and a duplicate wait line.
     Map errors with **per-disk** wording (different from up-front):
     ```rust
     match verify_credential_for_targets(
         runner, &[candidate_target],
         Credential::Passphrase(passphrase),
         color_enabled, |line| eprint!("{line}"),
     ) {
         Ok(()) => {}
         Err(CredentialVerifyError::Rejected { target }) => {
             return Err(EnrollKeyFileError::Validation(format!(
                 "wrong passphrase on {}", target.name,
             )));
         }
         Err(CredentialVerifyError::Luks { source, .. }) => {
             return Err(EnrollKeyFileError::Luks(source));
         }
     }
     ```
  3. `check_slot_one_available(runner, name, by_id)?` (existing).
  4. Push `NeedsEnroll`.

**No `From<CredentialVerifyError> for EnrollKeyFileError`** -- the
first-vs-later wording cannot be expressed in a blanket impl.

**Error mapping** (mode-independent, by call position):

- Up-front first-candidate rejection:
  `EnrollKeyFileError::Validation(format!("wrong passphrase (verified against {})", name))`
  (current `verify_first_candidate_passphrase` wording, retained).
- Per-disk `i > 0` rejection:
  `EnrollKeyFileError::Validation(format!("wrong passphrase on {}", name))`
  (current inline-block wording, retained).
- `Luks { source, .. }` -> `EnrollKeyFileError::Luks(source)` via the
  existing `From<LuksError>` impl.

**Wait-line emit.** Pass `|line| eprint!("{line}")`.

Delete `verify_first_candidate_passphrase` once both call sites are
migrated.

## Docs

Update both files to describe the **all-relevant-disk** rule and the
new exclusion:

- `docs/principles.md:28` -- replace "subsequent disks verify against an
  existing pool member" with: "Every reachable existing LUKS device that
  will remain in or enter post-operation pool membership has its slot-0
  verified before any irreversible operation. Fresh-format disks are
  excluded (they have no existing slot-0). The live-replace source is
  excluded when other retained members exist, so a divergent slot-0 on
  the disk being replaced does not block its own replacement." Add a
  one-line note that the same rule applies to keyfile credentials used
  by `mount`/`unlock`/`recover`.
- `docs/decisions/004-single-passphrase.md:24,28` -- replace "verify
  against an existing LUKS device in the pool" / "picks one that's
  currently open, and tests the passphrase against its underlying LUKS
  device" with the same result-membership wording. **Add a paragraph**
  explaining the precedence consequence in `add`: when a non-first
  pool member has a divergent slot-0 (credential mismatch) AND a
  closed `PresentLuks { mapper_open: false }` candidate would surface
  a foreign-FSID/no-btrfs identity error in Pass 1's
  `classify_braid_disk_fsid` (in `execute`), the widened preflight
  makes the pool-member credential error win over the candidate FSID
  error. Today's identity-first ordering on this mixed shape was an
  implementation accident of the narrow first-disk verify, not a
  documented invariant. A divergent slot-0 is a latent pool-wide
  integrity issue that affects every future operation; the
  candidate's FSID error is a one-off selection mistake. Surfacing
  the more load-bearing problem first is the intended behavior.

  Identity errors that surface during **planning** are outside the
  scope of this shift: `compile_add_steps_multi`
  (`cli/src/add.rs:1007`) calls `validate_braid_preconditions` for
  every `PresentLuks` candidate (foreign-label) and additionally
  calls `classify_braid_disk_fsid` for already-open candidates
  (foreign-FSID, no-btrfs). Both surface from `plan_add` before
  `AddPlan::execute` runs, before any passphrase read, and before
  the widened verify. The widened preflight cannot and does not
  shift those error orderings.

## Critical files

- New: `cli/src/credential_verify.rs`
- Modified: `cli/src/lib.rs` (or wherever the module list lives) -- add
  `mod credential_verify;`
- Modified: `cli/src/cmd.rs` -- add request log to `MockRunner` (see
  "MockRunner Request Log" in the test plan); production
  `RealRunner` is unaffected.
- Modified: `cli/src/mount.rs` -- passphrase + keyfile arms, post-verify
  wording, extract `open_disks_with_key_file`
- Modified: `cli/src/add.rs` -- replace the `pool.devices.first()` block
  with a single widened verify call
- Modified: `cli/src/replace.rs` -- replace both passphrase blocks with a
  single helper call (anchors + new disk)
- Modified: `cli/src/enroll_key_file.rs` -- replace
  `verify_first_candidate_passphrase` and the inline `i > 0` block;
  delete `verify_first_candidate_passphrase`
- Modified: `docs/principles.md`,
  `docs/decisions/004-single-passphrase.md`

## Reused existing pieces

- `luks::verify_passphrase` (`cli/src/luks.rs:410`)
- `luks::verify_key_file` (`cli/src/luks.rs:625`)
- `luks::VerifyOutcome`, `luks::LuksError` (`cli/src/luks.rs`)
- `credential_wait_line` / `CredentialKind` (`cli/src/status_tag.rs:66`)
- `emit_replace_stderr` (`cli/src/replace.rs:444`) -- still wraps the
  helper's emit closure on the replace path
- `explain_open_failure` (`cli/src/mount.rs:459`) -- still wraps the
  helper's errors on the mount path
- `PoolDevice.underlying` (`cli/src/types.rs:106`) -- replaces the
  `CryptsetupStatus { mapper }` round-trip in add/replace
- `name_from_mapper` -- to derive target names from `PoolDevice.mapper`
- Existing recording wrappers (`AddRecordingRunner` at
  `cli/src/add.rs:3337` and analogous wrappers in `cli/src/replace.rs`)
  -- still used by integration tests in those modules that need
  ordered call assertions. The new `MockRunner` request log
  (added in `cli/src/cmd.rs`; see test plan) is consumed by the
  helper unit tests in `cli/src/credential_verify.rs`, by the new
  focused unit tests in `cli/src/cmd.rs`, and by **one** focused
  `enroll_key_file.rs` test
  (`plan_generate_new_does_not_repeat_first_candidate_passphrase_verify`)
  that needs to count duplicate calls. Existing per-module wrappers
  stay in place; this plan does not migrate them onto
  `requests()`.

## Test plan

### MockRunner Request Log (`cli/src/cmd.rs`)

> **Test infrastructure -- `MockRunner` gains a request log.** Helper
> unit tests below need to assert that the helper actually issues the
> cryptsetup verify calls, in target order, and stops at the first
> rejection. Today's `MockRunner` is lookup-only: an emitted wait
> line does not prove a corresponding `run_with_stdin` call was made,
> and an unconsumed mock is silent. To close that gap without per-test
> wrappers, extend `MockRunner` itself.
>
> Changes to `cli/src/cmd.rs` (`MockRunner` only; `RealRunner`
> unaffected):
>
> - Add an internal shared log field to `MockRunner`:
>   `requests: Arc<Mutex<Vec<CmdRequest>>>`. The `Arc<Mutex<...>>`
>   wrapper provides interior mutability under `&self` (matching the
>   `CommandRunner` trait shape) and lets the same log be observed
>   across clones.
> - `MockRunner::default()`, `with_output(...)`, and
>   `with_output_stdin(...)` keep their current builder behavior. The
>   log is initialized empty in `default()` and is preserved by the
>   builder methods (since they take `mut self` and return `Self`,
>   the same `Arc` carries through).
> - In both `CommandRunner::run` and `CommandRunner::run_with_stdin`,
>   push `request.clone()` to the log **before** the existing
>   lookup/dispatch. Missing-mock and stdin-expectation behavior must
>   remain unchanged -- the log only adds an ordered record of every
>   issued request.
> - Add `pub fn requests(&self) -> Vec<CmdRequest>` returning a cloned
>   snapshot of the log (lock, clone the inner `Vec`, drop the lock).
> - **Do not record stdin in this generic log.** Existing
>   `stdin_expectations` already validate input bytes; a generic
>   stdin log risks spreading passphrase bytes into broad assertions
>   and journal dumps. The log records `CmdRequest` only.
>
> New focused unit tests in `cli/src/cmd.rs`:
>
> - **Happy path** (`mock_runner_requests_records_run_and_run_with_stdin_in_order`):
>   seed a `MockRunner` with one `run`-style request mock (e.g.
>   `CmdRequest::LsblkJson`) and one `run_with_stdin`-style request
>   mock (e.g. `CmdRequest::CryptsetupTestPassphrase { device: "/dev/x".into() }`
>   with a stdin expectation). Execute both via the runner.
>   `assert_eq!` `runner.requests()` against `vec![req1, req2]` in
>   order.
> - **Missing-mock recording**
>   (`mock_runner_requests_records_missing_mock_calls_too`): call
>   `run` with an *unseeded* `CmdRequest`. Assert (a) the call
>   returns `Err(CmdError::MissingMock)` (today's behavior at
>   `cli/src/cmd.rs:1069` and `:1092`), and (b) the unseeded request
>   was still appended to `requests()`. The log records every issued
>   call, including ones the runner has no output for.
> - **Stdin-mismatch preserves panic**
>   (`mock_runner_run_with_stdin_panics_on_stdin_mismatch_unchanged`):
>   seed a `run_with_stdin` mock with a stdin expectation, call it
>   with mismatching bytes, and confirm via
>   `#[should_panic(expected = "stdin mismatch")]` that the existing
>   `assert_eq!` panic at `cli/src/cmd.rs:1090` still fires unchanged.
>
> **Scope of use:**
>
> - Helper unit tests in `cli/src/credential_verify.rs` and the new
>   focused unit tests in `cli/src/cmd.rs` use
>   `MockRunner::requests()` directly.
> - `enroll_key_file.rs` integration tests use **one** focused
>   `MockRunner::requests()` assertion -- the no-duplicate-verify
>   test below -- because the contract it pins (disk1 verified
>   exactly once) cannot be expressed via mock-absence (mocks are
>   reusable, so a duplicate call is silently allowed). Other
>   `enroll_key_file.rs` tests continue to use mock-absence.
> - `add.rs` and `replace.rs` integration tests stay on their
>   existing per-module recording wrappers (`AddRecordingRunner` at
>   `cli/src/add.rs:3337` and `replace`'s analog) for ordered
>   call-list assertions.
> - `mount.rs` integration tests use mock-absence for "no follow-up
>   open after a rejected verify" assertions.
>
> Wholesale migration of integration tests onto `MockRunner::requests()`
> is a reasonable follow-up but would expand this plan beyond the
> credential-verification change; it is out of scope here.

### Helper unit tests (`cli/src/credential_verify.rs`)

`MockRunner`-based with `requests()` assertions for
`verify_credential_for_targets`. The first three behavior tests are
**table-driven across both `Credential` variants** (`Passphrase("p")`
and `KeyFile(Path::new("/k"))`) -- each test runs once per variant,
with the expected `CmdRequest` variant and `CredentialKind` derived
from the credential under test:

| `Credential` variant      | Expected `CmdRequest` variant                       | `CredentialKind`        |
| ------------------------- | --------------------------------------------------- | ----------------------- |
| `Passphrase(_)`           | `CryptsetupTestPassphrase { device }`               | `Passphrase`            |
| `KeyFile(_)`              | `CryptsetupTestKeyFile { device, key_file_path: ... }` | `KeyFile`            |

Each behavior test asserts **both** the emit list and the request
list -- the emit list alone does not prove the helper issued the
cryptsetup call, and the request list alone does not prove the wait
line was emitted before verify returned.

- `verify_credential_for_targets_authenticates_all_targets_in_order`:
  three targets, all return exit 0. Assert two things:
  1. **Wait-line emit list:** `assert_eq!` the `Vec<String>`
     collected by the emit closure against
     `targets.iter().map(|t| credential_wait_line(KIND, color_enabled, &t.name)).collect()`,
     for both `color_enabled = true` and `color_enabled = false`,
     where `KIND` is derived from the `Credential` variant under
     test. Pins the canonical `[wait] passphrase: checking against ...`
     and `[wait] keyfile: checking against ...` lines per target
     (memory: pin-preservation-claims).
  2. **Request log:** `assert_eq!` `runner.requests()` against
     `targets.iter().map(|t| EXPECTED_REQ(t.device.clone())).collect()`
     in target order, where `EXPECTED_REQ` is the variant from the
     table above. Pins that the helper actually dispatches to the
     correct low-level cryptsetup verifier (`verify_passphrase` ->
     `CryptsetupTestPassphrase`, `verify_key_file` ->
     `CryptsetupTestKeyFile`). An implementation that mis-routes
     the dispatch (e.g. always uses `CryptsetupTestPassphrase`)
     would pass the emit-list assertion but fail this one.
- `verify_credential_for_targets_stops_at_first_rejection`: three
  targets, target #2 returns exit 2. Assert
  `Err(Rejected { target: <#2> })`. `assert_eq!` collected emits to
  `[wait_line(#1), wait_line(#2)]` -- proves target #2's wait line
  was emitted **before** verify failed. `assert_eq!`
  `runner.requests()` to `[EXPECTED_REQ(#1), EXPECTED_REQ(#2)]` --
  target #3 must NOT appear, proving the helper stops on first
  rejection rather than falling through.
- `verify_credential_for_targets_returns_luks_on_non_auth_exit`:
  target #2 returns exit 1 (non-auth `OpenFailed`). Assert
  `Err(Luks { target: <#2>, source: ... })`. Same emit-list
  assertion as above. `assert_eq!` `runner.requests()` to
  `[EXPECTED_REQ(#1), EXPECTED_REQ(#2)]` -- target #3 must NOT
  appear; the helper must stop on the first non-`Authenticated`
  outcome regardless of whether it is `Rejected` or another
  `LuksError` variant.
- `verify_credential_for_targets_empty_list_is_ok`: no targets,
  helper returns `Ok(())`, emit closure was never invoked, **and**
  `runner.requests()` is empty. Only one test (no variant axis):
  the empty path does not dispatch to either credential branch, so
  running it twice would assert nothing additional.

### Integration tests

> Tests that need ordering or "no call issued" assertions use one of
> three strategies, picked per-test (see also "Scope of use" in the
> MockRunner Request Log section above):
>
> 1. **Existing per-module recording wrappers** where they exist
>    (`AddRecordingRunner` at `cli/src/add.rs:3337`, the analogous
>    wrapper in `cli/src/replace.rs`) -- for explicit ordered call-list
>    assertions in the `add` and `replace` tests below.
> 2. **Mock absence** (per the lookup-rule memory: missing mocks
>    surface as `MissingMock` -> `LuksError::Cmd`, which changes the
>    asserted error shape) -- for `enroll_key_file.rs`'s sequencing
>    test and for "no follow-up open after a rejected verify"
>    assertions in the `mount.rs` tests.
> 3. **`MockRunner::requests()`** -- used by one focused
>    `enroll_key_file.rs` test (`plan_generate_new_does_not_repeat_first_candidate_passphrase_verify`)
>    to count duplicate calls. Required because mock-absence cannot
>    catch a duplicate call (mocks are reusable). Not used by the
>    other integration tests; broader adoption is a follow-up.
>
> Tests that prove "no mutation occurred" additionally assert
> filesystem state (no journal file, no btrfs device add).

**`mount.rs`** -- extend existing first-disk-rejection coverage:

- `unlock_passphrase_mismatch_on_disk2_aborts_before_any_open` -- two-disk
  plan, mock disk1 verify exit 0, disk2 verify exit 2. Assert the error
  names disk2. Assert no `cryptsetup open` mock was needed -- if the
  implementation tried to open disk1 after a failed verify, the test
  would fail with `MissingMock`. (Per the lookup-rule memory: this
  pinns behavior via mock absence, not via consumption tracking.)
- `unlock_keyfile_mismatch_on_disk2_aborts_before_any_open` -- same
  shape for the keyfile arm (mock disk1 `verify_key_file` exit 0,
  disk2 `verify_key_file` exit 2). **Additionally** mock disk2's
  `probe_luks_header` chain to return `LuksHeaderState::Ok` (healthy
  header) so `explain_open_failure` returns its `ok_fallback`, not
  one of the unreadable/damaged-header guidance branches. Assert
  `assert_eq!` on the full error message against the exact string
  `"wrong keyfile (rejected by disk2)"` (memory:
  pin-preservation-claims), and assert the message does **not**
  contain the substring `"passphrase"`. Pins three things at once:
  (a) the consolidated `Credential` helper wires the keyfile arm to
  keyfile wording, not passphrase wording -- a bug the single
  enum-based helper makes easy to introduce by accident; (b) the
  rejecting target name is included; (c) on a healthy-header
  rejection (the common case), the user sees the keyfile fallback
  string, not header-recovery guidance. An implementation that
  routed keyfile rejection through the passphrase branch (or omitted
  the variant-specific wording) would fail this exact-equality
  assertion.
- `post_verify_open_rejection_wording_says_verified_against_all` -- mock
  all verifies as exit 0, mock disk2's `cryptsetup open` as exit 2.
  `assert_eq!` the full error message against the new wording (memory:
  pin-preservation-claims).
- `unlock_passphrase_verify_openfailed_on_disk2_routes_to_unreadable_header_guidance`
  -- two-disk plan, disk1 verify exit 0, disk2 `CryptsetupTestPassphrase`
  mocked to return a non-auth exit (e.g. exit 5 / EBUSY) producing a
  `LuksError::OpenFailed { exit_code: 5, .. }`. Mock disk2's
  `probe_luks_header` chain to return `Unreadable`. Assert the error
  message contains the `luks_header_unreadable_guidance` substring.
  Pins that non-first `OpenFailed` routes through `explain_open_failure`
  and surfaces header recovery guidance instead of raw cryptsetup hint
  text.
- `unlock_passphrase_verify_openfailed_on_disk2_routes_to_damaged_header_guidance`
  -- same shape but disk2's header probe returns `Damaged`. Assert the
  error message contains the `luks_header_damaged_guidance` substring.
- `unlock_keyfile_verify_openfailed_on_disk2_routes_to_unreadable_header_guidance`
  and
  `unlock_keyfile_verify_openfailed_on_disk2_routes_to_damaged_header_guidance`
  -- same two shapes for the keyfile arm.
- `unlock_passphrase_verify_non_openfailed_luks_returns_mounterror_luks_directly`
  -- two-disk plan, disk1 verify exit 0, disk2 `CryptsetupTestPassphrase`
  mocked to return `Err(CmdError::Failed("spawn failed".into()))`,
  which surfaces through the `From<CmdError> for LuksError` impl as
  `LuksError::Cmd(...)` (a non-`OpenFailed` variant). Assert the
  result is `Err(MountError::Luks(LuksError::Cmd(_)))` and the error
  message does **not** contain `luks_header_unreadable_guidance`,
  `luks_header_damaged_guidance`, or the "diagnosis could not be
  completed" substring from `explain_open_failure`. Pins that
  non-`OpenFailed` errors bypass header diagnosis.
- `unlock_keyfile_verify_non_openfailed_luks_returns_mounterror_luks_directly`
  -- same shape for keyfile.

**`add.rs`** -- existing
`cmd_add_bootstrap_aborts_on_passphrase_mismatch` (`:3508`) keeps
passing. New tests:

- `cmd_add_fresh_format_aborts_on_passphrase_mismatch_against_non_first_pool_member`
  -- three live pool members, one `PresentNotLuks` candidate. Mock
  member #1 verify `Authenticated`, member #2 verify `Rejected`. Assert
  (a) the error names member #2, (b) no journal file was written, (c)
  no `mkfs.btrfs` or `cryptsetup luksFormat` mock was hit (assertion via
  mock absence).
- `cmd_add_aborts_on_passphrase_mismatch_against_non_first_pool_member_closed_candidate_only`
  -- three live pool members, one closed `PresentLuks` braid-labeled
  candidate (no fresh-format). Mock member #2 verify `Rejected`. Assert
  the error names member #2; assert no journal write; assert the
  candidate was not opened (no `cryptsetup open` mock needed).
- `cmd_add_aborts_on_passphrase_mismatch_against_already_open_recoverable_candidate`
  -- mounted live pool, one `PresentLuks { mapper_open: true }`
  braid-labeled candidate that would classify as `BraidLabeledRecoverable`.
  Mock all live members verify `Authenticated`, candidate verify
  `Rejected`. Assert (a) the error names the candidate, (b) no journal
  write, (c) the candidate's mapper was not closed by `LuksCleanupGuard`
  (we never opened it).
- `cmd_add_already_in_pool_only_invocation_skips_verify_via_steps_empty_short_circuit`
  -- one `PresentLuks { mapper_open: true }` candidate that classifies
  as `BraidLabeledAlreadyInPool`, mounted live pool. Pass
  `passphrase_reader: &ScriptedPassphraseReader::new(["SENTINEL"])`.
  Assert (a) the result is `Ok(())`, (b) via the recording wrapper,
  **no** `CryptsetupTestPassphrase` request was issued (the `:373`
  steps-empty short-circuit fires before the widened verify), (c) the
  `ScriptedPassphraseReader` sentinel remains unread (no passphrase
  read happened either), (d) no journal file was written (assert
  filesystem state). Pins that no-op invocations preserve today's
  `no_journal_on_noop_add` guarantee end-to-end: no passphrase prompt,
  no cryptsetup call, no journal write. An implementation that moved
  the widened verify above the `:373` short-circuit would consume the
  sentinel and fail this test. The user-visible no-op message itself
  is rendered to stderr from a `PreviewNote::Info` and is already
  pinned by the existing `plan_add_already_in_pool_is_note_only_success`
  render test -- this `cmd_add` test scope is no-side-effects only,
  not message wording.
- `cmd_add_pool_member_credential_mismatch_wins_over_closed_candidate_foreign_fsid`
  -- live pool with three members where member #2's verify is
  `Rejected`, plus a closed `PresentLuks { mapper_open: false }`
  candidate that is **braid-labeled** (so it passes the planning-time
  `validate_braid_preconditions` at `compile_add_steps_multi:1007`)
  but whose btrfs superblock, if classified, would surface a
  foreign-FSID error in Pass 1's `classify_braid_disk_fsid`. Mock
  member #1 verify `Authenticated`, member #2 verify `Rejected`. Do
  **not** seed mocks for the candidate's `CryptsetupTestPassphrase`,
  `cryptsetup luksOpen`, or btrfs superblock probe -- the helper
  stops at member #2's rejection and Pass 1 never runs; if any of
  those are issued, `MissingMock` -> `LuksError::Cmd` shifts the
  error shape and the test fails. Assert the error is the credential
  rejection naming member #2, NOT the foreign-pool identity refusal
  from `identity_to_error`. Documents the only reachable precedence
  shift: pool-member credential mismatch winning over Pass 1's FSID
  classification on a closed candidate. Foreign-label and
  already-open identity errors are caught by planning's
  `compile_add_steps_multi` and are unreachable from `execute`'s
  widened verify.
- (No additional candidate-vs-itself precedence test.) For closed
  `PresentLuks { mapper_open: false }` candidates, the candidate's
  own slot-0 mismatch already wins over its own identity error
  trivially under today's code (`ensure_luks_open` fails before
  `classify_braid_disk_fsid` runs), and the lean's widened verify
  preserves that ordering. For already-open `PresentLuks { mapper_open: true }`
  candidates, planning surfaces identity errors before `execute` runs,
  so the widened verify is unreachable on that path -- the precedence
  shift simply does not apply, and there is nothing meaningful to
  pin with a test.

**`replace.rs`** -- existing
`wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal` (`:2900`)
keeps passing. New tests:

- `replace_aborts_on_passphrase_mismatch_against_non_first_pool_member`
  -- three live pool members, source is member #3 (so member #2 is a
  retained anchor). Fresh new disk. Mock member #2 verify `Rejected`.
  Assert error names member #2; no format; no journal.
- `replace_closed_preformatted_verifies_anchors_then_new_disk_in_order`
  -- use `replace`'s recording wrapper to assert the ordered call list:
  retained anchors first, then new disk.
- `replace_excludes_source_from_anchors_when_retained_members_exist` --
  three-disk live pool, replacing member #1 (which has a divergent
  slot-0). Fresh new disk. Mock #1 verify mock is absent (would error
  if called); mock #2 and #3 verify `Authenticated`. Assert replacement
  proceeds. Pins that source #1 is not an anchor.
- `replace_uses_source_as_only_anchor_when_no_other_members_remain` --
  one-disk live pool, replacing the sole disk. Fresh new disk. Mock
  source verify `Authenticated`; assert verify was called against
  source's `underlying` (via recording wrapper).
- `replace_aborts_on_passphrase_mismatch_against_already_open_new_disk`
  -- new disk is `PresentLuks { mapper_open: true }`. Mock new disk
  verify `Rejected`. Assert no journal write; assert the already-open
  mapper was not closed (we never opened it).
- `replace_missing_source_aborts_on_passphrase_mismatch_against_non_first_retained_member`
  -- missing-path replace (source is missing from `pool.devices`).
  `pool.devices` has 2 retained live members; both are anchors. Fresh
  new disk (`PresentNotLuks`). Mock retained member #1 verify
  `Authenticated`, retained member #2 verify `Rejected`. Assert (a)
  the error names retained member #2, (b) no journal write, (c) no
  `mkfs.btrfs` or `cryptsetup luksFormat` request was issued (assert
  via mock absence). Pins missing-path anchor coverage: the
  missing-replace branch must verify retained members before journal
  write, not skip straight to format.

**`enroll_key_file.rs`** --
`plan_divergent_passphrase_existing_keyfile_errors_on_disk2` (`:1469`)
and `plan_divergent_passphrase_generate_new_errors_on_disk2` (`:1534`)
keep passing. New tests:

- `plan_existing_keyfile_skips_passphrase_verify_on_already_enrolled` --
  **two** candidates: #1 needs enroll, #2 is already enrolled. Seed
  mocks only for the calls that should happen: candidate #1's
  `CryptsetupTestPassphrase` (up-front verify) returns
  `Authenticated`; candidate #1's keyfile probe returns `Rejected`;
  candidate #1's slot-1 check returns `Empty`; candidate #2's
  keyfile probe returns `Authenticated`. Do **NOT** seed
  `CryptsetupTestPassphrase` for candidate #2's `by_id`. Assert the
  plan succeeds and contains `NeedsEnroll` for #1 and `AlreadyEnrolled`
  for #2. Per the lookup-rule memory, mock absence is the behavioral
  pin: if the implementation issues a passphrase verify on candidate
  #2 (e.g. by calling the helper before checking the keyfile probe
  result), the runner returns `MissingMock`, which surfaces as
  `EnrollKeyFileError::Luks(LuksError::Cmd(...))` -- a different
  result shape than the asserted success, and the test fails. (Two
  candidates is enough to prove the skip; a third candidate would
  add unrelated `i > 0` sequencing into the same test.)
- `plan_generate_new_reports_slot1_conflict_on_disk1_before_passphrase_on_disk2`
  -- `GenerateNew` mode, two candidates. Seed mocks only for the
  calls that should happen: disk1's `CryptsetupTestPassphrase`
  (up-front verify) returns `Authenticated`; disk1's slot-1 check
  returns `Occupied`. Do **NOT** seed any mocks for disk2 (no
  passphrase verify, no slot-1 check). Assert the error is the
  canonical `"slot 1 on disk1 ... is occupied by an unknown key"`
  string from `check_slot_one_available`. Mock absence is the
  behavioral pin: if the implementation calls the helper with the
  full candidate list up front (verifying disk2 before disk1's
  slot-1 check), disk2's verify gets `MissingMock`, which surfaces
  as a `LuksError::Cmd` instead of the asserted slot-1-conflict
  string, and the test fails. Pins per-iteration sequencing
  (verify -> slot-1 check, per candidate) without needing a
  recording wrapper.
- `plan_generate_new_does_not_repeat_first_candidate_passphrase_verify`
  -- `GenerateNew` mode, two candidates. Mock disk1's
  `CryptsetupTestPassphrase` `Authenticated`, disk1's slot-1 check
  `Empty`, disk2's `CryptsetupTestPassphrase` `Authenticated`,
  disk2's slot-1 check `Empty`. Assert (a) the plan succeeds with
  `NeedsEnroll` for both, (b)
  `runner.requests().iter().filter(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { device } if device == &disk1.by_id.0)).count()`
  is exactly **1**. Pins the "do not repeat first candidate verify"
  contract: if an implementation calls the helper inside the per-disk
  loop without the `i > 0` guard, disk1 gets verified twice (once
  up-front, once at `i == 0`). `MockRunner` mocks are reusable, so
  the duplicate call would silently succeed without `requests()` --
  this test is the only behavioral pin for the no-duplicate-call
  contract. Possible to extend with an analogous count assertion for
  disk2 (must be exactly 1) to demonstrate the up-front+loop
  arithmetic.

## Verification

End-to-end:

1. `just test-rust` -- helper unit tests + per-command integration tests pass.
2. `just test-vm add-passphrase-mismatch replace-passphrase-mismatch
   replace-preformatted-luks-passphrase-mismatch braid-unlock
   braid-unlock-key-file braid-recover braid-enroll braid-enroll-generate`
   -- targeted VM tests for credential mismatch and unlock/enroll
   scenarios still pass end-to-end.
3. `just test-vm` -- full suite to catch regressions in unrelated paths
   that depend on current first-disk-only verify wording. The mount
   post-verify wording change is the most likely source of a
   snapshot-style assertion failure.

## Out of scope

- Changing how braid acquires the credential (passphrase prompt,
  keyfile read) -- only verification scope changes.
- Changing the LUKS slot model (one slot per credential type).
- Restructuring `add`'s Pass 1 (`validate_braid_preconditions`,
  `ensure_luks_open`, `classify_braid_disk_fsid`) -- it stays exactly
  as today.
- Restructuring `enroll_key_file.rs`'s mode dispatch beyond replacing
  the verification calls.
- Wholesale migration of integration tests onto the new
  `MockRunner.requests()` log. The log is added (see "MockRunner
  Request Log" in the test plan) and consumed by the helper unit
  tests, the new `cli/src/cmd.rs` focused tests, and one focused
  `enroll_key_file.rs` no-duplicate-verify test that mock-absence
  cannot express. The existing per-module recording wrappers
  (`AddRecordingRunner` etc.) stay in place for `add.rs` and
  `replace.rs`; mock-absence stays in place for the rest of the
  `mount.rs` and `enroll_key_file.rs` tests. Broader adoption is a
  reasonable follow-up but out of scope here.
