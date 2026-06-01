---
intent: Record why braid pins `-O block-group-tree` explicitly when running
  `mkfs.btrfs`. Read before changing pool-creation flags or bumping
  btrfs-progs in nixpkgs.
status: Active
---

# Decision: Pin `block-group-tree` at mkfs time


## Context

braid pins its toolchain to nixos-26.05's btrfs-progs 6.19.1, whose default
mkfs feature set enables `block-group-tree`. braid requests that one feature
bit explicitly rather than inheriting it from the default, so the on-disk
feature set is determined by braid and not by whichever btrfs-progs the running
toolchain links. (The flag predates the 26.05 bump: under the older
nixos-25.11 btrfs-progs 6.17.1, which did *not* default `block-group-tree`, the
same flag made new pools forward-compatible with the 6.19 default.)

This pin is deliberately narrow. `mkfs.btrfs` still starts from the linked
btrfs-progs default feature set; braid only adds `block-group-tree` to that
set. The rest of the on-disk feature set continues to track btrfs-progs
defaults.

## Decision

`cli/src/cmd.rs` passes `-O block-group-tree` on both `mkfs.btrfs`
invocations: single-disk bootstrap and RAID1 bootstrap. New pools carry the
`block-group-tree` bit explicitly -- matching the btrfs-progs 6.19 default that
braid's pinned toolchain ships -- without freezing any other mkfs default.

The long form is preferred over the `bgt` alias because it is the documented
primary name and matches the kernel sysfs entry `block_group_tree`.

## Where this is enforced

- `cli/src/cmd.rs` -- `MkfsBtrfs` and `MkfsBtrfsRaid1` build the
  `mkfs.btrfs` argv with `-O block-group-tree`.
- `cli/src/cmd.rs` -- `mkfs_btrfs_single_generates_correct_argv` and
  `mkfs_btrfs_raid1_generates_correct_argv` assert the exact argv.
- `tests/module/mkfs-block-group-tree.{nix,py}` -- VM coverage asserts the
  on-disk feature bit after `braid add` creates single-disk and RAID1 pools.

## Notes

- `block-group-tree` is a `compat_ro` feature. The kernel rejects unsupported
  `compat_ro` bits for read-write mount but may still allow a read-only mount
  if no log replay is required. The kernel-side feature has been available
  since 6.1; NixOS 26.05 ships kernel 6.18, so normal braid read-write
  operation is always supported.
- Existing pools created before this pin are unaffected. Offline conversion is
  possible via `btrfstune --convert-to-block-group-tree`; braid does not wrap
  that.
- Forward-compat note: a rescue boot from very old live media (kernel <6.1)
  cannot read-write mount a `block-group-tree` pool. A read-only mount may
  still succeed if no log replay is needed. This is not a blocker because braid
  does not ship rescue media, but the constraint should stay visible.
