# Fix: deterministic by-id symlink selection in discover

## Context

`braid discover` scans `/dev/disk/by-id/` for LUKS devices with braid labels. A single physical
drive can have multiple symlinks there — e.g. both `wwn-0x5000c500c095dc33` and
`ata-ST500LM036-2LU17A_WGS5N6F2` point to `/dev/sdb`. Both pass `cryptsetup isLuks` and return
the same LUKS label, so both match. The current code does an unconditional `BTreeMap::insert`,
meaning whichever symlink `read_dir()` returns last wins — non-deterministic across reboots.

Observed: `braid discover` returned `wwn-*` for sda but `ata-*` for sdb and sdc, while pool.json
already had correct `wwn-*` entries for all three. Running `--write` would have silently downgraded
two entries.

## File to modify

`cli/src/discover.rs` — all changes here, public API unchanged.

## Implementation

### 1. Add `by_id_priority(filename: &str) -> u8`

Pure function ranking symlink filename prefixes by stability (lower = more preferred):

| Prefix  | Source                                                        | Stable across ports?                                                                              |
|---------|---------------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| `wwn-`  | World Wide Name from firmware. Fully persistent, won't change across subsystems | Yes                                                                         |
| `nvme-` | Controller serial + namespace                                 | Yes                                                                                               |
| `scsi-` | SCSI Inquiry VPD page (device serial/EUI-64/T10 vendor ID) — from disk hardware | Yes                                                                        |
| `ata-`  | Model + serial, formatted by kernel ATA driver                | Yes (format can change across kernel versions)                                                    |
| `usb-`  | USB device serial number from the device descriptor           | Yes, but cheap drives sometimes report no serial or a shared serial, so symlink may not exist     |

Priority (lower value = more preferred): `wwn-` → 0, `nvme-` → 1, `scsi-` → 2, `ata-` → 3, `usb-` → 4, everything else → 5.

### 2. Extract `discover_from_dir(runner, by_id_dir: &Path)` (private)

Move the body of `discover_pool_members` into a private function that takes the directory path as
a parameter. The public function becomes:

```rust
pub fn discover_pool_members<R: CommandRunner>(runner: &R) -> Result<...> {
    discover_from_dir(runner, Path::new("/dev/disk/by-id"))
}
```

This makes the logic testable with a tempdir.

### 3. Conditional insert via `BTreeMap::Entry` with total ordering

Replace the unconditional insert with a comparison on `(priority, filename)` so ties are broken
lexicographically — fully deterministic regardless of `read_dir` order:

```rust
match members.entry(disk_name.to_owned()) {
    Entry::Vacant(e) => { e.insert(ByIdPath(path_str)); }
    Entry::Occupied(mut e) => {
        let existing_name = e.get().0.rsplit('/').next().unwrap_or("");
        let candidate_key = (by_id_priority(&name_str), name_str.as_ref());
        let existing_key  = (by_id_priority(existing_name), existing_name);
        if candidate_key < existing_key {
            e.insert(ByIdPath(path_str));
        }
    }
}
```

This always keeps the lexicographic minimum within the same priority class (e.g. two `ata-` aliases
would consistently pick the alphabetically earlier one).

## Tests

Add to the `#[cfg(test)]` block in `discover.rs`:

### Priority unit tests

```rust
#[test]
fn by_id_priority_ordering() {
    assert!(by_id_priority("wwn-0x123") < by_id_priority("nvme-SAMSUNG"));
    assert!(by_id_priority("nvme-SAMSUNG") < by_id_priority("scsi-360014"));
    assert!(by_id_priority("scsi-360014") < by_id_priority("ata-SEAGATE"));
    assert!(by_id_priority("ata-WD") < by_id_priority("usb-Kingston"));
    assert!(by_id_priority("usb-Kingston") < by_id_priority("dm-uuid-123"));
}
```

### Regression tests using tempdir + path-aware mock runner

Create regular files (not real symlinks) in a tempdir — `read_dir` only reads filenames, never
follows targets, so no block devices needed.

Define a `TestRunner` that maps each path to a label (so multi-disk scenarios work). Internally
it holds a `HashMap<String, String>` of `path → luks_label`. On `CryptsetupIsLuks` return OK for
any known path; on `CryptsetupLuksDumpText` look up the path and return the corresponding label.
Define a `mock_ok` helper locally (same pattern as `add.rs`/`replace.rs`).

Each test must begin with the standard three-part block comment required by AGENTS.md:
- **Intent** — what behavior the test verifies
- **Why it exists** — what regression it guards against
- **Scenario** — the real-world story / incident that inspired it

**Test 1 — wwn beats ata (same disk, two symlinks):**
Files: `ata-SEAGATE_XXXXXX` and `wwn-0xABCD`, both mapped to label `braid-sda`.
Create `ata-` first so the old last-wins code would produce `wwn-` by luck; then also create with
`wwn-` first to confirm it wins regardless of order. Assert result path ends with `wwn-0xABCD`.

**Test 2 — same-priority tie broken lexicographically:**
Files: `ata-ZZZZZ` and `ata-AAAAA`, both mapped to `braid-sda`.
Assert result path ends with `ata-AAAAA` (alphabetically earlier wins, not discovery order).

**Test 3 — two disks, each with multiple symlinks:**
Files: `ata-DISK1`, `wwn-0x0001` (→ `braid-alpha`) and `ata-DISK2`, `wwn-0x0002` (→ `braid-beta`).
Assert alpha gets `wwn-0x0001` and beta gets `wwn-0x0002`.

## Verification

```
just test-rust   # unit tests pass
just test        # existing VM tests still pass
```

Manually on the NAS: `sudo braid discover` should consistently show `wwn-*` for all three drives.
