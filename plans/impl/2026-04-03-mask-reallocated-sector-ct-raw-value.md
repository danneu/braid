# Mask `Reallocated_Sector_Ct` raw value to lower 16 bits

## Context

`classify_sata` in `cli/src/parse/smartctl.rs:131-135` checks `raw.value > 0` uniformly for all three SATA health attributes. This is wrong for attribute 5 (`Reallocated_Sector_Ct`) because `raw.value` is the full 48-bit raw and the sector count only lives in the lower 16 bits.

### Evidence

**1. JSON `raw.value` is the full 48-bit raw value, not a per-format interpretation.**

`reference/smartmontools/smartmontools/ataprint.cpp:1351-1353`:
```c
uint64_t rawval = ata_get_attr_raw_value(attr, defs);
jref["raw"]["value"] = rawval;   // full 48-bit value
jref["raw"]["string"] = rawstr;  // formatted per raw_format
```

`ata_get_attr_raw_value` (`reference/smartmontools/smartmontools/atacmds.cpp:1846-1889`) assembles all 6 raw bytes into one `uint64_t` regardless of format.

**2. Attribute 5 uses `raw16(raw16)` format — the sector count is only the lower 16 bits.**

The global default entry in `reference/smartmontools/smartmontools/drivedb.h:83`:
Permalink: https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L83
```
"-v 5,raw16(raw16),Reallocated_Sector_Ct "
```

The Toshiba N300 entry (`drivedb.h:4163-4173`) does **not** override attribute 5, so it inherits this default.
Permalink: https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L4163-L4173

The `raw16(raw16)` format (`RAWFMT_RAW16_OPT_RAW16`) is defined in `reference/smartmontools/smartmontools/atacmds.cpp:1979-1983`:
```c
case RAWFMT_RAW16_OPT_RAW16:
    s = strprintf("%u", word[0]);                      // word[0] = lower 16 bits = count
    if (word[1] || word[2])
        s += strprintf(" (%u %u)", word[2], word[1]);  // upper words = supplementary data
    break;
```

Where `word[0]` is bits 0-15 (sector count), `word[1]` is bits 16-31, `word[2]` is bits 32-47. If a drive has 0 reallocated sectors but non-zero event data in the upper words, `raw.value` is non-zero while the actual sector count is zero.

**3. Attributes 197 and 198 use `raw48` — the full value is the count.**

`reference/smartmontools/smartmontools/drivedb.h:118-119`:
Permalink: https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L118-L119
```
"-v 197,raw48,Current_Pending_Sector "
"-v 198,raw48,Offline_Uncorrectable "
```

No masking needed for these two.

## Changes

### `cli/src/parse/smartctl.rs`

**1. Mask `Reallocated_Sector_Ct` to lower 16 bits in `classify_sata` (~line 131-135):**

Keep name-based matching. SMART ID 5 is reused with different formats across drive types (e.g. `raw48` for SSDs at [`drivedb.h:579`](https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L579), [`614`](https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L614), [`691`](https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L691), [`3145`](https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L3145)), so ID-based matching would over-apply the mask. The name `Reallocated_Sector_Ct` is specific to the [`raw16(raw16)` default](https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L83).

```rust
let bad = attrs.table.iter().any(|a| match a.name.as_str() {
    // raw16(raw16) format: sector count is lower 16 bits
    // (drivedb.h:83 — https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L83)
    "Reallocated_Sector_Ct" => a.raw.value & 0xFFFF > 0,
    // raw48 format: full value is the count
    // (drivedb.h:118-119 — https://github.com/smartmontools/smartmontools/blob/RELEASE_7_5/smartmontools/drivedb.h#L118-L119)
    "Current_Pending_Sector" | "Offline_Uncorrectable" => a.raw.value > 0,
    _ => false,
});
```

**2. Add regression test (~after `sata_degraded_reallocated_sectors`):**

```rust
#[test]
fn sata_healthy_reallocated_zero_with_nonzero_upper_bytes() {
    // Intent: Reallocated_Sector_Ct with 0 sectors must not false-positive
    //   as Degraded when upper bytes of the raw value are non-zero.
    // Why: smartctl raw.value is the full 48-bit raw. Attribute 5 uses
    //   raw16(raw16) format where only the lower 16 bits are the sector
    //   count; upper words carry supplementary event data.
    // Scenario: a Toshiba N300 (or similar HDD using the drivedb default
    //   for attribute 5) reports 0 reallocated sectors but 5 reallocation
    //   events in the middle word → raw.value = 5 << 16 = 327680.
    let json = r#"{
        "smart_status": {"passed": true},
        "device": {"protocol": "ATA"},
        "ata_smart_attributes": {
            "table": [
                {"name": "Reallocated_Sector_Ct", "raw": {"value": 327680}},
                {"name": "Current_Pending_Sector", "raw": {"value": 0}},
                {"name": "Offline_Uncorrectable", "raw": {"value": 0}}
            ]
        }
    }"#;
    assert_eq!(parse_smartctl_health(&raw(json)), SmartHealth::Healthy);
}
```

## Verification

```
just test-rust
```
