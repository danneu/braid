# Plan: refuse `braid recover` for Replace journals on externally-mounted pools

## Context

`cmd_recover` mounts the pool when needed and runs a "scrub the kernel state"
cycle (`wait_for_kernel_replace_to_finish` + `relock_and_remount`) so that
`probe_pool` reads on-disk topology rather than the stale in-memory
`btrfs_fs_devices` that a kernel-resumed `dev_replace` leaves behind. The
cycle is gated at `cli/src/recover.rs:332-347` with `if just_mounted`.

The comment justifying the skip (lines 313-331, introduced by commit
`21011ab`) ends with: *"The bug only manifests on the mount session that
triggered the kernel resume, which is one we just opened ourselves."* That
sentence is a load-bearing claim, and it is incomplete. The bug manifests on
**whichever** mount session triggered `btrfs_resume_dev_replace_async`. When
the pool is already mounted at recover entry, that prior mount is **not**
ours.

We also cannot detect the staleness from userspace post-resume: after a
kernel-resumed replace completes, `btrfs replace status` reports
`ReplaceState::Finished` ("Started on ..., finished on ...") -- identical
output to a normally-completed replace started on the current mount
session. Even a non-`None` status does not prove the mounted view of
`fs_devices` is fresh, because the in-memory list is what's stale, not the
kernel's idea of "is a replace currently running."

The typical reach into this gap is closed by braid's existing pending-op
preflights: `braid unlock` and `braid-auto-unlock.service` both call
`preflight::check_no_pending_operation` (`cli/src/unlock.rs:155`) and
refuse to mount when a journal exists. The flock at
`/run/braid-pool.lock` (ADR 018) blocks concurrent braid operations.

The residual gap is administrative: an operator who manually opens LUKS and
mounts the pool (circumventing braid) after a crash can leave recover
running against a stale-state mount. Since we cannot safely `umount` an
externally-held mount (EBUSY risk) and we cannot detect the staleness
post-resume, the only safe remediation is to refuse and direct the operator
to `braid lock; braid recover`. `braid lock` works because it has no
pending-op preflight (`cli/src/lock.rs`).

The intended outcome:

- `plan_recover` fails fast when journal is `OpKind::Replace` and the pool is
  already mounted, telling the operator to `braid lock` first.
- The failure-path notes (entry banner + `AlreadyMounted` info note) are
  preserved so the existing render-on-failure pattern surfaces them.
- The misleading sentence in the comment block at lines 328-331 is
  rewritten to explain the planner-level gate as a precondition.
- Dry-run (`--dry-run`) shows the refusal too, since the gate sits in
  `plan_recover` upstream of the dry-run / real-run branch.
- Three docs that currently advertise "recover reuses an existing mount"
  are updated to call out the Replace-specific refusal and the
  `braid lock; braid recover` escape hatch.

## Scope

**In scope:**

- One new branch in `plan_recover` at `cli/src/recover.rs`.
- Convert the existing `matches!(journal.op, ...)` at
  `cli/src/recover.rs:558` to `matches!(&journal.op, ...)` so both
  `matches!` sites in `plan_recover` use by-reference (defense against
  any future partial-move compile failure).
- One comment-block rewrite at `cli/src/recover.rs:328-331`.
- One new unit test in the existing `mod tests` block, exercising both
  `dry_run = false` and `dry_run = true`.
- Rewrite of the existing `recover_replays_resize_after_replace` test
  (`cli/src/recover.rs:3972`) so it exercises the cycle path
  (`just_mounted == true`) using the existing `StatefulMockFs`
  (`cli/src/recover.rs:1285`) + `MapperClosingRunner`
  (`cli/src/recover.rs:1332`) pattern.
- Doc updates in `manual/commands/recover.md`,
  `manual/guides/recovery-scenarios.md`, and
  `docs/decisions/017-runtime-disk-membership.md`.

**Out of scope:**

- Changes to Add / Remove / RemoveMissing recovery (no kernel-resume bug).
- Changes to the `just_mounted == true` cycle (already correct).
- New VM tests. The unit test fully covers the planner contract; the
  scenario is fundamentally administrative and existing M3 / M11 VM tests
  already cover the sanctioned recover paths.
- Any change to `RecoverError` variants or public CLI surface.

## Changes

### 1. `cli/src/recover.rs` -- planner-level fail-fast in `plan_recover`

Insert a new branch in `plan_recover` (defined at `recover.rs:437`)
between the `let open_plan = match report.result { ... };` extraction
(current `recover.rs:503-511`) and the `// Build dry-run steps.` comment
(current `recover.rs:513`):

```rust
// Refuse Replace recovery on an already-mounted pool. The cycle that
// scrubs stale in-memory btrfs_fs_devices after a kernel-resumed
// dev_replace requires a clean umount-and-remount that we cannot safely
// perform when an external process holds the mount (EBUSY risk). The
// staleness is also undetectable from userspace post-resume: btrfs
// replace status reports `Finished` for both a normally-completed
// replace and a kernel-resumed replace, so we cannot use it to tell
// whether the in-memory fs_devices view is fresh.
//
// Operator's recovery path: `braid lock` (works with a journal present
// -- no pending-op preflight in lock.rs) then `braid recover`, which
// opens its own mount and takes the just_mounted == true cycle path.
if open_plan.is_none() && matches!(&journal.op, journal::OpKind::Replace { .. }) {
    return RecoverPlanReport {
        notes,
        result: Err(RecoverError::Failed(
            "recover refuses to probe an already-mounted pool when the journal \
             records a replace -- the kernel may have resumed an interrupted \
             dev_replace on this mount session, leaving stale in-memory device \
             state that probe_pool cannot distinguish from real topology.\n\n\
             To recover safely, fully cycle the mount yourself first:\n  \
             sudo braid lock\n  sudo braid recover\n\n\
             braid lock works with a pending-operation journal and unmounts + \
             closes LUKS, after which braid recover opens a fresh mount session \
             and clears the staleness via the relock cycle."
                .to_owned(),
        )),
    };
}
```

`&journal.op` (rather than `journal.op`) keeps the read by-reference. The
existing `matches!(journal.op, journal::OpKind::Replace { .. })` at
`recover.rs:558` (in the dry-run step guard) currently compiles because
the `{ .. }` pattern binds nothing, but adding more uses of `journal` in
`plan_recover` makes by-reference the safer convention. **Also change
`recover.rs:558` to `matches!(&journal.op, journal::OpKind::Replace { .. })`**
in the same edit so both call sites are consistent and any future code
that reads `journal` after this point is guaranteed not to hit a
partial-move compile failure.

The `notes` Vec at this point holds `[Info(entry_banner),
Info("pool already mounted at /mnt/storage")]` -- the entry banner is
pushed at `recover.rs:465`, and the `report.events` -> `PreviewNote` loop
runs at `recover.rs:477`. This is the standard preserved-context shape
consumed by `cmd_recover`'s render-on-failure path
(`render_notes_for_stderr_with(&report.notes, ...)` at `recover.rs:754`).

The branch sits upstream of the `--dry-run` already-mounted reconciliation
block (starts at `recover.rs:522` with the comment "Pool is already
mounted -- run the same read-only reconciliation"), so dry-run also sees
the refusal without first calling `probe::probe_pool` on the suspect
mount.

### 2. `cli/src/recover.rs:328-331` -- rewrite the comment paragraph

Replace the misleading "the bug only manifests..." paragraph with one that
admits the planner-level gate as a precondition. Lines 313-327 (the bug
mechanism description) stay verbatim; only the trailing paragraph changes.

New wording for lines 328-331:

```
// Skipped when the pool was already mounted before recover started: we
// don't know who's using that mount and umount could fail with EBUSY.
//
// Pre-condition for reaching this branch with `just_mounted == false`:
// the planner-level fail-fast in `plan_recover` rejects the
// (already-mounted, OpKind::Replace) combination outright, directing
// the operator to `braid lock; braid recover`. So when execute()
// reaches an already-mounted pool here, the journal op is Add /
// Remove / RemoveMissing -- none of which trigger the kernel
// dev_replace resume bug -- and skipping the cycle is sound.
```

### 3. New unit test: `plan_recover_refuses_replace_on_externally_mounted_pool`

Add to the `mod tests` block at `cli/src/recover.rs`, sibling to
`plan_recover_dry_run_stepful_already_mounted` (`recover.rs:4487`).
Reuses existing helpers: `replace_journal()` (`recover.rs:3903`),
`mountpoint_ok()` (project-wide test helper), `format_recover_entry()`.

```rust
/* Intent: when the journal records OpKind::Replace and the pool is already
 * mounted at planner entry, plan_recover MUST return RecoverError::Failed
 * with safe-recovery instructions, preserving the entry banner and
 * AlreadyMounted info note on report.notes. The refusal must fire for
 * both dry_run = false (real run) and dry_run = true (preview); the gate
 * sits upstream of that branch, so a regression that affects only one of
 * the two would still be a real regression.
 *
 * Why it exists: kernel-resumed btrfs_resume_dev_replace_async on a session
 * braid did not open leaves stale in-memory fs_devices that probe_pool
 * cannot distinguish from real topology. The mount cycle that scrubs this
 * state is gated on just_mounted == true (recover.rs:332-347), and an
 * admin-mounted pool takes the just_mounted == false path. Without this
 * fail-fast, recover would silently corrupt pool.json from stale topology.
 *
 * Scenario: post-crash, an admin ran `cryptsetup open` + `mount(8)`
 * directly (circumventing braid's pending-op preflight on `unlock`), then
 * invoked `braid recover`.
 */
```

Test body shape:

1. Build a `replace_journal()`; persist it via `journal::write_journal`.
2. Construct `MockRunner` with **only** the `mountpoint_ok()` mock.
   `plan_open_pool` short-circuits to `Ok(None)` before any per-disk probe,
   so no `BtrfsFilesystemShow` / `CryptsetupStatus` mocks are needed. Any
   subsequent `probe_pool` call would surface as `MissingMock` -- proving
   the fail-fast fires before probing.
3. Run the planner twice, with `dry_run = false` and `dry_run = true`,
   asserting the same outcome on both:
   - `report.result` is `Err(RecoverError::Failed(msg))` whose `msg`
     contains the substrings `"already-mounted"`, `"sudo braid lock"`,
     and `"sudo braid recover"`.
   - `report.notes.len() == 2`:
     - `notes[0]` is `PreviewNote::Info` with payload equal to
       `format_recover_entry(&journal)`.
     - `notes[1]` is `PreviewNote::Info` whose payload contains
       `"pool already mounted at /mnt/storage"`.

Two invocations rather than one because the `params.dry_run` branch is
downstream of the gate and a regression that wired the refusal only behind
`dry_run = true` (or only behind `dry_run = false`) would otherwise pass.

### 4. Rewrite `recover_replays_resize_after_replace` (`recover.rs:3972`)

The existing test pairs an `OpKind::Replace` journal with `mountpoint_ok()`
(already-mounted) and expects `cmd_recover` to succeed and replay
`pool_resize_device`. Under the new gate this combination is refused. The
test predates `21011ab`'s cycle and exercises a path that has no sanctioned
real-world reach today.

Rename to `recover_replays_resize_after_replace_via_mount_cycle`. Keep the
core assertion (resize-on-new-disk is replayed) but exercise it through the
cycle path using the established cycle-test infrastructure
(`StatefulMockFs` at `recover.rs:1285`; `MapperClosingRunner` at
`recover.rs:1332`) and the working pattern in
`recover_with_all_mappers_open_still_resolves_credential_for_cycle` at
`recover.rs:2147`.

The concrete API names below are cross-referenced from that working test
when implementing. Copy the exact helper invocations from there rather
than guessing -- the patterns below describe the *shape*, not literal API
names.

Wiring requirements:

- **`MapperClosingRunner` has no constructor.** Build it with the same
  struct-literal form used in
  `recover_with_all_mappers_open_still_resolves_credential_for_cycle`
  (`MapperClosingRunner { inner, fs_paths: fs_handle, closed:
  std::sync::Mutex::new(closed0) }`).
- **`closed0` must list every initially-closed mapper.** `probe_config_disk`
  emits `CryptsetupStatus` for every union LUKS member (`disk1`, `old`,
  `new`), and `MapperClosingRunner` returns its inactive-status stub from
  `closed`'s set rather than reaching the inner runner. Initialize the
  HashSet with `"braid-disk1"`, `"braid-old"`, `"braid-new"`; successful
  `CryptsetupLuksOpen` calls remove entries from `closed` and add their
  mapper paths to `fs_paths` (the relevant `match request { ... }` arm in
  `MapperClosingRunner::run` is at `recover.rs:1358` and surrounding).
- **`StatefulMockFs`** seeded with the union by-id paths
  (`/dev/disk/by-id/virtio-disk1`, `virtio-old`, `virtio-new`). Copy
  seeding pattern from
  `recover_with_all_mappers_open_still_resolves_credential_for_cycle`.
- **Credential**: `passphrase_file = Some(<tempfile containing
  "testpass">)` so the cycle's reopen can re-read the same passphrase.
  Copy verbatim from
  `recover_with_all_mappers_open_still_resolves_credential_for_cycle`.
- **MockRunner mock chain** -- use the existing helpers (`mountpoint_fail()`
  applied via `with_output(...)`; the device-scan-all helper
  (`CmdRequest::BtrfsDeviceScanAll`), mount-ok helper, etc., from the
  same neighbor test). Required entries in order:
  1. mountpoint check that returns failure -> `plan_open_pool` returns
     `Some(open_plan)` and recover takes the `just_mounted == true` path.
  2. Initial mount chain: per-union-mapper `CryptsetupTestPassphrase`
     stdin mock (the credential verifier `execute_unlock_and_mount` runs
     before each open), then `CryptsetupLuksOpen` per union mapper, the
     project's device-scan-all command, mount-ok.
  3. `BtrfsReplaceStatus` returning `ReplaceState::Finished` --
     stdout `"Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, \
     0 write errs, 0 uncorr. read errs\n"` (matches the parser's
     `Finished` arm in `cli/src/parse/btrfs_replace_status.rs` at the
     `if stdout.contains("finished on")` check). Not `None` -- the
     realistic post-resume status is `Finished`.
  4. `relock_and_remount` cycle: umount, the scoped scan-forget command,
     `CryptsetupClose` per union mapper, then for each reopen:
     `CryptsetupTestPassphrase` stdin mock followed by
     `CryptsetupLuksOpen` (the cycle reopen also goes through
     `execute_unlock_and_mount`, so credential verification runs again),
     then the device-scan-all command, mount-ok.
  5. Post-cycle `probe_pool`: `BtrfsFilesystemShow` returning
     `btrfs_show_disk1_and_new()` (existing helper at `recover.rs:3943`).
     This is a 2-disk topology (`disk1` devid 1 + `new` devid 2), which
     is exactly what `replace_journal()`'s `target_membership = {disk1,
     new}` models -- a 3-device mock would contradict the assertion.
     Then `CryptsetupStatus` + `CryptsetupLuksUuid` for `disk1` and `new`
     using the existing `cryptsetup_status_active` and
     `cryptsetup_uuid_ok` helpers (`recover.rs:1628` and `:1641`).
  6. `replay_post_mutation` mocks: the resize-on-devid command for the
     new disk's devid (devid 2 from `btrfs_show_disk1_and_new`), and a
     `CmdRequest::BtrfsBalanceRaid1Soft` mock (returned by the existing
     `ok_raw_empty("btrfs balance start")` pattern) for the post-replace
     soft balance.

Look up the exact `CmdRequest` variant names by inspecting
`recover_with_all_mappers_open_still_resolves_credential_for_cycle` and
the existing `recover_replays_resize_after_replace` when copying. The
project uses `CmdRequest::BtrfsDeviceScanAll` (not `BtrfsDeviceScan`),
`MockRunner::default()` (not `::new()`), and `mountpoint_fail()` returns
a `(CmdRequest, RawCommandOutput)` tuple applied via `with_output(...)`
-- never assume API names from this plan.

Assertions on the rewritten test:

- `cmd_recover` returns `Ok(())`.
- The resize-on-new-disk replay was issued for devid 2.
- `pool.json` matches `target_membership = {disk1, new}` (no `old`).
- `pending-op.json` cleared.

A grep for `OpKind::Replace` and `replace_journal` across `recover.rs`
confirms this is the only test that pairs Replace with `mountpoint_ok()`,
so it is the only test that needs rewriting.

### 5. Doc updates

The Replace-specific refusal flips a behavior three docs currently
advertise as "recover reuses an existing mount":

- **`manual/commands/recover.md`** -- step 3 in "What happens under the
  hood" (line 70) currently reads "Opens LUKS devices and mounts the pool
  (or reuses the existing mount if already mounted)." Add a note that
  Replace journals refuse the reuse path and instruct
  `braid lock; braid recover`. Add a new bullet to "Safety checks"
  (around line 79) listing the refusal.

- **`manual/guides/recovery-scenarios.md`** -- the discover-vs-recover
  table at line 11-14 describes recover as "Opens pool, probes live
  topology, ...". Add a short subsection covering the externally-mounted
  Replace case, with the `braid lock; braid recover` remediation.

- **`docs/decisions/017-runtime-disk-membership.md`** -- the Recovery
  bullet at line 65 says recover "opens LUKS devices and mounts the pool
  if needed (using the union of pre/target membership from the journal)".
  Add a sentence: "When the pool is already mounted by an external
  process (circumventing `braid unlock`'s pending-op preflight) and the
  journal records `OpKind::Replace`, recovery refuses and directs the
  operator to `braid lock; braid recover` so a fresh mount session can be
  opened and the relock cycle can clear the kernel-resumed-dev_replace
  staleness."

All three updates use `--` (not em-dash) per AGENTS.md and reference the
exact escape-hatch invocation.

## Verification

1. **Unit tests:** `just test-rust` -- the new test passes; the rewritten
   test passes; no other recover test regresses. Compilation guards the
   `&journal.op` partial-move fix (a regression to `journal.op` would fail
   to build).
2. **Targeted unit tests:**
   ```
   cargo test -p braid-cli plan_recover_refuses_replace_on_externally_mounted_pool
   cargo test -p braid-cli recover_replays_resize_after_replace_via_mount_cycle
   ```
3. **Existing forced-shutdown VM matrix:** `just test-vm
   recover-replace-completed ups-lb-during-replace` -- both should pass
   unchanged. They mount via braid (just_mounted == true), so the cycle
   path covers them and the new gate never fires.
4. **Doc spot-check:** `mdbook build` or equivalent for the manual; verify
   the recover page renders the new note and safety bullet.
5. **Manual smoke (optional, in a VM):**
   - Set up a pool, start a `braid replace`, kill the process mid-flight.
   - Reboot.
   - In a privileged shell, manually `cryptsetup open` each member and
     `mount /dev/mapper/braid-... /mnt/storage` (circumventing braid).
   - Run `braid recover`. Confirm it fails with the new wording and exits
     non-zero.
   - Run `braid lock; braid recover`. Confirm it succeeds.

## Critical Files

- `/Users/dan/Code/braid/cli/src/recover.rs` -- the only Rust file edited
  (planner gate + comment rewrite + two test changes).
- `/Users/dan/Code/braid/manual/commands/recover.md` -- step 3 + safety
  bullet.
- `/Users/dan/Code/braid/manual/guides/recovery-scenarios.md` -- new
  subsection for the externally-mounted Replace case.
- `/Users/dan/Code/braid/docs/decisions/017-runtime-disk-membership.md`
  -- Recovery bullet (line 65).
- `/Users/dan/Code/braid/cli/src/journal.rs` -- read-only reference for
  `OpKind::Replace` shape (`old_name`, `new_name`, `new_by_id`).
- `/Users/dan/Code/braid/cli/src/mount.rs` -- read-only reference for
  `plan_open_pool` returning `Ok(None)` on already-mounted and the
  `AlreadyMounted` -> `PreviewNote::Info` conversion.
- `/Users/dan/Code/braid/cli/src/lock.rs` -- read-only verification that
  `braid lock` has no pending-op preflight (the user's escape hatch).
- `/Users/dan/Code/braid/cli/src/parse/btrfs_replace_status.rs` --
  read-only reference for the `Finished` arm wording used by the
  rewritten test mock.
- `/Users/dan/Code/braid/AGENTS.md` -- CLI output style (`--` not
  em-dash, test block-comment three-section form).

## Risk Assessment

- **Blast radius:** narrow. One new branch, one comment rewrite, one new
  unit test, one rewritten unit test, three doc files. No public API
  change, no new error variants.
- **False-positive risk:** low. The refused combination has no sanctioned
  real-world reach: every scripted braid path either takes the cycle
  (`just_mounted == true`) or is gated by `check_no_pending_operation`. The
  refusal points the operator to `braid lock; braid recover`, which is the
  proven-correct path.
- **False-negative risk:** intentional. The gate is Replace-specific
  because the kernel-resume bug is dev_replace-specific. Add / Remove /
  RemoveMissing on already-mounted pools remain permitted -- they have no
  equivalent stale-state mode.
- **Dry-run parity:** identical, by construction (gate sits upstream of
  the dry-run branch in `plan_recover`); the test exercises both branches
  to pin this.
- **Backward compatibility:** none required (braid is unreleased, per
  AGENTS.md "No backwards compatibility").
