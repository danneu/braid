# Plan: capture a real `btrfs device usage --raw` MISSING-device fixture

## Context

`check_relocation_space` (and the rest of `remove_missing`) rests on a
specific assumption about `btrfs device usage --raw` output: when one
member is absent, btrfs-progs renders the path token as the literal
string `missing` and emits `Device size: 0` (confirmed against
`reference/btrfs-progs/cmds/filesystem-usage.c:819-831, 1341-1349`).

This shape is currently covered only by hand-rolled, format-pinned
artifacts:

- An inline unit test in `cli/src/parse/btrfs_device_usage.rs:227-265`
  (`device_usage_parses_missing_device_marker`) that hard-codes the
  v6.17.1 byte layout.
- `DeviceUsageSpec::missing` in `cli/src/test_fixtures/shared.rs:88-134`,
  which is the same v6.17.1 format used by every `remove_missing`,
  `replace`, and command-level test.

No captured-from-tool fixture exists for the missing-device shape in
either lane. The closest fixture, `btrfs-device-usage-removing.txt`, is
captured from a live in-progress removal -- the path token is still
`/dev/mapper/disk3`, not `missing`.

The unstable VM canary (`just test-all-unstable`, per `justfile:152`)
does run `braid-remove-missing-enospc` (registered under `checks` at
`flake.nix:597`) against nixos-unstable, so CLI-visible drift in the
missing-device output would eventually surface there. What's missing
is a cheap, parser-specific signal in either fixture lane: a
btrfs-progs format change in the missing-device stanza (the `missing`
path token, `Device size: 0`, the `Device slack` rendering, or the
allocation rows) would pass every Rust parser test in both `nixos-25.11`
and `nixos-unstable` lanes, and would only surface via the heavier full
VM canary -- and on the stable lane, only via
`braid-remove-missing-enospc.py` on the pinned toolchain.

The intended outcome: a captured-from-tool golden fixture that exercises
the missing-device branch in both the stable contract lane and the
unstable forecast lane, so future drift in btrfs-progs's
missing-device output surfaces as a parser-test failure -- targeted at
the parsed shape `check_relocation_space` and friends actually depend
on -- before the full VM canary catches it.

## Approach

Three small edits, plus capturing the resulting fixtures into both lane
directories.

### 1. Add the capture line to `tests/capture-tool-fixtures.py`

The script already creates a clean degraded RAID1 for the
`btrfs-device-stats-degraded.{json,txt}` fixtures at lines 319-339: it
re-mkfs's the pool, mounts cleanly on `braid-vdb`, unmounts, closes
`braid-vdc`, then mounts `braid-vdb` with `-o degraded` and captures
the two `device stats` outputs.

Two splices into this block:

**1a. Write data on the healthy mount so Data,RAID1 chunks exist.**
Between the clean mount (`mount /dev/mapper/braid-vdb {MOUNT}`) and
the subsequent unmount, add:

```python
machine.succeed(f"dd if=/dev/urandom of={MOUNT}/degradeddata bs=1M count=16")
machine.succeed("sync")
```

This is necessary because btrfs allocates chunks lazily: a fresh
mkfs'd pool with no writes has no `Data,RAID1` row at all, so the
allocation assertions below would have nothing to lock. 16 MB is
enough to trigger a Data,RAID1 chunk allocation (default Data chunk
size is 256 MB) on both members; after closing `braid-vdc` and
mounting degraded, the missing devid 2 stanza will still carry the
Data,RAID1 row btrfs tracked at mkfs/write time. Writing data does
not change the existing `btrfs-device-stats-degraded.{json,txt}`
fixtures, which only report error counters.

**1b. Capture the missing-device usage output.** After the two
existing `device stats` captures and before the trailing `umount`,
add:

```python
machine.succeed(
    f"btrfs device usage --raw {MOUNT}"
    f" > {FIXTURE_DIR}/btrfs-device-usage-missing.txt"
)
```

Place 1b between the existing `btrfs-device-stats-degraded.txt`
capture (line 337) and the final `umount {MOUNT}` (line 339). Place
1a earlier in the same block, between the clean `mount` and `umount`
lines.

### 2. Add a `golden_test!` entry in `cli/tests/support/golden_common.rs`

Add a new block immediately after the existing
`golden_btrfs_device_usage_removing` (around line 464). Follow the
project's `// Intent` / `// Why it exists` / `// Scenario` test-preamble
convention (per `AGENTS.md` and the existing `golden_upsc_*` blocks at
lines 502-584 of the same file), and use the `is_dm_or_mapper_path`
helper already in scope. The assertions lock every parser-visible
shape the missing-device branch depends on: path token, `device_size`,
`device_slack`, the implicit `Unallocated` field-presence check from
the parser's required-field validation, and the presence of `RAID1`
rows with `bytes > 0` for each of the three allocation types
(`Data`, `Metadata`, `System`) that `check_raid1_relocation_space`
sums independently.

```rust
// Intent: lock the captured-from-tool shape of a missing-device
//   stanza in `btrfs device usage --raw` -- path token, sizes,
//   and allocation rows -- against btrfs-progs output drift.
// Why it exists: `check_relocation_space` and the shared
//   `DeviceUsageSpec::missing` builder both encode the v6.17.1
//   format inline; without a live-tool golden, a btrfs-progs
//   format change in either lane would pass the parser tests
//   while making the ENOSPC preflight under `remove_missing`
//   undercount any of the Data, Metadata, or System chunks it
//   sums independently on the absent member.
// Scenario: degraded 2-disk RAID1 with one member's LUKS mapper
//   closed; btrfs renders the absent device as `missing, ID: 2`
//   with Device size 0 but its Data,RAID1, Metadata,RAID1, and
//   System,RAID1 chunks still tracked on devid 2.
golden_test!(
    golden_btrfs_device_usage_missing,
    "btrfs-device-usage-missing.txt",
    "btrfs device usage",
    parse::btrfs_device_usage::parse_btrfs_device_usage,
    |out: parse::types::BtrfsDeviceUsageOutput| {
        assert_eq!(out.devices.len(), 2, "expected 2 devices");

        // devid 1 -- the surviving live member.
        assert_eq!(out.devices[0].devid, 1);
        assert!(
            is_dm_or_mapper_path(&out.devices[0].path),
            "devid 1 path should be dm or mapper, got: {}",
            out.devices[0].path
        );
        assert!(
            out.devices[0].device_size > 0,
            "live device_size should be positive"
        );

        // devid 2 -- the absent member. This is the shape
        // check_relocation_space and DeviceUsageSpec::missing
        // both depend on.
        let missing = &out.devices[1];
        assert_eq!(missing.devid, 2);
        assert_eq!(
            missing.path, "missing",
            "missing-device path token must be the literal `missing`",
        );
        assert_eq!(
            missing.device_size, 0,
            "missing-device Device size must be 0",
        );
        assert_eq!(
            missing.device_slack, 0,
            "missing-device Device slack must be 0 \
             (calc_slack_size returns 0 when device_size == 0)",
        );
        // No exact assertion on missing.unallocated: btrfs prints
        // Unallocated as `devinfo->size - allocated` where
        // `devinfo->size` is `dev_info.total_bytes` even on the
        // missing branch (filesystem-usage.c:833, 1337), so the value
        // is positive and depends on how much data was written before
        // the device went missing. The parser already errors out with
        // ParseError::MissingField if the `Unallocated:` line is
        // absent, so a successful parse implicitly locks the line's
        // presence.
        //
        // Allocation rows must survive parsing for every type
        // check_raid1_relocation_space sums independently:
        // `for alloc_type in ["Data", "Metadata", "System"] { ... }`
        // at preflight.rs:327. `bytes_on_target == 0` silently
        // skips that type's ENOSPC check (preflight.rs:333-335),
        // so dropping or renaming any one row -- not just Data --
        // would let production undercount relocation demand while
        // the parser still parses. A freshly-mkfs'd RAID1 pool with
        // a 16 MiB write allocates all three chunk types on both
        // members; once braid-vdc is closed, devid 2 still tracks
        // them. Loop over the same triple production iterates.
        for required in &["Data", "Metadata", "System"] {
            let bytes: u64 = missing
                .allocations
                .iter()
                .filter(|a| a.alloc_type == *required && a.profile == "RAID1")
                .map(|a| a.bytes)
                .sum();
            assert!(
                bytes > 0,
                "missing device must have a {required},RAID1 allocation \
                 with bytes > 0 (got allocations: {:?})",
                missing.allocations,
            );
        }
    }
);
```

The block driver constants in `cli/tests/golden_nixos_25_11.rs` and
`cli/tests/golden_nixos_unstable.rs` `include!` `golden_common.rs`, so a
single `golden_test!` block runs in both lanes automatically. In the
unstable lane (`REQUIRE_FIXTURES = true`), a missing fixture file
panics; in the stable lane it skips. This is the existing convention --
no change to the include wiring is needed.

### 3. Capture and commit fixture files for both lanes

Run the captures and check in the two new fixture files:

- `cli/tests/fixtures/nixos-25.11/btrfs-device-usage-missing.txt`
- `cli/tests/fixtures/nixos-unstable/btrfs-device-usage-missing.txt`

Per `AGENTS.md`, both lanes regenerate from the same
`tests/capture-tool-fixtures.py` via `--override-input` on the unstable
nixpkgs. The two fixture files land in the same commit as the new
`golden_test!` block so the unstable lane (which panics on missing
fixture) is never red.

## Things explicitly kept

- The inline `device_usage_parses_missing_device_marker` unit test in
  `cli/src/parse/btrfs_device_usage.rs:227-265`. It pins the exact
  byte-level format (column widths, indentation) that the new golden
  test only checks structurally. These are complementary.
- `DeviceUsageSpec::missing` in `cli/src/test_fixtures/shared.rs`. It
  remains the shared builder for command-level tests; the new golden
  test backstops its format assumption so its v6.17.1 pin no longer
  drifts silently.
- The existing `golden_btrfs_device_usage` (`-2disk`) and
  `golden_btrfs_device_usage_removing` blocks -- both still valid;
  the new block is purely additive.

## Files touched

- `tests/capture-tool-fixtures.py` -- two inserts inside the degraded
  block (lines 319-339): a `dd`+`sync` pair on the healthy mount, and
  a `btrfs device usage --raw` capture right before the trailing
  unmount.
- `cli/tests/support/golden_common.rs` -- one new `golden_test!` block
  next to `golden_btrfs_device_usage_removing` (around line 464), with
  the project's three-line test-preamble.
- `cli/tests/fixtures/nixos-25.11/btrfs-device-usage-missing.txt` --
  new captured fixture.
- `cli/tests/fixtures/nixos-unstable/btrfs-device-usage-missing.txt` --
  new captured fixture.

No changes to `parse/btrfs_device_usage.rs`, no changes to
`check_relocation_space`, no changes to `DeviceUsageSpec`, no changes
to any lane-driver `golden_nixos_*.rs` file.

## Verification

1. `just capture-all-fixtures` -- regenerates the stable fixture
   directory, producing
   `cli/tests/fixtures/nixos-25.11/btrfs-device-usage-missing.txt`.
2. `just capture-all-fixtures-unstable` -- same, against the unstable
   nixpkgs, producing the unstable copy.
3. Read both captured fixtures and confirm the second stanza begins
   with the literal line `missing, ID: 2` and contains a
   `Device size:` line whose value is exactly `0`.
4. `just test-rust` -- the new `golden_btrfs_device_usage_missing`
   passes in the stable lane.
5. `just test-rust-unstable` -- the same test passes in the unstable
   lane (and would now panic instead of skip if the unstable fixture
   were missing).
6. To confirm the test would actually catch drift, locally hand-edit
   the captured fixture (e.g., change `missing` to `<missing>`, or
   delete the `Metadata,RAID1` or `System,RAID1` row from the missing
   stanza), re-run `just test-rust`, and verify the assertion fires.
   Revert afterward.

Steps 1-2 are the capture-script regression check on their own: a
broken capture-tool-fixtures script fails the `nix build
.#checks.{system}.capture-tool-fixtures` invocation each `just
capture-all-fixtures*` recipe runs.

## Implementation notes

- The capture disproved this plan's central premise. `btrfs device usage --raw`
  on a degraded mount renders the absent device's path token as the literal
  `<missing disk>`, not `missing`. The token comes from the Linux kernel:
  `btrfs_dev_name()` (`reference/linux/fs/btrfs/volumes.h`) returns
  `<missing disk>` for a device with BTRFS_DEV_STATE_MISSING set, delivered via
  the BTRFS_IOC_DEV_INFO ioctl; btrfs-progs copies `dev_info.path`
  (`load_device_info`, shared by both `device usage` and `filesystem usage`) and
  only falls back to the literal `missing` when the ioctl returns an empty path.
  The plan's `missing` assumption came from reading that btrfs-progs fallback
  (`filesystem-usage.c:821`) without checking the kernel path under
  `reference/linux/`. Confirmed byte-identical in both lanes (btrfs-progs 6.17.1
  and 6.19.1). Everything else the plan predicted held exactly: Device size 0,
  Device slack 0, all three Data/Metadata/System RAID1 rows present with
  bytes > 0, positive Unallocated.
- Per user decision ("correct everywhere"), the real `<missing disk>` token was
  applied beyond this plan's "Things explicitly kept" scope. Corrected from the
  fictional `missing` to `<missing disk>`: the new golden test, the inline
  parser unit test `device_usage_parses_missing_device_marker`
  (`cli/src/parse/btrfs_device_usage.rs`), and `DeviceUsageSpec::missing`
  (`cli/src/test_fixtures/shared.rs`) -- renderer default, doc comments, and its
  `device_usage_raw_body_renders_canonical_live_and_missing_devices` pin test.
  Production is unaffected: `remove_missing`/`replace` key on devid, never the
  path token, so all command-level tests reusing `DeviceUsageSpec::missing` pass
  unchanged (2156 lib tests, 0 failures).
- Fixtures were captured via direct
  `nix build .#checks.<system>.capture-tool-fixtures` (plus
  `--override-input nixpkgs .../nixos-unstable`) and a selective single-file
  copy of `btrfs-device-usage-missing.txt` into each lane, not the
  `just capture-all-fixtures{,-unstable}` wrappers. The wrappers regenerate the
  whole fixture set (`cp -f result/fixtures/*`) and the unstable wrapper
  `rm -rf`s the lane dir, which would churn unrelated UUID-bearing fixtures and
  the unstable progress/ups fixtures + README -- contradicting this plan's
  two-new-file scope. The underlying VM build is identical either way.
