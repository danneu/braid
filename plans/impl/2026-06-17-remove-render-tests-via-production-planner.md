# Plan: route remove render tests through the production planner

## Context

`cli/src/remove.rs` carries a `#[cfg(test)]` helper,
`remove_present_work_plan_for_test` (remove.rs:828-848), that is a **second
construction path** for `RemoveWorkPlan`. It re-implements production
`plan_remove`'s target-by-UUID lookup
(`pool.devices.iter().find(|d| d.luks_uuid == target_uuid)`, a verbatim copy
of remove.rs:556) and then calls the same `RemoveWorkPlan::new`. It exists
only to feed three render-only tests:

- `dry_run_render_3disk_removal` (remove.rs:1583)
- `dry_run_render_2disk_removal_includes_balance` (remove.rs:1638)
- `dry_run_render_helper_targets_by_uuid` (remove.rs:1734)

Two problems:

1. **Duplicate planner.** The helper is a parallel `RemoveWorkPlan`
   constructor that must be kept in step with the production planner by hand.
2. **A test that guards a test seam.** `dry_run_render_helper_targets_by_uuid`
   asserts that *the helper's own copy* of find-by-uuid selects by UUID --
   not that production does. (Its origin plan,
   `plans/impl/2026-05-18-remove-test-helper-target-by-uuid.md`, states the
   goal as making the helper *mirror* production's identity flow, then adds a
   test to prove the mirror -- the definition of testing scaffolding.)

The migration that makes this unnecessary is already underway: commits
`8f12221d` (derive previews from work plans) and `edb83f92` (migrate tests to
shared fixtures) moved remove's preview tests onto
`plan_remove(...).preview()`, and `plan_remove_2to1_preview_omits_confirmation_only_redundancy_warning`
(remove.rs:1692) already renders the 2->1 case through the production planner.
The helper is the unfinished tail of that migration.

**Intended outcome:** delete the helper and the three render-only tests; the
distinct assertions they carried (exact dry-run command strings; balance
present on 2->1 / absent on 3->2; observed-mapper-not-reconstructed under
drift) survive, but flow through `plan_remove(...).preview()` like every other
remove preview test. No production code changes.

## Why a straight drop is wrong

The exact command strings `btrfs device remove --enqueue ...`,
`cryptsetup close braid-...`, and the shell-quoted
`btrfs balance start --enqueue '-dconvert=single' '-mconvert=dup' -f ...` are
pinned **only** by these three render tests. `cmd.rs`'s
`render_dry_run_formats_steps_with_commands` (cmd.rs:3007) covers the render
*format* but via luksFormat/luksOpen; no `cmd.rs` test pins the argv of
`BtrfsDeviceRemove` / `CryptsetupClose` / `BtrfsBalanceSingle`. Deleting the
three tests outright unpins those strings (including the `shell_words` quoting
of the balance args). So the pivot must re-home the assertions, not drop them.

## The pivot

All changes are in `cli/src/remove.rs` test module, plus one doc annotation.

### 1. Delete the helper and its three callers

Remove `remove_present_work_plan_for_test` (remove.rs:828-848) and the three
`dry_run_render_*` tests (remove.rs:1583, 1638, 1734).

### 2. Strengthen the existing 2->1 production test (covers the balance scenario)

In `plan_remove_2to1_preview_omits_confirmation_only_redundancy_warning`
(remove.rs:1692), the test already builds the 2->1 case through `plan_remove`
and holds `rendered` from `preview.render()`. Add exact-line assertions on the
six rendered lines:

```
$ btrfs balance start --enqueue '-dconvert=single' '-mconvert=dup' -f /mnt/storage
$ btrfs device remove --enqueue /dev/mapper/braid-disk2 /mnt/storage
$ cryptsetup close braid-disk2
```

This pins all three command strings through the production planner and
subsumes `dry_run_render_2disk_removal_includes_balance`. Keep the existing
`WARNING:`-absence and byte-equivalence assertions.

### 3. Add a clean 3->2 production test (covers the no-balance scenario)

New test, e.g. `plan_remove_3to2_preview_omits_balance_step`, mirroring the
healthy fixture pattern of the soft-warn tests but with **no injected
failure**:

```rust
let f = PoolFixture::three_disk_healthy();
let runner = RemovalPool::three_disk().install(MockRunner::default());
let fs = MockFs::storage(vec![]);
let params = f.remove_params().dry_run(true).build(); // removes disk2
let plan = plan_remove(&runner, &fs, &params).expect("clean 3->2 plan");
```

Assert: `plan.notes.is_empty()`; `plan.preview().steps.len() == 2`; the
rendered output does **not** contain `"RAID1 -> single"` (explicit no-balance
guard); and the device-remove + close lines render for `braid-disk2`.
(Verified clean: the `remaining >= 2` branch of `check_eviction_space`
(remove.rs:695) runs `check_raid1_relocation_space` against the healthy
`valid_three_disk_*` fixtures and returns `Proceed` with zero notes.)

### 4. Add a production-path drift test (covers observed-mapper-not-reconstructed)

This is the one with genuinely unique coverage -- every other production
remove test has observed mapper == name-derived mapper, because `RemovalPool`
hardcodes `braid-disk{n}`. New test, e.g.
`plan_remove_renders_observed_mapper_under_drift`, drives the drift through
`plan_remove` instead of the helper's copy:

```rust
let f = PoolFixture::two_disk_healthy(); // pool.json: disk1->uuid1, disk2->uuid2
let runner = RemovalPool::two_disk()
    .install(MockRunner::default())
    .with_handler(|req| match req {
        // Rename only devid 1's observed mapper: braid-disk1 -> braid-renamed.
        CmdRequest::BtrfsFilesystemShow { .. } => Some(Ok(mock_ok(
            "btrfs filesystem show",
            "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 2 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-renamed\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
        ))),
        // The renamed mapper resolves to /dev/vdb; the default RemovalPool
        // handler already maps /dev/vdb -> canonical_luks_uuid(1) via
        // luks_uuid_for_device, so CryptsetupLuksUuid needs no override.
        CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-renamed" =>
            Some(Ok(mock_ok("cryptsetup status braid-renamed",
                "braid-renamed is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"))),
        _ => None, // everything else falls through to RemovalPool::install
    });
let params = f.remove_params().name("disk1").dry_run(true).build();
let plan = plan_remove(&runner, &fs, &params).expect("drift must not block planning");
let rendered = plan.preview().render();
```

Assert the rendered device-remove and close steps target the **observed**
mapper `/dev/mapper/braid-renamed` / `braid-renamed`, and that the output
contains neither `braid-disk1` (the name-derived mapper) nor any reconstruction
from the persisted name. This exercises production's find-by-uuid
(remove.rs:556) + `RemoveWorkPlan`'s observed-mapper storage (remove.rs:185) +
`render_steps` (remove.rs:194) -- the real chain the documented "close
observed, NEVER reconstructed via `mapper_name(&name)`" doctrine
(`RemoveWorkPlan.target_mapper`, remove.rs:134-139) protects.

This complements, rather than duplicates, the execute-time
`drifted_member_remove_closes_observed_mapper` (remove.rs:3159): that test
asserts `target_mapper` and the `execute()`-built close request under drift,
but `render_steps` is a preview-only path it never reaches. Sections 2-3 pin
`render_steps`' exact strings only in non-drift fixtures (where target_mapper
equals the name-derived mapper), so this is the sole test that proves the
**preview** renders the observed mapper rather than a name reconstruction.

Note: correlation in the eviction preflight is **by devid**
(`d.devid == target.devid`, remove.rs:737-745; survivor `d.devid != target_devid`,
remove.rs:813-816), so renaming the target's mapper needs **no** usage/df
fixture changes -- the existing `valid_two_disk_*` fixtures still correlate.
Precedent for driving a drifted observed mapper through a planner with
`with_handler` overrides: `mapper_name_drift_does_not_skip_open_mapper_verifier`
in `cli/src/replace.rs`.

### 5. Doc hygiene

Append a short note to
`plans/impl/2026-05-18-remove-test-helper-target-by-uuid.md` marking it
superseded by this pivot (the helper it created is removed; its identity-flow
intent is now satisfied by the production-path drift test above), so the plan
record stays honest.

## New-test requirements

Each new/changed test must keep the project's `// Intent / Why it exists /
Scenario` preamble (per AGENTS.md). The drift test's preamble should name the
"observed, not reconstructed" doctrine as the regression guarded.

## Files

- `cli/src/remove.rs` -- delete helper + 3 tests; strengthen 1 test; add 2 tests.
- `plans/impl/2026-05-18-remove-test-helper-target-by-uuid.md` -- superseded note.

## Reuse

- `plan_remove(...).preview().render()` -- the production preview path (remove.rs:233).
- `PoolFixture::two_disk_healthy` / `three_disk_healthy`, `RemovalPool::two_disk` /
  `three_disk`, `RemoveParamsBuilder` (`cli/src/test_fixtures/remove.rs`,
  `shared.rs`).
- `mock_ok`, `MockRunner::with_handler` fall-through pattern (already used by the
  soft-warn tests, remove.rs:2327+).
- `luks_uuid_for_device` / `mapper_underlying` defaults in
  `cli/src/test_fixtures/remove.rs` (the drift test leans on `/dev/vdb -> uuid1`).

## Verification

- `just test-rust` -- all remove unit tests pass; the strengthened 2->1 test and
  the two new tests go green through `plan_remove`.
- Sanity-check the deletions: `grep -n remove_present_work_plan_for_test
  cli/src/remove.rs` returns nothing.
- Confirm the exact strings are still pinned: the strengthened 2->1 test asserts
  all three command lines; the 3->2 test asserts no-balance; the drift test
  asserts the observed mapper.
- No production code changed, so VM tests (`tests/cli/braid-remove-disk.py`) are
  unaffected; no fixture-refresh event.

## Adjacent coverage that already exists (not re-done here)

The **execute-time** drift path is already covered end-to-end through
production, so this pivot deliberately does not touch it:

- `drifted_member_remove_closes_observed_mapper` (remove.rs:3159) drives a
  drifted mapper (`braid-WRONG`) through `plan_remove(...)` then
  `plan.execute(...)` and asserts both `plan.work_plan.target_mapper ==
  "braid-WRONG"` and that the post-commit `CryptsetupClose` targets the
  observed `braid-WRONG`, not a name-reconstructed mapper.
- `post_commit_close_uuid_probe_demotes_to_skip_on_mismatch` (remove.rs:3328)
  covers the post-commit `probe_observed_mapper_uuid` (remove.rs:456) guard for
  the double-drift case.

This pivot is scoped to the **dry-run preview render** path, which those
execute-time tests do not exercise: `execute()` builds its `CryptsetupClose`
directly, while `render_steps` (remove.rs:194) is preview-only. No existing
production-path test asserts `render_steps`' rendered output under a drift
scenario -- that is exactly the gap the new drift test in section 4 closes,
complementing (not duplicating) the execute-time tests above.

## Implementation notes

- The drift test (section 4) asserts the device-remove / close commands via
  full-command-string `contains` rather than positional `lines[3]`/`lines[5]`.
  Removing `disk1` from a healthy 2-disk pool is a 2->1 removal, so the
  preview also carries the balance step; the `contains` form keeps the
  observed-mapper assertions robust to that and to any future step-ordering
  change, while still pinning the mapper to the rendered `$ ...` command line
  (not merely the description line).
- The strengthened 2->1 test's preamble was extended (not just its body) to
  name its new role as the sole production-path pin for the balance /
  device-remove / close argv, so the `// Intent / Why it exists` block stays
  honest about what a regression there would break.
