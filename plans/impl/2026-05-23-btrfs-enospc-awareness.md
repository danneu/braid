# Pivot: ENOSPC awareness in braid (audit findings #2 + #3)

## Context

`btrfs-findings/b4-balance-enospc.md` [since removed from repo] raised three HIGH findings about
braid's ENOSPC handling. Verification against the current code and the
cited sources (`btrfs-links/forza_enospc.html` [since removed from repo],
`btrfs-links/lwn_metadata_enospc.html`, `reference/linux/fs/btrfs/`)
landed three different conclusions:

- **Finding #1** (post-degraded metadata rebalance) -- **already
  implemented**. `cli/src/cmd.rs:658-665` constructs
  `btrfs balance start --enqueue -dconvert=raid1,soft -mconvert=raid1,soft`
  and `cli/src/cmd.rs:2442-2465` pins both flags in a test. The audit
  author appears to have read `pool_balance_raid1_soft` in isolation
  without following the `CmdRequest` -> command-string mapping. **No
  code work needed.**

- **Finding #2** (`balance_error()` ENOSPC message is minimal) --
  partially valid. The current message at `cli/src/pool.rs:252-267`
  already includes a `-dusage=0` retry hint (commit `48249c2`), but the
  hint does not point the operator at the *diagnostic* step
  (`btrfs filesystem usage`) and does not acknowledge that the same
  ENOSPC can come from data-chunk fragmentation or metadata pressure --
  which have different remediations. Forza's article makes diagnosis
  the first step, and explicitly warns against rebalancing metadata
  chunks. **Improve the message.**

- **Finding #3** (no advisory before ENOSPC) -- the real gap. doctor
  has no capacity-pressure check. But the audit's proposed signal
  (`metadata.used / metadata.total > 70%`) is misframed: the kernel's
  80% threshold (`reference/linux/fs/btrfs/block-group.c:3915-3939`,
  `should_alloc_chunk`) triggers *automatic chunk allocation from
  unallocated space* -- normal expansion, not the ENOSPC trap. Forza
  confirms the actual trap is "metadata near full **AND** no
  unallocated space to grow into". A bare `>70%` warning would fire
  continuously on healthy pools every time the kernel allocates a new
  metadata chunk (used immediately re-approaches 80% before the next
  allocation). Forza also explicitly says **"Only balance DATA chunks,
  never METADATA chunks"**, so the audit's implied remediation
  (metadata rebalance) is wrong. **Add the check, but pivot the
  signal.**

This plan implements #2 and #3 as one bundle because they share the
same forza-derived remediation story (diagnose with
`btrfs filesystem usage`; if data is tight, compact data chunks with
`-dusage=NN`; if metadata is tight, delete files -- never rebalance
metadata).

## Scope

In scope:

- **#2**: improve `balance_error()` ENOSPC hint to lead with the
  diagnostic command and distinguish data vs metadata remediation.
- **#3**: new doctor check `metadata_enospc_pressure` that warns only
  when both metadata utilization is high **and** per-device unallocated
  headroom is exhausted.

Out of scope:

- Mirroring the advisory into `status` (ADR 008 keeps pool-invariant
  diagnostics in `doctor`; `status` already reports raw allocation per
  bg_type for operators who want the numbers).
- Any auto-balance behavior. Doctor warns; the operator decides.
- Updating `btrfs-findings/b4-balance-enospc.md`. That audit document
  is the user's notes -- they can amend it separately if they want.

## Work item A: improve `balance_error()` (finding #2)

### Current

`cli/src/pool.rs:252-267`

```rust
fn balance_error(label: &str, mount_point: &MountPoint, result: &RawCommandOutput) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("no space left") {
        PoolError::Failed(format!(
            "{label} failed (exit {}): {}\nhint: run `btrfs balance start -dusage=0 {mount_point}` to free empty block groups, then retry",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else { ... }
}
```

### Proposed

Replace the single-line `hint:` with a two-step recipe that mirrors
forza_enospc:

```
{label} failed (exit {n}): {stderr}
hint: ENOSPC during balance -- this is usually data-chunk
fragmentation, occasionally metadata pressure.
  1) diagnose: `btrfs filesystem usage {mount_point}` -- look at
     Device unallocated and the Data/Metadata Used vs Total lines.
  2) if data is tight: `btrfs balance start -dusage=0 {mount_point}`
     (then -dusage=20, -dusage=50 if needed) to compact data chunks.
  3) if metadata is tight: delete files to free space; do not rebalance
     metadata chunks.
```

Style: ASCII only (`--`), no em-dashes. Wrap the hint as a multi-line
string in source; the renderer at the operator's terminal handles
display.

### Files

- `cli/src/pool.rs:252-267` -- rewrite the `balance_error` body.
- `cli/src/pool.rs:1198-1242` -- update existing tests
  (`balance_error_detects_enospc`, `balance_error_no_hint_for_other_failures`).
  Assertions to add:
    - hint contains "btrfs filesystem usage {mount_point}";
    - hint contains "btrfs balance start -dusage=";
    - hint contains "delete files" or "delete";
    - hint **does not** contain "mconvert" or "musage" (negative
      assertion: we never tell the operator to balance metadata);
  Keep the existing "no hint for non-ENOSPC" test.

No new fixtures, no new dependencies. Pure stderr-string handling, all
unit-test covered.

## Work item B: new doctor check `metadata_enospc_pressure` (finding #3)

### Placement

`cli/src/doctor.rs`, alongside the profile-mismatch family. Register in
the `checks` vec at `cli/src/doctor.rs:1219-1228` directly after
`check_metadata_profile_mismatch`. Add the label-map entry in
`format_doctor_human_with` at `:1256-1273`.

### Structure

Mirrors `check_metadata_profile_mismatch` (`:787-796`) which delegates
to the shared `check_profile_mismatch` (`:620-691`). The new check is
similar in shape but has its own body because it joins two data
sources (df + device usage):

```rust
fn check_metadata_enospc_pressure<R: CommandRunner>(
    ctx: &mut DoctorContext<'_, R>,
) -> CheckResult {
    const NAME: &str = "metadata_enospc_pressure";
    if ctx.config.is_none() {
        return CheckResult::skip(NAME, "skipped (config not available)");
    }
    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip(NAME, "skipped (pool not mounted)");
    }

    ensure_df_snapshot(ctx);  // already cached by profile-mismatch checks

    let df = match ctx.df_snapshot.as_ref().unwrap() {
        DfSnapshot::Ok(df) => df,
        DfSnapshot::NotMounted => return CheckResult::skip(NAME, "skipped (pool not mounted)"),
        DfSnapshot::Error(e) => return CheckResult::warn(NAME,
            format!("could not inspect metadata pressure: {e}")),
    };

    let mount_point = ctx.config.as_ref().unwrap().mount_point();
    let usage_raw = match ctx.runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => return CheckResult::warn(NAME,
            format!("could not inspect device unallocated: {e}")),
    };
    let usage = match parse_btrfs_device_usage(&usage_raw) {
        Ok(u) => u,
        Err(e) => return CheckResult::warn(NAME,
            format!("could not parse device unallocated: {e}")),
    };
    // parse_btrfs_device_usage returns Ok with an empty vec on empty
    // stdout (see cli/src/parse/btrfs_device_usage.rs:115). Treat that
    // as inspection failure rather than panicking on the reduction below.
    if usage.devices.is_empty() {
        return CheckResult::warn(NAME,
            "could not inspect device unallocated: no devices reported".into());
    }

    let (meta_used, meta_total) = df.entries.iter()
        .filter(|e| e.bg_type == BtrfsBgType::Metadata)
        .fold((0u64, 0u64), |(u, t), e| (u + e.bg_used, t + e.bg_total));
    if meta_total == 0 {
        return CheckResult::ok(NAME, "no metadata block groups yet");
    }
    let meta_ratio = meta_used as f64 / meta_total as f64;

    // RAID1 metadata chunks need exactly 2 devices, not every device.
    // See reference/linux/fs/btrfs/volumes.c:67-79 -- BTRFS_RAID_RAID1
    // has devs_min=2, devs_max=2, ncopies=2; the allocator picks the
    // two devices with the most unallocated space. So a single tight
    // device on a 3+ device pool is fine as long as two other members
    // can satisfy the next chunk. We count members with headroom and
    // warn only when fewer than 2 can satisfy the allocation.
    let n_devices = usage.devices.len();
    let with_headroom = usage.devices.iter()
        .filter(|d| d.unallocated >= METADATA_CHUNK_HEADROOM)
        .count();

    if meta_ratio > METADATA_PRESSURE_RATIO && with_headroom < 2 {
        let pct = (meta_ratio * 100.0).round() as u64;
        return CheckResult::warn(NAME, format!(
            "metadata {pct}% used; only {with_headroom} of {n_devices} \
             device(s) have >= 1 GiB unallocated -- RAID1 needs 2 with \
             headroom for the next metadata chunk. Delete files to free \
             space, or compact data with `btrfs balance start -dusage=50 \
             {mount_point}` before metadata cannot grow.",
        ));
    }

    CheckResult::ok(NAME, "metadata pressure within bounds")
}
```

### Thresholds

At module top with the existing `SELFTEST_STALE_HOURS_THRESHOLD`
constant (around `doctor.rs:798`):

```rust
/// Metadata-utilization fraction above which the kernel forces a new
/// metadata chunk allocation. Kernel hard-codes 80% in
/// should_alloc_chunk() (fs/btrfs/block-group.c:3936). Warn at 75% --
/// it gives operator a window to act before the next allocation may fail.
const METADATA_PRESSURE_RATIO: f64 = 0.75;

/// Per-device unallocated headroom needed to participate in the next
/// metadata chunk allocation. btrfs metadata chunks are typically
/// 256 MiB - 1 GiB; 1 GiB is the conservative bound. RAID1 needs two
/// devices each holding this much unallocated to satisfy a chunk
/// (volumes.c:67-79: devs_min=2, ncopies=2).
const METADATA_CHUNK_HEADROOM: u64 = 1024 * 1024 * 1024;
```

### Reuse

- `parse_btrfs_df_json` (`cli/src/parse/btrfs_filesystem_df.rs`) --
  already emits `bg_used` / `bg_total` per bg_type.
- `parse_btrfs_device_usage` (`cli/src/parse/btrfs_device_usage.rs`) --
  already emits per-device `unallocated`.
- `ensure_df_snapshot` (`cli/src/doctor.rs`, used by
  `check_profile_mismatch`) -- df result is already cached within a
  single doctor run.
- `format_bytes` -- already used for human output in
  `check_profile_mismatch`.
- No new module in `preflight.rs`. The math is six lines; the data
  sources are doctor-specific (read-only, advisory). Inlining matches
  the `check_profile_mismatch` style.

### Output examples

```
[ok]   meta pressure   metadata pressure within bounds
[warn] meta pressure   metadata 78% used; only 1 of 2 device(s) have >= 1 GiB unallocated -- RAID1 needs 2 with headroom for the next metadata chunk. Delete files to free space, or compact data with `btrfs balance start -dusage=50 /mnt/storage` before metadata cannot grow.
[skip] meta pressure   skipped (pool not mounted)
```

### Files

- `cli/src/doctor.rs` -- new check function, constants, register in
  checks vec, label-map entry. JSON name
  `metadata_enospc_pressure`, human label `meta pressure` (14-char
  field width matches existing labels).
- `cli/src/test_fixtures/doctor.rs` -- new fixture constants for the
  high-metadata + low-unallocated scenario (mirroring
  `DF_RAID1_CLEAN`, `DF_MIXED_METADATA`).
- `docs/commands/doctor.md` -- add `metadata_enospc_pressure` row to
  the "What it checks" table.

## Verification

### Unit tests (`cli/src/doctor.rs`)

Mirror the `metadata_profile_*` test cluster (`doctor.rs:3397-3500`).
Each test uses the project preamble (Intent / Why / Scenario).

Required cases:

1. **Healthy pool -> Ok.** RAID1, metadata ~12%, devices ~80% unallocated.
2. **Metadata pressure alone -> Ok.** Metadata 78% used, each device
   has > 1 GiB unallocated. Pins the original 70%-recommendation false
   positive: we must NOT warn here.
3. **Unallocated pressure alone -> Ok.** Devices with < 1 GiB
   unallocated each, but metadata only 20% used.
4. **Both signals present, 2-device pool -> Warn.** Metadata 78% used
   AND both devices have < 1 GiB unallocated. Assertions:
    - message contains the numeric ratio,
    - message contains `RAID1 needs 2`,
    - message contains `btrfs balance start -dusage=`,
    - message contains "delete files",
    - message **does not** contain "mconvert" or "musage" (negative
      assertion: forza forbids metadata rebalance).
5. **3-device pool, one tight, two healthy -> Ok.** Metadata 78% used
   AND one device has 400 MiB unallocated, but the other two have
   multi-GiB each. Pins the allocator-aware semantics: RAID1 needs
   only 2 devices with chunk headroom, so a single tight device on a
   3+ device pool is not a warning. (See volumes.c:67-79.)
6. **3-device pool, two tight, one healthy -> Warn.** Metadata 78%
   used AND only one device has >= 1 GiB unallocated. Pins the
   "fewer than 2" boundary.
7. **Pool not mounted -> Skip.**
8. **df spawn/parse failure -> Warn "could not inspect".**
9. **device usage spawn/parse failure -> Warn "could not inspect".**
10. **device usage parses but returns empty device list -> Warn "no
    devices reported".** Pins the empty-vec guard so a malformed-but-
    zero-exit tool output cannot crash doctor (see parser at
    cli/src/parse/btrfs_device_usage.rs:115).

### Unit tests (`cli/src/pool.rs`)

Update `balance_error_detects_enospc` (`:1199-1222`) for the new
multi-step hint. Add a negative assertion that the message does NOT
contain "mconvert" or "musage" so the metadata-rebalance anti-pattern
never sneaks back in.

### End-to-end

- `just test-rust` -- runs the doctor and pool unit tests.
- Manual smoke: `cargo run -- doctor` against a real or VM pool to
  confirm the new row renders with correct column alignment alongside
  the existing checks.

No VM test is needed for either work item. Both operate on parsed
subprocess output exclusively, which the existing unit-test framework
covers. (A VM test would be required only if we touched NixOS-module
surface, systemd lifecycle, or pool-lock semantics; none of those
change here.)

## Non-goals

- No threshold-config knob. braid checks are hardcoded; matches the
  90-day SMART self-test constant at `doctor.rs:798-799`.
- No status-side mirror of the advisory. Add later only if operator
  demand surfaces.
- No automation. doctor warns; the operator runs `btrfs filesystem
  usage` and decides.
- No changes to the audit document `btrfs-findings/b4-balance-enospc.md`.

## Verified non-work

- Finding #1 of the audit asked for `-mconvert=raid1` in the
  post-degraded soft balance. The current code already emits
  `-mconvert=raid1,soft` (cmd.rs:665, test at cmd.rs:2442-2465).
  No work needed.

## Implementation notes

- Updated `cli/src/remove_missing.rs` because its ENOSPC propagation test pins
  the shared `pool::balance_error` text through the remove-missing error chain.

## Follow Up

- Run `sudo braid doctor` against a mounted real or VM pool to smoke-test the
  live `meta pressure` row; the local non-root smoke stopped at braid's root
  guard before rendering doctor output.
