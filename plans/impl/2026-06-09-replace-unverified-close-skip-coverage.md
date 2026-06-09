# Plan: execute-level coverage for the live-replace `Unverified` close-skip arm

## Context

A verified Testing-category finding: the post-commit old-mapper close-skip on a
UUID mismatch in live `replace` -- the `MapperOwnership::Unverified => {}` arm of
`ReplacePlan::execute` in `cli/src/replace.rs` (around line 941) -- has **no
execute-level test**. The two tests that advertise it,
`replace_post_commit_close_probe_mismatch_skips_close` and
`replace_post_commit_close_probe_match_allows_close` (around lines 7275/7330),
only call the probe helper `probe_observed_mapper_uuid` directly. Their
`close_count == 0` assertion is trivially satisfied because the helper never
issues a close; if the arm regressed to `Unverified => { close it }` -- the exact
double-drift hazard the guard exists to prevent (tearing down a foreign disk's dm
slot after a swap-in-place) -- both tests would still pass.

The same `Unverified => {}` skip-close arm exists at three call sites of the
shared helper (`cli/src/probe_mapper_uuid.rs#probe_observed_mapper_uuid`):
`replace.rs`, `remove.rs`, `recover.rs`. Investigation confirmed the two siblings
already pin this at execute level:

- `remove.rs#post_commit_close_uuid_probe_demotes_to_skip_on_mismatch` drives
  `cmd_remove` with `braid-WRONG` backed by a foreign UUID and asserts zero close.
- `recover.rs#recover_replace_old_close_foreign_mapper_warns_and_skips` drives
  `execute_replace_post_maintenance_recovery` the same way.

Live `replace` is the lone gap. It already has execute-level coverage for the
*Inactive* arm (`live_replace_old_close_inactive_warns_and_skips_close`) and the
*Owned* arm (`close_runs_before_resize_on_live_replace`,
`live_replace_old_close_failure_emits_warn_row`,
`live_replace_old_retries_on_busy_then_succeeds`) -- only the *Unverified* arm is
untested at execute level.

**Outcome:** bring live `replace` to parity with its siblings, and relocate the
helper's value-mismatch coverage to where the helper's other arm-tests live, so no
test in the tree advertises caller behavior it never exercises. This is
**test-only work**; the production arm at `replace.rs` line 941 stays
`Unverified => {}` unchanged.

## Scope (full cleanup)

- **A.** Add an execute-level test in `cli/src/replace.rs` that drives
  `cmd_replace` into the live `Unverified` close-skip arm.
- **B.** Add the missing value-mismatch unit test to
  `cli/src/probe_mapper_uuid.rs` (the one `Unverified` branch its 6 existing
  arm-tests don't cover).
- **C.** Delete the two overpromising helper tests in `cli/src/replace.rs`. Their
  genuine coverage is fully relocated by A and B; the `Owned`-allows-close control
  is already covered by three `cmd_replace`-driving tests.

No production code changes. No changes to `remove.rs` / `recover.rs` (already at
parity). No new imports in either test module -- every symbol used below is
already in scope for the neighboring tests.

## A. New execute-level test in `cli/src/replace.rs`

Add beside `live_replace_old_close_inactive_warns_and_skips_close` (whose harness
this mirrors exactly). Key mechanic, verified against the fixtures: in a live
disk2->disk3 replace, `braid-disk2`'s mapper status + backing UUID are probed
**only** at the post-commit close (`probe_observed_mapper_uuid` at `replace.rs`
~line 924); planning and the pre-journal seam never touch them. So the surprising
response is gated on the `replace_done` flag (set when `BtrfsReplaceStart` fires),
exactly like the Inactive sibling -- pre-commit falls through to the healthy pool
defaults. The foreign backing path `/dev/vdf` is unused by the two-disk topology
(vdb=disk1, vdc=disk2, vdd=disk3), so the probe count is unambiguous. The
journaled old UUID for disk2 is `22222222-2222-2222-2222-222222222222`
(`test_fixtures/shared.rs#PoolFixture::two_disk_healthy`). `with_handler` closures
registered after `.install()` win (reverse-order, first `Some` -- `cmd.rs`
dispatch), so the per-test arms override the pool defaults.

```rust
// Intent: live replace warns and SKIPS the old-mapper close when the post-commit
//   UUID probe finds braid-disk2's mapper now backs a FOREIGN LUKS volume
//   (operator double-drift: a different disk opened under the same mapper name
//   between plan and the post-commit close).
// Why it exists: the MapperOwnership::Unverified arm in ReplacePlan::execute is
//   the guard against tearing down a foreign disk's dm slot after a swap-in-place.
//   Until now only the probe helper was unit-tested, so the arm could regress to
//   `Unverified => { close it }` with every test still green. remove and recover
//   already pin this at execute level
//   (post_commit_close_uuid_probe_demotes_to_skip_on_mismatch,
//   recover_replace_old_close_foreign_mapper_warns_and_skips); this brings live
//   replace to parity.
// Scenario: live replace of disk2 -> disk3 commits; afterwards
//   `cryptsetup status braid-disk2` resolves to a foreign backing /dev/vdf whose
//   LUKS UUID is U_FOREIGN != the journaled 2222...2222.
#[test]
fn live_replace_old_close_foreign_mapper_warns_and_skips_close() {
    let f = PoolFixture::two_disk_healthy();
    let fs = MockFs::storage(vec![
        "/dev/disk/by-id/virtio-disk3".into(),
        "/dev/mapper/braid-disk3".into(),
    ]);
    let replace_done = Arc::new(AtomicBool::new(false));
    let runner = ReplacementPool::two_disk_healthy()
        .install(MockRunner::default(), replace_done.clone())
        .with_handler({
            let replace_done = replace_done.clone();
            move |req| match req {
                CmdRequest::BtrfsReplaceStart { .. } => {
                    replace_done.store(true, Ordering::Relaxed);
                    Some(Ok(mock_ok("btrfs replace start", "")))
                }
                // Post-commit: braid-disk2's mapper now backs a foreign disk.
                CmdRequest::CryptsetupStatus { mapper }
                    if mapper.as_str() == "braid-disk2"
                        && replace_done.load(Ordering::Relaxed) =>
                {
                    Some(Ok(mock_ok(
                        "cryptsetup status braid-disk2",
                        "braid-disk2 is active and is in use.\n  type:    LUKS2\n  \
                         device:  /dev/vdf\n  mode:    read/write\n",
                    )))
                }
                CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf" => {
                    Some(Ok(mock_ok(
                        "cryptsetup luksUUID /dev/vdf",
                        "99999999-9999-9999-9999-999999999999\n",
                    )))
                }
                // A regressed arm would issue this; answer it so the regression
                // fails on the assertion below, not on a dispatch error.
                CmdRequest::CryptsetupClose { .. } => {
                    Some(Ok(mock_ok("cryptsetup close", "")))
                }
                CmdRequest::BtrfsFilesystemResize { .. } => {
                    Some(Ok(mock_ok("btrfs filesystem resize", "")))
                }
                _ => None,
            }
        });

    let captured = crate::status_tag::testing::capture_with_color(false, || {
        cmd_replace(&runner, &fs, &f.replace_params().build())
            .expect("foreign old-mapper close skip must not fail replace");
    });

    let requests = runner.requests();
    // Core invariant: the foreign mapper is never closed.
    assert!(
        !requests.iter().any(|r| matches!(
            r,
            CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2"
        )),
        "foreign old-mapper probe must skip close: {requests:?}"
    );
    // The post-commit probe actually ran against the foreign backing.
    let foreign_probes = requests
        .iter()
        .filter(|r| matches!(r, CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf"))
        .count();
    assert_eq!(
        foreign_probes, 1,
        "exactly one post-commit UUID probe against braid-disk2's foreign backing"
    );
    // Execution continues past the skip -- maintenance still replays.
    assert!(
        requests
            .iter()
            .any(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. })),
        "resize must still replay after the foreign close skip: {requests:?}"
    );
    // Operator-facing warning names both UUIDs (emitted inside the probe helper).
    assert!(
        captured.contains(
            "Warning: post-commit close skipped for mapper braid-disk2: \
             expected LUKS UUID 22222222-2222-2222-2222-222222222222 \
             but observed 99999999-9999-9999-9999-999999999999\n"
        ),
        "foreign close skip must warn with both UUIDs: {captured:?}"
    );
}
```

## B. New helper unit test in `cli/src/probe_mapper_uuid.rs`

The helper's 6 existing `probe_returns_*` tests cover the Unverified branches for
status-runner-error, status-parse-fail, null-backing, luksUUID-runner-error,
luksUUID-parse-fail, plus the Inactive branch -- but **not** the value-mismatch
branch (`probe_mapper_uuid.rs` ~line 112: parsed cleanly, value differs). That is
the operator double-drift arm the whole guard exists for. Add it to the helper's
`tests` module, modeled on `probe_returns_unverified_when_luks_uuid_parse_fails`:

```rust
// Intent: an active mapper whose backing luksUUID parses cleanly but differs
//   from the expected UUID makes the close probe return Unverified, after both
//   probes run.
// Why it exists: this is the operator double-drift arm -- a foreign disk opened
//   under the same mapper name reports a valid-but-wrong UUID. It is the one
//   Unverified branch the other helper tests don't cover, and the close-skip
//   guard at every call site (replace/remove/recover) hinges on it.
// Scenario: status resolves braid-WRONG to /dev/vdc; `cryptsetup luksUUID
//   /dev/vdc` returns a valid foreign UUID != the expected UUID.
#[test]
fn probe_returns_unverified_when_uuid_value_differs() {
    let mapper = MapperName("braid-WRONG".into());
    let expected = test_uuid(716);
    let foreign = test_uuid(799);
    let runner = MockRunner::default()
        .with_output(
            CmdRequest::CryptsetupStatus { mapper: mapper.clone() },
            mock_ok(
                "cryptsetup status braid-WRONG",
                "braid-WRONG is active and is in use.\n  type:    LUKS2\n  \
                 device:  /dev/vdc\n  mode:    read/write\n",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid { device: "/dev/vdc".into() },
            mock_ok("cryptsetup luksUUID /dev/vdc", &format!("{foreign}\n")),
        );

    let ownership = probe_observed_mapper_uuid(&runner, &mapper, &expected);

    assert_eq!(
        ownership,
        MapperOwnership::Unverified,
        "a valid-but-different backing UUID must signal skip-close"
    );
    assert_eq!(
        runner.requests(),
        vec![
            CmdRequest::CryptsetupStatus { mapper: mapper.clone() },
            CmdRequest::CryptsetupLuksUuid { device: "/dev/vdc".into() },
        ],
        "value mismatch must run exactly one status probe and one UUID probe"
    );
}
```

## C. Delete the two overpromising helper tests in `cli/src/replace.rs`

Remove `replace_post_commit_close_probe_mismatch_skips_close` (Seed 620) and
`replace_post_commit_close_probe_match_allows_close` (Seed 621) -- the block
spanning roughly lines 7261-7349.

Coverage accounting (nothing lost):
- value-mismatch helper arm -> now in `probe_mapper_uuid.rs` (B), beside its
  siblings;
- execute-path Unverified skip -> now in `replace.rs` (A);
- `Owned`-allows-close control -> already covered at execute level by
  `close_runs_before_resize_on_live_replace`,
  `live_replace_old_close_failure_emits_warn_row`, and
  `live_replace_old_retries_on_busy_then_succeeds`.

**Keep** the shared builders `runner_with_active_mapper_uuid` and
`runner_with_luks_uuid_probe` -- they are still used by the open-boundary
re-probe tests (`runner_with_active_mapper_uuid` by three tests around lines
7501/7563/7671; `runner_with_luks_uuid_probe` by the two open-boundary tests
`replace_existing_luks_open_boundary_probe_mismatch_aborts` and
`replace_existing_luks_open_boundary_probe_match_continues`). Only the two test
fns are deleted, not the helpers.

**Fix the stale doc comment** on `runner_with_luks_uuid_probe`. Its `///`
currently reads "Used by the open-boundary re-probe and post-commit close
double-drift regression tests." That helper is in fact only used by the two
open-boundary tests above -- the deleted Seed 620/621 tests build their runner via
`runner_with_active_mapper_uuid`, not this helper -- so the "post-commit close
double-drift" clause is already a misnomer and is unambiguously wrong once Seed
620/621 are gone. Narrow it to the open-boundary use:

```rust
    /// Build a recording `MockRunner` that injects a `CryptsetupLuksUuid`
    /// probe response for `device`, returning the supplied canned
    /// `RawCommandOutput`. Used by the open-boundary re-probe tests
    /// (`replace_existing_luks_open_boundary_probe_*`).
```

No other comment references the deleted tests: a sweep for `Seed 620` / `Seed
621` and the deleted fn names finds hits only inside the deleted block and this
helper comment. `runner_with_active_mapper_uuid` carries no doc comment, so there
is no second comment to update.

## Verification

1. **TDD confirm-fail (do first, then revert).** Temporarily change the
   production arm in `replace.rs` from `Unverified => {}` to mirror the Owned arm:
   `Unverified => { close_mapper_best_effort(runner, params.sleeper, mapper, old_label, color_enabled); }`.
   Run `cargo test --lib live_replace_old_close_foreign_mapper_warns_and_skips_close`
   and confirm it FAILS on the "must skip close" assertion (the right reason).
   Revert the production edit.
2. **Run the new tests green:**
   `cargo test --lib live_replace_old_close_foreign_mapper_warns_and_skips_close`
   and `cargo test --lib probe_returns_unverified_when_uuid_value_differs`.
3. **Full Rust suite:** `just test-rust` (the project uses `cargo test --lib`, not
   nextest; the crate is `braid-cli`).
4. **ASCII lint:** `scripts/docs/check-output-ascii.py` exempts test code, so the
   new tests need no change there; `just test-rust` is the gate.

## Out of scope / deliberately unchanged

- Production behavior at `replace.rs` line 941 (`Unverified => {}`) -- stays.
- The three `Unverified => {}` match blocks are **not** unified: `remove`/`replace`
  warn on `Inactive` while `recover` is silent (already-closed is normal
  post-crash state). That divergence is intentional; collapsing it would be a
  regression.
- `remove.rs` / `recover.rs` tests -- already at execute-level parity.
