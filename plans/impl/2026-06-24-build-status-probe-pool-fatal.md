# Plan: pin the fatal half of the `build_status` probe_pool contract

## Context

`build_status` (`cli/src/status.rs#build_status`) has exactly one seam that can
return `Err`: the `probe_pool` match at the top of the function. Every other fault
-- membership load, device usage, df, capacity, config-disk probe, device stats --
deliberately degrades to an advisory and keeps rendering, because `status` is the
always-available read-only diagnostic (`principles.md`: `status` and `doctor`
"stay available").

That one seam is asymmetric *on purpose*:

- `ProbeError::NotBtrfs` -> soft. The `NotBtrfs` arm returns
  `Ok(not_mounted_status(...))` with the foreign fstype surfaced as an advisory.
- **every other** `probe_pool` error -> fatal. The catch-all arm is
  `Err(e) => return Err(e.into())`. The rationale: if `probe_pool` fails for any
  reason other than "not btrfs", the core pool data is untrustworthy, so the whole
  report must abort rather than render a blank/misleading body.

Three of the four quadrants of this contract are pinned by existing tests; the
fourth is not:

| seam | soft (advisory, Ok) | fatal (Err) |
|------|--------------------|-------------|
| `probe_pool` | `build_status_not_btrfs_surfaces_fstype_advisory` | **UNTESTED** |
| `probe_config_disk` | `status_surfaces_mapper_conflict` | n/a (config faults are never fatal) |

A grep for `unwrap_err|expect_err|is_err|assert.*Err(` across `cli/src/status.rs`
returns nothing: **no test asserts `build_status` returns `Err` at all.** A future
refactor that unified the two probe-handling styles -- swallowing a core
`probe_pool` error into a blank report, or conversely making a config-disk fault
fatal -- would pass the entire current suite. This change locks the missing
quadrant so the asymmetry is legible and regression-guarded.

This is a **test-only change**: no production code is touched. The behavior already
exists in the catch-all arm of `cli/src/status.rs#build_status`; the test is a
characterization/regression test that passes green on first run against current
code.

## Change

Add one `#[test]` in the `cli/src/status.rs` test module that seeds `probe_pool`
to return a non-`NotBtrfs` error and asserts `build_status` returns `Err`.

### How `probe_pool` is driven to a fatal error

`probe_pool` (`cli/src/probe.rs#probe_pool`) returns
`ProbeError::PoolDevice { detail: "not active" }` when a pool mapper's
`cryptsetup status` reports inactive. This is the simplest non-`NotBtrfs` fault to
seed and already has a probe-layer precedent in `probe_pool_mapper_not_active`.

Execution path inside `build_status` for this seed:
1. `fs` reports btrfs at the mount -> past the `NotBtrfs` arm.
2. `BtrfsFilesystemShow` mock -> a 1-disk pool whose device is `/dev/mapper/disk1`.
3. `probe_pool` calls `CryptsetupStatus` for `disk1`; the inactive mock parses to
   `CryptsetupStatusOutput::Inactive` -> `Err(PoolDevice { detail: "not active" })`.
4. `build_status` propagates it as `StatusError::Probe(...)` through the catch-all
   arm. The function returns **before**
   `load_membership`/`assemble_advisories`/config-disk probing, so no further setup
   is required.

### Test body (reuse existing fixtures)

All fixtures already exist -- no new helpers needed:

- `cli/src/test_fixtures/status.rs#status_fs_one_disk` -- btrfs mount at
  `/mnt/storage` exposing `/dev/mapper/disk1`. Gets past the `NotBtrfs` arm.
- `cli/src/test_fixtures/status.rs#status_config`, `status_mp`, `isolated_paths`
  -- standard scaffolding.
- `cli/src/test_fixtures/status.rs#status_btrfs_show_1disk` -- show output whose
  sole device path is `/dev/mapper/disk1`.
- `status_err_raw(cmd, exit_code, stderr)` -- the `err_raw` fixture re-exported
  under that alias into the `cli/src/status.rs` test module. Use
  `status_err_raw("cryptsetup status disk1", 4, "/dev/mapper/disk1 is not active.\n")`,
  mirroring the inactive fixture in `probe_pool_mapper_not_active`.
- `crate::test_fixtures::mock_virtio_backing_path_resolver()` -- required by the
  `build_status` signature but never consulted (probe_pool aborts first); same
  resolver the NotBtrfs test passes.

Runner needs exactly **two** mocks (`BtrfsFilesystemShow` + inactive
`CryptsetupStatus { mapper: "disk1" }`). No membership save, no df/usage/scrub/stats
mocks -- they are unreachable.

### Assertion

Pin the fatal classification at the right altitude -- it propagated as a probe
fault, not the soft `Ok(NotMounted)` path:

```rust
let result = build_status(
    &runner,
    &fs,
    &config,
    &paths,
    crate::test_fixtures::mock_virtio_backing_path_resolver(),
);
match result {
    Err(StatusError::Probe(ProbeError::PoolDevice { detail, .. })) => {
        assert_eq!(detail, "not active");
    }
    Ok(_) => panic!(
        "a non-NotBtrfs probe_pool fault must abort build_status, but it returned Ok"
    ),
    Err(other) => panic!("expected StatusError::Probe(PoolDevice), got: {other}"),
}
```

**Use a `match`, not `assert!(matches!(...), "...{result:?}")`.** `BuiltStatus`
(`cli/src/status.rs#BuiltStatus`) derives no `Debug`, so neither the `Result` nor
an `Ok` body can be `{:?}`-formatted -- a `{result:?}` panic argument would not
compile. `StatusError` (`cli/src/status.rs#StatusError`) derives `Debug` and gets
`Display` from `thiserror`, so the wrong-`Err` arm prints the error with `{other}`
(Display) and the `Ok(_)` arm prints nothing from the body. The
`PoolDevice`/`detail` arm confirms three things at once: it is an `Err`, it came
through the probe seam, and it is the inactive-mapper class we seeded.

### Placement and naming

Place the new test immediately after
`build_status_not_btrfs_surfaces_fstype_advisory`. Both exercise the *same*
`probe_pool` match -- the soft `NotBtrfs` arm and the fatal catch-all arm -- so the
two halves read side by side.

Suggested name: `build_status_probe_pool_fault_is_fatal`.

### Preamble (project convention: Intent / Why it exists / Scenario)

The preamble must make the contract a legible matched pair. It should:
- **Intent:** a non-`NotBtrfs` `probe_pool` error aborts `build_status` with `Err`
  (the report is untrustworthy), unlike the soft paths.
- **Why it exists:** name both soft siblings explicitly --
  `build_status_not_btrfs_surfaces_fstype_advisory` (the other arm of this match)
  and `status_surfaces_mapper_conflict` (the config-disk seam, which is *never*
  fatal). State the invariant: `probe_pool` faults are fatal because core pool
  data cannot be trusted; config-disk faults degrade to per-disk advisories. A
  refactor that unified the two styles would flip one of these and otherwise pass
  the suite.
- **Scenario:** an inconsistent/stale mapper -- mountinfo and `btrfs filesystem
  show` still identify the pool (so the probe clears `NotBtrfs` and enumerates
  `/dev/mapper/disk1`), while `cryptsetup status /dev/mapper/disk1` reports the
  mapper *inactive* (closed/torn down out from under the still-mounted pool).
  Note: this is **not** the hot-unplug case -- a vanished *backing* device leaves
  the mapper active with `device: (null)`, which `cli/src/probe.rs#probe_pool`
  routes to the soft `null_underlying` path (pinned by
  `probe_pool_device_null_underlying`), not to the fatal `PoolDevice` arm.

## Scope discipline (do NOT over-build)

- **One representative variant is correct.** The catch-all arm in
  `cli/src/status.rs#build_status` is literal (`Err(e) => ...`), so a single
  non-`NotBtrfs` variant exercises the entire fatal contract. Do **not** enumerate
  `Cmd`/`Parse`/`MountInfo` cases -- they re-test the same arm. (This is the
  opposite of `config_probe_advisory_names_disk`, which enumerates one-per-variant
  only because that path formats each variant differently. That precedent does not
  apply here.)
- Test-only. No change to `build_status`, `probe_pool`, or any fixture helper.

## Verification

1. **Green on current code** (regression test pins existing behavior):
   ```
   just test-rust
   ```
   or targeted: `cargo test --manifest-path cli/Cargo.toml build_status_probe_pool_fault_is_fatal`
2. **Prove it has teeth (mutation check, do not commit):** temporarily change the
   catch-all arm in `cli/src/status.rs#build_status` from
   `Err(e) => return Err(e.into())` to the soft form
   `Err(e) => return Ok(not_mounted_status(config, paths, assemble_advisories(paths, Some(e.to_string()))))`
   and confirm the new test now **fails** (while
   `build_status_not_btrfs_surfaces_fstype_advisory` still passes). Revert the
   mutation.
3. No fixture-refresh implications (no parser/`nixpkgs` change), no docs change
   (behavior is unchanged; only test coverage is added).
