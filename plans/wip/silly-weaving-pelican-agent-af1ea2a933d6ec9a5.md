# Deterministic symlink preference in discover_pool_members

## Problem

`discover_pool_members` iterates `/dev/disk/by-id/` using `std::fs::read_dir()` which returns entries in non-deterministic order. A single physical drive can have multiple symlinks (e.g. `wwn-0x5000c500c095dc33` and `ata-ST500LM036-2LU17A_WGS5N6F2` both pointing to `/dev/sdb`). Both pass `cryptsetup isLuks` and yield the same LUKS label, so both match — whichever is iterated last wins via `BTreeMap::insert`. This causes inconsistent output and potential pool.json desync.

## Design

### 1. Add `by_id_priority(filename: &str) -> u8` pure function

A small pure function in `discover.rs` that returns a numeric priority for a by-id filename based on its prefix. Lower number = higher priority (preferred).

```
wwn-          → 0  (most stable, globally unique)
nvme-eui.     → 1  (EUI-64 identifier, globally unique)
nvme-         → 2  (NVMe device, stable but less universal)
ata-          → 3  (ATA device, common on SATA)
everything else → 4
```

This function is `pub(crate)` so tests can exercise it directly, but it is not part of the public API.

### 2. Extract `discover_from_dir` internal function

Refactor the directory path out of `discover_pool_members` to make it injectable for testing:

```rust
pub fn discover_pool_members<R: CommandRunner>(
    runner: &R,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    discover_from_dir(runner, std::path::Path::new("/dev/disk/by-id"))
}

fn discover_from_dir<R: CommandRunner>(
    runner: &R,
    by_id_dir: &std::path::Path,
) -> Result<BTreeMap<String, ByIdPath>, DiscoverError> {
    // ... existing logic with conditional insert ...
}
```

The public API signature does not change. `discover_from_dir` is `fn` (private), not `pub`.

### 3. Conditional insert logic

Replace the unconditional `members.insert(disk_name.to_owned(), ByIdPath(path_str))` on line 66 with logic that only replaces an existing entry if the new symlink has strictly higher priority (lower numeric value):

```rust
use std::collections::btree_map::Entry;

match members.entry(disk_name.to_owned()) {
    Entry::Vacant(e) => {
        e.insert(ByIdPath(path_str));
    }
    Entry::Occupied(mut e) => {
        // Extract filename from existing path for priority comparison
        let existing_filename = e.get().0
            .rsplit('/')
            .next()
            .unwrap_or(&e.get().0);
        if by_id_priority(&name_str) < by_id_priority(existing_filename) {
            e.insert(ByIdPath(path_str));
        }
    }
}
```

This ensures that regardless of iteration order, the highest-priority symlink wins.

### 4. Unit tests for `by_id_priority`

Add tests within the existing `#[cfg(test)] mod tests` block in `discover.rs`:

```rust
#[test]
fn by_id_priority_ordering() {
    // wwn- is highest priority (0)
    assert_eq!(by_id_priority("wwn-0x5000c500c095dc33"), 0);
    // nvme-eui. is next (1)
    assert_eq!(by_id_priority("nvme-eui.0025385b71b07e5e"), 1);
    // nvme- (without eui.) is next (2)
    assert_eq!(by_id_priority("nvme-Samsung_SSD_980_PRO_1TB_S5GXNG0R400153T"), 2);
    // ata- is next (3)
    assert_eq!(by_id_priority("ata-ST500LM036-2LU17A_WGS5N6F2"), 3);
    // anything else is lowest (4)
    assert_eq!(by_id_priority("scsi-SATA_VBOX_HARDDISK_VB12345678-abcdefgh"), 4);
    assert_eq!(by_id_priority("usb-Kingston_DataTraveler_3.0_1234"), 4);
}

#[test]
fn by_id_priority_wwn_beats_ata() {
    assert!(by_id_priority("wwn-0x5000c500c095dc33") < by_id_priority("ata-ST500LM036-2LU17A_WGS5N6F2"));
}

#[test]
fn by_id_priority_nvme_eui_beats_plain_nvme() {
    assert!(by_id_priority("nvme-eui.0025385b71b07e5e") < by_id_priority("nvme-Samsung_SSD_980_PRO"));
}
```

### 5. Integration-style regression test with temp dir + MockRunner

This is the key regression test. It creates a temp directory with files simulating multiple symlinks for the same physical disk, sets up a MockRunner to respond to `CryptsetupIsLuks` and `CryptsetupLuksDumpText` for each path, and verifies the preferred symlink is selected regardless of filesystem iteration order.

**Test structure:**

```rust
#[test]
fn prefers_wwn_over_ata_symlink_for_same_disk() {
    let dir = tempfile::tempdir().unwrap();

    // Create two files representing symlinks to the same physical disk.
    // The ata- variant is alphabetically first so will likely be iterated first
    // on most filesystems — under the old code it would lose to the later wwn-.
    // Under the new code, wwn- should always win.
    let ata_name = "ata-ST500LM036-2LU17A_WGS5N6F2";
    let wwn_name = "wwn-0x5000c500c095dc33";
    std::fs::write(dir.path().join(ata_name), b"").unwrap();
    std::fs::write(dir.path().join(wwn_name), b"").unwrap();

    let ata_path = dir.path().join(ata_name).to_string_lossy().to_string();
    let wwn_path = dir.path().join(wwn_name).to_string_lossy().to_string();

    // Both devices pass isLuks and have the same braid label
    let luks_dump_stdout = "LUKS header information\n\
        Version:       \t2\n\
        Label:         \tbraid-toshiba\n\
        Subsystem:     \t(no subsystem)\n";

    let runner = MockRunner::default()
        // ata- device: isLuks succeeds, luksDump returns braid-toshiba
        .with_output(
            CmdRequest::CryptsetupIsLuks { device: ata_path.clone() },
            RawCommandOutput {
                cmd: format!("cryptsetup isLuks {ata_path}"),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
        .with_output(
            CmdRequest::CryptsetupLuksDumpText { device: ata_path.clone() },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {ata_path}"),
                stdout: luks_dump_stdout.to_owned(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
        // wwn- device: isLuks succeeds, luksDump returns braid-toshiba
        .with_output(
            CmdRequest::CryptsetupIsLuks { device: wwn_path.clone() },
            RawCommandOutput {
                cmd: format!("cryptsetup isLuks {wwn_path}"),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
        .with_output(
            CmdRequest::CryptsetupLuksDumpText { device: wwn_path.clone() },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {wwn_path}"),
                stdout: luks_dump_stdout.to_owned(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

    let members = discover_from_dir(&runner, dir.path()).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(
        members["toshiba"].0,
        wwn_path,
        "should prefer wwn- over ata-"
    );
}
```

**Additional regression test — three symlinks for the same disk:**

```rust
#[test]
fn prefers_highest_priority_among_three_symlinks() {
    let dir = tempfile::tempdir().unwrap();

    let scsi_name = "scsi-SATA_VBOX_HARDDISK_VB12345678";
    let ata_name = "ata-VBOX_HARDDISK_VB12345678-abcdefgh";
    let wwn_name = "wwn-0x600508b400c48a93";
    // Create files
    for name in [scsi_name, ata_name, wwn_name] {
        std::fs::write(dir.path().join(name), b"").unwrap();
    }

    // ... set up MockRunner for all three paths with same label braid-vbox ...

    let members = discover_from_dir(&runner, dir.path()).unwrap();
    assert_eq!(members.len(), 1);
    let expected_path = dir.path().join(wwn_name).to_string_lossy().to_string();
    assert_eq!(members["vbox"].0, expected_path);
}
```

**Test for mixed disks — two physical disks, each with two symlinks:**

```rust
#[test]
fn two_disks_each_with_two_symlinks() {
    let dir = tempfile::tempdir().unwrap();

    // Disk 1: toshiba (ata- and wwn- symlinks)
    let toshiba_ata = "ata-TOSHIBA_MN08ACA16T_12345";
    let toshiba_wwn = "wwn-0x5000c500c095dc33";
    // Disk 2: samsung (nvme- and nvme-eui. symlinks)
    let samsung_nvme = "nvme-Samsung_SSD_980_PRO_1TB_S5GXNG0R";
    let samsung_eui = "nvme-eui.0025385b71b07e5e";

    // Create all four files, set up runner for all four...

    let members = discover_from_dir(&runner, dir.path()).unwrap();
    assert_eq!(members.len(), 2);
    // toshiba should use wwn-
    assert!(members["toshiba"].0.contains(toshiba_wwn));
    // samsung should use nvme-eui.
    assert!(members["samsung"].0.contains(samsung_eui));
}
```

## File changes

| File | Change | Lines affected |
|---|---|---|
| `cli/src/discover.rs` | Add `by_id_priority` function | New function, ~12 lines |
| `cli/src/discover.rs` | Extract `discover_from_dir` from `discover_pool_members` | Lines 16-73 refactored |
| `cli/src/discover.rs` | Replace `members.insert()` with conditional `Entry` logic | Line 66 |
| `cli/src/discover.rs` | Add `use std::collections::btree_map::Entry` | Line 4 (imports) |
| `cli/src/discover.rs` | Add unit tests for `by_id_priority` | New tests in existing `mod tests` |
| `cli/src/discover.rs` | Add integration tests with tempdir + MockRunner | New tests in existing `mod tests` |

No other files are modified. The public API (`discover_pool_members`) signature is unchanged.

## Implementation sequence

1. **Add `by_id_priority` function** — pure function, no dependencies, immediately testable.
2. **Extract `discover_from_dir`** — mechanical refactor, `discover_pool_members` becomes a thin wrapper.
3. **Replace insert with Entry-based conditional logic** — the actual bug fix.
4. **Add unit tests for `by_id_priority`** — fast, no I/O.
5. **Add integration tests with tempdir + MockRunner** — the regression tests that prove the fix works end-to-end.

## Edge cases and considerations

- **Partition entries**: `is_partition_entry` check happens before priority logic, so `-part1` variants are already filtered. No change needed.
- **Non-LUKS devices**: Devices that fail `CryptsetupIsLuks` are skipped before reaching the insert logic. No change needed.
- **Single symlink per disk**: The common case. `Entry::Vacant` path handles this — no performance regression.
- **Unknown prefix types**: Fall through to priority 4. If two unknown-prefix symlinks exist for the same disk, the first one iterated wins (non-deterministic among equally-ranked candidates). This is acceptable because the known stable prefixes all have explicit ranks.
- **`nvme-eui.` vs `nvme-`**: The `nvme-eui.` check must come before the `nvme-` check in `by_id_priority` since `nvme-eui.` starts with `nvme-`. Implementation uses `starts_with("nvme-eui.")` first, then `starts_with("nvme-")`.
