# Plan: harden recovery against post-cycle "pool unmounted" probe

## Context

`RecoverCompletion::execute` (`cli/src/recover.rs:533-637`) probes the live
pool via `probe::probe_pool(runner, fs, &plan.mount_point)` at the start of
the completion phase, then dispatches into one of six per-op completion
handlers. `probe_pool` short-circuits to `mounted: false, devices: vec![]`
whenever `mount_check::fstype_at_mount_via_fs` finds no
`/proc/self/mountinfo` entry for the configured `mount_point`
(`cli/src/probe.rs:217-227`).

The `GenericLivePool` path (`execute_generic_live_pool_recovery`,
`cli/src/recover.rs:945-1015`) -- used by all `OpKind::Remove` and by
bootstrap `OpKind::Add` -- has no defensive check against this empty state.
It calls `build_membership_from_live_pool` (which walks `pool.devices` only
and returns an empty `PoolMembership`), then unconditionally
`save_membership(&recovered, ...)` writes `{"disks": {}}`, and
`journal::clear_journal(...)` removes the pending op marker. The next
`braid status` reports no membership and `braid unlock` errors with
`MembershipError::NotFound`; the operator must run `braid discover --write`
to rebuild. This is a silent durability regression.

The sibling completion handlers happen to dodge the bug because they
already encode richer "live pool must match expected membership"
invariants (`execute_add_post_balance_recovery` line 2007's `if live !=
target`; `execute_remove_missing_*` line 2311+/2390+'s
`live_pool_matches_membership` calls; etc.). `validate_live_members_allowed`
(line 1680) does NOT defend against an empty pool because its loop body
iterates `pool.devices` and trivially exits.

Today's real-world flow happens to dodge the GenericLivePool gap because
the immediately-preceding `RecoverWorkAction::InitialOpenPool` or
`RecoverWorkAction::RemountCycle` errors loudly when mount fails. The
window the finding identifies is the TOCTOU gap between that successful
mount and the post-cycle probe: external `umount(8)`, kernel btrfs
auto-remount-ro followed by full unmount, or a `mount_point` mismatch
between config and the actual mount target. The blast radius is high
(silent loss of pool.json membership) and the cost of a guard is small.

## Fix

Add one invariant check at the shared probe call site
(`cli/src/recover.rs:542`), immediately after `probe_pool` returns and
before the match dispatch. Single check protects all six completion
variants; existing per-op guards remain as defense-in-depth.

### Code change

File: `cli/src/recover.rs`, function `RecoverCompletion::execute` (around
line 533).

```rust
fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    &self,
    plan: &RecoverWorkPlan,
    state: &RecoverExecutionState,
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    let pool = probe::probe_pool(runner, fs, &plan.mount_point)?;

    // Post-mount invariant. plan_recover only reaches the completion
    // phase after plan_open_pool either opened the pool (emitting
    // RecoverWorkAction::InitialOpenPool, which errors on mount failure)
    // or observed the pool already mounted (open_plan = None). Either
    // way, by the time this fresh probe runs, probe_pool MUST see a
    // mounted btrfs pool with members. If it does not, an external
    // actor (umount, btrfs auto-remount-ro, mount_point mismatch) has
    // moved the pool out from under us -- failing closed here preserves
    // pool.json and the journal so the operator can investigate and
    // retry.
    if !pool.mounted
        || (pool.devices.is_empty() && !plan.union.disks.is_empty())
    {
        return Err(RecoverError::Failed(format!(
            "recovery aborted: post-mount probe at {} reports {} -- \
             expected a mounted pool with members. pool.json was not \
             written and the pending-op journal is preserved. \
             Investigate (external umount? btrfs auto-remount-ro? \
             mount_point mismatch?) and re-run braid recover.",
            plan.mount_point,
            if !pool.mounted {
                "no btrfs mount"
            } else {
                "zero btrfs devices"
            },
        )));
    }

    match self {
        // ... unchanged ...
    }
}
```

### Why this exact predicate

`!pool.mounted` catches the cited TOCTOU scenarios where
`/proc/self/mountinfo` has no entry for `plan.mount_point` -- the path
the finding's evidence trace identifies in `probe.rs:217-227`.

`pool.devices.is_empty() && !plan.union.disks.is_empty()` catches the
rarer-but-possible state where `mountinfo` reports the mount but
`BtrfsFilesystemShow` returns zero device rows (pathological corruption
of btrfs output). The `union` clause skips the (degenerate) case where
both pre and target membership are empty.

`RecoverError::Failed` matches the prevailing error idiom in this file
(see `execute_add_post_balance_recovery`, `execute_remove_missing_*`).
Returning before `save_membership` or `clear_journal` preserves the
journal so `braid recover` is retryable after the operator fixes the
underlying mount problem.

## Tests

Add **two** new tests next to the existing GenericLivePool tests in
`cli/src/recover.rs` (around line 12774, near
`cmd_recover_remove_with_genuinely_evicted_target_drops_membership`).
Each test pins one disjunct of the new guard predicate so neither
branch could be deleted without breaking a test.

### Test scaffolding to reuse

- `PoolFixture::empty()` (`cli/src/test_fixtures/shared.rs:257-268`) --
  starts with no `pool.json` seeded.
- `remove_2to1_journal_with_target_devid()`
  (`cli/src/recover.rs:12284-12312`) -- a Remove journal that routes
  through GenericLivePool.
- `mountpoint_ok()` (`cli/src/recover.rs:3893-3900`) -- runner mock for
  `CmdRequest::MountpointCheck` returning success, so planning sees the
  pool as already mounted and `RecoverWorkAction::InitialOpenPool` is
  not emitted.
- `resolver_for(...)` (`cli/src/recover.rs:3384-3397`) -- by-id resolver
  builder.

### What the tests exercise

The local `MockFs` (`cli/src/recover.rs:3251-3285`) hardcodes a mounted
btrfs entry for `/mnt/storage` in its `read_to_string` for
`/proc/self/mountinfo`.

Test 1 (`mounted=false` branch) needs a `MockFs` whose mountinfo does
NOT contain `/mnt/storage`, so that `mount_check::fstype_at_mount_via_fs`
returns `Ok(None)` and `probe_pool` returns `mounted: false, devices:
vec![]`. Implementation will either:

1. Add a constructor on the local `MockFs` (e.g.
   `MockFs::without_mounted_pool(devices)`) that returns mountinfo
   containing only a rootfs entry, mirroring the shared module's
   `MockFs::unmounted` (`cli/src/test_fixtures/shared.rs:70`), or
2. Reuse `MockFs::unmounted` from `test_fixtures/shared.rs` directly if
   it satisfies the recover test's other invariants.

Pick whichever is the minimum surface change once implementation starts.

Test 2 (`mounted=true && devices.is_empty()` branch) needs the default
`MockFs` (mountinfo reports `/mnt/storage` as btrfs) PLUS a
`BtrfsFilesystemShow` runner mock whose stdout parses to a non-`None`
FSID but an empty `devices` list (a `Total devices` line with no
following per-device rows). Check the existing `btrfs_show_*` helpers
in the test module for the closest template; if no helper produces
zero device rows, hand-roll the stdout fixture for this test only.

### Test body sketches

```rust
// Intent: post-cycle probe that finds the pool unmounted must abort
//   recovery without writing pool.json and without clearing the
//   journal.
// Why it exists: probe_pool returns mounted=false / devices=[] when
//   no /proc/self/mountinfo entry exists at the configured mount_point.
//   Without the guard at RecoverCompletion::execute, the
//   GenericLivePool path silently writes {"disks":{}} and clears
//   pending-op.json, destroying membership recovery state.
// Scenario: a Remove journal is pending, the planner sees the pool as
//   already mounted (open_plan = None, so InitialOpenPool is not
//   emitted), but between planning and completion the mountpoint has
//   disappeared (external umount or kernel remount-ro race).
//   probe_pool at the completion phase returns the empty-state
//   PoolState. The new guard must catch this.
#[test]
fn cmd_recover_aborts_when_post_cycle_probe_reports_unmounted() {
    let f = PoolFixture::empty();
    let fs = MockFs::without_mounted_pool(&[]); // see "What the tests exercise"

    let journal = remove_2to1_journal_with_target_devid();
    journal::write_journal(&f.paths, &journal).unwrap();

    // Planner sees pool as mounted via the runner so InitialOpenPool
    // is omitted; probe_pool at Complete then sees the fs disagree.
    let (mp_req, mp_out) = mountpoint_ok();
    let runner = MockRunner::default().with_output(mp_req, mp_out);

    let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
    let params = f.recover_params().passphrase_file(None).build();
    let err = cmd_recover(&runner, &fs, &resolver, &params)
        .expect_err("recover must fail when probe sees pool unmounted");

    let msg = format!("{err}");
    assert!(
        msg.contains("post-mount probe"),
        "error must name the probe state: {msg}"
    );

    assert!(
        !f.paths.pool_json().exists(),
        "pool.json must NOT be written when probe sees unmounted pool"
    );
    assert!(
        f.paths.pending_op_json().exists(),
        "journal must be preserved when probe sees unmounted pool"
    );
}

// Intent: post-cycle probe that sees a mounted btrfs filesystem but
//   zero btrfs devices must abort recovery without writing pool.json
//   and without clearing the journal.
// Why it exists: probe_pool can return mounted=true / devices=[] when
//   mountinfo reports the mount yet `btrfs filesystem show` parses to
//   an FSID with no device rows (pathological btrfs output). The
//   second disjunct of the guard predicate -- `pool.devices.is_empty()
//   && !plan.union.disks.is_empty()` -- exists only for this case;
//   without this test, that branch could be silently deleted.
// Scenario: a Remove journal is pending, the planner sees the pool as
//   already mounted, and `BtrfsFilesystemShow` returns parseable
//   stdout with an FSID line but zero `/dev/mapper` device rows.
//   probe_pool returns mounted=true with empty devices. The new guard
//   must catch this.
#[test]
fn cmd_recover_aborts_when_post_cycle_probe_reports_zero_devices() {
    let f = PoolFixture::empty();
    let fs = MockFs::new(&[]); // default mountinfo: /mnt/storage is btrfs

    let journal = remove_2to1_journal_with_target_devid();
    journal::write_journal(&f.paths, &journal).unwrap();

    let (mp_req, mp_out) = mountpoint_ok();
    let runner = MockRunner::default()
        .with_output(mp_req, mp_out)
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint("/mnt/storage".into()),
            },
            // Stdout: an FSID line so probe_pool's fsid assertion
            // passes, but zero `devid ... path /dev/mapper/...` rows.
            // Hand-roll if no existing helper matches.
            btrfs_show_zero_devices(),
        );

    let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
    let params = f.recover_params().passphrase_file(None).build();
    let err = cmd_recover(&runner, &fs, &resolver, &params)
        .expect_err("recover must fail when probe sees zero btrfs devices");

    let msg = format!("{err}");
    assert!(
        msg.contains("zero btrfs devices"),
        "error must name the zero-device state: {msg}"
    );

    assert!(
        !f.paths.pool_json().exists(),
        "pool.json must NOT be written when probe sees zero devices"
    );
    assert!(
        f.paths.pending_op_json().exists(),
        "journal must be preserved when probe sees zero devices"
    );
}
```

### Test scope deliberately narrow

These tests pin the guard, not the full TOCTOU race (which would
require dynamic MockFs state). That is acceptable for unit coverage:
the guard is structure-insensitive and the tests verify the durability
invariants the finding identified -- `pool.json` is not written and
`pending-op.json` is preserved. Together the two tests cover both
disjuncts of the predicate, so neither branch can be deleted without a
test failure.

## Docs

Add one bullet to the `## Safety checks` section of
`manual/commands/recover.md` (current section starts at line 83) so the
new failure mode is documented in the same place as recover's other
journal-preserving aborts:

> - Refuses to overwrite `pool.json` or clear `pending-op.json` if the
>   post-mount probe at the configured mount point sees the pool
>   unmounted or with zero btrfs devices (mount could have been removed
>   externally between recover's mount step and its membership probe).
>   `pool.json` and `pending-op.json` are both preserved -- investigate
>   the mount, then re-run `braid recover`.

No `README.md` change: the README is the cookbook-style user guide and
recover's per-failure-mode language already lives in the manual.

## Critical files

| Path                                                | Why                                                                  |
| --------------------------------------------------- | -------------------------------------------------------------------- |
| `cli/src/recover.rs:533-637`                        | Add the guard inside `RecoverCompletion::execute` after line 542.    |
| `cli/src/recover.rs:945-1015`                       | The function whose silent failure motivated the finding; no change. |
| `cli/src/probe.rs:212-307`                          | `probe_pool` is unchanged; documents the empty-state return.        |
| `cli/src/recover.rs` (test module, ~12774, ~3251)   | Add two new tests + (likely) extend the local `MockFs` constructor + (likely) add a `btrfs_show_zero_devices` helper. |
| `manual/commands/recover.md`                        | Append one bullet to `## Safety checks`.                            |

## Verification

1. `just test-rust` -- runs Rust unit tests including both new tests
   and all existing recover tests. Both new tests must pass; no
   existing test may regress.
2. Spot-check that no existing test regressed: in particular review
   tests around `cmd_recover_remove_*` (lines 12343-12803) and
   `recover_remount_cycle_*` (around 9450) to confirm none of them
   relied on the previously-silent empty-pool behavior.
3. `cargo build` to confirm no compilation issues.
4. Manually re-read the updated `manual/commands/recover.md` to
   confirm the new bullet sits cleanly with the existing safety-check
   list and uses the same wording style.

No NixOS VM test is needed: this is a pure guard at a known choke
point, exercised end-to-end by the new unit tests. `just test-vm` and
`just test-parsers` remain unchanged.

## Risks / non-goals

- The fix does NOT remove the per-op richer guards in sibling completion
  handlers (`live_pool_matches_membership`, `add_targets_all_live`,
  `if live != target`). Those continue to validate operation-specific
  invariants. The new guard is a baseline.
- The fix does NOT address probe_pool's behavior itself -- probe_pool
  is correct to report the empty state; the bug is in the consumer.
- This fix does NOT attempt to re-mount or recover from the
  disappeared-mount case; the operator is asked to investigate and
  retry. Auto-recovery is out of scope and risky.
- No changes to user-facing CLI surface beyond the error string and
  the new safety-check bullet in `manual/commands/recover.md`.
