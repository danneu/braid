[← Manual](../index.md)

# braid discover

Scans `/dev/disk/by-id/` for LUKS devices with `braid-*` labels, reads their LUKS UUIDs, and reconstructs UUID-keyed pool membership. This is a repair tool for recovering a lost or corrupt `pool.json`.

## When to use it

- Your `pool.json` was deleted or corrupted.
- You're migrating disks to a new machine and need to rebuild pool state.
- You want to verify which braid-labeled LUKS devices the system can see.

The normal path for adding disks is `braid add`. Use `discover` when `pool.json` is missing or corrupt.

## Basic example

Preview discovered membership (no changes):

```
sudo braid discover
```

Output:

```
  ironwolf = /dev/disk/by-id/ata-ST12000VN0008_XXXXXXXX
  toshiba = /dev/disk/by-id/ata-TOSHIBA_MN08ACA16T_XXXXXXXX
pass --write to save to /var/lib/braid/pool.json
```

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

1. Checks for a pending operation journal (refuses if one exists).
2. Refuses over an existing UUID-keyed `pool.json` (bare and `--write`). A corrupt or off-schema `pool.json` is the documented rebuild path: bare `discover` prints the rebuild remediation, and `discover --write` writes a forensic `pool.json.corrupt-<RFC3339-UTC>` snapshot adjacent to the new file, then rebuilds. If the snapshot cannot be written (full disk, read-only state directory), `discover --write` refuses rather than destroy the corrupt original.
3. Reads all entries in `/dev/disk/by-id/`, skipping partition entries (e.g., `ata-TOSHIBA-part1`).
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

- Refuses any operation on an existing UUID-keyed `pool.json`. Corrupt or off-schema files are allowed for `--write` rebuild only; the original is copied to `pool.json.corrupt-<RFC3339-UTC>` before overwrite, and `--write` refuses if that snapshot cannot be written (full disk, read-only state directory). Run with all intended pool members attached; see `docs/luks-unlock.md`.
- Refuses if a pending operation journal (`pending-op.json`) exists -- run `braid recover` to reconcile.
- Refuses if another braid operation is in progress (pool lock `/run/braid-pool.lock` is held) -- retry once it finishes.
- With `--expect-count`, refuses to write if the discovered member count is not exactly the requested count.
- Without `--write`, makes no changes at all -- read-only scan.
- Dangling `/dev/disk/by-id/` symlinks are skipped with a warning -- a diagnostic operators need when udev leaves a stale alias behind after a disk swap.
- LUKS1 devices are skipped with a warning.
- Refuses the scan if two distinct devices share the same `braid-<name>` LUKS label -- relabel or detach one disk before retrying.
- Refuses the scan if two distinct devices share the same LUKS UUID -- detach the cloned or unintended disk before retrying.

## Related commands

- [recover](recover.md) -- resume an interrupted operation (has its own membership rebuild from live pool state)
- [status](status.md) -- view current pool membership
