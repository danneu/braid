# Plan: Pre-flight target-size validation in `braid replace`

## Context

`braid replace` currently has no pre-flight size check on the target
disk. btrfs's own check (`reference/btrfs-progs/cmds/replace.c:287-297`)
fires at exec time: it reads `srcdev_size` from `di_args[i].total_bytes`
via `BTRFS_IOC_DEV_INFO` (line 261) and `dstdev_size` from
`device_get_partition_size(/dev/mapper/<new>)` (line 287), then refuses
when `srcdev_size > dstdev_size`. That backstop fires AFTER braid's
destructive `cryptsetup luksFormat` on a fresh-LUKS target and AFTER
journal write (`cli/src/replace.rs:590`), stranding `pending-op.json`.

`tests/repro/btrfs-replace-rejects-smaller-target.py` was added to inform
whether braid should add this preflight. `AGENTS.md` "Mutation Safety
Heuristics" resolves it: every uncertainty in a branch that can strand a
journal is a hard error.

This plan adds a fail-closed, plan-time size check that mirrors btrfs's
own `srcdev_size > dstdev_size` comparison, BEFORE any journal write,
luksFormat, or luksOpen.

## Revision history

- **v5 (this version)**: cleanup pass folding in two findings from
  review of v4. The "Files changed" entry for `cli/src/btrfs_ioctl.rs`
  still carried v3's stale "1056 bytes / unused[379] / no fsid" ABI;
  it now references Decision 2's authoritative 4096-byte layout and
  ABI guard so the section can't lead the implementer back to the
  wrong struct. VM subtest 3 (PresentLuks target with fixed segment
  size) is dropped: `cryptsetup luksFormat --offset` only calls
  `crypt_set_data_offset` (`reference/cryptsetup/src/cryptsetup.c:1587-1591`),
  so the LUKS2 segment still has `size: "dynamic"` -- the test
  would not exercise the `Luks2SegmentSize::Fixed` branch.
  Fixed-segment coverage moves entirely to the parser + preflight
  unit tests with synthetic JSON.
- v4 (superseded): added the nix `"ioctl"` feature, corrected the
  ABI struct layout in Decision 2, and switched to `Path::new(...)`
  for the mount-point conversion.
  v3 said the nix ioctl macros are available regardless of features
  -- wrong, `nix::sys::ioctl` is gated on `feature = "ioctl"` per
  `reference/nix-crate/src/sys/mod.rs`. v3 also described the
  `btrfs_ioctl_dev_info_args` struct with `unused[379]` and no
  `fsid` field, but `reference/linux/include/uapi/linux/btrfs.h:245`
  defines `fsid[16]` + `unused[377]` + `path[1024]` (4096 bytes
  total, padded to 4 KiB) -- and the kernel encodes
  `sizeof(struct)` into the ioctl request number, so a wrong shape
  would silently call the wrong ioctl. v3's preflight snippet wrote
  `config.mount_point().as_ref()`, but `MountPoint` only implements
  `as_str()` (`cli/src/types.rs:383-391`); the established pattern
  is `Path::new(cfg.mount_point().as_str())` per
  `cli/src/online_state.rs:265`.
- v3 (superseded): pivoted from persisted `DiskMember.size_bytes`
  to a `BTRFS_IOC_DEV_INFO` ioctl helper.
  v2 mirrored btrfs's source-size value via a persisted `size_bytes`
  on `DiskMember`. That made the change cross `add`, `replace`,
  `discover`, and `recover` more deeply than v2 accounted for: the
  suggested `discover --write` remediation conflicts with
  `write_discovered_membership`'s healthy-pool.json refusal
  (`cli/src/discover.rs:595-600`); replace's pre-resize membership
  commit (`cli/src/replace.rs:857`) does not match v2's "capture
  after resize but before commit" ordering; and recovery paths
  rebuild `DiskMember` manually so they would silently drop
  `size_bytes`. v2 also computed `target_mapper = raw - offset` for
  existing LUKS targets, ignoring that LUKS2 segment `size` can be
  fixed (`reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2531`).
  **Pivot**: drop persistence. Source size now comes from a direct
  `BTRFS_IOC_DEV_INFO` ioctl helper (the exact source btrfs replace
  uses), which works for live and missing devids alike without
  touching add/discover/recover. Target capacity now respects the
  fixed-vs-dynamic LUKS2 segment size.
- v2 (superseded): pivoted from `btrfs filesystem show --raw`
  (which prints `size 0` for missing devices, per
  `reference/btrfs-progs/cmds/filesystem.c:400-407`) to a persisted
  `DiskMember.size_bytes`.

## Design decisions (locked)

### 1. Source size: read `total_bytes` via `BTRFS_IOC_DEV_INFO`

Mirror btrfs replace's own source: open the mount, issue the ioctl
for the resolved devid, read `total_bytes`. Works for both Live and
Missing source paths because `BTRFS_IOC_DEV_INFO` returns the same
value btrfs's own preflight reads (`reference/btrfs-progs/cmds/replace.c:245-261`).

- Live source: ioctl on `replace_source.devid` (resolved from
  `pool.devices` by `old_uuid` upstream of the preflight).
- Missing source: ioctl on `replace_source.devid` (the missing
  devid -- already known at plan time via `resolve_replace_source`).
- Refuses if the ioctl fails, the devid is not in the returned set
  (the kernel returns `-ENODEV` for unknown devids), or
  `total_bytes == 0`.

### 2. New `BtrfsDevInfo` trait + `nix::ioctl_readwrite!` wrapper

**Cargo feature**: `nix::sys::ioctl` is gated on the `ioctl` feature
(`reference/nix-crate/src/sys/mod.rs`). `cli/Cargo.toml:16` currently
enables `["fs", "user", "term", "signal"]`; add `"ioctl"` to that
list.

New module `cli/src/btrfs_ioctl.rs`. The struct mirrors
`reference/linux/include/uapi/linux/btrfs.h:245-262` byte-for-byte
(the kernel encodes `sizeof(struct)` into the ioctl request number,
so any drift in the field shape silently calls the wrong ioctl):

```rust
// Mirrors struct btrfs_ioctl_dev_info_args from
// reference/linux/include/uapi/linux/btrfs.h:245-262. The kernel
// encodes sizeof(struct) into BTRFS_IOC_DEV_INFO's request number;
// the layout below MUST remain 4096 bytes (4 KiB) to call the right
// ioctl. The `fsid` field was added in kernel v6.3 but the slot
// existed in `unused` before then, so the struct size is stable.
#[repr(C)]
pub struct BtrfsIoctlDevInfoArgs {
    pub devid: u64,                  // in/out
    pub uuid: [u8; 16],              // in/out (BTRFS_UUID_SIZE)
    pub bytes_used: u64,             // out
    pub total_bytes: u64,            // out
    pub fsid: [u8; 16],              // out (since kernel 6.3)
    pub unused: [u64; 377],          // pad to 4 KiB
    pub path: [u8; 1024],            // out (BTRFS_DEVICE_PATH_NAME_MAX)
}

// Compile-time ABI guard. If this fails, the layout has drifted and
// the generated ioctl number will not match the kernel's expectation.
const _: () = assert!(std::mem::size_of::<BtrfsIoctlDevInfoArgs>() == 4096);

nix::ioctl_readwrite!(btrfs_dev_info_raw, 0x94, 30, BtrfsIoctlDevInfoArgs);

pub trait BtrfsDevInfo {
    fn total_bytes(&self, mount: &Path, devid: u64) -> Result<u64, BtrfsIoctlError>;
}

pub struct LinuxBtrfsDevInfo;
impl BtrfsDevInfo for LinuxBtrfsDevInfo {
    fn total_bytes(&self, mount: &Path, devid: u64) -> Result<u64, BtrfsIoctlError> {
        // open(mount, O_RDONLY); zero args; set devid; ioctl; return total_bytes
    }
}
```

The trait mirrors the existing `Filesystem` trait pattern
(`cli/src/probe.rs:18-23`): real impl wraps the syscall, mock impls
in `#[cfg(test)]` answer from a `HashMap<(PathBuf, u64), u64>`.

Reference for the ioctl number (`0x94`, nr 30), struct shape, and
size: `reference/linux/include/uapi/linux/btrfs.h:245-262`. The
constants `BTRFS_UUID_SIZE = 16` and `BTRFS_DEVICE_PATH_NAME_MAX =
1024` are defined in the same header (search for the `#define`s);
inline them as numeric literals in `btrfs_ioctl.rs` with the
citation in the doc comment above.

### 3. Target size: respect LUKS2 segment `size` and `offset`

Compute the mapper capacity btrfs will see (`BLKGETSIZE64` on
`/dev/mapper/<new>`) without opening the mapper:

- **`PresentLuks` target**: run `cryptsetup luksDump --dump-json-metadata <new_by_id>`.
  Parse `segments."0"`:
  - `offset` is mandatory (string of bytes; default 16777216 per
    `reference/cryptsetup/lib/luks2/luks2.h:145`).
  - `size` is mandatory and either `"dynamic"` (string) or a string
    of bytes (`reference/cryptsetup/lib/luks2/luks2_json_metadata.c:720-723`).
    When `"dynamic"`, mapper capacity is `raw_target - offset`.
    When fixed (u64), mapper capacity is `segment_size` directly
    (`reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2531-2535`).
  - `raw_target` comes from `lsblk -b` on `new_by_id`.
- **`PresentNotLuks` target**: reject offset-affecting LUKS format
  options (decision 4), compute `target_capacity = raw_target -
  LUKS2_DEFAULT_HDR_SIZE` (16777216, defined as a constant in
  `cli/src/luks.rs`).
- Refuses on: luksDump failure, missing/empty `segments."0"`,
  malformed offset or size, `raw_target == None`, `raw_target <=
  offset` (underflow), fixed segment_size == 0.

### 4. Reject payload-offset-changing LUKS format options

`cli/src/types.rs::is_managed_format_flag` rejects identity flags
today (`--uuid`, `--label`, `--header`, integrity opts) but allows
payload-offset-affecting flags. Add to `MANAGED_LUKS_FORMAT_LONG_FLAGS`:

- `--offset` (`reference/cryptsetup/src/cryptsetup_arg_list.h:135`)
- `--align-payload` (line 13)
- `--luks2-metadata-size` (line 115)
- `--luks2-keyslots-size` (line 113)
- `--sector-size` (line 175; conservative -- changes alignment)

And to `MANAGED_LUKS_FORMAT_SHORT_FLAGS` (currently `&['d', 'S', 'M',
'I', 'l']` at `cli/src/types.rs:308`):

- `'o'` (short alias for `--offset`)

### 5. Comparison

`target_capacity >= source_total_bytes` -> Ok. Otherwise refuse with
both sizes via `confirm::format_bytes`. Wording uses `--`, per
`AGENTS.md` "CLI Output Style".

### 6. Fail-closed scope

Hard refusal (`ReplaceError::Validation` via
`PlanFailure::with_notes(notes, ...)`) on every uncertainty:

- `BtrfsDevInfo::total_bytes` fails or returns `0`.
- `cryptsetup luksDump --dump-json-metadata` fails or omits `segments."0"`.
- `segments."0".offset` or `size` is missing or unparseable.
- `lsblk -b` on `new_by_id` returns empty.
- Underflow when computing `raw - offset`.
- Comparison fails (`target_capacity < source_total_bytes`).

No `PreviewNote::Warn` skip. Matches `AGENTS.md` "Set fail-closed
policy from the downstream failure mode".

### 7. Helper location

`cli/src/preflight.rs`, alongside `check_single_survivor_capacity`
(commit `3e88a7b`). Returns `Result<(), String>`; the caller wraps
in `ReplaceError::Validation`.

### 8. Plumbing the new trait

`plan_replace` already takes `runner: &R, fs: &F`. Add a third
generic param `dev_info: &D` where `D: BtrfsDevInfo`. Match the
existing trait-plumbing style in this file (`R: CommandRunner +
Sync, F: Filesystem + ?Sized`).

`cmd_replace` (`cli/src/replace.rs:1452`) currently constructs `&PanicFilesystem`-like
runtime fixtures from `params`. Adding `dev_info` propagates outward
to `cmd_replace` callers; the top-level dispatch (`cli/src/main.rs`
or `cli/src/dispatch.rs`) constructs `LinuxBtrfsDevInfo` once and
threads it through. Mirrors the existing pattern for `runner` and
`fs`.

## Files changed

### `cli/Cargo.toml`

Add `"ioctl"` to the `nix` feature list at line 16. Before:
`features = ["fs", "user", "term", "signal"]`. After:
`features = ["fs", "user", "term", "signal", "ioctl"]`. The
`nix::ioctl_readwrite!` macro and `nix::sys::ioctl` module are both
gated on this feature (`reference/nix-crate/src/sys/mod.rs`).

### `cli/src/btrfs_ioctl.rs` (new)

- `#[repr(C)]` struct `BtrfsIoctlDevInfoArgs` mirroring
  `reference/linux/include/uapi/linux/btrfs.h:245-262` byte-for-byte
  (4096 bytes total: 8 devid + 16 uuid + 8 bytes_used + 8
  total_bytes + 16 fsid + 8 * 377 unused + 1024 path). See Decision 2
  for the literal field declaration and rationale.
- Compile-time ABI guard:
  `const _: () = assert!(std::mem::size_of::<BtrfsIoctlDevInfoArgs>() == 4096);`
- `nix::ioctl_readwrite!(btrfs_dev_info_raw, 0x94, 30, BtrfsIoctlDevInfoArgs);`
- `pub trait BtrfsDevInfo` with one method (`total_bytes`).
- `pub struct LinuxBtrfsDevInfo;` real impl: open mount with
  `nix::fcntl::open(O_RDONLY)`, zero a `BtrfsIoctlDevInfoArgs`, set
  `devid`, call `btrfs_dev_info_raw(fd, &mut args)`, on `ENODEV` map
  to `BtrfsIoctlError::DevidNotFound { devid }`, else propagate.
- Error enum `BtrfsIoctlError` with variants `OpenFailed`,
  `IoctlFailed { errno }`, `DevidNotFound { devid }`.
- Test impls: `pub struct MockBtrfsDevInfo { map: HashMap<(PathBuf,
  u64), u64> }` and `PanicBtrfsDevInfo` (asserts no calls happen,
  matching the `PanicRunner` / `PanicFilesystem` pattern at
  `cli/src/test_fixtures/`).

### `cli/src/parse/cryptsetup_luks_dump.rs` + `cli/src/parse/types.rs`

- Extend `RawSegment` with `offset: String` and `size: String`
  (both mandatory in LUKS2 JSON).
- Extend `CryptsetupLuksDumpOutput` with `segment_offset_bytes: u64`
  and `segment_size: Luks2SegmentSize` where:
  ```rust
  pub enum Luks2SegmentSize {
      Dynamic,
      Fixed(u64),
  }
  ```
- Parse logic: `segments["0"].offset.parse::<u64>()` mandatory;
  `segments["0"].size` is `"dynamic"` -> `Dynamic`, else
  `Fixed(parse::<u64>())`.
- Refuse parse if `segments["0"]` is missing or values are
  malformed (`ParseError::InvalidJson` naming the field).
- Update the existing fixture-backed test
  (`cli/tests/fixtures/nixos-25.11/cryptsetup-luks-dump.json` shows
  `"offset":"16777216","size":"dynamic"`) to assert both fields.
- Add unit tests:
  - `parse_extracts_fixed_segment_size` (synthesize a fixture
    with `"size":"1073741824"`).
  - `parse_rejects_missing_segment_zero`.
  - `parse_rejects_malformed_offset`.
  - `parse_rejects_malformed_size`.

### `cli/src/types.rs` (LUKS format extras)

- `MANAGED_LUKS_FORMAT_LONG_FLAGS`: add the five flags listed in
  decision 4.
- `MANAGED_LUKS_FORMAT_SHORT_FLAGS`: add `'o'`.
- New unit tests:
  - `luks_format_extra_opts_rejects_offset` (long form).
  - `luks_format_extra_opts_rejects_offset_short` (`-o`).
  - `luks_format_extra_opts_rejects_align_payload`.
  - `luks_format_extra_opts_rejects_luks2_metadata_size`.
  - `luks_format_extra_opts_rejects_luks2_keyslots_size`.
  - `luks_format_extra_opts_rejects_sector_size`.
  - One `=`-suffix coverage test (mirrors existing `--uuid=` test at
    `cli/src/types.rs:741`).

### `cli/src/luks.rs`

- `pub const LUKS2_DEFAULT_HDR_SIZE: u64 = 16_777_216;` with a doc
  comment citing `reference/cryptsetup/lib/luks2/luks2.h:145`.

### `cli/src/preflight.rs`

New helper:

```rust
pub fn check_replace_target_capacity<R, D>(
    runner: &R,
    dev_info: &D,
    mount: &Path,
    source: ReplaceSourceProbe,
    target: ReplaceTargetProbe<'_>,
) -> Result<(), String>
where
    R: CommandRunner,
    D: BtrfsDevInfo,

pub struct ReplaceSourceProbe { pub devid: u64 }

pub enum ReplaceTargetProbe<'a> {
    PresentLuks { by_id: &'a str },
    PresentNotLuks { by_id: &'a str },
}
```

Internals:

- `source_total_bytes = dev_info.total_bytes(mount, source.devid)`.
  Error -> `Err(format!("failed to read btrfs total_bytes for devid
  {}: {e}", source.devid))`. Zero -> `Err(format!("btrfs reports
  total_bytes 0 for source devid {} -- cannot verify the new disk
  is large enough", source.devid))`.
- `target_capacity`: branch on `target`:
  - `PresentLuks { by_id }`:
    - `raw_target = query_disk_hw_info(runner, by_id).size`; `None`
      -> `Err(...)`.
    - Run `CryptsetupLuksDumpJson { device: by_id }` via runner;
      parse via `parse_cryptsetup_luks_dump`; on parse error,
      propagate as `Err(...)`.
    - `offset = parsed.segment_offset_bytes`.
    - Match on `parsed.segment_size`:
      - `Luks2SegmentSize::Dynamic`: `raw_target.checked_sub(offset)
        .ok_or_else(|| "target raw size smaller than LUKS2 segment
        offset -- header may be corrupt")?`.
      - `Luks2SegmentSize::Fixed(0)`: `Err("LUKS2 segment 0 has
        fixed size 0 -- header is malformed")`.
      - `Luks2SegmentSize::Fixed(n)`: `n`.
  - `PresentNotLuks { by_id }`:
    - `raw_target = query_disk_hw_info(runner, by_id).size`; `None`
      -> `Err(...)`.
    - `raw_target.checked_sub(LUKS2_DEFAULT_HDR_SIZE).ok_or_else(...)?`.
- Refuse if `target_capacity < source_total_bytes`:
  `format!("new disk is smaller than the disk being replaced --
  refusing to luksFormat / proceed. source devid {} btrfs size {} ({}),
  target {} mapper capacity {} ({}). Use a target at least as large
  as the source.", source.devid, source_total_bytes,
  format_bytes(source_total_bytes), by_id, target_capacity,
  format_bytes(target_capacity))`.

### `cli/src/replace.rs`

- `plan_replace` and `cmd_replace` gain a third generic `D:
  BtrfsDevInfo` and a `dev_info: &D` parameter.
- In `plan_replace`, after `new_probed` validation (around line
  1312) and before `assert_new_uuid_unique` (line 1384):
  ```rust
  let source_probe = ReplaceSourceProbe {
      devid: match &replace_source {
          ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
      },
  };
  let target_probe = match &new_probed.state {
      PresentConfigDiskState::PresentLuks { .. } =>
          ReplaceTargetProbe::PresentLuks { by_id: new_by_id.as_str() },
      PresentConfigDiskState::PresentNotLuks =>
          ReplaceTargetProbe::PresentNotLuks { by_id: new_by_id.as_str() },
  };
  let mount = Path::new(config.mount_point().as_str());
  if let Err(msg) = preflight::check_replace_target_capacity(
      runner, dev_info, mount, source_probe, target_probe,
  ) {
      return Err(PlanFailure::with_notes(
          notes, ReplaceError::Validation(msg),
      ));
  }
  ```
- **No changes** to the post-replace lifecycle (membership commit,
  journal rewrite, resize, close-old). v2's "capture size after
  resize" step is dropped entirely.

### `cli/src/dispatch.rs` (or wherever `cmd_replace` is called)

Construct `LinuxBtrfsDevInfo` once at top-level dispatch and thread
through to `cmd_replace`. Same shape as how `RealFilesystem` is
constructed today.

### `cli/src/add.rs`, `cli/src/discover.rs`, `cli/src/recover.rs`

**No changes.** v2's size-capture and recovery-update sites are not
needed in v3.

### `tests/cli/replace-rejects-smaller-target.nix` + `.py`

Two pool members + one undersized replacement, mirroring the existing
repro. The plan test should reach the real `BTRFS_IOC_DEV_INFO` path
(VM tests run a real kernel, so `LinuxBtrfsDevInfo` is exercised
end-to-end).

Subtest 1 -- **Live, fresh-LUKS target**:
- `braid add` disk1+disk2 (each 512 MiB).
- `braid replace --old disk2 --new disk3=/dev/disk/by-id/virtio-disk3
  --yes -p <passfile>` where disk3 is 256 MiB.
- Assert exit != 0; stderr contains "smaller than the disk being
  replaced"; `pending-op.json` does NOT exist; `cryptsetup isLuks
  /dev/disk/by-id/virtio-disk3` returns non-zero (disk was NOT
  luksFormatted).

Subtest 2 -- **Missing source**:
- `braid add` disk1+disk2.
- Make disk2 missing (`dmsetup remove braid-disk2; wipefs ...`).
- Run the same replace.
- Same assertions as subtest 1. Confirms the ioctl returns
  `total_bytes` for the missing devid (the data btrfs holds in
  metadata) -- this is the F1 case from v2 review.

**Fixed-segment branch coverage** is unit-test only. `cryptsetup
luksFormat --offset` only sets `crypt_set_data_offset`
(`reference/cryptsetup/src/cryptsetup.c:1587-1591`); LUKS2 format
still emits the crypt segment with `size: "dynamic"`. Fixed-size
segments in the wild come from reencrypt mid-operation states or
direct JSON manipulation, neither of which a VM test should
synthesize. The `Luks2SegmentSize::Fixed(_)` branch is exercised
by the parser test (`parse_extracts_fixed_segment_size`, synthetic
JSON with numeric `segments."0".size`) and the preflight tests
(`check_replace_target_capacity_existing_fixed_segment`,
`check_replace_target_capacity_refuses_when_fixed_size_zero`).

### `flake.nix`

Register `replace-rejects-smaller-target` next to
`replace-larger-disk` at line 336.

### `docs/commands/replace.md`

Add to "Safety checks / refusal cases" (after line 108):
- "Refuses if the new disk's mapper capacity is smaller than the
  source disk's btrfs `total_bytes` (read via `BTRFS_IOC_DEV_INFO`,
  the same value `btrfs replace start` compares against). For
  existing LUKS targets, mapper capacity is derived from the LUKS2
  segment `offset` and `size` (`dynamic` -> `raw - offset`, fixed
  -> the segment size). For fresh-LUKS targets, from the
  cryptsetup default 16 MiB offset (offset-affecting
  `--luks-format-arg` flags `--offset`/`-o`, `--align-payload`,
  `--luks2-metadata-size`, `--luks2-keyslots-size`,
  `--sector-size` are rejected for this reason)."

### `docs/internals/luks-unlock.md` (or new internals note)

One paragraph documenting: braid mirrors btrfs replace's exact
source-size authority by issuing `BTRFS_IOC_DEV_INFO` directly. The
ioctl is wrapped in the `BtrfsDevInfo` trait alongside the existing
`Filesystem` trait for testability; `LinuxBtrfsDevInfo` uses
`nix::ioctl_readwrite!`. Target capacity derives the
`BLKGETSIZE64`-equivalent value btrfs would see on
`/dev/mapper/<new>` from the LUKS2 segment metadata, accounting for
both dynamic-size segments (`raw - offset`) and fixed-size segments
(`segment.size`).

## New unit tests

### `cli/src/parse/cryptsetup_luks_dump.rs`
- Fixture-backed assertion for default `"size":"dynamic"`.
- Synthetic fixture for fixed `"size":"1073741824"`.
- Refusal tests for missing segments, malformed offset/size.

### `cli/src/types.rs`
- Six new reject tests listed in decision 4.

### `cli/src/btrfs_ioctl.rs`
- ABI guard: the `const _: () = assert!(size_of::<...>() == 4096);`
  at module scope catches struct-layout drift at compile time.
  Additionally, a `#[test]` named `btrfs_ioctl_dev_info_args_size_is_4kib`
  asserts `std::mem::size_of::<BtrfsIoctlDevInfoArgs>() == 4096`
  for explicit documentation in test reports.
- `MockBtrfsDevInfo` returns the configured value for `(mount,
  devid)`.
- `MockBtrfsDevInfo` returns `DevidNotFound` for unconfigured
  devids.
- One real ioctl smoke test gated behind `#[ignore]` (requires a
  mounted btrfs; documented but not run by default).

### `cli/src/preflight.rs`
- `check_replace_target_capacity_fresh_refuses_when_target_smaller`.
- `check_replace_target_capacity_fresh_accepts_equal_and_larger`.
- `check_replace_target_capacity_existing_dynamic_segment`.
- `check_replace_target_capacity_existing_fixed_segment`.
- `check_replace_target_capacity_refuses_when_dev_info_errors`.
- `check_replace_target_capacity_refuses_when_total_bytes_zero`.
- `check_replace_target_capacity_refuses_when_luks_dump_fails`.
- `check_replace_target_capacity_refuses_when_lsblk_none`.
- `check_replace_target_capacity_refuses_when_raw_below_offset`.
- `check_replace_target_capacity_refuses_when_fixed_size_zero`.

### `cli/src/replace.rs#tests`
- `plan_replace_refuses_when_target_smaller_live_fresh`.
- `plan_replace_refuses_when_target_smaller_live_existing_luks`.
- `plan_replace_refuses_when_target_smaller_missing`.
- `plan_replace_refuses_when_dev_info_devid_not_found`.
- Existing tests pass with `MockBtrfsDevInfo` returning sufficient
  source sizes by default in `test_fixtures/replace.rs`.

## Verification checklist

- `just test-rust` -- all new unit tests green; existing replace,
  add, remove, monitor, recover, lock, probe, doctor, discover
  tests green.
- `nix build .#checks.${system}.replace-rejects-smaller-target` --
  new VM test green (both subtests: Live fresh-LUKS undersized, and
  Missing source undersized -- the latter exercises the ioctl on a
  missing devid).
- `nix build .#checks.${system}.replace-larger-disk`,
  `replace-2disk-pool`, `replace-dead-disk`, `recover-replace-*` --
  regression guards.
- Manual VM check: 3 disks (512/512/256 MiB). Reproduce both
  subtests. Confirm `pending-op.json` never appears on refusal and
  `cryptsetup isLuks /dev/disk/by-id/virtio-disk3` returns non-zero
  (no LUKS header written to the undersized target).

## Out of scope

- Do NOT add the check to `add.rs` (no source disk to compare
  against).
- Do NOT modify btrfs's exec-time check.
- Do NOT touch `remove.rs`'s `check_single_survivor_capacity`.
- Do NOT add a `--skip-size-check` override (breaks fail-closed).
- Do NOT add `size_bytes` to `DiskMember` or touch `add` /
  `discover` / `recover` capture paths (v2 design, superseded).

## Critical files

- `/Users/dan/Code/braid/cli/Cargo.toml` (one feature added)
- `/Users/dan/Code/braid/cli/src/btrfs_ioctl.rs` (new)
- `/Users/dan/Code/braid/cli/src/types.rs` (LUKS format extras only)
- `/Users/dan/Code/braid/cli/src/parse/cryptsetup_luks_dump.rs`
- `/Users/dan/Code/braid/cli/src/parse/types.rs`
- `/Users/dan/Code/braid/cli/src/luks.rs` (one constant)
- `/Users/dan/Code/braid/cli/src/preflight.rs`
- `/Users/dan/Code/braid/cli/src/replace.rs` (plan_replace only)
- `/Users/dan/Code/braid/cli/src/dispatch.rs` (or main.rs -- one
  construction site)
- `/Users/dan/Code/braid/cli/src/test_fixtures/replace.rs`
- `/Users/dan/Code/braid/tests/cli/replace-rejects-smaller-target.nix` (new)
- `/Users/dan/Code/braid/tests/cli/replace-rejects-smaller-target.py` (new)
- `/Users/dan/Code/braid/flake.nix`
- `/Users/dan/Code/braid/docs/commands/replace.md`
- `/Users/dan/Code/braid/docs/internals/luks-unlock.md`
