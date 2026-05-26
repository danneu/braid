# Gate `metadata_enospc_pressure` on a degraded pool

## Context

`check_metadata_enospc_pressure` (`cli/src/doctor.rs:935-1020`) recommends
`btrfs balance start -dusage=50` whenever metadata utilization is high and
per-device headroom is tight. It never consults pool state, so on a **mounted
degraded pool** (a device is missing -- reachable via `braid unlock
--allow-degraded`, see `cli/src/mount.rs:418-422`) it tells the operator to
start a data balance.

That is the wrong order on a degraded pool. Chunk allocation during degraded
operation falls back to **single-profile** chunks
(`reference/linux/fs/btrfs/block-group.c:3977-3989`; btrfs warns against it in
`reference/btrfs-progs/Documentation/Balance.rst:156-171`), widening the
recovery surface. braid's invariant is **replace-first, then balance**
(`docs/design/decisions/001-btrfs-raid1.md:47-48`, `docs/design/principles.md:21`).

Every sibling check that touches pool state already honors this: `check_enospc_risk`
skips on degraded (`doctor.rs:808-810`), the profile-mismatch checks reword to
"replace missing device(s) first" (`doctor.rs:726-728`). This check is the lone
gap. Blast radius is small -- advisory text only, no mutation -- hence Low, but
it actively contradicts a documented invariant.

**Outcome:** on a degraded pool, the check defers to the replace-first path
instead of recommending a degraded balance, matching `check_enospc_risk`.

## Decision: skip on degraded (match `check_enospc_risk`)

Return `Skip` with the exact message `check_enospc_risk` uses --
`"skipped (pool is degraded)"`. Rationale: replace-first is the only correct
next action and `check_pool_missing_devices` already emits the concrete
`braid replace` command; a reworded warning would be redundant with 1-3 other
checks already saying "replace." Skip defers rather than suppresses -- the
metadata warning resurfaces (now with a safe balance recommendation) on the
next `braid doctor` run once the device is replaced and the pool is no longer
degraded.

### Gate placement: inside the warn condition, not early

Place the gate at the top of the existing
`if metadata_ratio > METADATA_PRESSURE_RATIO && with_headroom < 2` block
(`doctor.rs:1008`) -- i.e. at the exact moment we are about to recommend the
balance -- **not** early like `check_enospc_risk`. Two reasons:

- **Semantic precision.** Degraded-ness only matters when we would otherwise
  recommend the dangerous balance. A degraded pool with healthy metadata
  headroom has nothing to warn about and correctly returns `Ok` (this chosen
  semantics is pinned by the test in edit #5 below, so a future move to an early
  skip would fail loudly).
- **Surgical test ripple.** Only the two warn-path tests reach the gate; all
  Ok-path and error-path tests return earlier and stay green untouched. Early
  placement would force ~9 existing tests to seed pool state.

## Production change

`cli/src/doctor.rs`, inside `check_metadata_enospc_pressure`, at the top of the
`if metadata_ratio > METADATA_PRESSURE_RATIO && with_headroom < 2` block
(before constructing the warn at `:1009`):

```rust
if metadata_ratio > METADATA_PRESSURE_RATIO && with_headroom < 2 {
    // About to recommend a data balance. That is the wrong move on a degraded
    // pool: chunk allocation during degraded operation falls back to
    // single-profile chunks (reference/linux/fs/btrfs/block-group.c), widening
    // the recovery surface. braid's invariant is replace-first, then balance
    // (docs/design/decisions/001-btrfs-raid1.md). Defer to the replace-first
    // path that check_pool_missing_devices already surfaces.
    ensure_pool_state(ctx);
    match ctx
        .pool_state
        .as_ref()
        .expect("ensure_pool_state seeds the cache when config is present and mounted")
    {
        Err(e) => {
            return CheckResult::warn(
                NAME,
                format!("could not probe pool state -- metadata pressure indeterminate: {e}"),
            );
        }
        Ok(pool) if pool.missing_count > 0 => {
            return CheckResult::skip(NAME, "skipped (pool is degraded)");
        }
        Ok(_) => {}
    }

    let pct = (metadata_ratio * 100.0).round() as u64;
    // ... existing warn unchanged ...
}
```

Notes:
- `ensure_pool_state` / `.expect(...)` are safe here: config-present and
  mounted are already checked at the top of the function, which is the
  invariant the `.expect` documents (identical to `check_enospc_risk:796-800`).
- Borrow-safe: `usage` (the `&ctx.device_usage` borrow) is last used computing
  `n_devices`/`with_headroom` at `:1001-1006`, before this block, so the
  `&mut ctx` in `ensure_pool_state` does not conflict.
- The `Err` arm is the **fail-closed** behavior: if pool state is
  indeterminate we cannot confirm the pool is healthy, so we suppress the
  balance recommendation rather than emit it. Mirrors `check_enospc_risk:802-806`.

Reuses existing helpers: `ensure_pool_state` (`doctor.rs:654`), `PoolState`
fields `missing_count: u64` (`cli/src/types.rs:425`).

## Test changes (`cli/src/doctor.rs` `#[cfg(test)]` module)

All existing Ok-path and error-path metadata tests are unaffected (they return
before the gate). The edits add two coverage types: end-to-end probe tests
(#1-#3, fresh context driven through `ensure_pool_state` -> `probe_pool`) and
deterministic gate-unit tests (#4-#5, pre-cached `pool_state`).

1. **New helper** mirroring `metadata_pressure_result` but seeding pool state
   via the existing `pool_state_runner` (`cli/src/test_fixtures/doctor.rs:290`)
   and the `DoctorMockFs::mounted_btrfs_only()` / `for_test_parsed_with_fs`
   setup used by `enospc_risk_degraded_pool_skips` (`doctor.rs:4235-4246`):

   ```rust
   fn metadata_pressure_result_with_pool(
       df: &str,
       usage: &str,
       present: Vec<(&'static str, u64, &'static str, LuksUuid)>,
       missing_devids: &[u64],
   ) -> CheckResult {
       let (df_req, df_out) = df_json(df);
       let (usage_req, usage_out) = device_usage_raw(usage);
       let runner = pool_state_runner(present, missing_devids)
           .with_output(df_req, df_out)
           .with_output(usage_req, usage_out);
       let (_dir, paths) = isolated_paths();
       let fs = DoctorMockFs::mounted_btrfs_only();
       let mut ctx =
           DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json());
       check_metadata_enospc_pressure(&mut ctx)
   }
   ```

   (`pool_state_runner` already wires `mountpoint_ok`; do not re-add it.)

2. **Repoint the two warn-path tests** to the new helper with `&[]` (no missing
   devids) so probing succeeds, `missing_count == 0`, the gate falls through,
   and the existing assertions on the warn text still hold:
   - `metadata_pressure_two_device_pool_warns_when_both_signals_present`
     (`:4322`) -- 2 present devices, `DF_METADATA_78_USED` + `DEVICE_USAGE_TWO_TIGHT`.
   - `metadata_pressure_three_device_pool_two_tight_warns` (`:4374`) -- 3
     present devices, `DF_METADATA_78_USED` + `DEVICE_USAGE_THREE_TWO_TIGHT`.

3. **New regression test** directly mirroring `enospc_risk_degraded_pool_skips`,
   with the Intent/Why/Scenario preamble (per AGENTS.md Test Conventions):

   ```rust
   // Intent: metadata_enospc_pressure skips a degraded pool instead of
   //   recommending a data balance.
   // Why it exists: a balance on a degraded RAID1 pool allocates single-profile
   //   chunks and widens the recovery surface; braid's invariant is replace-first,
   //   then balance (docs/design/principles.md, 001-btrfs-raid1.md). Pins parity
   //   with check_enospc_risk's degraded skip.
   // Scenario: btrfs reports one MISSING devid while metadata is 78% used and
   //   both members are tight on unallocated space -- the exact state that would
   //   otherwise emit the `btrfs balance start -dusage=50` recommendation.
   #[test]
   fn metadata_pressure_degraded_pool_skips() {
       let check = metadata_pressure_result_with_pool(
           DF_METADATA_78_USED,
           DEVICE_USAGE_TWO_TIGHT,
           vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))],
           &[2],
       );
       assert_eq!(check.status, CheckStatus::Skip);
       assert_eq!(check.message, "skipped (pool is degraded)");
   }
   ```

4. **New pre-cache helper + fail-closed (`Err`) regression test.** The repointed
   warn tests (#2) and degraded-skip test (#3) only exercise *healthy* and
   *degraded* probe results, so a regression that fell through on `Err` and
   re-emitted the balance would still pass. Add a helper that seeds
   `ctx.pool_state` directly to pin the gate's branch behavior without coupling
   to `probe_pool`'s internal command sequence:

   ```rust
   fn metadata_pressure_with_cached_pool_state(
       df: &str,
       usage: &str,
       pool_state: Result<PoolState, ProbeError>,
   ) -> CheckResult {
       let (mp_req, mp_out) = mountpoint_ok();
       let (df_req, df_out) = df_json(df);
       let (usage_req, usage_out) = device_usage_raw(usage);
       let runner = MockRunner::default()
           .with_output(mp_req, mp_out)
           .with_output(df_req, df_out)
           .with_output(usage_req, usage_out);
       let (_dir, paths) = isolated_paths();
       let mut ctx = parsed_doctor_ctx(&runner, &paths);
       ctx.pool_state = Some(pool_state); // ensure_pool_state short-circuits on is_some()
       check_metadata_enospc_pressure(&mut ctx)
   }
   ```

   ```rust
   // Intent: metadata_enospc_pressure fails closed when pool state is
   //   indeterminate -- it must not emit the balance recommendation.
   // Why it exists: the degraded gate's whole point is to never recommend a
   //   degraded balance; if probing the pool fails we cannot confirm the pool is
   //   healthy, so the unsafe `btrfs balance start -dusage=50` text must be
   //   suppressed. The healthy/degraded tests would not catch a fall-through here.
   // Scenario: metadata is 78% used with both members tight (the warn condition),
   //   but the pool probe errored.
   #[test]
   fn metadata_pressure_indeterminate_pool_state_warns_without_balance() {
       let check = metadata_pressure_with_cached_pool_state(
           DF_METADATA_78_USED,
           DEVICE_USAGE_TWO_TIGHT,
           Err(ProbeError::PoolDevice {
               mapper: "braid-disk1".to_owned(),
               detail: "simulated probe failure".to_owned(),
           }),
       );

       assert_eq!(check.status, CheckStatus::Warn);
       assert!(
           check.message.contains("metadata pressure indeterminate"),
           "expected fail-closed probe warning: {}",
           check.message
       );
       assert!(
           !check.message.contains("btrfs balance start"),
           "must not recommend a balance when pool state is unknown: {}",
           check.message
       );
   }
   ```

5. **New "degraded but not pressured returns Ok" regression test.** Pins the
   late-gate semantics from the placement decision: a degraded pool with no
   metadata pressure returns `Ok` (the gate is never reached), not an early skip.
   Guards against a future move to early-gate placement.

   ```rust
   // Intent: metadata_enospc_pressure returns Ok on a degraded pool when there
   //   is no metadata pressure -- it does not early-skip like check_enospc_risk.
   // Why it exists: the gate is placed inside the warn condition by design
   //   (degraded-ness only matters when about to recommend a balance). An
   //   accidental move to an early degraded skip would flip this to Skip.
   // Scenario: one devid is MISSING, but metadata is only 20% used and both
   //   members have ample unallocated space -- nothing to warn about.
   #[test]
   fn metadata_pressure_degraded_but_no_pressure_returns_ok() {
       let check = metadata_pressure_with_cached_pool_state(
           DF_METADATA_20_USED,
           DEVICE_USAGE_TWO_HEALTHY,
           Ok(PoolState {
               mounted: true,
               devices: vec![],
               missing_count: 1,
               missing_devids: vec![2],
               total_devices: 2,
               fsid: None,
               null_underlying: vec![],
           }),
       );

       assert_eq!(check.status, CheckStatus::Ok);
       assert!(
           check.message.contains("within bounds"),
           "degraded pool without pressure must report Ok: {}",
           check.message
       );
   }
   ```

   Edits #4-#5 pre-cache `ctx.pool_state` rather than driving `probe_pool`, which
   keeps them deterministic and structure-insensitive (they do not encode
   `probe_pool`'s internal command order). The real `ensure_pool_state` -> probe
   -> gate path on a fresh context is already exercised by the repointed warn
   tests (#2) and the degraded-skip test (#3), so this does not lose the
   "gate actually calls `ensure_pool_state`" coverage. `PoolState` (pub fields,
   `cli/src/types.rs:420`) and `ProbeError::PoolDevice` (`cli/src/probe.rs:64`)
   are constructed inline, matching existing `PoolState { .. }` literals in
   `status.rs` / `preflight.rs`.

## Considered and rejected

- **Reword to replace-first (keep Warn).** Redundant with `check_pool_missing_devices`
  and the profile-mismatch checks already saying "replace" on the same run;
  inconsistent with `check_enospc_risk`'s skip. Skip is cleaner and loses nothing
  (warning resurfaces post-replace).
- **Early gate placement (mirror `check_enospc_risk` structure exactly).**
  Consistent skip text on every degraded run, but forces ~9 existing tests to
  seed pool state and makes the check probe pool state even when there is no
  pressure to act on. Rejected for the surgical in-branch placement.
- **Extract a shared degraded-gate helper across the 4 checks.** The three
  existing checks gate *early* and do different things (skip vs reword); this one
  gates *late* inside a condition. Unifying across those mismatched placements
  adds indirection without removing much. Disproportionate for a Low finding.

## Verification

Pure Rust unit-test logic -- no systemd/mount/module change, so **no VM tests
needed**.

1. `just test-rust` -- runs `cargo test` for `braid-cli`, covering the doctor
   test module.
2. Confirm the targeted tests pass: the three new tests
   (`metadata_pressure_degraded_pool_skips`,
   `metadata_pressure_indeterminate_pool_state_warns_without_balance`,
   `metadata_pressure_degraded_but_no_pressure_returns_ok`), the two repointed
   warn tests, and the untouched Ok/error-path metadata tests (`:4268`-`:4511`)
   plus `metadata_pressure_registered_with_human_label` (`:4519`) remain green.

Expected result: the only behavioral change is that a degraded mounted pool now
yields `Skip "skipped (pool is degraded)"` from `metadata_enospc_pressure`
instead of a balance recommendation.
