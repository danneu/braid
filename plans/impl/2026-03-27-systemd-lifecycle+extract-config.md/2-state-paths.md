# Plan: Replace BRAID_STATE_DIR with StatePaths abstraction

## Context

State files under `/var/lib/braid/` are accessed via hardcoded constants scattered
across modules. `membership.rs` has a half-done `BRAID_STATE_DIR` env var override
that only covers `pool.json`. This bakes a test-only runtime override into production
behavior and leaves all other state paths unoverridable.

Replace with an explicit `StatePaths` struct — one injected root that resolves all
state paths. Thread through every `/var/lib/braid` consumer in one pass so the
abstraction is the single state-path boundary.

## StatePaths struct

New file: `cli/src/state_paths.rs`

```rust
#[derive(Debug, Clone)]
pub struct StatePaths {
    root: PathBuf,
}

impl StatePaths {
    pub fn production() -> Self { Self { root: "/var/lib/braid".into() } }
    pub fn custom(root: PathBuf) -> Self { Self { root } }

    pub fn pool_json(&self) -> PathBuf { self.root.join("pool.json") }
    pub fn disk_map_json(&self) -> PathBuf { self.root.join("disk-map.json") }
    pub fn acked_stats_json(&self) -> PathBuf { self.root.join("acked-stats.json") }
    pub fn smartd_alert(&self) -> PathBuf { self.root.join("smartd-alert") }
    pub fn alert_latch_json(&self) -> PathBuf { self.root.join("alert-latch.json") }
    pub fn luks_headers_dir(&self) -> PathBuf { self.root.join("luks-headers") }
}
```

Register in `cli/src/lib.rs` as `pub mod state_paths;`.

## Complete caller inventory

Every production-path function, every call site. This is the acceptance checklist —
every line must be updated. Parameter name: `paths: &StatePaths`.

### membership.rs

Remove `const POOL_PATH`, `fn pool_path()`, and `BRAID_STATE_DIR` env var logic.

| Function | New signature | Delegates to |
|----------|--------------|-------------|
| `load_membership` | `(paths: &StatePaths) -> Result<…>` | `load_membership_from(&paths.pool_json())` |
| `save_membership` | `(m, paths: &StatePaths) -> Result<…>` | `save_membership_to(m, &paths.pool_json())` |

Keep `load_membership_from(path)` and `save_membership_to(m, path)` for unit tests.

**Callers of `load_membership()`:**

| Call site | File:line |
|-----------|-----------|
| cmd_add body | `add.rs:213` |
| cmd_remove body | `remove.rs:156` |
| cmd_replace body | `replace.rs:196` |
| cmd_remove_missing body | `remove_missing.rs:155` |
| cmd_status body | `status.rs:388` |
| check_declared_disks (doctor) | `doctor.rs:174` |
| tui::run | `tui/mod.rs:33` |
| main.rs Unlock | `main.rs:339` |
| main.rs EnrollKeyFile | `main.rs:370` |
| main.rs Lock | `main.rs:401` |
| disk_name_candidates | `main.rs:558` |

**Callers of `save_membership()`:**

| Call site | File:line |
|-----------|-----------|
| cmd_add body | `add.rs:342` |
| cmd_remove body | `remove.rs:159` |
| cmd_replace body | `replace.rs:200` |
| cmd_remove_missing body | `remove_missing.rs:158` |
| main.rs Discover | `main.rs:506` |

### disk_map.rs

Remove `pub const DISK_MAP_FILE`.

| Function | New signature | Delegates to |
|----------|--------------|-------------|
| `load_disk_map` | `(paths: &StatePaths) -> DiskMap` | `load_disk_map_at(&paths.disk_map_json())` |
| `save_disk_map` | `(paths: &StatePaths, map) -> Result<…>` | `save_disk_map_at(…)` |
| `try_load_disk_map` | `(paths: &StatePaths) -> DiskMapLoad` | `try_load_disk_map_at(…)` |
| `update_disk_map_best_effort` | `(paths: &StatePaths, f)` | uses paths internally |

Keep `_at` variants for unit tests.

**Callers of `update_disk_map_best_effort()`:**

| Call site | File:line |
|-----------|-----------|
| cmd_add | `add.rs:513` |
| cmd_remove | `remove.rs:166` |
| cmd_replace | `replace.rs:331` |
| cmd_remove_missing | `remove_missing.rs:182` |
| cmd_unlock | `unlock.rs:204` |

**Callers of `load_disk_map()`:**

| Call site | File:line |
|-----------|-----------|
| cmd_remove_missing | `remove_missing.rs:148` |

**Callers of `try_load_disk_map()`:**

None in current status.rs (confirmed via grep — status.rs has no disk_map usage).

### alert.rs

Remove `pub const ACKED_STATS_FILE`, `pub const SMARTD_ALERT_FILE`, `pub const ALERT_LATCH_FILE`.

| Function | New signature | Delegates to |
|----------|--------------|-------------|
| `load_acked_stats` | `(paths: &StatePaths)` | `load_acked_stats_at(&paths.acked_stats_json())` |
| `save_acked_stats` | `(stats, paths: &StatePaths)` | `save_acked_stats_at(&paths.acked_stats_json(), stats)` |
| `smartd_alert_active` | `(paths: &StatePaths) -> bool` | `paths.smartd_alert().exists()` |
| `remove_smartd_alert_flag` | `(paths: &StatePaths) -> Result<…>` | uses `paths.smartd_alert()` |
| `load_alert_latch` | `(paths: &StatePaths) -> Option<…>` | uses `paths.alert_latch_json()` |
| `save_alert_latch` | `(alert_state, paths: &StatePaths)` | uses `paths.alert_latch_json()` |
| `remove_alert_latch` | `(paths: &StatePaths) -> Result<…>` | uses `paths.alert_latch_json()` |

Keep `_at` variants for unit tests.

**Callers of alert production functions:**

| Call site | Functions used | File:line(s) |
|-----------|---------------|-------------|
| cmd_monitor | load_acked_stats, save_acked_stats, smartd_alert_active, load_alert_latch, save_alert_latch | `monitor.rs:43,49,73,93,95,103,110` |
| cmd_ack | load_alert_latch, save_acked_stats, remove_alert_latch, remove_smartd_alert_flag, smartd_alert_active | `ack.rs:11,45,48,49,65,72,73` |
| cmd_status | load_alert_latch, smartd_alert_active | `status.rs:470,471` |

### luks.rs

Remove `pub(crate) const HEADER_BACKUP_DIR`.

| Function | New signature | Change |
|----------|--------------|--------|
| `backup_luks_header` | `(runner, device, mapper, paths: &StatePaths)` | passes `paths.luks_headers_dir()` to `backup_luks_header_to` |
| `header_backup_advisories` | `(paths: &StatePaths) -> Vec<String>` | passes `paths.luks_headers_dir()` to `header_backup_advisories_in` |

Keep `backup_luks_header_to(…, dir)` and `header_backup_advisories_in(dir)` for unit tests.

**Callers:**

| Call site | Function | File:line |
|-----------|----------|-----------|
| cmd_add | backup_luks_header | `add.rs:363` |
| cmd_replace | backup_luks_header | `replace.rs:214` |
| cmd_status | header_backup_advisories | `status.rs:249,343` |
| tui Model::new | header_backup_advisories | `tui/model.rs:146` |

For `tui/model.rs:146`: Move the `header_backup_advisories(paths)` call to `tui::run` and
pass the result into `Model::new(…, advisories)` — the view model shouldn't know about paths.

## Command signature changes

Every command that reads/writes state gains `paths: &StatePaths`.

| Command | File | membership | disk_map | alert | luks |
|---------|------|:----------:|:--------:|:-----:|:----:|
| cmd_add | add.rs | load + save | update_best_effort | — | backup_header |
| cmd_remove | remove.rs | load + save | update_best_effort | — | — |
| cmd_replace | replace.rs | load + save | update_best_effort | — | backup_header |
| cmd_remove_missing | remove_missing.rs | load + save | load + update_best_effort | — | — |
| cmd_unlock | unlock.rs | — | update_best_effort | — | — |
| cmd_status | status.rs | load | — | load_latch + smartd_active | header_advisories |
| cmd_doctor | doctor.rs | load (via DoctorContext) | — | — | — |
| cmd_monitor | monitor.rs | — | — | all | — |
| cmd_ack | ack.rs | — | — | all | — |
| tui::run | tui/mod.rs | load | — | — | header_advisories |

### doctor.rs threading

Add `paths: &'a StatePaths` field to `DoctorContext<'a, R>`. Thread through:
- `cmd_doctor(config_path, paths, json)` → `run_doctor(config_path, &runner, paths)`
  → `DoctorContext { ..., paths }` → `check_declared_disks` uses `ctx.paths`

### main.rs wiring

- Construct `let paths = StatePaths::production();` once after CLI parse
- Pass `&paths` to every command call and every bare `load_membership`/`save_membership`
- `disk_name_candidates()`: construct `StatePaths::production()` internally (clap
  requires zero-arg fn pointer; tab completion is always production)

## TDD: tests first

Write these tests before implementing. They will fail to compile until the API exists.

### 1. StatePaths unit tests (`state_paths.rs`)

```rust
#[test]
fn production_resolves_expected_paths() {
    let p = StatePaths::production();
    assert_eq!(p.pool_json(), PathBuf::from("/var/lib/braid/pool.json"));
    assert_eq!(p.disk_map_json(), PathBuf::from("/var/lib/braid/disk-map.json"));
    assert_eq!(p.acked_stats_json(), PathBuf::from("/var/lib/braid/acked-stats.json"));
    assert_eq!(p.smartd_alert(), PathBuf::from("/var/lib/braid/smartd-alert"));
    assert_eq!(p.alert_latch_json(), PathBuf::from("/var/lib/braid/alert-latch.json"));
    assert_eq!(p.luks_headers_dir(), PathBuf::from("/var/lib/braid/luks-headers"));
}

#[test]
fn custom_resolves_under_given_root() {
    let p = StatePaths::custom(PathBuf::from("/tmp/test-braid"));
    assert_eq!(p.pool_json(), PathBuf::from("/tmp/test-braid/pool.json"));
    assert_eq!(p.luks_headers_dir(), PathBuf::from("/tmp/test-braid/luks-headers"));
}
```

### 2. Membership roundtrip through StatePaths (`membership.rs`)

```rust
#[test]
fn roundtrip_via_state_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let paths = StatePaths::custom(tmp.path().into());
    let mut m = PoolMembership::empty();
    m.disks.insert("d1".into(), ByIdPath("/dev/disk/by-id/ata-X".into()));
    save_membership(&m, &paths).unwrap();
    let loaded = load_membership(&paths).unwrap();
    assert_eq!(m, loaded);
}
```

### 3. Disk map roundtrip through StatePaths (`disk_map.rs`)

```rust
#[test]
fn roundtrip_via_state_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let paths = StatePaths::custom(tmp.path().into());
    let mut map = DiskMap::new();
    record_disk(&mut map, "d1", "/by-id/1", "u1", 1);
    save_disk_map(&paths, &map).unwrap();
    let loaded = load_disk_map(&paths);
    assert_eq!(loaded.disks.len(), 1);
}
```

### 4. Command test without env var (`remove.rs`)

Replace `setup_membership` + `STATE_DIR_LOCK` + `BRAID_STATE_DIR` with:
```rust
fn setup_membership(disks: &[(&str, &str)]) -> (tempfile::TempDir, StatePaths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = StatePaths::custom(tmp.path().into());
    let mut m = PoolMembership::empty();
    for (name, by_id) in disks {
        m.disks.insert(name.to_string(), ByIdPath(by_id.to_string()));
    }
    membership::save_membership_to(&m, &paths.pool_json()).unwrap();
    (tmp, paths)
}
```
Tests pass `&paths` to `cmd_remove(…, &paths, …)`. No mutex, no env var, parallel-safe.

## Implementation order

1. Create `state_paths.rs` with struct + tests. Add to `lib.rs`. (`cargo test` — new tests pass)
2. Update `membership.rs` — remove env var, update signatures, add roundtrip test. (Breaks callers)
3. Update `disk_map.rs` — remove constant, update signatures, add roundtrip test. (Breaks callers)
4. Update `alert.rs` — remove constants, update signatures. (Breaks callers)
5. Update `luks.rs` — remove constant, update signatures. (Breaks callers)
6. Update all commands: add, remove, replace, remove_missing, unlock, status, doctor, monitor, ack, tui. (Fixes callers)
7. Update `main.rs` — construct `StatePaths::production()`, wire everything. (Compiles)
8. Update `remove.rs` tests — remove env var/mutex pattern.
9. Cleanup: grep for any remaining `/var/lib/braid` in `cli/src/`.

Steps 2-7 can be done as one atomic commit since intermediate states don't compile.

## Files to modify

1. `cli/src/state_paths.rs` — **new**
2. `cli/src/lib.rs` — add module
3. `cli/src/membership.rs` — remove env var, update signatures
4. `cli/src/disk_map.rs` — remove constant, update signatures
5. `cli/src/alert.rs` — remove constants, update signatures
6. `cli/src/luks.rs` — remove constant, update signatures
7. `cli/src/add.rs` — add `paths` param, pass through
8. `cli/src/remove.rs` — add `paths` param, pass through, fix tests
9. `cli/src/replace.rs` — add `paths` param, pass through
10. `cli/src/remove_missing.rs` — add `paths` param, pass through
11. `cli/src/unlock.rs` — add `paths` param, pass through
12. `cli/src/status.rs` — add `paths` param, pass through
13. `cli/src/doctor.rs` — add `paths` to DoctorContext, thread through
14. `cli/src/monitor.rs` — add `paths` param, pass through
15. `cli/src/ack.rs` — add `paths` param, pass through
16. `cli/src/tui/mod.rs` — add `paths` param, resolve advisories in run()
17. `cli/src/tui/model.rs` — accept advisories as param to Model::new
18. `cli/src/main.rs` — construct StatePaths, wire everything

## What does NOT change

- NixOS VM tests (Python scripts in `tests/`): call the binary, which uses `StatePaths::production()`
- NixOS module (`modules/braid/`): no Rust library calls; `monitor.nix` still writes
  `/var/lib/braid/smartd-alert` via shell — that matches `StatePaths::production()`
- `_from`/`_at`/`_in`/`_to` path-accepting variants: kept for module-level unit tests
- No behavioral change: `StatePaths::production()` resolves identical paths

## Verification

1. `just test-rust` — all Rust unit tests pass (new StatePaths tests + existing roundtrips + remove.rs command tests)
2. `just test` — NixOS VM tests pass (unchanged binary behavior)
3. `grep -r 'BRAID_STATE_DIR' cli/` — no remaining references
4. `grep -rn '"/var/lib/braid' cli/src/` — only in `state_paths.rs` (the single source of truth)
