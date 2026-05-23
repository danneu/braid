# Plan: RAID1-aware ENOSPC risk advisory in `braid status` + `braid doctor`

## Context

`braid status` and `braid doctor` today have no proactive surface for ENOSPC
risk on a RAID1 pool. ENOSPC is handled reactively elsewhere:
`preflight::check_raid1_relocation_space` (`cli/src/preflight.rs:319-367`)
refuses `remove` and `remove-missing` mutations that would ENOSPC on survivors
(called from `cli/src/remove.rs:721` and `cli/src/remove_missing.rs:562`).
`replace` does not call this preflight at all -- it uses
`check_replace_target_capacity` (`cli/src/preflight.rs:430`) which only
validates the replacement-target disk size, since `btrfs replace` reconstructs
data onto the new disk rather than relocating to survivors. The other safety
net is `pool::balance_error` (`cli/src/pool.rs:250-272`), which appends a
`btrfs balance start -dusage=0 -musage=0 <mp>` recovery hint when a braid
balance fails.

The gap: an operator who silently fills their pool to near-100% allocation
gets no warning until a disk dies, at which point `braid remove-missing` either
refuses at preflight, or worse, the underlying `btrfs device remove` lands in
the catastrophic dangerous-middle-band documented in
`docs/internals/btrfs/enospc-vs-hang.md` (partial relocation -> transaction
abort -> forced read-only). `replace` does not relocate so it is not subject
to that failure mode, but it still benefits from the same proactive visibility
so the operator can keep the pool healthy and rely on btrfs's own error
handling as the reactive net. The independent design note
`self-notes/auto-rebalance-after-braid-commands.md:168-180` flagged this exact
"persistent visibility" gap as a non-negotiable prerequisite.

The fix is a RAID1-aware advisory anchored to btrfs's own urgency threshold.
In `reference/linux/fs/btrfs/space-info.c:2025-2032`, `is_reclaim_urgent`
treats `unalloc < calc_effective_data_chunk_size(fs_info)` as the urgent
reclaim band. The kernel computes that effective chunk size as
`min(data_sinfo->chunk_size, 10% of total_rw_bytes)` clamped to 1 GiB
(`calc_effective_data_chunk_size`, lines 394-414) -- so the threshold scales
with pool size and only equals 1 GiB on pools of 10 GiB or more. The advisory
mirrors that formula per-pool, with a RAID1-aware predicate that warns when
the pool is one disk-loss away from insufficient chunk-pair capacity (matching
the catastrophic case the advisory is meant to head off).

The advisory fires in `braid status` (carried in `StatusReport.advisories`,
rendered by the existing `warning: <msg>` formatter at
`cli/src/status.rs:1046-1048`) and as a parallel `enospc_risk` row in
`braid doctor`. The underlying RAID1 capacity math is lifted out of
`preflight.rs` into a shared `cli/src/capacity.rs` module so the existing
preflight check and the new advisory both call one helper.

## Approach

### Step 1 -- New `cli/src/capacity.rs` module

Create a new module that exports:

- `pub fn enospc_risk_threshold(total_device_bytes: u64) -> u64` --
  mirrors the kernel's `calc_effective_data_chunk_size` formula
  (`reference/linux/fs/btrfs/space-info.c:394-414`). Returns
  `min(1 GiB, total_device_bytes / 10)`. Scales with pool size so a 4 GiB
  test pool gets a ~400 MiB threshold while a multi-TiB NAS gets the 1 GiB
  cap. `total_device_bytes` is the sum of `device_size` across all present
  devices (matching the kernel's `total_rw_bytes`).
- `pub fn raid1_chunk_pair_capacity(unallocated_desc: &[u64]) -> u64`
  -- lifted from the inline math at `cli/src/preflight.rs:346-350`. Given a
  descending-sorted slice of per-device unallocated values: if `len < 2`
  return 0; else
  `largest = u[0]; rest = u[1..].sum(); total = u.sum();
   if largest > rest { rest } else { total / 2 }`.
- `pub fn enospc_risk_advisory(devices: &[BtrfsDeviceUsageEntry],
  missing_count: u64) -> Vec<String>` -- returns a 0-or-1-item vec matching
  the sibling helper signatures (`journal::pending_op_advisories`,
  `luks::header_backup_advisories`). Returns empty when
  `missing_count > 0` (the degraded banner is the louder signal) OR
  `devices.len() < 2` (no RAID1 chunk-pair geometry). Otherwise:
  - Computes the current-pool threshold
    `current_threshold = enospc_risk_threshold(sum(device.device_size))`
    -- used for the K count in the rendered message.
  - **2-disk pool**: warn iff either device has
    `unallocated < current_threshold`. With only two devices, RAID1 needs
    chunk-pair space on both right now; a disk loss leaves a single-disk
    fragment that can't allocate RAID1 chunks at all regardless of
    unallocated bytes.
  - **3+ device pool**: simulate each hypothetical single-disk loss with
    a one-pass loop over `0..devices.len()`. For each candidate-lost disk
    `i`, recompute the survivor threshold against the post-loss pool size:
    `survivor_threshold = enospc_risk_threshold(sum(device_size for
    survivors))`. Warn iff for any `i`,
    `raid1_chunk_pair_capacity(sorted_desc(survivor_unallocated))
    < survivor_threshold`. Using the survivor-set threshold (rather than
    the pre-loss threshold) keeps the predicate consistent with the
    kernel's own per-pool chunk-size formula and avoids re-introducing
    F1-class false positives on small pools where the post-loss formula
    shrinks the threshold. Catches the 3-disk-with-only-two-headroom case
    where losing either headroom disk would strand the pool.

Advisory wording (one line, ASCII `--`, defers to the troubleshooting doc;
`K` is the count of devices with `unallocated < current_threshold`,
`format_bytes` is the existing helper in `cli/src/preflight.rs`):

```
ENOSPC risk: K of N devices have less than <format_bytes(current_threshold)> unallocated -- pool may be unable to allocate new RAID1 chunks. Free up files or run 'btrfs balance start -dusage=0 -musage=0 <mount>' to reclaim empty chunks.
```

The rendered byte value is `current_threshold` so the user sees the
threshold for the current pool size; the predicate trigger uses
per-simulation survivor thresholds as described above. The K count
remains meaningful: if K = 0 the predicate cannot fire (every disk is
above the larger current threshold, so every survivor set is above
its smaller survivor threshold and chunk-pair capacity is bounded
below by it).

Register `pub mod capacity;` in `cli/src/lib.rs`. Promote `format_bytes`
from `preflight.rs` to `capacity.rs` (or to a small shared `fmt.rs`) if
that helper isn't already exposed cross-module; pick whichever placement
matches existing project conventions in a quick `git grep` for
`format_bytes`.

### Step 2 -- Refactor `get_total_bytes` to return full device usage

`cli/src/status.rs:699-709` -- rename to `get_device_usage`, change return
type from `u64` to `BtrfsDeviceUsageOutput`. Update the 1 production call
site (`cli/src/status.rs:444`) to derive `total_bytes` via
`estimate_pool_capacity(&sizes)` from the returned output; update the 2 test
fixture call sites (~1502 and ~3225). This unblocks per-device unallocated
reuse without doubling the `btrfs device usage` shellout.

### Step 3 -- Wire advisory into `build_status`

`cli/src/status.rs:387-397` (`assemble_advisories`) stays unchanged -- the
new advisory is a mounted-pool advisory, not a recovery-mode one.

The current `build_status` nests `get_total_bytes` inside the
`df.as_ref()` Some-arm at `cli/src/status.rs:441-466`. After Step 2's
refactor (`get_total_bytes` -> `get_device_usage` returning
`BtrfsDeviceUsageOutput`), hoist the device-usage probe out of that arm
so it runs independently of `btrfs filesystem df` and `btrfs filesystem
usage`. Shape:

```rust
let dev_usage = if pool.missing_count == 0 {
    match get_device_usage(runner, config.mount_point()) {
        Ok(out) => Some(out),
        Err(_) => {
            advisories.push(
                "btrfs device usage failed -- pool total capacity and \
                 ENOSPC-risk advisory unavailable"
                    .to_owned(),
            );
            None
        }
    }
} else {
    None
};

let df = match fetch_df(...) { ... };
let capacity = match df.as_ref() {
    Some(df) => {
        let total_bytes = dev_usage.as_ref().map(|out| {
            estimate_pool_capacity(
                &out.devices.iter().map(|d| d.device_size).collect::<Vec<_>>(),
            )
        });
        match get_capacity(runner, config.mount_point(), df, total_bytes) {
            Ok(capacity) => Some(capacity),
            Err(_) => {
                advisories.push(
                    "btrfs filesystem usage failed -- pool capacity unavailable"
                        .to_owned(),
                );
                None
            }
        }
    }
    None => None,
};

if let Some(out) = dev_usage.as_ref() {
    advisories.extend(capacity::enospc_risk_advisory(
        &out.devices,
        pool.missing_count,
    ));
}
```

The `get_capacity` branch keeps the existing `Err` arm verbatim
(matching the current `cli/src/status.rs:458-466` shape) so that the
`"btrfs filesystem usage failed -- pool capacity unavailable"`
diagnostic is preserved; the only structural change is hoisting
`get_device_usage` out so its result is in scope independently. The
advisory fires whenever `btrfs device usage` parsed successfully on
a mounted, non-degraded pool, regardless of whether `btrfs filesystem
df` or `btrfs filesystem usage` succeeded. This preserves the
persistent-visibility goal: an operator whose pool is at the catastrophic
cliff still gets the warning even when other btrfs commands are
intermittently failing. Update the existing "btrfs device usage failed"
advisory text to mention that both `pool total capacity` and the
`ENOSPC-risk advisory` are now affected by the same probe.

Advisory ordering remains: probe-failure noise from
`assemble_advisories` and the df/usage failure messages sort above the
ENOSPC advisory, matching the existing convention. No formatter
changes -- the existing `warning: <msg>` loop at
`cli/src/status.rs:1046-1048` handles it.

### Step 4 -- Lift `check_raid1_relocation_space` math

`cli/src/preflight.rs:346-350` -- replace the inline
`if largest > rest { rest } else { total / 2 }` expression with
`let raid1_capacity = capacity::raid1_chunk_pair_capacity(&remaining_unalloc);`.
The 2-device precondition check at `cli/src/preflight.rs:337-344` stays put
because its error message references `alloc_type` ("cannot relocate
{alloc_type} chunks: ..."). Existing preflight tests pass unchanged --
this is a pure math extraction.

### Step 5 -- Doctor `enospc_risk` check

`cli/src/doctor.rs` -- add a cached `device_usage` field on `DoctorContext`
mirroring the existing `df_snapshot` cache; see the `ensure_pool_state`
pattern used by `check_pool_missing_devices` at lines 693-730 and replicate
it as `ensure_device_usage(ctx)`. Add `check_enospc_risk(ctx)` that:

- Early-skips when config unavailable or pool not mounted
  (`CheckResult::skip`).
- Calls `ensure_pool_state(ctx)` to populate the cached pool state. The
  cache holds `Result<PoolState, ProbeError>`, matching the existing
  pattern at `cli/src/doctor.rs:601-618`. Three pool-state branches,
  mirroring `check_pool_missing_devices` at `cli/src/doctor.rs:693-730`:
  - `Err(e)`: `CheckResult::warn("enospc_risk", format!("could not probe
    pool state -- ENOSPC risk indeterminate: {e}"))`. Fail-loud so an
    operator with a busted probe sees the check, not a silent pass.
  - `Ok(pool)` with `pool.missing_count > 0`: `CheckResult::skip(...,
    "skipped (pool is degraded)")`. The degraded banner / missing-devices
    check is the louder signal here.
  - `Ok(pool)` with `missing_count == 0`: continue.
- Probes `btrfs device usage` via the new cached helper. The cache is
  populated independently of the existing `df_snapshot` cache so an
  upstream df probe failure does not suppress the ENOSPC check -- same
  decoupling as Step 3.
- If the device-usage probe errored: `CheckResult::warn("enospc_risk",
  "btrfs device usage failed -- ENOSPC risk indeterminate")`. Failure-to-
  probe is a louder signal than "we don't know" silently passing.
- Otherwise calls `capacity::enospc_risk_advisory(&devices, missing_count)`.
  If the vec is empty: `CheckResult::ok("enospc_risk", "per-device
  unallocated space healthy")`. Otherwise: `CheckResult::warn("enospc_risk",
  advisory[0])` (reusing the exact advisory string keeps status and doctor
  in lockstep).

Register the check in `run_doctor` after `check_pool_missing_devices`.

Add to `docs/commands/doctor.md` checks table (lines 60-72):

```
| `enospc_risk` | Warns when the pool is one disk-loss away from insufficient RAID1 chunk-pair space. Per-device threshold scales with pool size (min(1 GiB, 10% of total device bytes), matching the kernel's effective data chunk size) |
```

### Step 6 -- User-facing docs

`docs/commands/status.md` Advisories section (lines 144-198) -- append an
"ENOSPC risk on RAID1 pool" subsection mirroring the existing structure
(foreign filesystem, pending recovery journal, pending LUKS header backups).
Brief: explain when it fires (pool is one disk-loss away from insufficient
RAID1 chunk-pair space; 2-disk pools fire when either disk drops below the
threshold; per-device threshold = min(1 GiB, 10% of total device bytes),
matching the kernel's effective data chunk size), and link to
`docs/guides/troubleshooting.md` for the full recovery procedure.

No changes needed to `docs/guides/troubleshooting.md` -- the "Balance fails
with No space left on device" section at lines 7-39 already documents the
`-dusage=0 -musage=0` reclaim and the "free up files" alternative.

## Tests

Unit tests in `cli/src/capacity.rs`:

Threshold function:
- `enospc_risk_threshold_caps_at_1_gib`
  (`enospc_risk_threshold(100 * (1 << 40)) == 1 << 30`).
- `enospc_risk_threshold_scales_below_10_gib`
  (`enospc_risk_threshold(5 * (1 << 30)) == 512 * (1 << 20)` -- 10% of 5 GiB
  is exactly 512 MiB; clean boundary value to avoid integer-rounding noise
  in the assertion).
- `enospc_risk_threshold_zero` (`enospc_risk_threshold(0) == 0`).

RAID1 helper:
- `raid1_chunk_pair_capacity_empty` / `_single_device` / `_two_equal`
  (`[5, 5] -> 5`) / `_bottlenecked_by_largest` (`[10, 1, 1] -> 2`) /
  `_balanced_three_disk` -- mirror the existing `estimate_pool_capacity_*`
  test style.

Advisory predicate (each test uses device sizes that anchor the threshold
to a known value so assertions are unambiguous):
- `enospc_risk_advisory_silent_on_single_disk` (1 device -> empty).
- `enospc_risk_advisory_silent_on_degraded` (`missing_count = 1` -> empty).
- `enospc_risk_advisory_silent_on_healthy_tiny_raid1`
  (**F1 regression**: 2 x 256 MiB disks; total = 512 MiB; threshold = 51 MiB;
  both disks have ~200 MiB unallocated -> empty. Catches the regression where
  a hard-coded 1 GiB threshold warns forever on VM fixtures with small
  disks).
- `enospc_risk_advisory_silent_on_healthy_large_raid1`
  (3 x 12 TiB disks all with 5 TiB unallocated -> empty; threshold = 1 GiB,
  count_below = 0).
- `enospc_risk_advisory_fires_on_2_disk_pool_with_one_low`
  (2 x 100 GiB disks; one with 10 MiB unallocated -> 1 item starting with
  `"ENOSPC risk:"`; threshold = 1 GiB).
- `enospc_risk_advisory_fires_on_3_disk_loss_simulation`
  (**F2 regression** for the previous round: 3 x 100 GiB disks, unallocated
  `[10 GiB, 10 GiB, 50 MiB]` -> warn. Current threshold and per-survivor
  threshold both round to 1 GiB on a 300 GiB pool (200 GiB / 10 still caps
  at 1 GiB); losing either 10 GiB disk leaves survivors `[10 GiB, 50 MiB]`
  with chunk-pair capacity 50 MiB < 1 GiB. The previous predicate
  (count_above < 2) would have stayed silent here, missing the catastrophic
  single-disk-loss scenario).
- `enospc_risk_advisory_silent_on_4_disk_with_one_low`
  (4 x 100 GiB disks; unallocated `[10 GiB, 10 GiB, 10 GiB, 50 MiB]` -> empty.
  Losing the 50 MiB disk leaves three 10 GiB disks (chunk-pair capacity
  10 GiB); losing any 10 GiB disk leaves `[10 GiB, 10 GiB, 50 MiB]`
  (chunk-pair capacity 10 GiB). No single-disk loss drops survivors below
  the 1 GiB threshold. Confirms the predicate is fault-tolerant aware,
  not just a flat count_above check).
- `enospc_risk_advisory_uses_survivor_threshold_not_pre_loss`
  (**F2 regression** for the current round: 3 x 4 GiB disks, unallocated
  `[3 GiB, 3 GiB, 900 MiB]` -> empty. Pre-loss total = 12 GiB so
  `current_threshold = min(1 GiB, 1.2 GiB) = 1 GiB`; losing a 3 GiB disk
  leaves survivors with total 8 GiB and
  `survivor_threshold = min(1 GiB, 800 MiB) = 800 MiB`; chunk-pair capacity
  of `[3 GiB, 900 MiB]` is 900 MiB, which is `>= 800 MiB` -> no warn. If
  the predicate used the pre-loss 1 GiB threshold against survivors,
  900 MiB < 1 GiB would falsely fire. This test pins the survivor-set
  threshold semantics).

Integration tests in `cli/src/status.rs`:

- Extend an existing status fixture path to inject a low-unallocated
  `btrfs device usage` output and assert
  `report.advisories.iter().any(|a| a.starts_with("ENOSPC risk:"))`.
  Use the same `starts_with` style as the existing pending-op advisory test
  at `cli/src/status.rs:3291`.
- Healthy-pool regression for the small-device case: assert that an
  existing healthy-pool fixture that mirrors the VM small-disk geometry
  (256 MiB-style) continues to have an empty `advisories` vec -- regression
  coverage for the prior round's F1 at the integration layer.
- **Probe-failure isolation** (F1 regression for the prior round): in a
  fixture where the `btrfs filesystem df` runner request errors but
  `btrfs device usage` returns a low-unallocated payload, assert the report
  carries BOTH the existing `"btrfs filesystem df failed -- ..."` advisory
  AND the `"ENOSPC risk: ..."` advisory. This pins the decoupled wiring so
  a future refactor cannot re-introduce the gating.
- **`get_capacity` failure preserves both diagnostics** (new F1 regression
  for the current round): in a fixture where `btrfs filesystem df` succeeds
  and `btrfs device usage` returns a low-unallocated payload but
  `btrfs filesystem usage` errors, assert the report carries BOTH the
  existing `"btrfs filesystem usage failed -- pool capacity unavailable"`
  advisory AND the `"ENOSPC risk: ..."` advisory. Pins that the Step 3
  hoist did not regress the existing capacity-error advisory.

Doctor tests in `cli/src/doctor.rs`: healthy 3-disk fixture asserts an `Ok`
result row for `enospc_risk`; low-unallocated fixture asserts a `Warn` with
the advisory wording; device-usage-failure fixture asserts a `Warn` row
with "btrfs device usage failed -- ENOSPC risk indeterminate" (pins the
fail-loud device-usage probe-error branch from Step 5); pool-state-failure
fixture asserts a `Warn` row whose message starts with
`"could not probe pool state -- ENOSPC risk indeterminate:"` (pins the
fail-loud pool-state probe-error branch from Step 5); degraded fixture
(`missing_count > 0`) asserts a `Skip` row noting the pool is degraded.
Mirror the existing `check_pool_missing_devices`-style test layout.

No new VM test required -- the existing
`tests/repro/btrfs-remove-enospc-crash.nix/.py` already exercises the
fill-to-near-full scenario at the raw btrfs level; the manual VM walkthrough
below confirms end-to-end against the same fixture.

## Verification

1. `just test-rust` -- runs the new unit tests in `cli/src/capacity.rs` and
   the updated `status.rs` / `doctor.rs` tests.
2. `just test-vm braid-status-rust` (or the closest existing status VM test
   -- confirm with `ls tests/` before running) -- exercises production
   `build_status` against real `btrfs device usage` output.
3. **Manual end-to-end** in the existing `btrfs-remove-enospc-crash` VM
   fixture (3 x 4 GiB disks, adaptive fill to <800 MiB per device per
   `docs/internals/btrfs/enospc-vs-hang.md:101-103`). On this fixture
   `total_device_bytes = 12 GiB` so the computed threshold is
   `min(1 GiB, 1.2 GiB) = 1 GiB`. Walkthrough:
   - Boot, fill to the dangerous-middle-band, run `braid status`.
     Confirm the advisory appears, prefixed `warning: ENOSPC risk: ...`,
     and that the byte value in the message reads `1.0 GiB` (or whatever
     `format_bytes(1 << 30)` renders).
   - Run `braid doctor`. Confirm `enospc_risk` row is `warn` with matching
     wording.
   - Recover by either deleting files or running
     `btrfs balance start -dusage=0 -musage=0 /mnt/storage` until the
     single-disk-loss predicate is satisfied (no single disk loss leaves
     survivors below the chunk-pair threshold). Re-run `braid status` and
     `braid doctor`; confirm both surfaces go quiet (no advisory; `ok`
     row).

## Critical files

- `cli/src/capacity.rs` (new module: threshold function, RAID1 helper,
  advisory function)
- `cli/src/status.rs` (refactor `get_total_bytes` -> `get_device_usage`;
  wire advisory)
- `cli/src/preflight.rs` (lift inline math to shared helper)
- `cli/src/doctor.rs` (new `check_enospc_risk` + cached device-usage probe)
- `cli/src/lib.rs` (`pub mod capacity;`)
- `docs/commands/status.md` (Advisories section)
- `docs/commands/doctor.md` (checks table row)

## Reused / referenced functions

- `parse_btrfs_device_usage` (`cli/src/parse/btrfs_device_usage.rs:40-116`)
  + `BtrfsDeviceUsageEntry` (`cli/src/parse/types.rs:466-492`) -- existing
  parser already exposes per-device `unallocated`.
- `estimate_pool_capacity` (`cli/src/status.rs:108-115`) -- consumed inline
  in Step 2's refactor.
- `journal::pending_op_advisories`, `luks::header_backup_advisories` --
  signature precedent for `enospc_risk_advisory`.

## Out of scope

- No NixOS module option for the threshold. The formula is anchored to
  the kernel's own `calc_effective_data_chunk_size`
  (`reference/linux/fs/btrfs/space-info.c:394-414`); no existing braid
  threshold is configurable today, and exposing a kernel-derived value as
  a config option is over-engineering. Easy to adjust in one place if
  telemetry surfaces miscalibration.
- No new `braid balance` subcommand. The recovery action remains a literal
  `btrfs balance start -dusage=0 -musage=0 <mp>` quoted in the advisory and
  documented in `docs/guides/troubleshooting.md`.
- No alert (latched, ack-required). ENOSPC risk is a transient condition
  that resolves automatically when the user frees space or runs reclaim;
  it fits the advisory pattern, not the alert pattern (see
  `docs/design/decisions/014-alerts.md:15-17`).
- No changes to `docs/guides/troubleshooting.md`; the existing ENOSPC
  section already covers the recovery in detail.
