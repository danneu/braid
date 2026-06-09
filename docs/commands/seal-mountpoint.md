---
experimental: true
---
[← braid](../index.md)

# braid seal-mountpoint

{{#include ../_includes/experimental-command-callout.md.inc}}

Set the immutable attribute (`chattr +i`) on the pool mountpoint while it is
unmounted, so a process that writes the path before the pool mounts fails loudly
with `EPERM` instead of silently landing data on the root filesystem (which the
pool then hides when it mounts over it).

You rarely run this by hand: the NixOS module runs the bare form automatically
from the `braid-seal-mountpoint` boot/activation unit. The explicit-path forms
are maintenance levers. See
[ADR 028](../design/decisions/028-immutable-unmounted-mountpoint.md).

## When to use it

- **Re-seal now** after the doctor reports the mountpoint is mutable while
  offline (instead of waiting for the next boot or `nixos-rebuild switch`).
- **Seal a separate-path subvolume mountpoint** the boot seal does not cover
  (e.g. `/var/lib/jellyfin/media` -- see [Mounting subvolumes](../guides/mounting-subvolumes.md)).
- **Clear an orphaned old mountpoint** after changing `braid.mountPoint`.

## Basic example

Seal the configured mount point (what the boot unit runs):

```
sudo braid seal-mountpoint
```

## Common variations

Seal a specific directory (e.g. an offline subvolume mountpoint):

```
sudo braid seal-mountpoint /var/lib/jellyfin/media
```

Clear the immutable attribute on a path (e.g. an orphaned old mount point):

```
sudo braid seal-mountpoint --unseal /mnt/old-storage
```

## Important flags

| Flag       | Purpose                                                                        |
| ---------- | ------------------------------------------------------------------------------ |
| `PATH`     | Seal a specific directory instead of the configured mount point                |
| `--unseal` | Clear the immutable attribute instead of setting it (requires `PATH`)          |

## What happens under the hood

1. Opens the path as a directory (a non-directory is refused).
2. Confirms the path is **not** currently a mountpoint (via `statx`'s
   `STATX_ATTR_MOUNT_ROOT`, on the same file descriptor, so a racing mount cannot
   cause it to seal a live filesystem root). A live mountpoint is skipped.
3. Sets (or, with `--unseal`, clears) `FS_IMMUTABLE_FL` on the bare directory's
   inode. The attribute is persistent -- it survives unmount and reboot.

The pool mounts normally over a sealed directory; the mounted filesystem's own
root governs writes.

## Exit codes

- **Bare form (`braid seal-mountpoint`)** is best-effort and always exits 0 --
  boot must not fail on it. Problems are logged as warnings to the journal.
- **Explicit forms (`braid seal-mountpoint <path>` / `--unseal <path>`)** report
  an honest desired-state exit code: exit 0 only if the path actually ends up in
  the requested state (immutable for seal, mutable for unseal), non-zero
  otherwise -- so a manual seal that silently failed to protect a path is visible.

## Error handling

- `--unseal` **refuses the currently configured mount point** -- it must stay
  sealed while the pool is offline. Change `braid.mountPoint` first, then unseal
  the old path.
- `--unseal` acquires the pool lock and fails fast if another braid operation is
  in progress, so it cannot interleave with an `unlock` that would remount over
  the path mid-operation.
- A live mountpoint is never touched (the explicit forms report this as a
  failure).
- If the root filesystem does not support the immutable attribute, the bare form
  logs one clear warning and protection is unavailable (rare -- only non-NAS
  roots like vfat/9p/nfs).

## Related commands

- [braid doctor](doctor.md) -- warns when the offline mountpoint is mutable
- [braid lock](lock.md) -- take the pool offline (the seal persists across the cycle)
- [braid unlock](unlock.md) -- mount the pool over the sealed directory
