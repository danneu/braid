---
intent: Record why braid pins `-O block-group-tree` explicitly when running
  `mkfs.btrfs`. Read before changing pool-creation flags or bumping
  btrfs-progs in nixpkgs.
status: Active
---

# Decision: Pin `block-group-tree` at mkfs time


## Context

btrfs-progs 6.19 (2026-02-13) flips the `block-group-tree` feature to be
on by default in `mkfs.btrfs`. Without an explicit pin, the on-disk feature
set of new pools varies silently across nixpkgs bumps.

## Decision

`cli/src/cmd.rs` passes `-O block-group-tree` on both `mkfs.btrfs`
invocations: single-disk bootstrap and RAID1 bootstrap. The unit tests in the
same file assert the exact argv, and the VM test
`braid-module-mkfs-block-group-tree` asserts the resulting on-disk bit.

The long form is preferred over the `bgt` alias because it is the documented
primary name and matches the kernel sysfs entry `block_group_tree`.

## Notes

- `block-group-tree` is a `compat_ro` feature. The kernel rejects unsupported
  `compat_ro` bits for read-write mount but may still allow a read-only mount
  if no log replay is required. The kernel-side feature has been available
  since 6.1; NixOS 25.11 ships 6.12 and 26.05 ships 6.18, so normal braid
  read-write operation is always supported.
- Existing pools created before this pin are unaffected. Offline conversion is
  possible via `btrfstune --convert-to-block-group-tree`; braid does not wrap
  that.
- Forward-compat note: a rescue boot from very old live media (kernel <6.1)
  cannot read-write mount a `block-group-tree` pool. A read-only mount may
  still succeed if no log replay is needed. This is not a blocker because braid
  does not ship rescue media, but the constraint should stay visible.
