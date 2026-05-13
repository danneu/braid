# Narrow `probe_pool` for the alert pipeline (ack + monitor)

## Context

`cmd_ack_impl` (`cli/src/ack.rs:49`) and `cmd_monitor` (`cli/src/monitor.rs:51`)
both call `probe_pool` even though neither consumes per-device LUKS UUIDs or
the pool FSID. `probe_pool`'s per-device loop issues two shell-outs per disk
(`cryptsetup status` + `cryptsetup luksUUID`); only the first is needed by
the alert pipeline. `cryptsetup status` detects the active state and the
`device: (null)` (null-underlying) case; `cryptsetup luksUUID` only populates
`PoolDevice::luks_uuid`, which the alert pipeline never reads.

Cost of the unnecessary work:

- Ack on an N-disk pool runs N extra `cryptsetup luksUUID` shell-outs. The
  comment at `ack.rs:26-33` already calls out that "`probe_pool` is slow
  enough (multiple per-disk shell-outs) for the asynchronous smartd hook
  to fire during it"; shortening the probe shrinks the smartd-mid-probe
  race window.
- Monitor runs the same extra work on every systemd timer fire.
- A per-device luksUUID failure (any `CmdError`/`ParseError` on the
  underlying device's cryptsetup output) surfaces as an ack failure -- a
  single mismatched mapper-LUKS-UUID can block the operator from acking
  an unrelated alert -- and as a monitor `ComputationError` fail-closed
  beep. The beep should reflect a real indeterminate alert condition, not
  a luksUUID hiccup the alert pipeline did not need.

Project precedent: `probe_fsid` (`cli/src/probe.rs:341`) already exists as
a deliberately-narrowed sibling for `cmd_lock`, with the doc comment
"without probing per-device cryptsetup state". This plan adds the analogous
narrowed variant for the alert pipeline.

Identity correlation by LUKS UUID is reserved for setup, repair, and the
planning/status commands (ADR 024: `docs/decisions/024-luks-uuid-identity.md`).
Ack and monitor are not identity-correlation surfaces; both already work
strictly devid-keyed.

## Approach

### New struct + function in `cli/src/probe.rs`

Add a dedicated return type that documents the alert pipeline's exact
consumption:

```rust
/// Devid-only view of the pool returned by `probe_pool_alerts`. Carries
/// the mount state, present devid set, btrfs-MISSING devids, and null-
/// underlying devices that ack and monitor need; explicitly omits per-
/// device LUKS identity, FSID, and per-device counts -- those belong to
/// the identity-correlation surfaces (status, add, remove, replace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertPoolState {
    pub mounted: bool,
    pub present_devids: Vec<u64>,
    pub missing_devids: Vec<u64>,
    pub null_underlying: Vec<NullUnderlyingDevice>,
}

impl AlertPoolState {
    /// Same semantics as `PoolState::alert_missing_devids` -- the btrfs-
    /// MISSING set unioned with null-underlying devids, deduped + sorted.
    pub fn alert_missing_devids(&self) -> Vec<u64> {
        self.missing_devids
            .iter()
            .copied()
            .chain(self.null_underlying.iter().map(|d| d.devid))
            .collect::<BTreeSet<u64>>()
            .into_iter()
            .collect()
    }
}
```

Add `probe_pool_alerts` next to `probe_pool`. Body mirrors `probe_pool`
except:

- Skip the per-device `CmdRequest::CryptsetupLuksUuid` call (currently
  `probe.rs:301-311`).
- Do not extract `fsid` from the `btrfs filesystem show` output -- still
  run `btrfs filesystem show` (it is the source of `missing_devids` and
  the device list), but drop `show.uuid`.
- For each non-null `cryptsetup status` row, push `bdev.devid` to
  `present_devids` (mirrors `probe_pool`'s `devices.push(...)` minus the
  `luks_uuid`/`underlying` capture).
- Return `AlertPoolState`, not `PoolState`.

The function must carry a `///` doc comment (per AGENTS.md "Doc
Comments") naming why it exists at this boundary -- something on the
order of:

```rust
/// Narrowed alert-pipeline probe for `cmd_ack` and `cmd_monitor`.
/// Preserves `probe_pool`'s mount-state, btrfs-MISSING, and null-
/// underlying detection (one `cryptsetup status` per pool device) but
/// omits per-device LUKS UUID lookup and FSID extraction -- the alert
/// pipeline is devid-keyed and does not consume either.
pub fn probe_pool_alerts<R, F>(...) -> Result<AlertPoolState, ProbeError>
```

Reuse `probe_pool`'s existing `ProbeError` variants (`Cmd`, `Parse`,
`PoolDevice`, `NotBtrfs`, `MountInfo`). Monitor's exhaustive ProbeError
match (`monitor.rs:51-69`) stays valid -- same error type, same set of
reachable variants minus the already-unreachable
`UnsupportedLuksVersion`/`MapperConflict`.

### Wire up `cmd_ack_impl` (`cli/src/ack.rs:49`)

- Import `probe_pool_alerts` instead of `probe_pool`.
- `pool.mounted` (`ack.rs:54`) and `pool.alert_missing_devids()`
  (`ack.rs:76`) work unchanged -- `AlertPoolState` exposes the same
  surface.

### Wire up `cmd_monitor` (`cli/src/monitor.rs:51`)

- Import `probe_pool_alerts` instead of `probe_pool`.
- Change `monitor.rs:99` from
  `pool.devices.iter().map(|d| d.devid).collect()` to
  `pool.present_devids.iter().copied().collect()`.
- `pool.null_underlying.iter().map(|d| d.devid)` (`monitor.rs:103`) and
  `pool.missing_devids.iter().copied()` (`monitor.rs:104`) stay unchanged
  -- same field names.
- The exhaustive `ProbeError` match stays valid.

### Trim fixtures

`cli/src/test_fixtures/ack.rs:210-242`:
- Remove the two `CryptsetupLuksUuid` mock entries (lines 224-229 and
  236-241) from `ack_mounted_probe_runner()`.

`cli/src/test_fixtures/monitor.rs`:
- Remove the `CmdRequest::CryptsetupLuksUuid { .. }` match arm in
  `MonitorTestRunner::run` (`monitor.rs:164`) and
  `MonitorReconcileRunner::run` (`monitor.rs:198`). Each runner's
  `other =>` arm panics, which is the intended assertion that ack/monitor
  must not request luksUUID after the refactor.
- Drop the now-unused `LUKS_UUID` constant if it has no remaining
  references.

## Files touched

- `cli/src/probe.rs` -- add `AlertPoolState`, `probe_pool_alerts`, new
  unit tests.
- `cli/src/ack.rs:6, 49` -- swap import + call to `probe_pool_alerts`.
- `cli/src/monitor.rs:8, 51, 99` -- swap import + call to
  `probe_pool_alerts`; change `pool.devices.iter()` access to
  `pool.present_devids.iter()`.
- `cli/src/test_fixtures/ack.rs:210-242` -- delete the two
  `CryptsetupLuksUuid` mock entries.
- `cli/src/test_fixtures/monitor.rs:164, 198` -- delete the
  `CryptsetupLuksUuid` match arms (and unused `LUKS_UUID` constant if
  applicable).
- `docs/decisions/014-alerts.md:47, 136` -- two co-located edits in the
  same ADR:
  - **Line 47** ("Ack snapshots gating inputs before probing" section):
    name `probe_pool_alerts` instead of `probe_pool`, and add one short
    sentence clarifying that the alert probe is devid-keyed and
    intentionally does not depend on LUKS UUID identity or pool FSID.
    The existing "Ack state keyed by btrfs devid" section at line 53-55
    already establishes the devid-keyed principle; this is a localized
    name correction plus a one-sentence scope note.
  - **Line 136** ("Monitor reconcile" defense-in-depth bullet under
    "Acked-stats hygiene across pool membership changes"): replace the
    stale field names with the alert-probe shape. Currently reads
    "devid no longer in `pool.devices`, `pool.null_underlying`, or
    `pool.missing_devids`"; update to "devid no longer in
    `pool.present_devids`, `pool.null_underlying`, or
    `pool.missing_devids`" (the alert-probe fields). The reconcile
    semantics are unchanged; only the names referenced by the ADR
    move from `PoolState` to `AlertPoolState`.

No changes to `PoolState`, `PoolDevice`, `probe_pool`, `probe_fsid`,
`probe_config_disk`, `status.rs` (it keeps using full `probe_pool` and
its `pool.alert_missing_devids()` call at `status.rs:415` is unaffected),
or any other call sites of `probe_pool`.

ADR 024 (luks uuid identity) remains authoritative as-is.

## Tests to add

In `cli/src/probe.rs`'s test module, mirroring the existing `probe_pool`
test set. Each test follows the `// Intent / // Why it exists / //
Scenario` preamble required by `docs/testing.md`.

| Test | Asserts |
|------|---------|
| `probe_pool_alerts_unmounted` | `mounted=false`; all collections empty. |
| `probe_pool_alerts_mounted_2disk` | `mounted=true`; `present_devids=[1,2]`; `missing_devids` and `null_underlying` empty. No `CryptsetupLuksUuid` mock is seeded -- `MockRunner`'s panic-on-unmocked-request is the witness that the narrow probe does not issue that call. |
| `probe_pool_alerts_with_btrfs_missing_sentinel` | `present_devids=[1]`, `missing_devids=[2]`, `null_underlying` empty. |
| `probe_pool_alerts_with_null_underlying` | `present_devids=[1]`, `null_underlying=[{devid:2}]`, `missing_devids` empty. |
| `probe_pool_alerts_tolerates_missing_fsid` | Seed `btrfs filesystem show` output with `Total devices` and the mapper rows but **no** `uuid:` line; seed only `CryptsetupStatus` for the live mappers. Assert the call returns `Ok` with `mounted=true` and the expected `present_devids`. This is the failure-layer test for the FSID-narrowing claim: if an implementation accidentally retains `probe_pool`'s `show.uuid.ok_or_else(...)` rejection at `probe.rs:243`, this test fails. The other mounted tests all carry a valid `uuid:` line and would not catch the regression. |
| `probe_pool_alerts_not_btrfs` | `ProbeError::NotBtrfs { fstype: "ext4", .. }` propagated. |
| `probe_pool_alerts_mapper_not_active` | `ProbeError::PoolDevice { detail: "not active", .. }` propagated. |
| `probe_pool_alerts_non_mapper_device` | `ProbeError::PoolDevice { detail: "not a /dev/mapper/ path", .. }` propagated. |
| `probe_pool_alerts_propagates_mountinfo_io_error` | `ProbeError::MountInfo(MountInfoError::Io(_))` propagated. |
| `probe_pool_alerts_alert_missing_devids_method` | Direct unit test of `AlertPoolState::alert_missing_devids()`: union of `missing_devids` and `null_underlying`, deduped + sorted (mirrors the `PoolState` method's behavior). |

Existing ack and monitor tests in `ack.rs` and `monitor.rs` keep passing
unchanged: their `pool.mounted` / `pool.alert_missing_devids()` /
`pool.null_underlying` / `pool.missing_devids` reads have the same shape
on `AlertPoolState`. The fixture luksUUID removals above are the only
visible change in those test paths.

## Verification

1. **Rust unit tests:** `just test-rust`. New probe tests pass; existing
   ack/monitor tests pass; nothing else regresses.
2. **VM tests:** `just test-vm`. Specifically watch for any
   ack/monitor/degraded scenarios in the existing suite -- they exercise
   the alert pipeline end-to-end (mounted ack, offline ack, MissingDevice
   latching, null-underlying handling). All should still pass.
3. **Parser canary:** `just test-parsers`. Proves the narrower probe is
   still compatible with live `cryptsetup status` + `btrfs filesystem
   show` output.
4. **Callgraph sanity check:** after the refactor, the following greps
   should report no hits, proving the narrowing landed:
   - `grep -n "CryptsetupLuksUuid" cli/src/ack.rs cli/src/monitor.rs cli/src/test_fixtures/ack.rs cli/src/test_fixtures/monitor.rs`
   - `grep -nE "probe_pool\b" cli/src/ack.rs cli/src/monitor.rs`
