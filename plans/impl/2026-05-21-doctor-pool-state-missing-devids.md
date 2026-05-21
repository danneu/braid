# Plan: consolidate doctor onto `pool_state.missing_devids`

## Context

`braid doctor` currently loads "which devids are missing?" from two
different btrfs subcommands in the same run, and from a third source
that is already loaded by the time the second check fires:

- `check_pool_missing_devices` (`cli/src/doctor.rs:686`) and the
  degraded-routing branch of `check_profile_mismatch`
  (`cli/src/doctor.rs:654`) both call `preflight::probe_missing_devids`,
  which issues `BtrfsDeviceUsageRaw` and filters devices with
  `device_size == 0`.
- `check_foreign_luks_uuid` (`cli/src/doctor.rs:732`) calls
  `ensure_pool_state` -> `probe::probe_pool`, which issues
  `BtrfsFilesystemShow` and populates `pool_state.missing_devids` from
  `MISSING` sentinels (`cli/src/probe.rs:494`,
  `cli/src/parse/btrfs_filesystem_show.rs:114-125`).

On a healthy pool that gets `1x BtrfsDeviceUsageRaw + 1x
BtrfsFilesystemShow`. On a degraded pool with both data and metadata
profile mismatches it grows to `3x BtrfsDeviceUsageRaw + 1x
BtrfsFilesystemShow`, all answering the same question. The two
subcommands can also disagree during a balance or hot-unplug, so the
`pool_missing_devices` row and the profile-mismatch suggestion can name
different missing devids in the same report.

`PoolState::missing_devids` is documented as "Authoritative to btrfs"
(`cli/src/types.rs:424`) and is what `replace --missing-id` validates
against (`cli/src/replace.rs:1682,1694`). Aligning doctor onto it
collapses three sources of truth into one, removes the divergence
window, and removes a parser helper whose only callers live in doctor.

The earlier plan that introduced the degraded-routing branch
(`plans/impl/2026-05-18-doctor-degraded-profile-suggestion.md:44-53`)
consciously chose the duplicate probe over caching, but did not
consider that `pool_state.missing_devids` is already loaded by the
adjacent `check_foreign_luks_uuid`. Once that's noticed, the right
move is the structural replacement, not a cache.

## Goal

Replace doctor's only uses of `preflight::probe_missing_devids` with
reads from `pool_state.missing_devids`, drop the now-unused helper and
its tests, and migrate the affected doctor unit tests onto the
`BtrfsFilesystemShow` + cryptsetup mock chain that
`check_foreign_luks_uuid` already exercises.

## Code changes

### `cli/src/doctor.rs`

- **`check_pool_missing_devices`** (lines 675-708): replace the
  `match preflight::probe_missing_devids(ctx.runner, &mount_point)`
  block at line 686 with `ensure_pool_state(ctx)` plus a match on
  `ctx.pool_state` that mirrors the pattern used at lines 732-742 in
  `check_foreign_luks_uuid`.
  - `Ok(pool)` with `pool.missing_devids.is_empty()` -> existing "no
    missing devices" Ok result.
  - `Ok(pool)` with a non-empty `pool.missing_devids` -> existing
    "pool has N missing device(s) (devid(s): ...); replace with: braid
    replace ... --missing-id <devid>" Warn result, sourcing devids
    from `pool.missing_devids`.
  - `Err(e)` -> existing "could not probe for missing devices: {e}"
    Warn result (preserve message wording so operator-facing strings
    don't drift).
- **`check_profile_mismatch`** degraded-routing (lines 644-672):
  replace the `match preflight::probe_missing_devids(...)` at line 654
  with `ensure_pool_state(ctx)` plus a match that distinguishes the
  degraded case from the fallback:
  - `Ok(pool)` with a non-empty `pool.missing_devids` -> existing
    "pool is degraded -- replace missing device(s) first, then
    rebalance" suggestion.
  - any other case (`Ok(empty)` or `Err(_)`) -> existing soft-balance
    suggestion. Preserve the conservative-fallback rationale captured
    in `plans/impl/2026-05-18-doctor-degraded-profile-suggestion.md:92-96`:
    on an `Err`, the operator sees the matching "could not probe ..."
    line emitted by `check_pool_missing_devices` and has the context
    to interpret the balance suggestion correctly. (Same `pool_state`
    cache feeds both checks, so the two warnings stay consistent.)
- **Remove the `crate::preflight` import** at line 28 if no other
  doctor code uses it. `grep -n 'preflight::' cli/src/doctor.rs`
  confirms the only uses are at lines 654 and 686, both being replaced.

### `cli/src/preflight.rs`

- Delete `pub fn probe_missing_devids` (lines 298-319) and its doc
  comment.
- Delete the two unit tests `probe_missing_devids_returns_missing`
  (line 861) and `probe_missing_devids_returns_empty_when_healthy`
  (line 899).
- Drop any imports made dead by the deletion (e.g. `CmdRequest` if it
  was only used by those tests -- check with `cargo check`).

### `docs/decisions/012-intent-cli.md`

- Update line 60: today it reads "`--missing-id` ... Validated
  against actual missing devids via `probe_missing_devids()`." Both
  the named function and the framing are wrong after this pivot --
  rewrite as "Validated against `PoolState::missing_devids` (live
  btrfs state via `probe::probe_pool`)." This matches the
  authoritative source `replace.rs:1682,1694` already uses and keeps
  Architecture Authority pointing at a real API. (The doc was
  already stale -- `replace` checks `pool.missing_devids`, not
  `probe_missing_devids()` -- but this pivot removes the named
  function entirely, so the fix is non-optional.)

## Test changes

### Shared mock helper

The doctor test module already has `foreign_luks_uuid_runner`
(`cli/src/doctor.rs:1492-1527`), which composes
`BtrfsFilesystemShow` + per-mapper `CryptsetupStatus` + per-mapper
`CryptsetupLuksUuid`. After this pivot, three more tests need that
same chain plus the ability to inject `MISSING` sentinels into the
show output.

Refactor in this order to minimise churn:

1. **Extend `doctor_btrfs_show`** at `cli/src/doctor.rs:1387-1404` to
   accept `missing_devids: &[u64]` in addition to the existing
   `devices: &[(&str, u64)]`. Emit one
   `\tdevid {devid:>4} size 0 used 0 path MISSING\n` line per
   missing entry (format from
   `cli/src/parse/btrfs_filesystem_show.rs:107-108`). The
   `Total devices` count must include the missing rows.
2. **Hoist `foreign_luks_uuid_runner`** out of its current
   single-test closure into a module-level helper named something
   like `pool_state_runner(pool_devices, missing_devids)` so it can
   be reused by `check_pool_missing_devices` and
   `check_profile_mismatch` tests. Keep the existing
   `foreign_luks_uuid_runner` call sites compiling by either
   renaming the helper and updating its callers, or keeping a thin
   wrapper.

### Inject `DoctorMockFs` everywhere `pool_state` must resolve to `Ok`

`ensure_pool_state` calls `probe::probe_pool`, which first reads
mountinfo through the doctor context's injected filesystem
(`cli/src/probe.rs:389` -> `mount_check::fstype_at_mount_via_fs`)
before issuing `BtrfsFilesystemShow` or any cryptsetup probe. Today
the affected tests pass `&RealFilesystem` to `run_doctor` or use
`parsed_doctor_ctx` (which wraps `for_test_parsed`, hard-wired to
`REAL_FILESYSTEM_FOR_TESTS` per `cli/src/doctor.rs:1318-1319`). After
the pivot those tests need `probe_pool` to see the mount, otherwise
they will accidentally exercise the `mounted: false` short-circuit at
`cli/src/probe.rs:390-400` and assert against an empty `pool_state`
instead of the intended `missing_devids` path.

The required scaffolding already exists:
`DoctorMockFs::mounted_btrfs_only()` at
`cli/src/test_fixtures/doctor.rs:110-116` returns a btrfs mount line
for `/mnt/storage`, and `DoctorContext::for_test_parsed_with_fs` at
`cli/src/doctor.rs:1322-1342` accepts an injected filesystem. The
direct-context test
(`pool_missing_devices_does_not_require_filesystem_df`) must call
`for_test_parsed_with_fs` directly rather than `parsed_doctor_ctx`,
and every `run_doctor(..., &RealFilesystem, ...)` call site listed
below must swap to a `DoctorMockFs::mounted_btrfs_only()` instance.

### Affected doctor unit tests

Swap each of these from a `BtrfsDeviceUsageRaw` mock to the new
show-based mock chain, and supply `DoctorMockFs::mounted_btrfs_only()`
in place of `&RealFilesystem`. Line numbers as of master (verify
before editing):

- `pool_missing_devices_ok_when_healthy` (~3566): show with one
  present device, no missing.
- `pool_missing_devices_does_not_require_filesystem_df` (~3679):
  show with one present device. Replace
  `parsed_doctor_ctx(&runner, &paths)` with a two-statement form
  that binds the mock filesystem to a local so it outlives the
  context (`for_test_parsed_with_fs` stores the reference at
  `cli/src/doctor.rs:1336`; an inline `&DoctorMockFs::mounted_btrfs_only()`
  is a temporary that would be dropped at the end of the `let`
  statement, leaving `ctx` with a dangling borrow that the compiler
  will reject):

  ```rust
  let fs = DoctorMockFs::mounted_btrfs_only();
  let mut ctx = DoctorContext::for_test_parsed_with_fs(
      &runner, &fs, &paths, valid_config_json(),
  );
  ```

  Change the assertion that today expects
  `CmdRequest::BtrfsDeviceUsageRaw { .. }` to expect
  `CmdRequest::BtrfsFilesystemShow { .. }` instead. The "must not
  request df" assertion stands -- it's the invariant this test
  exists to pin.
- `pool_missing_devices_warns_with_replace_recommendation` (~3630):
  show with one present device and one missing devid; assert the
  warning message still contains the missing devid and the
  `braid replace ... --missing-id` invocation.
- `pool_missing_devices_skip_when_not_mounted` (~3674): if the
  check short-circuits on `ensure_mountpoint_is_mounted`, the
  show mock isn't reached -- verify and leave untouched. If it
  reaches `ensure_pool_state`, pass `DoctorMockFs::empty()`
  (`cli/src/test_fixtures/doctor.rs:120-124`) so `probe_pool`
  returns `mounted: false`.
- `data_profile_mismatch_recommends_replace_when_degraded` (~3113)
  and `metadata_profile_mismatch_recommends_replace_when_degraded`
  (~3457): swap `device_usage_with_missing` for the show-with-missing
  chain (one present device, one missing devid).
- `data_profile_mismatch_recommends_balance_when_healthy` (~3157)
  and its metadata twin if it exists: swap `device_usage_healthy`
  for the show chain (present devices only, no missing).
- `human_format_contains_missing_devs_label` (~3809): swap to the
  show chain accordingly.

### Test comment + scaffold drift

Three places still describe the pre-pivot probe chain and must be
rewritten:

1. **`data_profile_mismatch_recommends_balance_when_healthy`** (the
   `// Why it exists` comment around `cli/src/doctor.rs:3149-3155`,
   line numbers shift as edits land): today reads "the existing
   `data_profile_mixed_warns` would not catch it because that test
   exercises the Err fallback by leaving `BtrfsDeviceUsageRaw`
   unmocked." After the pivot, the Err fallback comes from a missing
   `BtrfsFilesystemShow` mock -- rewrite to reference that command.
2. **`pool_missing_devices_does_not_require_filesystem_df`** (the
   block comment around `cli/src/doctor.rs:3671-3677`): today reads
   "missing-device detection only needs the mountpoint state and
   `btrfs device usage`; tying it to df makes an unrelated parser or
   command failure hide the more specific device probe." Rewrite the
   `Why it exists` line so it names the new probe chain
   (`BtrfsFilesystemShow` + per-mapper cryptsetup status / luksUUID
   via `probe::probe_pool`) while preserving the headline invariant
   the test pins: `check_pool_missing_devices` must not query
   `btrfs filesystem df`.
3. **`PoolMissingDevicesRunner`** in
   `cli/src/test_fixtures/doctor.rs:381-414`: rewrite both the doc
   comment ("Mountpoint Ok, device usage healthy,
   `BtrfsFilesystemDfJson` panics. Pins the invariant that
   `check_pool_missing_devices` is decoupled from df.") and the
   `CommandRunner::run` arms (line 393-404) to model the new probe
   chain. After the pivot the runner must serve `MountpointCheck`,
   `BtrfsFilesystemShow` (one present device), `CryptsetupStatus`
   per mapper, `CryptsetupLuksUuid` per device, and continue to
   `panic!("pool_missing_devices must not query filesystem df")` on
   `BtrfsFilesystemDfJson`. Reuse the existing
   `doctor_btrfs_show` / `doctor_cryptsetup_status_active` /
   `doctor_cryptsetup_uuid_ok` helpers from `cli/src/doctor.rs`
   (or whichever module they end up in after the helper hoist
   above) for the response bodies.

## Verification

1. `just test-rust` -- all doctor + preflight unit tests pass after
   the mock-chain swaps. The `preflight::probe_missing_devids` tests
   are gone; no other consumer relies on them.
2. `cargo clippy --all-targets` -- catches any stranded imports
   (`use crate::preflight;` line in `cli/src/doctor.rs:28`, etc.).
3. `just test-vm braid-doctor doctor-metadata-mixed braid-doctor-foreign-luks-uuid`
   -- VM tests that exercise `braid doctor` end-to-end against a real
   mounted pool, with and without missing devices. Selectors must
   match the attribute names registered in `flake.nix` exactly
   (`flake.nix:265,280,290`); `just test-vm` passes them verbatim to
   nix per `docs/testing.md:30-42`.
4. `just test-repro repro-degraded-soft-balance` -- repro VM test
   (registered at `flake.nix:597`) that pins the degraded -> replace
   -> soft-balance recovery sequence. The `repro-` prefix is
   required by `docs/testing.md:30-40`.
5. Manual sanity check (optional, against a live test VM): on a
   degraded pool, run `braid doctor` and confirm the `missing devs`
   row's devid list matches the one `braid replace --missing-id`
   accepts. Before this pivot they could diverge; after, they read
   from the same `pool_state.missing_devids` field.

## Out of scope

- No change to `BtrfsDeviceUsageRaw` callers outside doctor
  (`cli/src/remove.rs`, `cli/src/remove_missing.rs`,
  `cli/src/status.rs`). Those use it for free-space and balance
  calculations, not missing-devid detection.
- No change to the `replace` command's own `pool.missing_devids`
  validation -- it already reads from the same source the doctor
  pivot adopts.
- No change to monitor / ack alert pipelines; those already use
  `PoolState::alert_missing_devids` and `null_underlying`
  (`cli/src/types.rs:443-458`).
