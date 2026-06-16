# Plan: document the deliberate present-only scope of the Browse SMART canary

## Context

A Low-severity testing finding observed that `tests/cli/braid-tui-browse.py`
pins only the *present-member* SMART path (a live `/dev/vd` node), while the
complementary *offline-member -> by-id* fallback is proven only by Rust unit
tests. The worry: a future refactor could blank offline rows or route every row
through the live path, and the VM lane would stay green because its pool has no
offline member.

Investigation showed the behavior is already fully covered, and the proposed
fix (a degraded-pool VM test) is the wrong shape:

- **The fallback is dual-unit-pinned, on both halves.** The picker side is
  pinned by `state.rs#smartctl_picker_resolves_present_member_to_live_path`
  (disk2, absent from `disk_underlying`, resolves to its by-id handle). The
  probe side is pinned by `probe.rs#smartctl_health_for_present_member_uses_live_underlying`
  (a declared-but-offline member is omitted from `disk_underlying`).
- **Divergence is structurally precluded.** Both SMART surfaces consume the same
  `present_underlying` map (`probe.rs` builds it once, feeds the Data-tab loop,
  and stores it verbatim as `PoolState.disk_underlying`) through the same
  `model.rs#smart_query_device` helper. The finding's hypothesized regressions
  would both fail the existing picker unit test.
- **An offline member necessarily means a degraded pool.** Exercising the
  fallback in this lane would require converting a clean, healthy 2-disk canary
  (it asserts `mountpoint /mnt/storage`) into a fragile `-o degraded` scenario,
  for near-zero marginal regression protection over the unit tests.

So the gap is not coverage -- it is **discoverability**. The VM test is silent
about *why* it only pins the present path, which is what invited the finding and
will invite the next reviewer to re-file it. The ideal change is a comment that
records the deliberate split and points at the tests that own the other half.

Decision confirmed with the user: docs comment, not a new VM test.

## The change

Single edit, one file. Extend the existing SMART comment in
`tests/cli/braid-tui-browse.py` (currently lines 89-91, just above the
`r"/dev/vd"` assertion) to record the deliberate present-only scope.

Replace:

```python
    # disk1 is a present, unlocked member, so the SMART detail/footer dispatches
    # against the live backing node (decision 024), not the persisted by-id
    # handle. `/dev/vd` matches whichever virtio node cryptsetup reports.
    machine.wait_until_tty_matches("2", r"/dev/vd")
```

with:

```python
    # disk1 is a present, unlocked member, so the SMART detail/footer dispatches
    # against the live backing node (decision 024), not the persisted by-id
    # handle. `/dev/vd` matches whichever virtio node cryptsetup reports.
    #
    # The complementary offline-member -> by-id fallback is deliberately not
    # exercised here: it would need a degraded pool, while this canary pins a
    # healthy mount. Both halves are unit-pinned and route through one shared
    # model.rs#smart_query_device, so the two SMART surfaces cannot diverge:
    # state.rs#smartctl_picker_resolves_present_member_to_live_path (picker
    # by-id fallback) and probe.rs#smartctl_health_for_present_member_uses_live_underlying
    # (probe omits the offline member from disk_underlying).
    machine.wait_until_tty_matches("2", r"/dev/vd")
```

### Conventions followed (existing precedent)

- **"covered elsewhere, not here" prose** mirrors
  `tests/cli/recover-bootstrap-crash.py` ("covered by N Rust unit tests ... but
  no VM test exercises it end-to-end").
- **`path#symbol` citations in Python test comments** mirror
  `tests/cli/braid-ack-cleanup-pending.py` (`status.rs#resolve_alert_state`,
  `ack.rs#cmd_ack_impl`). Bare-filename form is the house style; full paths are
  reserved for whole-file referents. Matches `docs/dev/doc-citations.md`
  (`path#symbol`, never line numbers).
- **`decision 024`** is the form already used on the line above (and across the
  suite).
- **ASCII** (`--`, `->`) matches the existing comment. Python test comments are
  outside `check-output-ascii.py`'s scanned paths anyway (`cli/src/**/*.rs` and
  `modules/**/*.nix` only), so this is style-matching, not a hard requirement.

## Explicitly NOT changing

- No change to the VM pool topology (`tests/cli/braid-tui-browse.nix` stays a
  clean 2-disk RAID1).
- No new VM/integration test, and no `-o degraded` scenario.
- No Rust code or unit-test changes -- the two cited tests and the shared helper
  already encode the invariant correctly.

## Verification

This is a comment-only change to a Python test file; it alters no behavior and no
assertion. Verification is limited to confirming nothing was broken and the
citations are accurate:

1. **Citations resolve.** Confirm the three cited symbols still exist:
   - `rg -n "fn smartctl_picker_resolves_present_member_to_live_path" cli/src/tui/browse/state.rs`
   - `rg -n "fn smartctl_health_for_present_member_uses_live_underlying" cli/src/tui/probe.rs`
   - `rg -n "fn smart_query_device" cli/src/tui/model.rs`
2. **Test still parses / runs unchanged.** The `braid-tui-browse` VM check is the
   same logic as before; if run, it must still pass (`nix build .#checks.aarch64-darwin.braid-tui-browse`
   per the repo's VM-test workflow, via the linux-builder). No new assertions to
   satisfy.
3. **Doc-citation linter** (if part of CI) stays green:
   `scripts/docs/check-see-paths.py` / `check-output-ascii.py` -- the latter does
   not scan `tests/**`, so it is unaffected.
