# Refactor: Typed `DeviceStatsTarget` for btrfs device stats

## Context

`btrfs device stats` emits `<missing disk>` as the device path for absent drives during degraded mount. Today this sentinel flows through as a plain `String` in `DeviceErrorStats.device_path`, and two call sites in `alert.rs` guard against it with `== "<missing disk>"` string comparisons. This leaks parser-level syntax into business logic and risks future regressions where someone treats the sentinel like a real path.

This refactor moves the sentinel-to-semantics conversion into the parser and replaces all downstream string comparisons with pattern matching on a typed enum.

## Design

```rust
// cli/src/parse/types.rs

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceStatsTarget {
    Path(String),
    MissingDisk,
}

impl DeviceStatsTarget {
    pub fn as_path(&self) -> Option<&str> {
        match self {
            Self::Path(p) => Some(p),
            Self::MissingDisk => None,
        }
    }
}

impl std::fmt::Display for DeviceStatsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => f.write_str(p),
            Self::MissingDisk => f.write_str("<missing disk>"),
        }
    }
}
```

`DeviceErrorStats.device_path: String` becomes `target: DeviceStatsTarget`.

`Hash` is derived so the parser can continue using it as a HashMap key.

## Exhaustive `device_path` usage (from grep)

The field rename from `device_path` to `target` will cause compile errors at every access site. The compiler enforces completeness, but for the implementer's reference, here is every site that references the struct field:

| File | Lines | Kind |
|------|-------|------|
| `cli/src/parse/types.rs` | 271 | Field definition |
| `cli/src/parse/btrfs_device_stats.rs` | 54, 60–63 | Constructor (parser) |
| `cli/src/parse/btrfs_device_stats.rs` | 114, 116 | Test assertions |
| `cli/src/alert.rs` | 116, 121, 123 | `compute_alert_state_with_devid_map` |
| `cli/src/alert.rs` | 204, 209, 211 | `snapshot_current` |
| `cli/src/alert.rs` | 291 | Test helper `zero_device` constructor |
| `cli/src/tui/probe.rs` | 146 | `strip_prefix` for TUI display |
| `cli/src/status.rs` | 771 | `find` for verbose status report |
| `cli/src/replace.rs` | 241 | `contains` for pre-flight error warning |

Note: `cli/src/pool.rs:272` has a local variable named `device_path` — unrelated to the struct field; no change needed.

## Changes by file

### 1. `cli/src/parse/types.rs`
- Add `DeviceStatsTarget` enum with `Display`, `as_path()`, and `Hash`.
- Change `DeviceErrorStats.device_path: String` → `target: DeviceStatsTarget`.

### 2. `cli/src/parse/btrfs_device_stats.rs` (parser)
- After nom extracts the raw device string, convert:
  ```rust
  let target = if device == "<missing disk>" {
      DeviceStatsTarget::MissingDisk
  } else {
      DeviceStatsTarget::Path(device.to_owned())
  };
  ```
- Change `device_order: Vec<String>` → `Vec<DeviceStatsTarget>` and `stats_map: HashMap<String, _>` → `HashMap<DeviceStatsTarget, _>`.
- Store `target` in the `DeviceErrorStats` struct.
- Update existing tests: assert `out.devices[0].target.as_path() == Some("/dev/mapper/braid-vdb")` instead of `out.devices[0].device_path == "..."`.
- Add new parser contract test `device_stats_parses_missing_disk_sentinel`: feed a stats block containing a `[<missing disk>]` line alongside a normal device, assert that the parsed entry has `target == DeviceStatsTarget::MissingDisk` and the normal device has `target == DeviceStatsTarget::Path(...)`. This is the core contract for the sentinel conversion and must live in the parser, not downstream.

### 3. `cli/src/alert.rs`
- **`compute_alert_state_with_devid_map` (line 111):** Replace string guard with:
  ```rust
  let path = match &dev.target {
      DeviceStatsTarget::MissingDisk => continue,
      DeviceStatsTarget::Path(p) => p,
  };
  ```
  Then use `path` for the `path_to_devid` lookup and `UnmappedDeviceError`.
- **`snapshot_current` (line 201):** Same pattern.
- **Test helper `zero_device(path: &str)`:** Keep signature, wrap in `DeviceStatsTarget::Path` internally. Add `zero_missing_device()` for the MissingDisk variant.
- **Two tests** (`missing_disk_sentinel_skipped_in_alert`, `missing_disk_sentinel_skipped_in_snapshot`): Change `zero_device("<missing disk>")` → `zero_missing_device()`.

### 4. `cli/src/tui/probe.rs` (line 146)
```rust
// before:
if let Some(name) = dev.device_path.strip_prefix("/dev/mapper/braid-") {
// after:
if let Some(name) = dev.target.as_path().and_then(|p| p.strip_prefix("/dev/mapper/braid-")) {
```
MissingDisk naturally returns `None` → skipped. No behavioral change.

### 5. `cli/src/status.rs` (line 771)
```rust
// before:
.find(|d| d.device_path == dev_path)
// after:
.find(|d| d.target.as_path() == Some(dev_path.as_str()))
```
MissingDisk never matches a constructed mapper path. No behavioral change.

### 6. `cli/src/replace.rs` (line 241)
The existing code uses `d.device_path.contains(&mapper.0)` — a substring match that can false-positive (e.g. mapper `braid-disk1` matches path `/dev/mapper/braid-disk10`). Since we're already touching this line, tighten it to an exact comparison:
```rust
// before:
d.device_path.contains(&mapper.0)
// after:
d.target.as_path() == Some(&format!("/dev/mapper/{}", mapper.0) as &str)
```
MissingDisk naturally excluded. The substring false-positive is also fixed.

## Sequencing

1. `types.rs` — define enum, change struct field (creates compile errors everywhere)
2. `btrfs_device_stats.rs` — fix parser + its tests
3. `alert.rs` — fix business logic + test helpers + tests
4. `tui/probe.rs`, `status.rs`, `replace.rs` — fix remaining consumers (any order)

## Verification

1. `just test-rust` — all unit tests pass (parser, alert, golden)
2. `just test` — NixOS VM integration tests pass (braid-monitor, braid-smartd-alert cover the full alert lifecycle including degraded mount with missing disk)
