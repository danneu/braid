# Two-Phase Apply (LUKS Pre-Phase)

**Status: Active**

## Context

After a reboot, all LUKS mappers are closed and the btrfs pool is unmounted. The planner runs after probe, which sees no open mappers and no mounted pool. This causes two problems:

1. **Misleading plan display**: `braid plan` shows `mkfs.btrfs -f (may run)` for disks that are actually returning pool members. The execute-time superblock check prevents data loss, but the plan output is alarming for routine re-mounts.

2. **Mount failure after reboot**: Per-device `btrfs device scan <device>` doesn't reliably assemble multi-device pools. Even after opening all LUKS mappers and scanning each device individually, `mount` can fail with "missing members" because the kernel's btrfs subsystem hasn't been told about all members atomically. `btrfs device scan` (no arguments) scans all block devices and reliably assembles multi-device pools.

## Decision

Move LUKS opening into a **pre-phase** that runs before plan generation. The sequence changes from:

```
old:  checkpoint check → probe → plan → checkpoint → execute
new:  checkpoint check → luks_prephase → probe → plan → checkpoint → execute
```

The `luks_prephase` function:

1. **Opens closed LUKS mappers** — iterates config disks, skips absent and already-open, reads `BRAID_PASSPHRASE` lazily (only when the first closed mapper is encountered).
2. **Scans all** — runs `btrfs device scan` (no arguments) to register all open btrfs members with the kernel.
3. **Mounts pool** — if the mount point is not already mounted, finds the first open mapper and attempts mount. Missing-members errors are tolerated (not all disks may be available). Hard errors propagate.

After the pre-phase, probe sees accurate state: all available mappers are open, the pool is mounted (if all members are present) or truly empty (bootstrap). The plan has no OPEN_LUKS actions for available disks, and `is_bootstrap` is accurate.

### Resume with closed LUKS

If `braid apply --resume` detects closed LUKS mappers (device exists but `/dev/mapper/<name>` does not), the checkpoint is invalidated and `fresh_apply` is called instead. This handles the case where a checkpoint was created pre-reboot and the system has since rebooted. The pre-phase in `fresh_apply` opens LUKS and re-probes, generating a correct plan.

This avoids the complexity of reconciling a stale checkpoint against post-reboot state. The `ActionState` state machine doesn't allow `Pending → Completed`, so marking pre-reboot work as completed would require weakening type safety.

### BtrfsDeviceScanAll

A new `CmdRequest::BtrfsDeviceScanAll` variant runs `btrfs device scan` with no arguments, scanning all block devices. This replaces per-device scans in the pre-phase and in the `execute_btrfs_add` bootstrap-with-existing-btrfs path.

## Pre-Phase Side-Effect Policy

After the pre-phase, LUKS is open and the pool is mounted **even if the planner subsequently returns `Blocked`**. This is a change from the old invariant:

- **Old**: Blocked = no mutations.
- **New**: Blocked = no *planned* mutations, but LUKS/mount happened as a pre-condition for accurate planning.

This is correct operationally — the pool was supposed to be online — but operators should be aware that `braid apply` with a blocked plan still opens LUKS and mounts the pool. The passphrase is consumed before the plan is generated.

## Alternatives Considered

### Reconcile stale checkpoint on resume

Walk the old checkpoint, detect which actions completed pre-reboot (by checking mapper/mount state), and mark them completed. Rejected because:
- `ActionState::transition_to` doesn't allow `Pending → Completed`
- Complex reconciliation logic with risk of incorrect state detection
- Simpler to invalidate and re-plan since pre-phase makes re-planning cheap

### Keep LUKS opening in the execute phase

Could add `btrfs device scan` (no args) to the execute phase. Rejected because the planner would still see inaccurate state, generating misleading plans with `mkfs.btrfs (may run)` for returning pool members.

### Dry-run LUKS open (check without opening)

Could probe LUKS UUIDs without opening to give the planner hints. Rejected as more complex than just opening — LUKS needs to be open anyway, so doing it early is simpler.
