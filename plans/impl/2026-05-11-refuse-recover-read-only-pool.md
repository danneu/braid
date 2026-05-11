# Plan: refuse recover when pool is mounted read-only

## Context

`braid recover` runs after an interrupted `add` / `remove` / `replace` /
`remove-missing`. The completion path probes the live pool, writes a
fresh `pool.json` from the kernel-visible membership, calls
`replay_post_mutation` (which issues `pool_balance_resume` and/or
`pool_balance_raid1_soft` for Add), and then clears the journal.

`probe_pool` only inspects the `fstype` field of `/proc/self/mountinfo`,
so it cannot tell a writable pool from a read-only one. When the kernel
auto-remounts the btrfs superblock read-only on metadata corruption /
EIO / ENOSPC, recover:

1. Sees a "healthy" probe (mounted btrfs with devices).
2. Overwrites `pool.json` with the live membership.
3. Calls `replay_post_mutation`, where `btrfs balance` returns the bare
   stderr `ERROR: ... Read-only file system`.
4. Surfaces a confusing balance error with no signal that RO is the
   root cause, and no pointer to `btrfs check` / `mount -o remount,rw`.

The journal IS preserved today (because `clear_journal` only runs after
a successful `replay_post_mutation`), but `pool.json` has already been
rewritten and the operator gets no diagnostic naming the actual problem.

The verified finding (see `feature-findings/recover.md`) proposed
"skip replay + clear journal." That direction is wrong for Add
recovery: it discards the only structured record of the still-pending
balance work. The correct shape is to refuse early -- match the
existing fail-closed gate at `cli/src/recover.rs:548-562`, which
already handles "no mount" and "zero devices" with the same
"pool.json was not written and the pending-op journal is preserved"
contract.

## Approach

Extend the existing fail-closed gate in `RecoverCompletion::execute`
with a third refusal: pool is mounted read-only. Refuse uniformly
across op kinds (even Remove, whose `replay_post_mutation` is a
no-op, because a RO pool is an abnormal state and recovery should
not declare itself complete on it). Mirror the same refusal in the
dry-run already-mounted reconciliation path so preview agrees with
execute.

Read-only is detected with `mount_check::mount_entry_at_via_fs` and a
`,`-split exact-token check against both `vfs_options` (field 6,
VFS-level per-mount flags) and `fs_options` (field 11,
superblock/filesystem options). Both fields must be inspected
because a btrfs superblock can be marked RO without the VFS flag
also being set (the kernel auto-remount-ro on I/O errors is the
canonical case); the converse is not symmetric, and we do NOT
claim source attribution from field state alone. This is exactly
the contract in the auto-memory
`~/.claude/projects/-Users-dan-Code-braid/memory/feedback_mountinfo_vfs_vs_fs_options.md`,
and `preflight.rs` already implements it via a module-private
`has_ro` helper.

Reuse, do not copy: promote the predicate next to its parser in
`mount_check.rs` and have `preflight` call it. This keeps the
field-semantics knowledge in one module and lets `recover` import it
without depending on `preflight`.

## Implementation steps

### 1. Promote the RO predicate to `mount_check`

`cli/src/mount_check.rs` (just after `mount_entry_at_via_fs` at line
188): add

```rust
/// True if either mountinfo option field marks the mount read-only.
/// Field 6 (vfs_options) carries VFS-level per-mount flags;
/// field 11 (fs_options) carries superblock/filesystem options.
/// Both can independently carry `ro` (e.g. kernel auto-remount-ro
/// on metadata errors sets only the superblock flag), so both
/// must be checked. The field that carries `ro` is state
/// evidence, not source attribution.
pub(crate) fn entry_is_read_only(entry: &MountEntry) -> bool {
    has_ro(&entry.vfs_options) || has_ro(&entry.fs_options)
}

fn has_ro(opts: &str) -> bool {
    opts.split(',').any(|opt| opt.trim() == "ro")
}
```

`cli/src/preflight.rs` (line 273): replace
`has_ro(&entry.vfs_options) || has_ro(&entry.fs_options)` with
`mount_check::entry_is_read_only(&entry)`. Delete the private
`has_ro` at `preflight.rs:282-284`. The four preflight RO tests
(`preflight.rs:763-880`) keep passing because the behavior is
identical.

### 2. Add the RO refusal to `RecoverCompletion::execute`

`cli/src/recover.rs`, immediately after the existing `probe_pool`
call at line 542 and BEFORE the existing fail-closed gate at lines
548-562:

```rust
let pool = probe::probe_pool(runner, fs, &plan.mount_point)?;

match mount_check::mount_entry_at_via_fs(fs, plan.mount_point.as_str()) {
    Ok(Some(entry)) if mount_check::entry_is_read_only(&entry) => {
        return Err(RecoverError::Failed(format!(
            "recovery aborted: pool at {mp} is mounted read-only \
             (vfs_options={:?}, fs_options={:?}) -- btrfs may have \
             auto-remounted the superblock after an I/O error, or \
             an operator may have remounted it. pool.json was not \
             written and the pending-op journal is preserved. \
             Investigate with `btrfs check` and remount read-write \
             with `mount -o remount,rw {mp}`, then re-run braid \
             recover.",
            entry.vfs_options, entry.fs_options, mp = plan.mount_point
        )));
    }
    Ok(Some(_)) | Ok(None) => {} // not RO, or no entry -- fall to existing gate
    Err(e) => return Err(RecoverError::Probe(ProbeError::MountInfo(e))),
}

// existing fail-closed gate at 548-562 follows unchanged
```

The RO check runs before the mount/devices gate so a test can use
the simple `btrfs_show_zero_devices()` fixture without needing
cryptsetup mocks. Unmounted pools fall through to the existing gate
on `Ok(None)`. A mountinfo `Err` (IO, malformed line, or duplicate
target) is fail-closed: it propagates as
`RecoverError::Probe(ProbeError::MountInfo(_))`, matching the same
error shape `probe_pool` itself uses on a mountinfo read failure
(see `probe.rs:1493`, `probe.rs:1512`). `probe_pool` reads
mountinfo separately, so a race window where the kernel rewrites
mountinfo between the two reads is real -- silently dropping `Err`
here would let recover continue toward `save_membership` on an
unverified mount state.

### 3. Mirror in the dry-run already-mounted path

`cli/src/recover.rs:1211-1232` (the `plan_recover` block that
re-probes for the already-mounted reconciliation). After the
`probe_pool` call at line 1220 and BEFORE
`validate_live_members_allowed`:

```rust
match mount_check::mount_entry_at_via_fs(fs, mount_point.as_str()) {
    Ok(Some(entry)) if mount_check::entry_is_read_only(&entry) => {
        return Err(PlanFailure::with_notes(
            notes,
            RecoverError::Failed(format!(
                "recover dry-run: pool at {mp} is mounted read-only \
                 (vfs_options={:?}, fs_options={:?}) -- execute \
                 would refuse. Investigate with `btrfs check` and \
                 remount read-write with `mount -o remount,rw {mp}` \
                 before re-running braid recover. pool.json and the \
                 pending-op journal are unchanged.",
                entry.vfs_options, entry.fs_options, mp = mount_point
            )),
        ));
    }
    Ok(Some(_)) | Ok(None) => {}
    Err(e) => {
        return Err(PlanFailure::with_notes(
            notes,
            RecoverError::Probe(ProbeError::MountInfo(e)),
        ));
    }
}
```

`PlanFailure::with_notes` preserves the entry banner and any
`PreviewNote`s already accumulated in `notes` (including the
`AlreadyMounted` info note added by `plan_open_pool` for this
branch) so the rendered preview matches the rest of the dry-run
failure shapes in this function. Mountinfo `Err` propagates the
same way -- never silently swallowed.

### 4. Wording invariants

- Both messages use `--` (double hyphen), per `AGENTS.md` CLI Output
  Style.
- Both messages reuse the existing-gate phrasing
  "pool.json was not written and the pending-op journal is
  preserved" verbatim (execute path) or
  "pool.json and the pending-op journal are unchanged" (dry-run
  path).
- Both cite `btrfs check` AND `mount -o remount,rw <mp>` explicitly
  so the operator has both the diagnostic and the remount lever.
- Both include the captured `vfs_options` / `fs_options` snapshots
  so the operator can see which field carries `ro`. This is state
  evidence (which kernel-reported flag is set), not source
  attribution -- the message body lists both operator-issued
  remount and kernel auto-remount-ro as hypotheses without
  claiming the field state alone identifies the cause.

## Test plan

Three new tests in `cli/src/recover.rs` covering both refusal
sites (real-run execute path and dry-run `plan_recover` path) and
both option fields where `ro` can appear.

First, add two MockFs constructors next to `MockFs::new`
(`recover.rs:3277`) and `MockFs::without_mounted_pool` (line 3286):

```rust
fn with_mounted_pool_ro_vfs(paths: &[&str]) -> Self {
    Self {
        paths: paths.iter().map(|s| s.to_string()).collect(),
        mountinfo:
            "36 35 0:32 / /mnt/storage ro shared:1 - btrfs \
             /dev/mapper/braid-disk1 rw\n".to_owned(),
    }
}

fn with_mounted_pool_ro_fs(paths: &[&str]) -> Self {
    Self {
        paths: paths.iter().map(|s| s.to_string()).collect(),
        mountinfo:
            "36 35 0:32 / /mnt/storage rw shared:1 - btrfs \
             /dev/mapper/braid-disk1 ro,space_cache=v2\n".to_owned(),
    }
}
```

### Tests for the real-run refusal in `RecoverCompletion::execute`

Both mirror `cmd_recover_aborts_when_post_cycle_probe_reports_zero_devices`
(`recover.rs:12412`): `PoolFixture::empty`, a remove journal
(`remove_2to1_journal_with_target_devid`), `mountpoint_ok` +
`btrfs_show_zero_devices` mocks, assertions on message content and
`pool.json` / `pending_op_json` paths. Placement: right after line
12447.

- `cmd_recover_aborts_when_post_mount_probe_reports_vfs_read_only`
  -- `ro` lands in field 6 (vfs_options). Asserts msg contains
  `mounted read-only` and `remount,rw`; asserts
  `!pool_json.exists()` and `pending_op_json.exists()`.
- `cmd_recover_aborts_when_post_mount_probe_reports_fs_read_only`
  -- `ro` lands in field 11 (fs_options) alongside
  `space_cache=v2`. This is the superblock-level RO state that
  escapes a VFS-only check (the kernel auto-remount-ro case that
  motivates the fix). Same assertions.

Both tests use the existing `btrfs_show_zero_devices()` fixture
because the RO refusal fires before the device-count gate -- no
cryptsetup mocks needed.

### Test for the dry-run refusal in `plan_recover`

Mirrors the dry-run scaffolding of
`plan_recover_refuses_replace_on_externally_mounted_pool`
(`recover.rs:13156`) but, unlike that test, must mock
`BtrfsFilesystemShow`. The Replace refusal sits upstream of the
dry-run `probe_pool` call (lines 1193-1209), so it only needs
`mountpoint_ok`; the RO refusal in this plan sits AFTER
`probe_pool` (line 1220), which reads `BtrfsFilesystemShow` after
fstype detection (`probe.rs:238`). With a mounted-btrfs
mountinfo, the show command runs; if not mocked, the test fails
on a missing-mock before reaching the RO refusal.

Decision 022 (`docs/decisions/022-dry-run-preview-model.md`)
requires the preview to refuse anything execute would refuse, so
this test pins the preview/execute agreement.

- `plan_recover_dry_run_refuses_already_mounted_read_only_fs_options`
  -- uses `MockFs::with_mounted_pool_ro_fs(&[])` and a
  `MockRunner::default()` populated with
  `mountpoint_ok` plus `BtrfsFilesystemShow { mount_point:
  "/mnt/storage" }` -> `btrfs_show_zero_devices()` (same
  device-show fixture as the real-run tests; zero devices keeps
  `probe_pool` from hitting any cryptsetup commands). Calls
  `plan_recover(..., dry_run = true)`, expects
  `PlanFailure { notes, error }`. Asserts:
  - `failure.notes.len() == 2`.
  - `failure.notes[0]` is the entry banner returned by
    `format_recover_entry(&journal)`.
  - `failure.notes[1]` is the `PreviewNote::Info("pool already
    mounted at /mnt/storage")` from the AlreadyMounted ProbeEvent.
  - `failure.error` is `RecoverError::Failed(msg)` where `msg`
    contains `mounted read-only`, `recover dry-run`, and
    `remount,rw`.

The error-message assertions are what pin the refusal -- only the
new RO check produces this exact `RecoverError::Failed` body, so a
regression that bypassed the gate would surface as a missing
substring rather than as a mock miss.

One dry-run test covering the fs_options case is sufficient: the
predicate `entry_is_read_only` is exercised on both fields by the
two real-run tests, and the dry-run wiring (PlanFailure shape,
note preservation) is the only thing that needs separate coverage
here.

Run `just test-rust` to confirm all three new tests pass and the
four existing preflight RO tests still pass against the relocated
`entry_is_read_only` helper.

## Critical files

- `cli/src/recover.rs` -- two refusal sites (execute + dry-run),
  two MockFs constructors, three new tests.
- `cli/src/mount_check.rs` -- add `entry_is_read_only` and a private
  `has_ro`.
- `cli/src/preflight.rs` -- call the relocated helper, delete the
  duplicate `has_ro`.

## Verification

End-to-end:

1. `just test-rust` -- the three new tests pass; the four existing
   `check_not_read_only` tests at `preflight.rs:763`+ still pass
   against the relocated helper.
2. Spot-grep: `git grep -n "fn has_ro"` returns exactly the
   private definition inside `mount_check.rs`; no duplicates.
3. VM-level confirmation (optional, not required to land): add a
   one-shot integration to the recover VM test that, after the
   forced-shutdown scaffold seeds a pending journal, remounts
   `/mnt/storage` `ro` and runs `braid recover`, expecting the new
   error string and an intact `pool.json` + journal. Not in scope
   if the unit tests above already pin the contract.

## Non-goals / out-of-scope

- Do NOT add a `read_only: bool` field to `PoolState`. The 46
  `PoolState { .. }` construction sites would each need a default
  value and the structural change is broader than what this finding
  alone justifies. The same RO state can be queried directly via
  `mount_entry_at_via_fs` at the two recover call sites.
- Do NOT fix the sister finding for `braid status`
  (`feature-findings/status.md:70-73`). That work is a separate
  recommendation: `build_status` should call
  `mount_check::mount_entry_at_via_fs` and surface the read-only
  state in the human banner and `StatusReport` JSON. The helper
  relocated in step 1 (`entry_is_read_only`) is the seam status
  will reuse.
- Do NOT touch `replay_post_mutation`
  (`recover.rs:1602-1682`). The refusal sits one frame up; balance
  command behavior is unchanged.
- Do NOT introduce a `PreviewNote::Warn` for RO. The decision is
  refuse-early, not warn-and-proceed: a RO pool means recover's
  post-mutation work cannot complete, and proceeding would
  corrupt the operator's mental model of what `braid recover`
  guarantees.
- Do NOT modify the existing fail-closed gate's wording at
  `recover.rs:554-561`. The new check is additive and uses the
  same voice; the gate stays in place to cover unmounted /
  zero-device paths.
