# Fix TUI pool Usage showing >100% (unit mismatch)

## Context

`braid tui` reports `Usage 112% 782.1 GiB / 698.6 GiB (Estimated)` for a
3-disk RAID1 pool -- mathematically impossible. Two numbers in different
units are compared:

- `PoolState.capacity_used_bytes` (`cli/src/tui/probe.rs:271`) is
  `fs_usage.used_bytes` -- the `Used:` line from `btrfs filesystem usage
  --raw`. That is the **aggregate raw** byte count across all block
  group types (Data, Metadata, System), i.e. every mirror copy is
  counted.
- `PoolState.capacity_total_bytes` (`cli/src/tui/probe.rs:195`) is
  `estimate_pool_capacity(device_sizes)` = `min(sum/2, sum - max)` --
  **logical** usable RAID1 capacity (already halved).

At 56% real usage the view renders 112%.

`cli/src/status.rs:628` has the same bug in `CapacityReport.used_bytes`.
It is less visible there because `braid status` prints Used/Free/Total
as separate lines with no percent.

Intended outcome: `capacity_used_bytes` and `CapacityReport.used_bytes`
both mean **logical filesystem-used bytes**, defined precisely as:

```
Data.used + Metadata.used + System.used    (excluding GlobalReserve)
```

using the per-type `used` values reported by `btrfs filesystem df
--json`. Metadata and System are intentionally included -- they are
real on-disk filesystem consumption, not overhead to hide. Only
`GlobalReserve` is excluded, because btrfs docs define it as
artificial/internal emergency space accounted within metadata, not
additional stored bytes.

With that contract, `used <= total` is an invariant and the rendered
percent is always in range.

## Fix approach

Do **not** divide `fs_usage.used_bytes` by `fs_usage.data_ratio`.
`Data ratio:` describes only the data profile; `Used:` aggregates data +
metadata + system, each potentially on independent profiles (e.g. Data
RAID1, Metadata DUP). Dividing aggregate by one ratio preserves the
same class of unit bug in a narrower form.

Instead, derive logical used from `btrfs filesystem df --json`, which
reports per-block-group `used` already in logical bytes. The output is
already parsed at `cli/src/parse/btrfs_filesystem_df.rs` and already
reached in both code paths (`cli/src/tui/probe.rs:41`,
`cli/src/status.rs:569`).

Upstream rationale: per btrfs-progs docs, `btrfs filesystem df`'s
per-type `used` is the space occupied by file extents (Data),
metadata blocks (Metadata), and small internal structures (System).
`GlobalReserve` is described as an artificial reservation carved out
of metadata for emergency writes; it is not additional stored data.
Summing Data + Metadata + System and excluding GlobalReserve
therefore matches "how full is this filesystem" without double-counting
or phantom bytes.

### Shared helper

Add to `impl BtrfsDfOutput` in `cli/src/parse/types.rs` (next to the
existing `profiles_for` on the same type, ~line 105):

```rust
/// Logical filesystem-used bytes: Data.used + Metadata.used +
/// System.used, excluding GlobalReserve. GlobalReserve is an internal
/// emergency reservation carved out of Metadata, not additional
/// on-disk data.
pub fn logical_used_bytes(&self) -> u64 {
    self.entries
        .iter()
        .filter(|e| e.bg_type != BtrfsBgType::GlobalReserve)
        .map(|e| e.bg_used)
        .sum()
}
```

### Call sites

1. `cli/src/tui/probe.rs:271` -- `df` is already in scope:
   ```rust
   capacity_used_bytes: df.logical_used_bytes(),
   ```

2. `cli/src/status.rs` -- today `summarize_df` (line 562) both runs
   `BtrfsFilesystemDfJson` and consumes the result into a `DfSummary`
   that discards the raw entries; `get_capacity` (line 605) does not
   see df at all. `DfSummary` cannot feed the new helper because the
   per-type `bg_used` values are flattened away into
   `AllocationEntry`.

   Prescribed wiring (single path -- do **not** double-run the df
   command and do **not** try to reconstruct from `DfSummary`):

   a. At each top-level status caller (line 350 and line 452), run
      `BtrfsFilesystemDfJson` once and parse to `BtrfsDfOutput` --
      e.g. a small `fn fetch_df(runner, mount_point) ->
      Result<BtrfsDfOutput, StatusError>` or inline parse.
   b. Change `summarize_df`'s signature to
      `fn summarize_df(df: &BtrfsDfOutput) -> DfSummary` -- pure,
      no runner. Move its profile/allocation logic to consume
      `df.entries` directly.
   c. Change `get_capacity`'s signature to take
      `df: &BtrfsDfOutput` as an additional parameter.
   d. At the callers, pass the same `&df` into both `summarize_df`
      and `get_capacity` so they see the same snapshot.
   e. Inside `get_capacity`, replace line 628:
      ```rust
      used_bytes: df.logical_used_bytes(),
      ```

   `free_bytes` stays `usage.free_estimated_bytes` (already logical --
   btrfs's Free (estimated) answers "how much more can I write", in
   user-visible space).

## Tests

### New helper unit test

**Location:** `cli/src/parse/btrfs_filesystem_df.rs` tests module
(next to the existing `profiles_for` test around line 179).

**Test:** `logical_used_bytes_excludes_global_reserve`. A focused
contract test on the helper itself, independent of probe/status
wiring. Construct a `BtrfsDfOutput` in-memory with one entry of each
block group type, including a **nonzero** `GlobalReserve.bg_used` to
prove the filter actually excludes it rather than happening to be
zero in fixtures.

```rust
let df = BtrfsDfOutput {
    entries: vec![
        BtrfsDfEntry { bg_type: BtrfsBgType::Data,          bg_profile: BtrfsProfile::Raid1, bg_used: 100, bg_total: 200 },
        BtrfsDfEntry { bg_type: BtrfsBgType::Metadata,      bg_profile: BtrfsProfile::Dup,   bg_used:  20, bg_total:  40 },
        BtrfsDfEntry { bg_type: BtrfsBgType::System,        bg_profile: BtrfsProfile::Dup,   bg_used:   3, bg_total:  10 },
        BtrfsDfEntry { bg_type: BtrfsBgType::GlobalReserve, bg_profile: BtrfsProfile::Single, bg_used: 999, bg_total: 999 },
    ],
};
assert_eq!(df.logical_used_bytes(), 123); // 100 + 20 + 3, GlobalReserve's 999 excluded
```

The nonzero 999 is the load-bearing piece: a forgotten filter would
yield 1122 and the test fails loudly.

### New probe-layer regression test

**Location:** `cli/src/tui/probe.rs` tests module, alongside
`test_probe_2disk_raid1_pool` (~line 648-818).

**Test:** `test_probe_raid1_pool_high_usage_unit_invariant` -- fails on
master, passes after fix.

**Setup:** extend the 2-disk mock to sizes/usage large enough to trip
the >100% bug pre-fix. A compact scenario avoids big numbers:

- 2x equal disks, `Device size: 536870912` each (same as existing test
  to minimise mock churn)
- `BtrfsFilesystemUsageRaw` with `Used: 570458112` (raw -- the sum of
  mirrored allocations implied by the df mock: Data RAID1 used x2 +
  Metadata DUP used x2 + System DUP used x2 = 536870912 + 33554432 +
  32768), `Data ratio: 2.00`, `Free (estimated): <something logical>`
- `BtrfsFilesystemDfJson` with:
  - Data,RAID1: `used = 268435456`, `total = 268435456` (logical)
  - Metadata,DUP: `used = 16777216`, `total = 33554432`
  - System,DUP: `used = 16384`, `total = 8388608`
  - GlobalReserve: any values -- must be excluded by the helper
- `BtrfsDeviceUsageRaw`: 2 devids with per-device allocation consistent
  with the df logical totals (each disk holds 268 MB Data + 33 MB
  Metadata + 8 MB System raw).

Why `Used: 570458112`: on **master**, the buggy path compares that raw
570_458_112 to `capacity_total_bytes = 536_870_912`, producing the
user-visible `used > total` (~106%) that is the actual failure mode.
The cross-field invariant assertion then fires. On the fixed path,
`capacity_used_bytes` becomes the df-sum 285_229_056 and the invariant
holds. Using `Used: total_bytes` exactly would only fail the
exact-value assertion, not the invariant, weakening the durable guard.

Post-fix expected:
- `capacity_total_bytes = Some(536870912)` (2x equal -> min(sum/2, sum-max) = 536 MB)
- `capacity_used_bytes = 268435456 + 16777216 + 16384 = 285_229_056`
  (sum of non-GlobalReserve logical `used`).

**Assertions:**
```rust
// Cross-field unit invariant -- durable guard for this class of bug.
assert!(
    pool.capacity_used_bytes <= pool.capacity_total_bytes.unwrap(),
    "used ({}) must not exceed total ({}) -- unit mismatch?",
    pool.capacity_used_bytes,
    pool.capacity_total_bytes.unwrap(),
);
// Exact value -- pins the semantic: Data+Metadata+System logical
// used, GlobalReserve excluded, df-derived.
assert_eq!(pool.capacity_used_bytes, 285_229_056);
```

### New status-layer regression test

**Location:** `cli/src/status.rs` tests module, adjacent to existing
`get_capacity` tests (~line 1709 / 2985).

**Test:** `get_capacity_raid1_used_is_logical`. Same mock shape as the
probe test, but exercises `get_capacity` directly (passing the parsed
`&BtrfsDfOutput` per the prescribed wiring) and asserts:

```rust
assert!(report.used_bytes <= report.total_bytes.unwrap());
assert_eq!(report.used_bytes, 285_229_056);
```

Rationale: the plan changes `CapacityReport.used_bytes` semantics; a
dedicated status-layer test prevents future drift where TUI and status
diverge if the helper stops being shared.

### Existing assertions to update

1. `cli/src/tui/probe.rs:817`
   Current: `assert_eq!(pool.capacity_used_bytes, 33914880);`
   This is a 2-disk RAID1 with Data,RAID1 + Metadata,DUP + System,DUP.
   Post-fix it becomes the logical sum of the existing df mock's
   `bg_used` entries (look them up in the test's df mock -- likely
   `16777216 + 65536 + 16384 = 16859136`). Update.

2. `cli/src/status.rs` -- audit the ~25 `CapacityReport { ... }`
   assertions (lines 1771, 1850, 1942, 1971, 2046, 2111, 2185, 2238,
   2290, 2335, 2387, 2449, 2504, 2880, 2916, 2944, 3433, 3494, 3536,
   3570). For each, look at the associated df mock's `used` values:
   post-fix `used_bytes` is their non-GlobalReserve sum. Single-disk
   pools (Data ratio 1.00, ratio-identical profiles) may be
   numerically unchanged; RAID1 cases with df entries will be.

   Heuristic for quick triage: literals with `used_bytes: 536870912`
   and `total_bytes: Some(1073741824)` (the 2x tell) are almost
   certainly RAID1 cases that need updating to the df-sum value.

### Optional view-layer snapshot

Add one snapshot test in `cli/src/tui/view/mod.rs` tests (use
`sample_pool()` around line 1254 as the base, override both capacity
fields) to lock in the Usage line formatting at a high-but-valid
usage (e.g. 85%). Guards the percent/formatting path.

## Verification

1. Apply test changes first. Confirm the new probe test and updated
   `probe.rs:817` assertion fail on master for the expected reason
   (raw vs logical off by the right magnitude).
2. Add the helper on `BtrfsDfOutput`.
3. Thread `df` into `get_capacity`; apply both call-site fixes.
4. `just test-rust` -- all green. Status.rs fixture updates may need
   iteration.
5. `just test-vm` -- smoke-check that nothing downstream depends on
   raw `used_bytes`.
6. Manual smoke: run `braid status` and `braid tui` against a VM pool,
   confirm Used <= Total and Usage <= 100%.

## Out of scope

- Renaming fields (`capacity_used_bytes` -> `logical_used_bytes`). Fix
  is about matching the field's documented meaning, not the name.
- Status output format changes.
- JSON schema changes -- same field name and type, corrected value unit.

## Critical files

- `cli/src/parse/types.rs` -- add `BtrfsDfOutput::logical_used_bytes`.
- `cli/src/tui/probe.rs` -- line 271 call site; new regression test.
- `cli/src/status.rs` -- `get_capacity` signature and line 628 call
  site; new regression test; fixture audit.
- `cli/src/parse/btrfs_filesystem_df.rs` -- reference only (already
  parses `bg_used` / `bg_total` correctly; no changes).
