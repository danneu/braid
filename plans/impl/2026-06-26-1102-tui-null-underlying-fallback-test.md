# Plan: TUI-level test for the `null_underlying` persisted-devid fallback

## Context

A Low-severity testing finding flagged the `tui_disks()` fixture in
`cli/src/tui/probe.rs` for maintaining two parallel identity maps (`luks_uuid`
and `devid`) whose cross-consistency is only asserted indirectly, and claimed a
transposed `devid` map could let the persisted-devid fallback test "silently
test the wrong axis."

Investigation (verify-issue) showed the finding's headline is **wrong**: the
`devid` map is already pinned by `device_errors_for_missing_devid_use_persisted_prior_binding`
-- transposing it makes that test fail loudly, not silently. The finding's
proposed comment was also inaccurate ("devid map must match the btrfs show
mock's devid->mapper assignment" -- for a missing/null device there is no live
mapper row to match).

What the investigation *did* surface is a real, narrow coverage gap. The probe
joins btrfs rows to member names via `devid_to_name` (`cli/src/tui/probe.rs:226-240`),
whose fallback (for non-live members) has **two** sibling branches:

- `missing_devids` (`:235-239`) -- btrfs reports `path MISSING`. **Tested** at
  the TUI level by `device_errors_for_missing_devid_use_persisted_prior_binding`.
- `null_underlying` (`:230-234`) -- mapper open but `cryptsetup status` reports
  `device: (null)` (backing drive hot-unplugged). **No TUI-level test.**

The `null_underlying` *domain* path is covered (`cli/src/probe.rs` ::
`probe_pool_device_null_underlying`, `probe_pool_alerts_with_null_underlying`),
but the TUI consumer has zero coverage despite running in `probe_pool_for_tui`.
It has **two** consumer sites, and the test must pin **both**:

- the name join at `:230-234` -> feeds `device_errors` and `devid_names`.
- the classification at `:181-185` (`null_underlying` -> `(Unlocked, None)` in
  `mounted_classification`) -> feeds `build_disk_luks_states` (`:106-150`, the
  returned `states` map) and, derivatively, `disk_underlying` (`:478`).

**Outcome:** add the missing TUI-level test (the structural twin of the
missing-devid test) so both `null_underlying` consumer sites are pinned with
*discriminating* assertions, and bundle a corrected comment documenting why
`tui_disks().devid` exists. As a side effect, the `devid` map becomes
load-bearing for two tests, dissolving the finding's "asserted only indirectly"
concern at the root.

## Change 1 (primary): new test `device_errors_for_null_underlying_use_persisted_prior_binding`

Add to the `tests` module in `cli/src/tui/probe.rs`, next to its sibling
`device_errors_for_missing_devid_use_persisted_prior_binding` (~`:1617-1718`).
Mirror that test's structure; the material differences are (a) member 2 is
**present-but-null-backed** instead of `MISSING`, and (b) the test supplies a
`by_id` map so the classification site is reachable and assertable.

### Preamble (repo convention)

Use a contiguous block of `//` line comments (`// Intent:`, `// Why it exists:`,
`// Scenario:`) directly above `#[test]`, per
[`docs/dev/testing.md#preamble-literal-line-comment-form`](../../docs/dev/testing.md).
**Template:** `smartctl_health_for_present_member_uses_live_underlying`
(`cli/src/tui/probe.rs:1360-1375`), which is compliant. Do **not** copy the
sibling missing-devid test's `/* ... */` block -- it is a stale outlier that
predates the convention.

Content: Intent -- a `null_underlying` member (mapper open, backing
hot-unplugged) has its btrfs stats row attached to the member by persisted
devid, is exposed in `devid_names`, and is classified `Unlocked` with no live
backing path. Why it exists -- the `null_underlying` TUI fallback is the sibling
of the missing-devid fallback but had zero TUI-level coverage; only the domain
`probe_pool` was tested. Scenario -- btrfs reports disk1 live and
`braid-ironwolf` open but `cryptsetup status` returns `device: (null)` (backing
yanked); device stats reports devid 2 with a read error.

### Fixture and MockRunner setup

Use `tui_disks_with_by_id(disk_by_id)` (not bare `tui_disks()`), with the proven
two-member `by_id` shape (see `cli/src/tui/probe.rs:1876-1885`):

```rust
let disk_by_id = HashMap::from([
    ("toshiba".to_owned(),  "/dev/disk/by-id/braid-toshiba".to_owned()),
    ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
]);
```

Keep `StubFs::empty()` and `mock_virtio_backing_path_resolver()`. Inline the
`cryptsetup status` strings as the existing TUI tests do (there is no
`null`-backing helper in this `tests` module). Mocks:

- `BtrfsFilesystemShow` -- both members **present** with real mapper paths (NOT
  `MISSING`): `devid 1 ... path /dev/mapper/braid-toshiba`,
  `devid 2 ... path /dev/mapper/braid-ironwolf`.
- `CryptsetupStatus { braid-toshiba }` -> `device:  /dev/vda` (live).
- `CryptsetupLuksUuid { /dev/vda }` -> `11111111-...-111111111111` (resolves
  devid 1 -> toshiba via the live UUID join).
- `CryptsetupStatus { braid-ironwolf }` -> `device:  (null)` -- **the trigger**;
  routes devid 2 into `null_underlying` (`cli/src/probe.rs`, `BackingDevice::Null`).
- **No** `CryptsetupLuksUuid` for ironwolf (the `null` branch `continue`s before
  the luksUUID query; never called).
- `BtrfsFilesystemDfJson` -- copy verbatim from the sibling.
- `BtrfsDeviceUsageRaw` -- **two** device rows, `ID: 1` (`/dev/dm-0`) **and**
  `ID: 2` (`/dev/dm-1`). This differs from the sibling (which omits ID 2 because
  its devid 2 is `MISSING`). Here devid 2 is *present* (its dm node still exists
  while only the LUKS backing is gone), so listing ID 2 is the internally
  consistent mock. It is also load-bearing for harness hygiene: `disk_usage`
  gains an `ironwolf` key (via the `:244` devid join), so the unpooled-disk loop
  (`:370-373`) **skips** ironwolf instead of misclassifying a pool member as an
  Absent unpooled disk.
- `BtrfsBalanceStatus` -- `No balance found` (copy from sibling).
- `BtrfsDeviceStatsJson` -- one row: `devid 2`, `read_io_errs: 9`, with a
  path that does not match the mapper (e.g. `"device": "/dev/dm-1"`) so the join
  is forced through devid, not path.

Scrub / SMART / lsblk / `CryptsetupLuksDump` (the `build_disk_luks_states`
metadata probe) are all `.ok()`/`.unwrap_or`-tolerant and need no mocks; adding
`by_id` makes `build_disk_luks_states` and the SMART loop run, but both degrade
gracefully on missing mocks.

### Capture and assertions

Capture **both** tuple elements (the sibling discards `.0` via `expect_pool`):

```rust
let (states, pool_state) = probe_pool_for_tui(...).unwrap();
let pool = pool_state.expect("pool should be Some");
```

**A. Classification site `:181-185` (the discriminating pin, per review):**

- `states["ironwolf"].lock == DiskLockState::Unlocked` -- **this is the
  branch-pinning assertion.** Deleting `:181-185` drops ironwolf from
  `mounted_classification`, so `build_disk_luks_states` falls back to
  `fallback_disk_luks_lock`, which runs `cryptsetup status braid-ironwolf`, sees
  `device: (null)`, and returns `(Unknown, None)` (`cli/src/tui/probe.rs:47-49`).
  The lock therefore flips `Unlocked -> Unknown`, failing this assertion.
- `states["ironwolf"].underlying_present == None` -- characterizes the state
  (the reviewer asked for both); note it does **not** discriminate the mutation
  (it is `None` either way), so `.lock` is the one that bites.
- Contrast: `states["toshiba"].lock == DiskLockState::Unlocked` and
  `states["toshiba"].underlying_present == Some("/dev/vda".to_owned())` (live
  member keeps its backing path).

**B. Name-join site `:230-234` (the original gap):**

- `pool.device_errors.get("ironwolf").read == 9`, via
  `.expect("null-underlying devid 2 must resolve to ironwolf by persisted binding")`.
- `pool.devid_names.get(&Devid::new(2)) == Some("ironwolf")`.
  (Deleting `:230-234` drops devid 2 from `devid_to_name`, so both fail.)

**C. Secondary output checks (kept, explicitly non-pinning):**

- `pool.disk_underlying.get("toshiba").map(String::as_str) == Some("/dev/vda")`.
- `!pool.disk_underlying.contains_key("ironwolf")`. These document the
  `disk_underlying` surface but, as the review noted, do **not** pin `:181-185`
  on their own (deleting that branch leaves ironwolf absent from
  `disk_underlying` either way) -- assertion A is what pins it.

## Change 2 (bundle): corrected comment on `tui_disks().devid`

On the `devid` field in `tui_disks()` (`cli/src/tui/probe.rs:996-999`), add a
short ASCII comment stating its role precisely:

> Persisted prior devid->name binding. Consumed only by the
> `null_underlying`/`missing_devids` fallback in `probe_pool_for_tui` (live
> members resolve by LUKS UUID, not this map); pinned by
> `device_errors_for_{missing_devid,null_underlying}_use_persisted_prior_binding`.

This replaces the finding's inaccurate proposed wording and documents why the
map exists so future readers (and reviewers) don't re-raise the same concern.

## Out of scope / rejected

- **Triple-based `DiskIdentity` test builder** (`&[(name, uuid, devid)]`):
  considered to dissolve the parallel-map shape across `tui_disks` /
  `transport_test_disks`, but over-engineering for two/three two-entry fixtures.
- **Domain-level `null_underlying` test**: already covered in `cli/src/probe.rs`
  (`probe_pool_device_null_underlying`, `probe_pool_alerts_with_null_underlying`).
- **Rewriting the sibling's stale `/* ... */` preamble** to `//` form: a real
  inconsistency, but a separate cleanup; this plan only ensures the *new* test
  is compliant.
- **The finding's original comment wording**: inaccurate; superseded by Change 2.

## Verification

1. Run the new test:
   `cargo test --manifest-path cli/Cargo.toml device_errors_for_null_underlying_use_persisted_prior_binding`
   -- expect **pass** against current code (production path already exists and is
   correct; this is a regression/characterization test).
2. **Mutation checks (prove each assertion bites), one at a time, then revert:**
   - Delete the `null_underlying` clause at `:230-234` -> assertions in group B
     fail (ironwolf no longer resolves in `device_errors`/`devid_names`).
   - Delete the `null_underlying` classification at `:181-185` -> assertion A
     (`states["ironwolf"].lock == Unlocked`) fails (flips to `Unknown`). Confirm
     the group-C `disk_underlying` checks do **not** catch this mutation -- that
     is the gap this test closes.
3. Run the whole probe test module for regressions:
   `cargo test --manifest-path cli/Cargo.toml tui::probe`.
4. `just test-rust` (full Rust suite) before committing.

## Follow Up

- Convert the sibling test's preamble in
  `cli/src/tui/probe.rs#device_errors_for_missing_devid_use_persisted_prior_binding`
  from the stale `/* ... */` block to the `//` line-comment form mandated by
  `docs/dev/testing.md#preamble-literal-line-comment-form`. The new
  `device_errors_for_null_underlying_use_persisted_prior_binding` sits directly
  above it in the compliant form, making the inconsistency adjacent and obvious.
