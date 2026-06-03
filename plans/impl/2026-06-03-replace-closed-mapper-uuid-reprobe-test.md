# Plan: close the cmd_replace wiring-test gap for the closed-mapper open-boundary UUID re-probe

## Context

`braid replace`'s execute path re-probes the new target's LUKS UUID at the
open boundary so a disk swapped between planning and execution cannot route
pool data into a foreign LUKS volume. On the **ExistingLuks closed-mapper**
arm this is `probe_existing_luks_new_target_uuid` (`cli/src/replace.rs#probe_existing_luks_new_target_uuid`),
called from `cli/src/replace.rs#ReplacePlan::execute` in the
`ReplaceTargetPrep::ExistingLuks` `!mapper_open` branch (currently ~line 746),
just before `ensure_luks_open`.

This guard is **load-bearing and singular** on the closed-mapper path:

- `cli/src/luks.rs#ensure_luks_open` does not independently verify the by-id
  UUID for a closed mapper. `cli/src/luks.rs#classify_mapper_ownership` returns
  `MapperOwnership::Inactive` early (the `CryptsetupStatusOutput::Inactive`
  arm, ~line 888) **without evaluating the UUID closure**, so `ensure_luks_open`
  then runs `CryptsetupLuksOpen` against whatever disk currently sits at the
  by-id. Remove the probe and a swapped foreign volume is opened and handed to
  `btrfs replace start`.
- The pre-journal gate `cli/src/replace.rs#verify_replace_execute_live_pool_uuid`
  does not catch this -- it only rejects the *planned* UUID as a live-pool
  duplicate, not a by-id that no longer holds the planned UUID.

The current coverage is asymmetric:

- The **closed-mapper** probe is only tested by direct helper calls
  (`replace_existing_luks_open_boundary_probe_mismatch_aborts` /
  `_match_continues`). These prove the helper's logic, not its wiring into
  `execute`. A refactor that dropped the call site would leave every test green
  while reopening the foreign-disk hazard.
- The **open-mapper** sibling arm already has a `cmd_replace`-driven wiring test
  (`mapper_name_drift_does_not_skip_open_mapper_verifier`), which drives
  `cmd_replace` end-to-end and asserts `NewTargetUuidMismatchAtOpen`.
- The cloned-header VM test (`tests/cli/replace-cloned-luks-header-rejected.py`)
  exercises only the `mapper_open=true` backing-path arm; the recovery VM test
  (`tests/cli/recover-replace-existing-luks-uuid-mismatch.py`) exercises
  `braid recover`, not the original-run execute path.

**Intended outcome:** one `cmd_replace`-driven Rust unit test that pins the
closed-mapper open-boundary re-probe wiring, closing the asymmetry. No
production code changes -- the guard is correct and well-placed; only the test
coverage is missing.

## Approach: one `cmd_replace`-driven unit test (test-only change)

Add a single `#[test]` to the `mod tests` in `cli/src/replace.rs`. It is a
direct synthesis of two existing, proven tests -- no new test infrastructure:

- **Closed-mapper end-to-end setup** from
  `wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal`
  (fixture, `MockFs` with only the by-id, `.with_mapper_closed("braid-disk3")`).
- **Call-count UUID handler + `NewTargetUuidMismatchAtOpen` assertions** from
  `mapper_name_drift_does_not_skip_open_mapper_verifier`.

### The crux: probe call sequencing

For the closed-mapper, no-keyfile case there are **exactly two**
`CryptsetupLuksUuid` requests against the new by-id
(`/dev/disk/by-id/virtio-disk3`):

| Call index | Origin | UUID the handler returns |
| ---------- | ------ | ------------------------ |
| `0` | planning -- `cli/src/probe.rs#probe_config_disk`, becomes the journaled `new_uuid` | `U_NEW` |
| `1` | execute boundary -- `probe_existing_luks_new_target_uuid` at `replace.rs:746` | `U_FOREIGN` |

Nothing between them re-probes the new by-id: `verify_credential_for_targets`
uses `CryptsetupTestPassphrase`; `verify_replace_execute_live_pool_uuid` →
`probe_pool` probes existing members, not the new by-id; and the closed-mapper
`ensure_luks_open` adds no by-id UUID probe (Inactive short-circuit above).

So the handler keys on the by-id and uses an `Arc<AtomicU32>` counter:
`0 => U_NEW`, `_ => U_FOREIGN` (the `_` catch-all mirrors the sibling test and
stays correct even if an extra probe ever appears).

### What NOT to copy from the open-mapper drift test

- **Do not** override `BtrfsFilesystemShow`. The closed-mapper scenario is a
  normal 2-disk pool (disk1+disk2); the canonical
  `ReplacementPool::two_disk_healthy()` pre-show is correct. (The drift test
  injected a 3-device show only because it models a pool row already named
  `braid-disk3`.)
- **Do not** list `/dev/mapper/braid-disk3` in `MockFs::storage`; the mapper is
  closed. List only `/dev/disk/by-id/virtio-disk3`, matching the
  wrong-passphrase closed-mapper test.
- **Do not** override `CryptsetupTestPassphrase`; the canonical handler returns
  success, so the passphrase preflight passes and execution reaches the journal
  and the probe.

### Required test preamble (Test Conventions)

```rust
// Intent: cmd_replace, on the ExistingLuks closed-mapper path, re-probes the
//   new target's by-id LUKS UUID at the execute-time open boundary and aborts
//   with NewTargetUuidMismatchAtOpen when it no longer matches the UUID
//   captured at planning -- before any CryptsetupLuksOpen or BtrfsReplaceStart
//   touches the disk.
//
// Why it exists: the open-boundary re-probe (probe_existing_luks_new_target_uuid)
//   is the ONLY guard on the closed-mapper path -- ensure_luks_open blindly
//   opens whatever sits at the by-id (classify_mapper_ownership returns Inactive
//   without checking UUID for a closed mapper), and verify_replace_execute_live_pool_uuid
//   only rejects the planned UUID as a live-pool duplicate. Existing coverage
//   calls the helper directly, so dropping the call site would leave tests green
//   while routing pool data into a foreign LUKS volume. The mapper_open=true arm
//   already has a cmd_replace-driven wiring test
//   (mapper_name_drift_does_not_skip_open_mapper_verifier); this closes the
//   matching gap on the closed-mapper arm.
//
// Scenario: operator runs `braid replace --old disk2 --new disk3=<by-id>` where
//   disk3 is already LUKS-formatted with its mapper closed. Between planning
//   (UUID = U_NEW, journaled) and the execute-time open, the by-id slot is
//   swapped to a foreign LUKS volume (UUID = U_FOREIGN, no pool-member
//   collision). The command must abort at the open boundary: journal written,
//   inhibitor held, but no LUKS open and no btrfs replace start.
```

### Skeleton (corrected for the closed-mapper path)

```rust
#[test]
fn cmd_replace_existing_luks_closed_mapper_open_boundary_swap_aborts() {
    let f = PoolFixture::two_disk_healthy();
    // Mapper closed -> only the by-id exists, not /dev/mapper/braid-disk3.
    let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
    let replace_done = Arc::new(AtomicBool::new(false));

    let u_new = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
    let u_foreign = LuksUuid::parse("44444444-4444-4444-4444-444444440746").unwrap();
    let u_new_h = u_new.clone();
    let u_foreign_h = u_foreign.clone();

    let runner = ReplacementPool::two_disk_healthy()
        .with_mapper_closed("braid-disk3")
        .install(MockRunner::default(), replace_done)
        .with_handler({
            let calls = Arc::new(AtomicU32::new(0));
            move |req| match req {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let uuid = if n == 0 { &u_new_h } else { &u_foreign_h };
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    )))
                }
                _ => None,
            }
        });

    let result = cmd_replace(&runner, &fs, &f.replace_params().build());

    // Core safety assertions (each independently fails if the probe is removed):
    match result {
        Err(ReplaceError::NewTargetUuidMismatchAtOpen { by_id, expected, observed }) => {
            assert_eq!(by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
            assert_eq!(expected, u_new);
            assert_eq!(observed, u_foreign.as_str());
        }
        other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
    }
    assert!(
        !runner.requests().iter().any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
        "no CryptsetupLuksOpen may issue on the swap-abort path"
    );
    assert!(
        !runner.requests().iter().any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
        "no BtrfsReplaceStart may issue on the swap-abort path"
    );

    // Boundary-pinning assertions: prove the abort is the POST-journal open
    // boundary, not an earlier preflight (the probe fires after write_journal
    // and after inhibitor acquisition). Distinguishes this from the pre-journal
    // wrong-passphrase abort, which asserts journal None + acquire_count 0.
    assert!(
        journal::load_journal(&f.paths).unwrap().is_some(),
        "journal must be written -- the swap abort is post-journal"
    );
    assert_eq!(
        f.inhibitor.acquire_count(), 1,
        "sleep inhibitor must be acquired once before the open-boundary probe"
    );
}
```

`Arc`, `AtomicBool`, `AtomicU32`, and `Ordering` are already imported/used in
the `mod tests` (the sibling drift test uses them); no new imports needed.

## Out of scope (deliberate)

- **FreshLuks path:** has no open-boundary UUID re-probe by design --
  `cryptsetup luksFormat` writes the journaled UUID directly. Not part of this
  gap.
- **A `cmd_replace`-driven "match continues" test:** redundant -- but *not*
  because an existing test already drives the closed-mapper happy path through
  `replace.rs:746`. None does: the other `with_mapper_closed` tests are either
  FreshLuks (which skips the re-probe by design -- see the comment above
  `ensure_luks_open` at ~`replace.rs:670`), or `.dry_run(true)` planning-only
  previews that never enter `execute`; the post-mount-probe test exercises the
  open-mapper `mapper_open: true` arm (`verify_existing_luks_open_mapper_target`,
  ~`replace.rs:769`); and the `execute_gate_runner` tests build FreshLuks plans
  (`fresh_luks_execute_plan_for_test`). It is redundant instead because (a) the
  existing helper test `replace_existing_luks_open_boundary_probe_match_continues`
  already pins the helper's Ok-on-match, and (b) the new mismatch test already
  pins the call-site arguments (`by_id` and `expected == new_uuid`). The only
  increment a match-continues `cmd_replace` test would add is that `?` on
  `Ok(())` falls through to `ensure_luks_open` -- a language guarantee, not
  behavior worth a test.
- **A VM test:** not recommended. A mid-operation by-id swap cannot be modeled
  deterministically in the VM harness; the `cmd_replace` seam is the correct,
  sufficient altitude. The existing cloned-header VM test already covers the
  integration-level open-mapper backing-path arm.
- **Fixture refresh / formatter runs:** none -- no parser or tool-version
  change; do not run `cargo fmt`.

## Critical files and reused patterns

- **Edit (test-only):** `cli/src/replace.rs` -- add the `#[test]` in `mod tests`,
  ideally adjacent to `mapper_name_drift_does_not_skip_open_mapper_verifier`
  (open-mapper wiring) and the helper tests
  `replace_existing_luks_open_boundary_probe_mismatch_aborts` / `_match_continues`.
- **Reuse, no change:**
  - Fixtures in `cli/src/test_fixtures/replace.rs`: `ReplacementPool::two_disk_healthy`,
    `.with_mapper_closed`, `.install`, `.with_handler`; `PoolFixture::two_disk_healthy`,
    `.replace_params()`, `.paths`, `.inhibitor.acquire_count()`.
  - `mock_ok` (shared test helper), `journal::load_journal`.
  - Precedents to mirror: `wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal`
    (closed-mapper setup) and `mapper_name_drift_does_not_skip_open_mapper_verifier`
    (call-count handler + assertions).
- **Read for context (no change):** `cli/src/luks.rs#ensure_luks_open`,
  `cli/src/luks.rs#classify_mapper_ownership`, `cli/src/probe.rs#probe_config_disk`,
  `cli/src/replace.rs#verify_replace_execute_live_pool_uuid`.

## Verification

1. **Prove it catches the regression (TDD-style):** temporarily comment out the
   `probe_existing_luks_new_target_uuid(runner, &new_by_id, &new_uuid)?;` call in
   `ReplacePlan::execute` (~`replace.rs:746`). Run the new test -- it must FAIL
   (it should now reach `CryptsetupLuksOpen` / return a non-`NewTargetUuidMismatchAtOpen`
   result). Restore the line; the test must pass. This confirms the test guards
   the wiring, not just the helper.
2. **Run the unit suite:** `just test-rust` (the CLI crate is `braid-cli`; this
   recipe runs `cargo test`). Confirm the new test passes and no sibling test
   regresses.
3. No VM tests, fixture capture, or formatter runs are required for this change.
