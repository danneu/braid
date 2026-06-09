---
experimental: true
---
[← braid](../index.md)

# braid discover

{{#include ../_includes/experimental-command-callout.md.inc}}

Scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels, reads their LUKS UUIDs, and reconstructs UUID-keyed pool membership. This is a repair tool for recovering a lost or corrupt `pool.json`.

## When to use it

- Your `pool.json` was deleted or corrupted.
- You're migrating disks to a new machine and need to rebuild pool state.

The normal path for adding disks is `braid add`. Use `discover` only when `pool.json` is missing or corrupt -- it refuses to run while a valid `pool.json` exists. To see the disks already in a healthy pool, use [`braid status`](status.md).

## Basic example

When `pool.json` is missing, preview the membership `discover` would rebuild before saving it (no changes):

```
sudo braid discover
```

Output:

```
  ironwolf = /dev/disk/by-id/ata-ST12000VN0008_XXXXXXXX
  toshiba = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_XXXXXXXX
pass --write to save to /var/lib/braid/pool.json
```

Bare `discover` prints this preview only when `pool.json` is absent. Over a valid `pool.json` it exits with an error -- use [`braid status`](status.md) to view current membership. Over a corrupt `pool.json` it also refuses, pointing you to `discover --write` (see [Common variations](#common-variations)).

The membership rows are written to stdout; the `pass --write to save` hint, the
`--write` "pool membership written" confirmation, scan warnings, and errors go
to stderr. So `braid discover > members` (or `braid discover | grep <disk>`)
captures only the rows.

## Common variations

Write the discovered membership to pool.json:

```
sudo braid discover --write
```

If you can name the expected member count ahead of time, pass it as a
fail-closed guard against a detached disk or stray braid-labeled disk:

```
sudo braid discover --write --expect-count 3
```

## Flags

| Flag | Effect |
| --- | --- |
| `--write` | Persist the discovered membership to `pool.json` |
| `--expect-count <N>` | With `--write`, refuse to write if the discovered member count is not exactly `N` |

## What happens under the hood

1. With `--write`, refuses if a pending operation journal (`pending-op.json`) exists. Bare `discover` is read-only and skips this gate.
2. Refuses over an existing UUID-keyed `pool.json` (bare and `--write`). A corrupt or off-schema `pool.json` is the documented rebuild path: bare `discover` prints the rebuild remediation, and `discover --write` writes a forensic `pool.json.corrupt-<RFC3339-UTC>` snapshot adjacent to the new file, then rebuilds. If the snapshot cannot be written (full disk, read-only state directory), `discover --write` refuses rather than destroy the corrupt original.
3. Reads all entries in `/dev/disk/by-id/` in sorted filename order, skipping partition entries (e.g., `ata-TOSHIBA-part1`). Sorting up front makes label-collision reporting (step 10) independent of `read_dir` order.
4. Resolves each by-id symlink to its canonical kernel device. Skips with a `cannot canonicalize` warning when the symlink is dangling (e.g., udev didn't clean up after a disk removal).
5. For each entry, runs `cryptsetup isLuks` to check if it's a LUKS device.
6. Runs `cryptsetup luksDump` to read the LUKS label, version, and UUID.
7. Skips LUKS1 devices (braid requires LUKS2).
8. Matches labels of the form `braid-<name>` and extracts the disk name.
9. Uses the canonical kernel device resolved above to detect multiple `/dev/disk/by-id/` symlinks for the same physical disk (i.e. `wwn-` and `ata-` aliases), then picks the most stable one (preference order: wwn > nvme > scsi > ata > usb > other, with lexicographic tie-breaking).
10. If two symlinks that share the same `braid-<name>` label resolve to different kernel devices, refuses the entire scan with an error. Two physically distinct disks share a label -- typically after a `dd` clone or a manual mislabel -- and braid cannot safely choose one. Relabel or detach one disk before retrying.
11. If two distinct devices share one LUKS UUID, refuses the entire scan. This usually means a cloned disk is attached.
12. With `--write`, saves the discovered UUID-keyed membership to `pool.json`.

## Safety checks

- Refuses any operation on an existing UUID-keyed `pool.json`. Corrupt or off-schema files are allowed for `--write` rebuild only; the original is copied to `pool.json.corrupt-<RFC3339-UTC>` before overwrite, and `--write` refuses if that snapshot cannot be written (full disk, read-only state directory). Run with all intended pool members attached; see `docs/internals/luks-unlock.md`.
- With `--write`, refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- With `--write`, refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- With `--expect-count`, refuses to write if the discovered member count is not exactly the requested count.
- Without `--write`, makes no changes at all -- read-only scan that takes no pool lock and does not consult the pending-op journal.
- Dangling `/dev/disk/by-id/` symlinks are skipped with a warning -- a diagnostic operators need when udev leaves a stale alias behind after a disk swap.
- LUKS1 devices are skipped with a warning.
- If no braid-labeled LUKS2 devices are found, `discover` exits 1 with `no braid-labeled LUKS2 devices found -- ...` (both bare and `--write`) -- check the intended members are attached, readable, and labeled `braid-<name>` as LUKS2. An array that is entirely LUKS1, detached, or unreadable lands here, with any present-but-skipped disk warned about above.
- Refuses the scan if two distinct devices share the same `braid-<name>` LUKS label -- relabel or detach one disk before retrying.
- Refuses the scan if two distinct devices share the same LUKS UUID -- detach the cloned or unintended disk before retrying.

## Related commands

- [recover](recover.md) -- resume an interrupted operation (has its own membership rebuild from live pool state)
- [status](status.md) -- view current pool membership
