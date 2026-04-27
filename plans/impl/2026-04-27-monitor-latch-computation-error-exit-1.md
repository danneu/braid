# Fix: monitor latches ComputationError but never beeps

## Context

`cmd_monitor` in `cli/src/monitor.rs` has two related blind spots that defeat the
"fail closed" intent of the monitor service:

1. **UnmappedDeviceError path latches but exits 2.** When
   `compute_alert_state_with_devid_map` returns `UnmappedDeviceError`,
   `cmd_monitor` writes a `ComputationError { detail }` AlertCause to the latch
   (cli/src/monitor.rs:98-110) and then returns `Err(MonitorError::UnmappedDevice(e))`.
   `main.rs` (cli/src/main.rs:600-603) maps every `MonitorError` to exit 2.
   The systemd wrapper at `modules/braid/monitor.nix:131-141` only starts
   `braid-alert.service` on `rc==1`; `rc>=2` just logs `"braid monitor failed"`.
   Result: the latch shows up in `braid status`, but on a headless NAS the
   speaker stays silent until the operator SSHes in.

2. **ProbeError variants exit 2 with no latch at all.** Every non-`NotBtrfs`
   `ProbeError` (`Cmd`, `Parse`, `PoolDevice`, `UnsupportedLuksVersion`,
   `MapperConflict`) propagates through `?`/match into `MonitorError::Probe(e)`
   (cli/src/monitor.rs:31). Same exit-2 path -- but unlike the unmapped-device
   case, no `ComputationError` is even written to the latch. `braid status` will
   show nothing. Same gap exists for the `runner.run(BtrfsDeviceStatsJson)`
   `CmdError` and `parse_btrfs_device_stats` `ParseError` paths
   (cli/src/monitor.rs:39-42).

This is the same exit-2-instead-of-exit-1 failure mode that
`plans/wip/structured-splashing-fern.md` already fixed for hot-unplug, just
resurrected here for the strictly-more-dangerous "we couldn't even probe" and
"we lost track of a mapping" cases.

The intended outcome: any failure that leaves pool state indeterminate latches
a `ComputationError` and exits 1 so the wrapper starts `braid-alert.service`
and the speaker beeps. Exit 2 is reserved for "we could not even attempt"
states (config unreadable -- already handled in main.rs:582-588 outside
`cmd_monitor`), where a beep would be meaningless anyway.

## Approach

Treat "latch written" as the universal exit-1 trigger. Funnel every
indeterminate-state failure inside `cmd_monitor` through one helper that
writes the latch and returns `MonitorResult::Alert(merged)`. After the fix,
`cmd_monitor` cannot fail -- so `MonitorError` is deleted and the function
signature becomes `MonitorResult` directly.

### Files modified

- **`cli/src/monitor.rs`** -- the entire code fix lives here.
- **`cli/src/main.rs`** -- collapse the `Err` arm now that `cmd_monitor` is
  infallible; update the `Monitor` clap help string at line 52.
- **`docs/decisions/014-alerts.md`** -- update the exit-code contract at
  lines 57-60 (Active ADR; the contract is changing, so the doc must change
  with it).

### Code changes

#### 1. New helper: `latch_computation_error`

Add to `cli/src/monitor.rs`. Single source of truth for the latch+log+merge
pattern, called from every indeterminate-state branch:

```rust
fn latch_computation_error(detail: String, paths: &StatePaths) -> MonitorResult {
    eprintln!("error: {detail}");
    let causes = vec![AlertCause::ComputationError { detail }];
    let existing = alert::load_alert_latch(paths);
    let merged = merge_into_latch(existing.as_ref(), &causes);
    if let Err(e) = alert::save_alert_latch(&merged, paths) {
        eprintln!("Warning: failed to write alert latch: {e}");
    }
    MonitorResult::Alert(merged)
}
```

Reuses existing helpers:
- `alert::load_alert_latch` / `alert::save_alert_latch`
- `alert::merge_into_latch` -- causes-set union with same-key replacement, so a
  fresh `ComputationError` cleanly replaces a stale one (cli/src/alert.rs:272-306,
  `same_cause_key` matches `ComputationError` by variant alone).

#### 2. Rewrite `cmd_monitor` signature and error sites

Change return type from `Result<MonitorResult, MonitorError>` to
`MonitorResult`. At every former error site, return
`latch_computation_error(e.to_string(), paths)` instead of bubbling:

| Site | Current behaviour | New behaviour |
| --- | --- | --- |
| `probe_pool` non-`NotBtrfs` (line 31) | `Err(MonitorError::Probe)` | `latch_computation_error` |
| `runner.run(BtrfsDeviceStatsJson)` (line 39-41) | `?` -> `MonitorError::Cmd` | match -> `latch_computation_error` |
| `parse_btrfs_device_stats` (line 42) | `?` -> `MonitorError::Parse` | match -> `latch_computation_error` |
| `compute_alert_state_with_devid_map` (line 98-110) | latch + `Err(MonitorError::UnmappedDevice)` | `latch_computation_error` (folds the existing latch logic) |

The `Err(ProbeError::NotBtrfs { .. })` arm at line 28-30 stays as-is and still
returns `Ok(MonitorResult::PoolOffline)` -> exit 0. That's "not our pool",
not indeterminate.

#### 3. Delete `MonitorError`

Remove the `MonitorError` enum (cli/src/monitor.rs:134-144) entirely. No
external consumers exist (verified: only `monitor.rs` and `main.rs` reference
it). This follows AGENTS.md "no backwards compatibility" -- braid is
unreleased.

#### 4. Simplify `main.rs` match

`cli/src/main.rs:590-604` collapses to three arms (no `Err` arm):

```rust
match braid_cli::monitor::cmd_monitor(&runner, config.mount_point(), &paths) {
    braid_cli::monitor::MonitorResult::PoolOffline => std::process::exit(0),
    braid_cli::monitor::MonitorResult::Ok => std::process::exit(0),
    braid_cli::monitor::MonitorResult::Alert(_) => std::process::exit(1),
}
```

The pre-existing `config_read` failure at main.rs:582-588 still exits 2 -- that
is the "could not even attempt" case the finding identifies as the legitimate
home for exit 2.

#### 5. Systemd wrapper: no change

`modules/braid/monitor.nix:131-141` keeps its current shape -- exit 1 starts
`braid-alert.service`, exit 2 logs. After the fix, `cmd_monitor` will never
emit exit 2, so the `elif [ "$rc" -ge 2 ]` branch is now reached only by
`config_read` failure in main.rs. That's the correct division.

#### 6. Documentation updates

- **`docs/decisions/014-alerts.md:57-60`** -- replace the current exit-code
  contract:

  ```
  Exit codes:
  - 0 -- ok or pool offline with no active alerts
  - 1 -- alert active (disk health issue OR indeterminate state latched as
    ComputationError -- e.g. probe failure, parse failure, unmapped device)
  - 2 -- pre-monitor setup failure (config unreadable). Reserved for "could
    not even attempt to detect"; never emitted by `cmd_monitor` itself.
  ```

  Also expand the "fail closed" rationale in a new sub-paragraph: any
  failure inside `cmd_monitor` that leaves pool state indeterminate latches
  a `ComputationError` cause and reports exit 1, so the systemd wrapper
  starts the beeper. Exit 2 means the monitor never ran; a beep would be
  meaningless because there is no AlertState to report.

- **`cli/src/main.rs:52`** -- update the `Monitor` clap doc comment from
  `/// Check disk health: exit 0 = ok/offline, exit 1 = alert, exit 2 = error`
  to reflect the new contract, e.g.
  `/// Check disk health: exit 0 = ok/offline, exit 1 = alert (incl. probe/compute failure latched as ComputationError), exit 2 = setup error (config)`.

## Tests

Per `feedback_test_at_failure_layer.md`, every rewritten failure site needs a
behavioral test that FAILS when that arm is reverted. Add unit tests in
`cli/src/monitor.rs` (first test module in this file). Per
`feedback_test_preamble_block_comment_literal.md`, each test gets a literal
`/* Intent / Why it exists / Scenario */` block comment. Per
`feedback_local_runner_over_shared_mock.md`, write a file-local runner;
do not extend a shared `MockRunner`.

### Probe command sequence (test scaffolding reference)

`cmd_monitor` invokes `probe_pool` first, which issues this `CmdRequest`
sequence (cli/src/probe.rs:218-322):
1. `FindmntJson` -- mount-point check
2. `BtrfsFilesystemShow` -- pool layout + MISSING sentinels
3. For each pool device: `CryptsetupStatus` (and `CryptsetupLuksUuid` when
   the mapper has an underlying device)

Then `cmd_monitor` itself issues:
4. `BtrfsDeviceStatsJson` (cli/src/monitor.rs:39-41)

The file-local runner must respond to all four `CmdRequest` variants used
by the success path, and selectively fail one to drive each test branch.

### Unit test 1: UnmappedDeviceError -> Alert (not Err)

Runner returns successful responses for `FindmntJson`, `BtrfsFilesystemShow`,
`CryptsetupStatus`, `CryptsetupLuksUuid` (yielding a normal pool with no
null-underlying devices), then a `BtrfsDeviceStatsJson` payload that
references a mapper path NOT present in the devid map (e.g. a stats entry
for `/dev/mapper/braid-stale` when the pool only has `braid-disk1` /
`braid-disk2`). This forces `compute_alert_state_with_devid_map` to return
`UnmappedDeviceError`.

Asserts:
- `cmd_monitor` returns `MonitorResult::Alert(state)` (NOT `Err`)
- `state.causes` contains `AlertCause::ComputationError { .. }`
- The alert latch file on disk contains the same `ComputationError`

Reverting just the `UnmappedDevice` arm of the fix (return `Err(...)`) must
make this test fail. That's the gate for finding (1) of the original review.

### Unit test 2: ProbeError -> Alert with latched ComputationError

Runner fails the very first probe command -- `FindmntJson` returns a
`CmdError`. This propagates as `ProbeError::Cmd` from `probe_pool`.

Asserts:
- `cmd_monitor` returns `MonitorResult::Alert(state)`
- `state.causes` contains exactly one `ComputationError`
- The latch file was written

This test catches the worse-than-the-original bug: the path that today
exits 2 with **no latch at all**.

### Unit test 3 (table-driven): stats-path failures -> Alert

Single test, two cases sharing identical probe-success setup
(`FindmntJson`, `BtrfsFilesystemShow`, `CryptsetupStatus`,
`CryptsetupLuksUuid` all return well-formed responses). The cases differ
only at the `BtrfsDeviceStatsJson` step:

| Case | Runner response for `BtrfsDeviceStatsJson` | Expected error path |
| --- | --- | --- |
| stats-cmd-failure | `Err(CmdError::Failed("...".into()))` | `MonitorError::Cmd` site (line 39-41) |
| stats-parse-failure | `Ok` with malformed JSON | `parse_btrfs_device_stats` error site (line 42) |

Note: braid's `CommandRunner` contract returns non-zero process exits as
`Ok(RawCommandOutput)` (cli/src/cmd.rs:841-858); only spawn/signal failures
yield `Err(CmdError::Failed)`. So `stats-cmd-failure` must construct the
`Err` variant directly in the file-local runner -- a non-zero exit payload
would slip into `parse_btrfs_device_stats` and accidentally exercise the
parse case instead.

Each case asserts:
- `cmd_monitor` returns `MonitorResult::Alert(state)`
- `state.causes` contains exactly one `ComputationError`
- The latch file was written

Reverting either rewrite (re-introducing `?` -> `MonitorError::Cmd` /
`MonitorError::Parse`) must fail the corresponding case. That covers the
two sites the original Tests section missed.

### Existing tests stay green

- `tests/cli/monitor-hot-unplug.{nix,py}` -- still asserts `MissingDevice`
  cause + exit 1 via the post-fix `null_underlying` path. The new helper is
  only reached when the devid map can't resolve a path even after that
  chaining, so this test does not move.
- `tests/cli/braid-monitor.py` -- standard lifecycle, unaffected.
- `tests/module/monitor-lifecycle.py` -- exercises the systemd wrapper
  end-to-end (timer fires `braid-monitor.service`, which on `rc==1` starts
  `braid-alert.service`). Unaffected by this fix; the wrapper rc==1
  contract is preserved.
- `cli/src/alert.rs` unit tests for `compute_alert_state_with_devid_map`
  (e.g. `unmapped_device_is_error_in_alert`, line 560) -- unchanged; they
  test the alert layer, not `cmd_monitor`.

### Skip a new VM test

The systemd wrapper's "rc==1 -> start braid-alert.service" rule is
unchanged and already exercised by `tests/module/monitor-lifecycle.py`.
The unit tests above lock the function-level contract for the four
rewritten failure sites. A VM-level beep regression test would need to
engineer a scenario where the post-fix code lands on the new helper
rather than the existing `MissingDevice` path -- harder to stage with no
incremental coverage benefit.

## Verification

Sequential checks before committing:

1. `cargo test -p braid-cli monitor` -- new unit tests pass; existing tests
   still pass. (Per `feedback_verify_cargo_package_name.md`, the crate is
   `braid-cli`.)
2. `cargo test -p braid-cli` -- full Rust test suite green.
3. `just test-vm braid-monitor monitor-hot-unplug monitor-lifecycle` --
   both CLI lifecycle tests still pass, and the systemd-wrapper test
   (`tests/module/monitor-lifecycle.py`) confirms the rc==1 ->
   `braid-alert.service` contract is preserved end-to-end.
4. Sanity-check the gate: temporarily revert just the UnmappedDevice arm
   (`return Err(...)` instead of `latch_computation_error`); unit test 1
   must fail. Revert the revert.

## Critical files

- `cli/src/monitor.rs` -- helper + 4 call-site rewrites + delete
  `MonitorError` + new unit tests
- `cli/src/main.rs:52,590-604` -- update clap doc comment, collapse match
- `docs/decisions/014-alerts.md:57-60` -- update exit-code contract
- `cli/src/alert.rs` (read-only reference) -- `AlertCause::ComputationError`,
  `merge_into_latch`, `load_alert_latch`, `save_alert_latch`
- `cli/src/probe.rs` (read-only reference) -- `probe_pool` `CmdRequest`
  sequence (FindmntJson, BtrfsFilesystemShow, CryptsetupStatus,
  CryptsetupLuksUuid)
- `modules/braid/monitor.nix` (no change) -- wrapper rc==1 contract preserved
