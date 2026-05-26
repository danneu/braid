# Pivot: correct the `exceeds_acked` rationale (don't delete the guard)

## Context

A `/ultrareview` finding (Low/Simplicity) flagged the `current < acked` branch
in `exceeds_acked` (`cli/src/alert.rs:145-151`) as defending a scenario braid
cannot produce, and asked to replace it with a plain `current > acked` plus
delete the `counter_reset_detection` test.

Verification confirmed half of that: the comment's stated cause is **false**.
btrfs device-stats counters are persistent and monotonic -- "printed at mount
time and updated during filesystem lifetime" (`reference/btrfs-progs/Documentation/btrfs-device.rst:359-361`),
reset only by `btrfs device stats -z` (`btrfs-device.rst:273-275`), which braid
never runs (`cli/src/cmd.rs:533-545` builds `device stats` with no `-z`). So the
comment's "(e.g. across a remount)" teaches a falsehood: counters survive remount.

But the **prescription is wrong**. `exceeds_acked` returns `current > acked`
whenever `current >= acked`; the `current < acked` branch only ever makes it
alert *more* (it returns `current > 0`). It is a fail-loud guard that never
suppresses a real alert. And `current < acked` *is* reachable -- it means a
stale-**high** baseline, which has two real sources:

1. **The committed-but-closed crash window ADR 014 itself names.** `add.rs:1448`
   drops the stale acked entry *after* btrfs commits the device add
   (`add.rs:1457`). A reused devid (`last_devid+1`, ADR 014:147) inherits the
   prior holder's higher baseline if add crashes before the drop. `monitor.rs:104`
   reconcile won't prune a *present* devid, so that ghost baseline reaches
   `compute_alert_state` (`monitor.rs:113`). If the fresh disk has errors *below*
   the ghost count, `current > acked` would stay silent -- exactly the
   "ghost baseline suppresses health alerts" bug ADR 014:147 exists to prevent.
   `exceeds_acked` fails loud instead.
2. **A manual `btrfs device stats -z`** resets counters to 0; later errors below
   the old baseline would be silently suppressed by a plain `current > acked`.

For a NAS health monitor, suppressing a failing-disk alert is the worst failure
mode. The right work is to **keep the guard and fix the misleading rationale** --
in the code comment, in the test, and in the authority doc that currently omits
this guard from its defense narrative.

The false claim is isolated to two comments, both in `cli/src/alert.rs`. Every
other "remount/reset" hit in the tree is the btrfs *balance* subsystem (chunk
counters that genuinely do reset on remount -- `parse/btrfs_balance_status.rs`,
`doctor.rs`) and is correct and unrelated.

## The pivot (three edits)

### 1. `cli/src/alert.rs:145-151` -- rewrite the helper comment

Convert the plain `//` comment to a `///` doc comment (house style: private
helpers in this file use `///` -- see `quarantine_corrupt_latch`,
`same_cause_key`). State the true invariant. The function body is **unchanged**.

```rust
/// Alert when `current` exceeds the acked baseline, treating a baseline
/// *above* `current` as 0 so braid fails loud instead of suppressing.
///
/// btrfs device-stats counters are persistent and monotonic -- reset only by
/// `btrfs device stats -z`, which braid never runs -- so a current value below
/// the ack baseline is not a comparable post-ack counter value. It means the
/// baseline belongs to a different counter stream: either a reused devid
/// inherited a ghost baseline before add/recover cleanup dropped its acked entry
/// (ADR 014), or an operator reset the live counters with `-z`. Treat the
/// baseline as 0 so any nonzero current still alerts.
fn exceeds_acked(current: u64, acked: u64) -> bool {
    current > if current < acked { 0 } else { acked }
}
```

### 2. `cli/src/alert.rs:1073-1095` -- rename + re-comment the test

Rename `counter_reset_detection` -> `stale_high_baseline_does_not_suppress_alert`.
Replace the false 2-line inline comment with a proper Intent/Why/Scenario
line-comment preamble (the more common form in this file). The test **values are
unchanged** (current `read_io_errs=1`, acked `read_io_errs=5` => alert active) --
it already models exactly the stale-high scenario, and it is the regression gate
that would fail if someone tried the finding's `current > acked` deletion. But
the body is **not** fully unchanged: the assertion message at `alert.rs:1094`
currently reads `"counter reset should trigger alert"`, which embeds the same
false framing being removed -- change it to `"stale-high baseline should trigger
alert"`.

```rust
// Intent: when the acked baseline is HIGHER than the current counter,
//   exceeds_acked treats the baseline as 0 and alerts on any nonzero current
//   rather than suppressing.
// Why it exists: btrfs device-stats counters are persistent and monotonic, so a
//   current value below the ack baseline is not a comparable post-ack counter
//   value -- the baseline belongs to a different counter stream (a reused-devid
//   ghost baseline before add/recover cleanup, or a manual `-z`). This pins the
//   fail-loud behavior so a future "simplify to current > acked" change, which
//   would silently suppress a later nonzero counter, fails here.
// Scenario: an add reused devid 1 (last_devid+1) and crashed before
//   drop_ghost_acked_for_devids ran, so the acked baseline still reads
//   read_io_errs=5 from the prior holder. A monitor cycle runs before recover
//   sweeps it, and the fresh disk has already logged 1 read error.
#[test]
fn stale_high_baseline_does_not_suppress_alert() {
    // ... values unchanged (read_io_errs=1 vs acked 5); only the assertion
    // message changes from the old "counter reset" framing ...
    let alert = compute_alert_state(&stats, &acked, &[1], &[], false);
    assert!(alert.active(), "stale-high baseline should trigger alert");
}
```

### 3. `docs/design/decisions/014-alerts.md` -- document the backstop

The "Acked-stats hygiene across pool membership changes" section (~lines
145-155) lists three layers that all aim to *remove* a stale baseline, but never
mentions this guard. Add a short paragraph **after** the numbered list (frame it
as a distinct backstop property, not a 4th detection layer -- it does not enforce
"never inherit"; it limits the damage when a stale baseline transiently
survives):

> Backstop: independently of those three layers, the alert computation fails
> loud when the acked baseline is no longer comparable to the current counter
> stream. `compute_alert_state` treats an acked counter that exceeds the current
> `btrfs device stats` value as 0 and alerts on any nonzero current. btrfs
> device-stats counters are persistent and monotonic (reset only by `-z`, which
> braid never runs), so the only ways a current value can sit below the ack
> baseline are a reused devid that inherited a ghost baseline before add/recover
> cleanup dropped its acked entry (the committed-but-closed crash window above),
> or an operator resetting the live counters with `btrfs device stats -z`. The three layers aim to *remove* a stale baseline; this guard ensures that
> if one transiently survives, it cannot *suppress* a later nonzero counter.

## Files

- `cli/src/alert.rs` -- edits 1 and 2 (comment rewrite; test rename + preamble).
  No logic change.
- `docs/design/decisions/014-alerts.md` -- edit 3 (one paragraph in the
  acked-stats-hygiene section).

## Out of scope / explicitly not doing

- **Not** changing `exceeds_acked`'s logic or deleting the branch (the finding's
  prescription) -- it is a fail-loud guard the crash window relies on.
- **Not** touching the btrfs-balance remount comments
  (`parse/btrfs_balance_status.rs:37,107`, `doctor.rs:3705`) -- those describe a
  different subsystem and are correct.

## Verification

- `just test-rust` -- compiles `cli/` and runs the renamed test; it must pass
  (logic is unchanged, so behavior is identical). This is the only test lane
  needed: no production code path changes, so no VM/parser tests are implicated.
- Optional: `mdbook build docs` to confirm the ADR edit leaves the docs tree
  building cleanly (the added paragraph introduces no new cross-links).
- Sanity grep after editing: `rg -n "remount|counter reset" cli/src/alert.rs`
  should return nothing (both false-claim comments and the stale assertion
  message removed).
