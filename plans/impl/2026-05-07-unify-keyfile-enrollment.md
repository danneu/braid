# Unify keyfile enrollment across `add`, `replace`, and `enroll`

## Context

`braid add --enroll DIR` and `braid replace --enroll DIR` both silently drop
the keyfile when the target disk is already LUKS-formatted. The user passes
`--enroll` to make the new disk auto-unlockable; today nothing surfaces the
fact that it's a no-op. After the operation, `braid unlock` falls back to
passphrase for the new disk while every other pool member opens silently,
and the auto-unlock service can't open it at all.

The cited finding called out `replace`. The same shape exists in `add`'s
`OpenRecoverable` and `ClosedPresentLuks` paths -- a returning braid disk
that was originally added without a keyfile (or whose keyfile was rotated
since) gets the same silent drop. Standalone `braid enroll` already does
the right thing (idempotent skip when keyfile authorizes; refuse when slot
1 has an unknown key; otherwise enroll), but its logic is gated on
pool-membership iteration and isn't reusable.

The fix is structural: extract the per-disk enrollment-planning logic into
one helper that all three commands route through. After the refactor,
`Some(kf) + already-LUKS target` is structurally unrepresentable as a
silent drop -- every code path that handles that combination lands in the
same function.

Outcome:

- `replace --enroll DIR` against a `PresentLuks` new disk enrolls the
  keyfile (or idempotent-skips if already enrolled, or refuses cleanly on
  slot-1 conflict before any journal write).
- `add --enroll DIR` does the same for `OpenRecoverable` and
  `ClosedPresentLuks` targets.
- Crash recovery replays enrollment for both commands.
- One canonical helper owns the slot-1-check + idempotent-skip + refuse
  logic; the standalone `enroll` command also routes through it.

## Design

### 1. Extract `plan_single_disk_enrollment` from `enroll_key_file.rs`

Today `enroll_key_file::plan_enrollment` (`cli/src/enroll_key_file.rs:150`)
does up-front passphrase verification across all candidates, then iterates
per-candidate doing `verify_key_file` + `check_slot_one_available` +
classify. The per-candidate loop body has no cross-iteration state and can
be lifted as-is.

New `pub(crate)` helper:

```rust
pub(crate) fn plan_single_disk_enrollment<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
    key_file_path: &Path,
    mode: EnrollmentPlanMode,
) -> Result<DiskEnrollAction, EnrollKeyFileError>
```

- `ExistingKeyfile` mode: probe `verify_key_file`. `Authenticated` -> return
  `AlreadyEnrolled`. `Rejected` -> fall through to slot-1 check.
- `GenerateNew` mode: skip `verify_key_file` (file doesn't exist yet).
- Slot-1 check: if `Occupied`, return
  `Err(EnrollKeyFileError::Validation(...))` with the existing remediation
  text (matches `check_slot_one_available` at
  `enroll_key_file.rs:123-137`). If `Empty`, return `NeedsEnroll`.
- Per-disk `[wait]/[ok]/[skip]` status rows stay inside the helper.

`plan_enrollment` keeps its current shape: passphrase-verify-up-front, then
loop body becomes `plan.push(plan_single_disk_enrollment(...)?)`. Make
`EnrollmentPlanMode` and `DiskEnrollAction` `pub(crate)` so callers in
`replace.rs` / `add.rs` can pattern-match.

### 2. `replace.rs` integration

`cli/src/replace.rs`:

- Extend `ReplaceTargetPrep::ExistingLuks` to carry
  `enroll_key_file: Option<PathBuf>` (currently only `mapper_open: bool`).
- `plan_replace` (around line 928, after the existing asymmetry warning
  block): when `enroll_key_file.is_some() && new_probed.state ==
  PresentLuks`, call `plan_single_disk_enrollment(... ExistingKeyfile)`.
  Match the result:
  - `AlreadyEnrolled` -> `enroll_key_file: None` into target_prep (idempotent).
  - `NeedsEnroll` -> `enroll_key_file: Some(kf)` into target_prep.
  - `Err(_)` -> propagate as `ReplaceError::Validation`. Refusal happens
    before journal write; matches the existing pre-journal pattern.
- `ReplaceWorkPlan::render_steps` (line 165) `ExistingLuks` arm: when
  `enroll_key_file: Some(kf)`, emit `CryptsetupLuksAddKeyFile` then
  `CryptsetupLuksHeaderBackup` before the existing `CryptsetupLuksOpen`.
  Order matches the `FreshLuks` ordering pin (luksFormat -> addKey ->
  headerBackup -> open) at `replace.rs:121-176` and the test at
  `replace.rs:3394-3460`.
- `ReplacePlan::execute` (line 513) `ExistingLuks` arm: when
  `enroll_key_file: Some(kf)`, call `crate::luks::enroll_key_file` +
  `backup_luks_header_post_mutation` before `ensure_luks_open`. Wrap with
  the same `[wait]/[ok]` status rows the FreshLuks arm uses (line 467-485).
- `build_replace_journal_target` (line 1107) for `PresentLuks`: pass the
  planned `enroll_key_file` into the new `ReplaceJournalMode::ExistingLuks`
  field. `None` for the AlreadyEnrolled / no-enroll case.

### 3. `add.rs` symmetric integration

`cli/src/add.rs`:

- `RecoverableBraidTarget` (line 276) and `ClosedPresentLuksCandidate`
  (line 286) grow `enroll_key_file: Option<PathBuf>`.
- `build_add_work_plan` (around line 1430-1499): for `OpenRecoverable` and
  `ClosedPresentLuks` paths, when `params.enroll_key_file.is_some()` call
  `plan_single_disk_enrollment(... ExistingKeyfile)`. Same match semantics
  as replace.
- `AddWorkPlan::render_steps` (line 337) for `OpenRecoverable` and
  `ClosedPresentLuks` arms: when `enroll_key_file: Some(kf)`, emit
  `CryptsetupLuksAddKeyFile` + `CryptsetupLuksHeaderBackup` steps. For
  `ClosedPresentLuks` these come after the LUKS open + identity verify
  step (the disk has to be open to verify identity first); for
  `OpenRecoverable` they come at the start of the target's steps.
- **Existing flow already supports uniform journaling.**
  `AddPlan::execute` runs a Pass-1 pre-mutation phase (`add.rs:732-800`)
  that opens every `ClosedPresentLuks` target, runs
  `classify_braid_disk_fsid`, builds a `RecoverableBraidTarget`, and
  inserts it into the runtime `journal_targets` map at `:792-793`.
  `journal::write_journal` then runs once at `add.rs:847` covering all
  target classes (Fresh, OpenRecoverable, verified ClosedPresentLuks).
  The refactor threads `enroll_key_file` through the existing target
  constructors and journal builder; **no new mechanism, no new variant,
  and no change to which targets get journaled**.
- **Journal schema:** extend
  `AddJournalMode::RecoverableBraidLabeled` (currently
  `{ verified_pool_fsid, luks_uuid }`) with
  `enroll_key_file: Option<PathBuf>`. Do **not** introduce a separate
  `ExistingLuks` add variant -- doing so would lose the
  `verified_pool_fsid` guard recovery relies on at `recover.rs:2116`
  for the multi-layer identity check required by `docs/principles.md`.
  One variant covers every journaled returned-disk add target, open
  or closed, with or without enrollment.
- **Wiring per case:**
  - `OpenRecoverable + Some(kf)` / `OpenRecoverable + None`: journal
    upfront via `recoverable_journal_target` (line 1398). Extend the
    builder to take and serialize `enroll_key_file`.
  - `ClosedPresentLuks + Some(kf)` / `ClosedPresentLuks + None`:
    extend the verified `RecoverableBraidTarget` constructor at
    `add.rs:784-791` (and the planning-time
    `ClosedPresentLuksCandidate`) to thread `enroll_key_file` through
    to `recoverable_journal_target(&verified)` at `:793`.
- **Mutation order in `execute` (after `write_journal`):** for any
  recoverable target with `enroll_key_file: Some(kf)`, run
  `cryptsetup luksAddKey` + `backup_luks_header_post_mutation` before
  the existing pool-add. For `enroll_key_file: None`, behavior is
  unchanged.

### 4. `journal.rs` schema additions

`cli/src/journal.rs`:

- `ReplaceJournalMode::ExistingLuks` (line 87): add
  `enroll_key_file: Option<PathBuf>`.
- `AddJournalMode::RecoverableBraidLabeled` (line 38): add
  `enroll_key_file: Option<PathBuf>`. **No new variant.** The existing
  `verified_pool_fsid` and `luks_uuid` fields stay; both now apply to
  open and closed recoverable targets.
- Update roundtrip test fixtures at `journal.rs:323-351`, `:437-500`, and
  the recoverable add fixture around `:382-400` to carry the new field
  (covering `Some(kf)` and `None` cases for both modes).

Per `AGENTS.md` ("No backwards compatibility -- braid is unreleased"), no
migration path needed.

### 5. Recovery integration

`cli/src/recover.rs`:

This refactor extends both recovery arms from "rollback / identity
bookkeeping" into "replay LUKS mutation (keyfile enrollment + header
backup) when journaled". The replace arm needs a new identity probe
**and** a new credential dance it lacks today; the add arm already has
both an identity probe and credential verification, and only needs new
keyfile-mutation work threaded through preview and execution.

**Replace `ReplaceJournalMode::ExistingLuks` arm (`recover.rs:2439`):**

Today this arm rolls back to pre-replace topology with no probe and no
passphrase work. Promoting it to LUKS mutation requires the same
credential discipline as the `FreshLuks` arm (`:2464-2492`):

1. Probe `new_target.by_id` via `probe::probe_config_disk`; require
   `ConfigDiskState::PresentLuks { uuid, .. }` to equal the journaled
   `luks_uuid`. On mismatch (wrong disk replugged, header zeroed),
   preserve the journal (do **not** `clear_journal`) and return
   `RecoverError::Failed(...)`. Mirrors the FreshLuks label-match guard
   at `:2456-2462`, but uses LUKS UUID since `ExistingLuks` has no
   braid-assigned label to compare against.
2. Resolve and verify the credential, mirroring `:2464-2472`:
   `recover_passphrase_for_context(credential, params, "replace
   recovery")` then `verify_replace_fresh_prep_passphrase(runner, pool,
   new_name, &new_target.by_id, passphrase.expose_secret())`. Wrong
   passphrase aborts before any LUKS mutation, with the journal
   preserved -- preserves the single-passphrase invariant
   (`docs/principles.md`).
3. Acquire the sleep inhibitor (matches `:2473-2478`).
4. If `enroll_key_file: Some(kf)`: `ensure_keyfile_enrolled(runner,
   &new_target.by_id.0, passphrase.expose_secret(), kf)?` then
   `luks::backup_luks_header(runner, &new_target.by_id.0,
   &new_target.mapper_name, params.paths)?`. Matches the FreshLuks
   sequence at `:2479-2492`.
5. `save_membership(&journal.pre_membership, params.paths)?` +
   `clear_journal`, as today.

**Add `AddJournalMode::RecoverableBraidLabeled` recovery -- preview +
executor:**

Add recovery is split between a dry-run preview renderer and a real
executor; both must be updated together to keep `recover --dry-run`
aligned with actual replay (per
`docs/decisions/022-dry-run-preview-model.md`). Update both arms in
the same change.

- **Preview renderer** -- `render_add_pool_mutation_recovery_steps`
  at `recover.rs:647`, `RecoverableBraidLabeled` arm at `:687-707`.
  Pattern-match on the new `enroll_key_file` field. When `Some(kf)`,
  prepend two commands **before** the existing
  `BtrfsDeviceScanForget` / `WipefsBtrfs` / `BtrfsDeviceAdd` triple:
  - `CmdRequest::CryptsetupLuksAddKeyFile { device: by_id,
    key_file_path }`
  - `CmdRequest::CryptsetupLuksHeaderBackup { device: by_id,
    backup_path }` (path under `plan.luks_headers_dir` matching the
    FreshLuks renderer at `:723-730`).

  The order matches the executor exactly: enrollment + header backup
  run before `pool_add_device(force=true)` at `:2152-2153`, and that
  one call expands to `BtrfsDeviceScanForget` -> `WipefsBtrfs` ->
  `BtrfsDeviceAdd` internally (`pool.rs:19-50`). Putting addKey /
  headerBackup ahead of scanForget keeps `recover --dry-run` byte-
  aligned with replay; placing them between `WipefsBtrfs` and
  `BtrfsDeviceAdd` would falsely show wipefs running before
  enrollment. When `enroll_key_file: None`, the rendered command list
  is unchanged.
- **Executor** -- `execute_add_pool_mutation_recovery` at
  `recover.rs:2030`, `RecoverableBraidLabeled` arm at `:2116-2154`.
  The existing flow already enforces (a) LUKS UUID match at
  `:2120-2135`, (b) `ensure_luks_open` at `:2136-2143`, (c)
  `verified_pool_fsid` match at `:2144-2151`, (d) `pool_add_device`
  at `:2152-2153`. Insert a new step between (c) and (d): if
  `enroll_key_file: Some(kf)`, call `ensure_keyfile_enrolled(runner,
  &target.by_id.0, passphrase.expose_secret(), kf)?` followed by
  `luks::backup_luks_header(runner, &target.by_id.0,
  &target.mapper_name, params.paths)?`. Passphrase is already resolved
  and verified at the top of `execute_add_pool_mutation_recovery`
  (around `:2095-2104`) -- reuse it. Verifying-everything-before-mutation
  preserves the existing identity-mismatch -> error -> journal-
  preserved contract.

Reuse helpers:
`ensure_keyfile_enrolled` (`recover.rs:1966`),
`recover_passphrase_for_context` (`:2367`),
`verify_replace_fresh_prep_passphrase` (`:2384`),
`luks::backup_luks_header`. No new recovery helpers required.

### 6. Reused helpers (no new code)

- `crate::luks::check_key_slot` (`luks.rs:907`) -- pub.
- `crate::luks::verify_key_file` (`luks.rs:870`) -- pub.
- `crate::luks::enroll_key_file` (`luks.rs:883`) -- pub.
- `crate::luks::backup_luks_header_post_mutation` (`luks.rs:505`) -- pub.
- `crate::luks::backup_luks_header` -- used by the recovery enrollment
  path (matches `recover.rs:2487-2492`).
- `crate::luks::LUKS_SLOT_KEYFILE`, `KeySlotState`, `VerifyOutcome` -- pub.
- `crate::luks::format_keyfile_asymmetry_warning` -- unchanged; existing
  asymmetry warnings for `--enroll absent` still fire.
- `crate::probe::probe_config_disk` -- used in the new replace
  recovery identity probe to fetch the on-disk `luks_uuid` for
  comparison against the journaled value. (The add executor already
  uses this at `recover.rs:2120-2135` for its existing identity check.)
- `recover::ensure_keyfile_enrolled` (`recover.rs:1966`) -- existing
  recovery helper, called from the new enrollment-bearing recovery arms.
- `recover::recover_passphrase_for_context` (`recover.rs:2367`) and
  `recover::verify_replace_fresh_prep_passphrase` (`recover.rs:2384`)
  -- existing FreshLuks credential helpers, now also called from the
  new `ReplaceJournalMode::ExistingLuks` recovery path.

## Critical files

- `cli/src/enroll_key_file.rs` -- extract `plan_single_disk_enrollment`,
  expose `EnrollmentPlanMode` + `DiskEnrollAction` as `pub(crate)`.
- `cli/src/replace.rs` -- planner, render_steps, execute, journal target
  builder.
- `cli/src/add.rs` -- planner, target structs, render_steps, execute,
  journal target builder (`recoverable_journal_target` and the verified
  closed-target constructor at `add.rs:784-791` both extended to thread
  `enroll_key_file`; no new mechanism, the existing Pass-1 pre-mutation
  flow already journals closed targets uniformly).
- `cli/src/journal.rs` -- new fields on `ReplaceJournalMode::ExistingLuks`
  and `AddJournalMode::RecoverableBraidLabeled`. Roundtrip tests.
- `cli/src/recover.rs` -- two separate updates:
  - `ReplaceJournalMode::ExistingLuks` arm at `:2439`: gains LUKS-UUID
    identity probe, credential resolution + verification (mirroring
    FreshLuks at `:2464-2492`), and enrollment + header-backup replay.
  - `AddJournalMode::RecoverableBraidLabeled`: identity probe and
    credential checks already exist (`:2120-2135`, top-of-function
    passphrase resolution at `:2095-2104`); update both the preview
    renderer (`render_add_pool_mutation_recovery_steps` at `:687-707`)
    and the executor (`execute_add_pool_mutation_recovery` at
    `:2116-2154`) in the same change so dry-run preview matches replay.
- `flake.nix` -- register each new VM test attribute under
  `checks.<system>` so `just test-vm` and `nix flake check` actually run
  them. Per `docs/testing.md:24`, unregistered `tests/cli/*.nix` files
  do not run. Match the existing registration pattern for
  `replace-new-already-luks`, `braid-add-enroll`, etc.

## Implementation order (commit-friendly slices)

1. **Extract helper.** `plan_single_disk_enrollment` in
   `enroll_key_file.rs`; rewrite `plan_enrollment`'s loop to call it. No
   behavior change. Pin with a unit test that covers all three outcomes
   (AlreadyEnrolled, NeedsEnroll, Err on slot-1-occupied).
2. **Schema additions.** Add `enroll_key_file: Option<PathBuf>` to
   `ReplaceJournalMode::ExistingLuks` and to
   `AddJournalMode::RecoverableBraidLabeled` (no new variants). Update
   roundtrip fixtures. Recovery and call sites still pass `None`
   everywhere.
3. **Replace integration.** Extend planner, render_steps, execute,
   journal builder. New unit test:
   `replace_work_plan_existing_luks_with_enroll_renders_addkey_and_backup`.
   Recovery hook update: identity probe (LUKS UUID match) +
   `recover_passphrase_for_context` + `verify_replace_fresh_prep_passphrase`
   + enrollment replay + header backup. New recovery unit tests:
   - identity-mismatch preserves the journal (bad replug);
   - bad-passphrase preserves the journal and aborts before any LUKS
     mutation;
   - happy path replays enrollment + header backup, then clears the
     journal.
4. **Add integration.** Same shape as 3. Thread `enroll_key_file`
   through `RecoverableBraidTarget` (planning + the runtime verified
   target at `add.rs:784-791`), `recoverable_journal_target` (line
   1398), `render_steps`, and the post-`write_journal` mutation phase
   in `execute`. No new journal-write mechanism -- both `OpenRecoverable`
   and `ClosedPresentLuks` paths already feed the existing single
   upfront `write_journal` call at `add.rs:847`. New unit tests for
   both work-plan paths. **Update both add recovery sites in the
   same change**: the preview renderer in
   `render_add_pool_mutation_recovery_steps` (`recover.rs:687-707`)
   and the executor in `execute_add_pool_mutation_recovery`
   (`recover.rs:2116-2154`). New tests:
   - dry-run preview test: `recover --dry-run` for a journal carrying
     `RecoverableBraidLabeled { enroll_key_file: Some(_) }` lists
     `cryptsetup luksAddKey` and `cryptsetup luksHeaderBackup`
     **before** `btrfs device scan --forget`, `wipefs`, and
     `btrfs device add` -- mirroring the executor's pre-`pool_add_device`
     insertion point.
   - executor test: replay path enrolls + backs up header before
     `pool_add_device`;
   - LUKS UUID mismatch already covered by existing tests; preserved.
5. **VM tests + flake.nix registration.** For each new test, add the
   `.nix` and `.py` files under `tests/cli/` AND register the attribute
   in `flake.nix` `checks.<system>` (mirror the existing entries for
   `replace-new-already-luks`, `braid-add-enroll`, etc.). Verify
   registration by running `nix flake show` and confirming each new
   attribute appears.

   - `tests/cli/replace-enroll-existing-luks.{nix,py}`: pre-format a fresh
     disk with `cryptsetup luksFormat`, run `replace --enroll DIR`,
     assert keyfile in slot 1 + auto-unlock works.
   - `tests/cli/replace-enroll-existing-luks-slot-conflict.{nix,py}`:
     pre-format with a stale key in slot 1, assert refusal with
     remediation text and no journal write.
   - `tests/cli/add-enroll-recoverable.{nix,py}`: build pool, lock,
     remove disk1 from membership, re-add with `--enroll DIR` against
     the recoverable disk that has empty slot 1; assert enrollment.
   - Idempotent re-enroll: same scenario but slot 1 already has the
     correct keyfile; assert `[skip]` and no mutation.
   - `tests/cli/recover-replace-existing-luks-enroll.{nix,py}`: kill
     `braid replace --enroll` between journal write and pool mutation
     against a `PresentLuks` target; `braid recover` replays enrollment
     after identity probe matches; reboot confirms auto-unlock.
   - `tests/cli/recover-replace-existing-luks-uuid-mismatch.{nix,py}`:
     same setup, but swap the disk between crash and recover so the
     LUKS UUID no longer matches the journal; assert recovery refuses,
     preserves the journal, and emits the expected remediation
     wording.

## Verification

- `just test-rust` -- new unit tests for the helper, both render_steps
  paths, both journal roundtrips.
- `nix flake show | grep <new-test>` -- confirm each new VM test is
  registered in `flake.nix` `checks.<system>` before relying on
  `just test-vm`.
- `just test-vm replace-enroll-existing-luks
  replace-enroll-existing-luks-slot-conflict add-enroll-recoverable
  recover-replace-existing-luks-enroll
  recover-replace-existing-luks-uuid-mismatch` -- the new VM tests above.
- `just test-vm replace-new-already-luks replace-preview-warnings
  braid-add-enroll braid-add-warnings recover-replace-not-started
  recover-replace-completed` -- existing tests still pass (regression
  net for the changed code paths, including journal schema roundtrips).
- Manual sanity check on a 3-disk pool VM: `replace --enroll /tmp` against
  a pre-formatted disk; reboot; confirm auto-unlock opens the new disk
  silently alongside the others.

## Out of scope

- The "operator passed `--enroll` and every target was already authorized"
  case: `plan_single_disk_enrollment` returns `AlreadyEnrolled` and the
  flow proceeds without an enrollment step. No new info note today; if
  later feedback shows confusion, add a `[info]` note in a follow-up.
- The existing `format_keyfile_asymmetry_warning` (fires when fresh-format
  target + no `--enroll` + pool already has keyfile) is unchanged.
