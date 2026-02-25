# Disk Identity Map

## Context

When a btrfs device goes missing (disk failure), `btrfs filesystem show` only reports the devid — there's no backward link to which physical disk (by-id path) or LUKS UUID it was. This makes it hard to know which bay to pull or what to pass to `braid replace --missing-id`.

This adds a persistent map file at `/var/lib/braid/disk-map.json` that records `name → {by_id, luks_uuid, devid}` on every add/remove/replace. When a disk disappears, the map tells you exactly which physical disk it was.

## Format

```json
{
  "schema_version": 1,
  "disks": {
    "toshiba": {
      "by_id": "/dev/disk/by-id/ata-Toshiba_MN07_XXXX",
      "luks_uuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "devid": 1,
      "added_at": "2026-02-25T12:00:00Z"
    },
    "ironwolf": {
      "by_id": "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY",
      "luks_uuid": "e5f6g7h8-i9j0-...",
      "devid": 2,
      "added_at": "2026-02-25T13:00:00Z"
    }
  }
}
```

## Files to create/modify

### New: `cli/src/disk_map.rs`

New module following the same pattern as `checkpoint.rs`:
- `DISK_MAP_FILE` const: `/var/lib/braid/disk-map.json`
- `DiskMap` struct (schema_version + BTreeMap<String, DiskMapEntry>)
- `DiskMapEntry` struct: `by_id`, `luks_uuid`, `devid`, `added_at`
- `load_disk_map()` → loads existing or returns empty map
- `save_disk_map(map)` → atomic write (write tmp, rename)
- `record_disk(map, name, by_id, luks_uuid, devid)` → upserts entry
- `remove_disk(map, name)` → removes entry

### Modify: `cli/src/lib.rs`

Add `pub mod disk_map;`

### Modify: `cli/src/add.rs`

After the final success message ("Done. {} is now part of the pool."):
1. Re-probe the pool with `probe_pool()` to get current devids
2. Find the new device by matching its mapper name
3. Get the LUKS UUID from the probed device
4. Load disk map, call `record_disk`, save disk map
5. On failure: warn to stderr but don't fail the add (map is advisory)

### Modify: `cli/src/remove.rs`

After the final success message ("Done."):
1. Load disk map, call `remove_disk(name)`, save
2. On failure: warn to stderr

### Modify: `cli/src/replace.rs`

After the final success message ("Done."):
1. Remove old disk entry from map
2. Re-probe pool, find new device's devid
3. Record new disk entry
4. Save map
5. On failure: warn to stderr

## Design decisions

- **Advisory, not authoritative**: The map is best-effort. If it gets corrupted or out of sync, nothing breaks. Load failures return an empty map; save failures print a warning.
- **Atomic writes**: Same tmp+rename pattern as checkpoint.rs.
- **Same directory**: `/var/lib/braid/` alongside existing state files.
- **Re-probe for devid**: After add, call `probe_pool` to get the btrfs-assigned devid rather than guessing. This is a few extra syscalls but is correct.
- **No dry-run writes**: Map is only updated when operations actually execute.

## Verification

1. `make test-rust` — unit tests for load/save/record/remove
2. Manual: `braid add` a disk, check `/var/lib/braid/disk-map.json` has the entry
3. Manual: `braid remove` a disk, check entry is gone
