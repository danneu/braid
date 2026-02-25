# Plan: Add disk identity map for missing-device resolution

## Context

When a btrfs member is missing, operators mostly get a `devid` from btrfs tooling, but not a direct mapping back to braid disk key (`name`) and by-id path. This plan adds an advisory map at `/var/lib/braid/disk-map.json` to preserve `name/by_id/luks_uuid/devid` across command executions so missing-device flows (`replace --missing-id`, `remove-missing --missing-id`) are easier and safer.

## Approach (TDD — tests first)

### Step 1: Add failing tests for map behavior

**File: `cli/src/disk_map.rs` (new tests in module)**

- Add unit tests for:
  - load when file missing (returns empty map)
  - atomic save + reload roundtrip
  - `record_disk` upsert behavior
  - `remove_disk` behavior
  - prune-by-devid behavior for remove-missing flows

**File: `tests/braid-remove-disk.py`**

- Extend remove lifecycle to validate map updates:
  - after `braid add disk1|2|3`: map contains all 3 names with devids
  - after graceful `braid remove disk3`: `disk3` map entry removed
  - after failed `braid remove disk3` on missing disk: map unchanged
  - after `braid remove-missing --yes`: missing entry removed
  - for multi-missing + `--missing-id`: only targeted entry removed

Notes:
- Read map via `cat /var/lib/braid/disk-map.json` and parse with Python `json.loads`.
- Keep assertions on behavior (entry present/absent + expected keys), not exact timestamps.

### Step 2: Confirm tests fail

Run:
1. `make test-rust`
2. `make test-one t=braid-remove-disk`

Expected initial failures:
- missing `disk_map` module/functions
- no map file created/updated by command flows

### Step 3: Implement disk map module

**New file: `cli/src/disk_map.rs`**

- Add:
  - `DISK_MAP_FILE: &str = "/var/lib/braid/disk-map.json"`
  - `DiskMap { schema_version, disks: BTreeMap<String, DiskMapEntry> }`
  - `DiskMapEntry { by_id, luks_uuid, devid, added_at }`
  - helpers:
    - `load_disk_map() -> DiskMap` (production wrapper using `DISK_MAP_FILE`)
    - `save_disk_map(&DiskMap) -> io::Result<()>` (production wrapper using `DISK_MAP_FILE`)
    - `load_disk_map_at(path: &Path) -> DiskMap` (testable path-injected helper)
    - `save_disk_map_at(path: &Path, map: &DiskMap) -> io::Result<()>` (testable path-injected helper; tmp + rename in same dir)
    - `record_disk(...)` (upsert)
    - `remove_disk(...)` (by name)
    - `remove_disks_by_devids(...)` (for remove-missing pruning)
- Add unit tests from Step 1.
- Unit tests must use `*_at(...)` helpers with temp paths, not `/var/lib/braid/disk-map.json`.

**File: `cli/src/lib.rs`**

- Add `pub mod disk_map;`

### Step 4: Wire map updates into command flows

**File: `cli/src/add.rs`**

- After successful add:
  - re-probe pool
  - resolve added device mapper => `devid` + `luks_uuid`
  - load map, `record_disk(name, by_id, luks_uuid, devid)`, save
- If map update fails: warn on stderr; do not fail add.
- No updates in `--dry-run`.

**File: `cli/src/remove.rs`**

- After successful present-disk remove:
  - load map, `remove_disk(name)`, save
- Warn-only on map update errors.
- No map writes on failed remove.

**File: `cli/src/replace.rs`**

- After successful replace:
  - remove old disk entry
  - re-probe pool for new disk `devid/luks_uuid`
  - record new disk entry
  - save once
- Warn-only on map update errors.

**File: `cli/src/remove_missing.rs`**

- After successful remove-missing:
  - re-probe pool to collect current devid set
  - if `--missing-id <devid>`: remove map entries with that devid
  - else: remove entries whose devid is no longer in pool
  - save map
- If command fails, do not mutate map.
- Warn-only on map update errors.

### Step 5: Verify

1. `make test-rust` — disk-map unit tests + existing Rust tests pass
2. `make test-one t=braid-remove-disk` — map assertions + remove semantics pass
3. `make test-one t=replace-failed-disk` — replace flow unchanged and still green
4. `make test` — full suite green

### Step 6: Document behavior

**File: `README.md`**

- Add brief operator note that braid tracks an advisory disk identity map at `/var/lib/braid/disk-map.json` for missing-device workflows.
- Clarify it is non-authoritative and rebuilt/updated by command executions.

## Files to modify

| File | Change |
|------|--------|
| `cli/src/disk_map.rs` | **New** — map schema, load/save, helpers, unit tests |
| `cli/src/lib.rs` | Export `disk_map` module |
| `cli/src/add.rs` | Record new disk mapping after successful add |
| `cli/src/remove.rs` | Remove disk mapping after successful remove |
| `cli/src/replace.rs` | Remove old mapping + record new mapping after replace |
| `cli/src/remove_missing.rs` | Existing file: prune mapping entries after successful missing-device eviction |
| `tests/braid-remove-disk.py` | Add map assertions for add/remove/remove-missing flows |
| `README.md` | Document advisory disk map |

## Design constraints

- Advisory only: map must never be treated as source of truth over live pool probing.
- Best effort: map write/read failures must not fail successful storage operations.
- Atomic writes only (`tmp` + `rename`).
- No writes during dry-run paths.
- Keep schema versioned for future migrations.
