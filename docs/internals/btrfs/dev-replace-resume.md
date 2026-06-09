---
intent: "Record btrfs dev_replace resume-on-mount behavior and the recover relock cycle for braid maintainers. Read before changing related behavior or docs."
status: Active
---
# btrfs dev_replace resume-on-mount and the recover relock cycle

## Background

A `btrfs replace` interrupted mid-flight by an unclean crash leaves the
on-disk `dev_replace_item` in `STARTED`. On the next mount, the kernel sees
that state and resumes the replace from the on-disk cursor.

For braid, this matters during `braid recover`: the command may be the first
thing to mount the pool after the crash, so it is also the thing that triggers
the kernel's resume-on-mount path.

## Kernel resume-on-mount behavior

`btrfs_resume_dev_replace_async` runs as a detached kthread. `umount` does not
wait for that worker.

The worker commits the post-completion devid swap to disk correctly, but it
does not update the in-memory `btrfs_fs_devices` for the mount session that
triggered the resume. A probe taken from that session reads stale topology: a
phantom `MISSING` devid 0 plus both the source and target devices. In the
captured failure, that meant five device entries and `braid status` reporting
`DEGRADED` even though every disk was online.

## Why the LUKS close+reopen is load-bearing

The important empirical result is narrower than "remount after replace":
`umount + btrfs device scan --forget + remount` is not enough if the dm devices
stay alive. That cycle can leave the cached `fs_devices` attached to the live dm
devices, so the next probe still sees the stale topology from the original
resume-triggering mount.

Only tearing down and recreating the dm devices forces the kernel to re-read the
chunk tree from disk and build a fresh `fs_devices` that reflects the
post-resume on-disk state.

## What braid recover does

Recover splits this into two explicit work actions:
`RecoverWorkAction::WaitForKernelReplace` and
`RecoverWorkAction::RemountCycle`.

First, `cli/src/recover.rs#wait_for_kernel_replace_to_finish` polls
`btrfs replace status` until the kernel reports `Finished` or no replace is in
progress. `Running` is intentionally unbounded because interrupting the kernel
worker would strand the same recovery problem. `Suspended` or unparseable output
fails closed and preserves the journal.

Then `cli/src/recover.rs#relock_and_remount` runs the full relock cycle:
`umount`, `btrfs device scan --forget`, close the LUKS membership union, reopen
the pool through the standard `plan_open_pool` flow, and remount through the
standard executor. The second mount sees the completed on-disk replace with a
fresh `fs_devices`.

## Coverage

`tests/repro/btrfs-replace-interrupted-mid-flight.py` pins the unclean-kill
path end-to-end. It starts a real `braid replace`, kills the VM mid-flight,
boots again, runs `braid recover`, and asserts that the resumed replace drains,
`pool.json` swaps in the new disk, the old disk is evicted, and a later
`braid lock; braid unlock` cycle stays clean.

## Path B: v6.19+ freeze/signal cancellation

The unclean-kill repro does not cover the v6.19+ freeze and signal cancellation
path inside the btrfs replace worker loop. An unclean kernel kill bypasses the
in-loop `try_to_freeze` and `fatal_signal_pending` checks entirely.

A sibling repro test is needed when kernel >= 6.19 reaches NixOS stable. Its
sequencing depends on whether `braid replace` should inhibit suspend for the
operation's duration; that policy question is orthogonal to the crash path this
note documents.

## See also

- [recover command](../../commands/recover.md#what-happens-under-the-hood)
- [recovery scenarios](../../guides/recovery-scenarios.md#recover-for-a-replace-journal-when-the-pool-is-already-mounted)
- [ADR 012: Intent CLI](../../design/decisions/012-intent-cli.md)
