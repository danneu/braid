# Fix `format_remove_missing_confirm` post-op shape and rebalance-hint accuracy on multi-missing pools

## Context

`format_remove_missing_confirm` (`cli/src/remove_missing.rs:579-607`) renders the
final "are you sure?" prompt before `braid remove-missing` writes a journal and
mutates the pool. Two lines in that prompt currently lie about the outcome when
the pool has more than one missing device:

1. **Post-op shape line** (`cli/src/remove_missing.rs:595-605`) -- always
   renders `-> {remaining_present} disk(s)`, implying the pool is fully
   restored. With `missing_count >= 2`, this is wrong: `remove-missing`
   removes ONE missing entry per invocation, so the pool actually ends as
   `{remaining_present} present + {missing_count - 1} missing` (still
   degraded). Example: a 4-disk RAID1 with 3 dead drives reaches the
   formatter with `(remaining_present=1, missing_count=3)` and the operator
   sees `Pool: 1 present + 3 missing -> 1 disk` -- the "1 disk" target reads
   as "fully recovered".

2. **Rebalance-hint line** (`cli/src/remove_missing.rs:590-594`) -- when
   `remaining_present >= 2`, says `Data on remaining disks will be
   rebalanced.` But the planner only queues a balance step when
   `will_clear_last_missing && remaining_present >= 2` (`render_steps`, lines
   108-133; `restore_raid1_after_commit`, lines 104-106). With
   `missing_count >= 2`, no balance runs, yet the prompt still promises
   rebalancing.

Both bugs share one root cause: the formatter ignores `missing_count` and
assumes the op fully clears the missing set. The current tests
(`remove_missing_confirm_with_rebalance` at `cli/src/remove_missing.rs:1601`,
`remove_missing_confirm_single_survivor` at line 1612) only exercise
`missing_count == 1`, so the misleading multi-missing branch has no coverage.

This message is the operator's last confirmation gate before a destructive op
runs. Inaccurate post-op shape + a false rebalance promise could lead an
operator to confirm under wrong expectations.

**Reachability of the buggy branch.** The bug fires only when `missing_count >= 2`
reaches the formatter. Relevant gates upstream of the formatter:

- `plan_remove_missing` rejects `pool.missing_count == 0`
  (`cli/src/remove_missing.rs:374`) and rejects targets that are not in
  `pool.missing_devids` or are live devices
  (`cli/src/remove_missing.rs:377-396`).
- `plan_remove_missing` rejects the exact 2-disk RAID1 + 1 missing case at
  `cli/src/remove_missing.rs:398-425` (kernel refuses
  `BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET`; operator is redirected to
  `braid replace --missing-id`). This precondition only matches
  `pool.total_devices == 2`, so it does **not** filter multi-missing pools.
- The `pool.devices.len() >= 2` check at `cli/src/remove_missing.rs:434`
  runs the relocation-space preflight; on insufficient survivor space it
  returns a hard `RemoveMissingError::Validation` before the formatter is
  reached. Pools with `remaining_present == 1` skip this check.
- `pool.missing_count` itself is unbounded -- computed in `probe.rs:314` via
  `show.total_devices.saturating_sub(devices.len())`.

The new 2-disk rejection does not filter multi-missing pools. The buggy
formatter branch remains reachable for any multi-missing pool that also
passes target-devid validation and (when `remaining_present >= 2`) the
relocation-space preflight. Representative reachable cases:
`(remaining_present=1, missing_count=2)` (3-disk pool, 2 dead) and
`(remaining_present=2, missing_count=2)` (4-disk pool, 2 dead, survivors
have space) -- these are the two cases the new test asserts.

## Approach

Two targeted changes in `format_remove_missing_confirm`, plus one new
regression test. Keep the function signature
`(name, devid, remaining_present, missing_count)` -- both predicates needed
(`missing_count == 1` for "fully restored", `restore_raid1_after_commit ==
missing_count == 1 && remaining_present >= 2` for "balance will run") fall out
of the existing arguments without threading the work plan through.

### 1. Post-op shape line (`cli/src/remove_missing.rs:595-605`)

Branch on `missing_count == 1`:

- `missing_count == 1` (op fully restores pool): keep the existing form
  `-> {remaining_present} disk(s)`. This preserves the satisfying readback for
  the common case and matches the simpler `Pool: ... -> X disks` shape used by
  `format_remove_confirm` (`cli/src/remove.rs:656`, `Pool:` line at 673) and
  `format_replace_confirm` (`cli/src/replace.rs:1444`, `Pool:` line at 1502).
- `missing_count >= 2` (pool stays degraded): render
  `-> {remaining_present} present + {missing_count - 1} missing`. This is
  symmetric with the pre-op shape and accurately conveys the residual state.

The asymmetry is load-bearing: the disk-count form signals "fully recovered",
the present+missing form signals "still degraded".

### 2. Rebalance-hint line (`cli/src/remove_missing.rs:590-594`)

Tighten to gate on the same predicate the planner uses for the balance step:

- `missing_count == 1 && remaining_present >= 2` (balance step queued):
  `Data on remaining disks will be rebalanced.` (status quo)
- `missing_count == 1 && remaining_present == 1` (last missing, single
  survivor; no balance needed because 2-device RAID1 already has all data on
  every survivor): `Surviving disk already has all data.` (status quo)
- `missing_count >= 2` (no balance step queued, pool stays degraded):
  `Pool will remain degraded -- {missing_count - 1} missing entr{y|ies} will remain.`

Use `--` (double hyphen), not em-dash, per project style (`AGENTS.md`, "CLI
Output Style"). Pluralize `entry/entries` based on `missing_count - 1`.

### 3. New regression test

Add one table-driven test in the existing
`// --- Confirmation formatter tests ---` block at
`cli/src/remove_missing.rs:1598`, placed directly after
`remove_missing_confirm_single_survivor` (which ends at line 1616), matching
the file's `.contains()` assertion idiom. The table must cover both
`remaining_present == 1` and `remaining_present >= 2` so the test exercises
**both** new branches:

- The post-op shape branch fires for any `missing_count >= 2`, so a single
  `remaining_present` value is enough to catch the `-> X disk(s)` regression.
- The rebalance-hint branch only fires when `remaining_present >= 2`. The
  pre-fix formatter said `Surviving disk already has all data.` when
  `remaining_present == 1`, so a `(1, _)` case alone cannot detect a
  regression of the rebalance-hint fix -- the `(2, 2)` case is required.

The preamble must be `//` line comments **directly above** `#[test]`, per
[`docs/testing.md`](docs/testing.md) line 11-22 (not inside the body):

```rust
// Intent: verify the confirm prompt accurately describes residual
//   degradation and does not promise a rebalance when the pool stays
//   degraded -- exercising both new branches added for missing_count >= 2.
// Why it exists: regression guard against (a) the "-> X disk(s)" post-op
//   shape that previously implied a fully-restored pool when one or more
//   missing entries remain, and (b) the "Data on remaining disks will be
//   rebalanced" hint that previously promised a balance step that the
//   planner does not actually queue when missing_count > 1.
// Scenario: pool stays degraded after remove-missing because more than
//   one missing entry exists. The (1, 2) case models a 3-disk RAID1 with
//   2 dead drives (total_devices = remaining_present + missing_count = 3),
//   removing one of the two missing entries. The (2, 2) case models a
//   4-disk RAID1 with 2 dead drives, removing the first of two missing
//   entries -- this case is the one that catches a regression of the
//   rebalance-hint fix.
#[test]
fn remove_missing_confirm_multiple_missing() {
    let cases: &[(usize, u64, &str, &str)] = &[
        (1, 2, "1 present + 2 missing -> 1 present + 1 missing", "-> 1 disk"),
        (2, 2, "2 present + 2 missing -> 2 present + 1 missing", "-> 2 disks"),
    ];
    for (rp, mc, expected_shape, forbidden_shape) in cases {
        let msg = format_remove_missing_confirm("toshiba", 2, *rp, *mc);
        assert!(
            msg.contains(expected_shape),
            "case ({rp}, {mc}): expected post-op shape {expected_shape:?} in:\n{msg}"
        );
        assert!(
            msg.contains("Pool will remain degraded"),
            "case ({rp}, {mc}): expected degraded hint in:\n{msg}"
        );
        assert!(
            !msg.contains("rebalanced"),
            "case ({rp}, {mc}): unexpected rebalance promise in:\n{msg}"
        );
        assert!(
            !msg.contains("Surviving disk already has all data"),
            "case ({rp}, {mc}): unexpected single-survivor hint in:\n{msg}"
        );
        assert!(
            !msg.contains(forbidden_shape),
            "case ({rp}, {mc}): unexpected fully-restored shape {forbidden_shape:?} in:\n{msg}"
        );
    }
}
```

Existing tests stay green unchanged (both pass `missing_count = 1`, which
hits the preserved branches).

## Files to modify

- `cli/src/remove_missing.rs` -- `format_remove_missing_confirm` body (lines
  585-606) and one new test in the `tests` module placed after
  `remove_missing_confirm_single_survivor` (current end at line 1616). No
  other files change.

## Reused code / patterns

- `.contains()` assertion idiom for confirm-formatter tests: established at
  `cli/src/remove_missing.rs:1600-1616`.
- `--` (double hyphen) for CLI output: project convention from `AGENTS.md`.
- The `if remaining_present == 1 { "disk" } else { "disks" }` pluralization
  at `cli/src/remove_missing.rs:600-604` stays in the `missing_count == 1`
  branch unchanged.
- No new helpers needed -- exploration confirmed no existing "X present +
  Y missing" formatter elsewhere; siblings `format_remove_confirm`
  (`cli/src/remove.rs:656`, `Pool:` line at 673) and `format_replace_confirm`
  (`cli/src/replace.rs:1444`, `Pool:` line at 1502) use the simpler
  `disks -> disks` shape that this fix matches in the fully-restored branch.

## Out of scope

- The `Surviving disk already has all data.` line in the `(remaining_present
  == 1, missing_count == 1)` formatter branch. This branch is preserved as
  existing formatter behavior and is still exercised by the
  `remove_missing_confirm_single_survivor` unit test
  (`cli/src/remove_missing.rs:1612`), but at the command level the only pool
  shape that would reach this branch (`total_devices == 2 &&
  remaining_present == 1 && missing_count == 1`, since `total_devices ==
  remaining_present + missing_count`) is now rejected by the 2-disk RAID1
  precondition at `cli/src/remove_missing.rs:398-425` before confirmation.
  The branch is effectively dead code on the operator-facing path; no change
  in scope here.
- The `(remaining_present == 1, missing_count >= 2)` case involves the same
  "Surviving disk already has all data" claim, which is questionable for a
  pool that started with N >= 3 disks. With this fix, that case now renders
  the new "Pool will remain degraded" hint instead, which is accurate
  regardless of disk-content semantics. No further change needed.
- Refactoring to thread `RemoveMissingWorkPlan` through the formatter so it
  can use `restore_raid1_after_commit()` directly. The Plan agent confirmed
  this would add churn without clarity gain -- the local
  `missing_count == 1 && remaining_present >= 2` predicate carries the same
  information at this call site.

## Verification

1. Read the modified function and the new test in
   `cli/src/remove_missing.rs`.
2. `just test-rust` -- runs `cargo test`. Expect all three confirm-formatter
   tests (`remove_missing_confirm_with_rebalance`,
   `remove_missing_confirm_single_survivor`,
   `remove_missing_confirm_multiple_missing`) to pass. Existing two should
   pass unchanged.
3. `cargo check -p braid-cli` to confirm no compile errors.
4. The four representative cases are all covered by Rust unit tests after
   this change:
   - `(2, 1)` -- existing `remove_missing_confirm_with_rebalance`: asserts
     `2 present + 1 missing -> 2 disks` + `rebalanced`. Unchanged.
   - `(1, 1)` -- existing `remove_missing_confirm_single_survivor`: asserts
     `1 present + 1 missing -> 1 disk` + `Surviving disk already has all
     data`. Unchanged.
   - `(1, 2)` -- new `remove_missing_confirm_multiple_missing` first row:
     asserts `1 present + 2 missing -> 1 present + 1 missing`, `Pool will
     remain degraded`, no `rebalanced`, no `-> 1 disk`, no `Surviving disk
     already has all data`.
   - `(2, 2)` -- new `remove_missing_confirm_multiple_missing` second row:
     asserts `2 present + 2 missing -> 2 present + 1 missing`, `Pool will
     remain degraded`, no `rebalanced`, no `-> 2 disks`, no `Surviving disk
     already has all data`. This row is what catches a regression of the
     rebalance-hint fix.
5. No VM test needed -- this is a pure formatter change with no command
   surface or side effects. `just test-vm` would only confirm the unrelated
   end-to-end paths still pass and is unnecessary for this change.
