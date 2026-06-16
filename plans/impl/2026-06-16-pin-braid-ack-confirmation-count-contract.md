# Pin the `braid ack` confirmation-count contract

## Context

`cmd_ack_impl` prints two user-facing confirmation lines after a successful ack
(`cli/src/ack.rs`):

- **Counted** (mounted, `!causes.is_empty()`): `acknowledged {N} alert{s}`, where
  `N = causes.len()` -- latched causes only.
- **No-count** (`acknowledged current alerts`): printed in three places -- the
  cleanup-only retry branch (`ack.rs:60`), the mounted smartd-only / corrupt-latch
  fall-through (`ack.rs:114`), and every offline success (`ack.rs:169`, which prints
  no count *even when `causes` is non-empty*, e.g. a latched `MissingDevice`).

`docs/commands/ack.md` documents the counted form (`acknowledged 3 alerts`) as the
contract.

The no-count line is pinned end-to-end for two of its three code sites
(`braid-smartd-alert.py:51` for the mounted smartd-only fall-through,
`braid-ack-cleanup-pending.py:112` for the cleanup-only retry). Two branches are
unpinned: the **counted line** (a repo-wide search for `acknowledged [0-9]` / `alert{s}`
matches only the source line `ack.rs:109`) and the **offline non-empty no-count
line** (`ack.rs:169`, which prints no count even for a latched `MissingDevice`). A
regression that printed a wrong count, dropped the `alert`/`alerts` pluralization,
printed `acknowledged 0 alerts` for a smartd-only ack, folded the synthesized smartd /
cleanup-pending signals into the count (as `status::resolve_alert_state` deliberately
does for its *own* display), or routed the offline site through the count helper
(turning an offline `MissingDevice` ack into `acknowledged 1 alert`) would pass the
entire suite.

This is a pivot from the originating finding, which (a) overstated the gap -- it
claimed no test pins the no-count string, missing `braid-smartd-alert.py:51`, so
its bare-smartd case is already covered -- and (b) proposed in-process stdout
capture, which it itself called "awkward." The real, narrow gap is the counted
branch, and it is reachable far more cleanly.

## The gap, precisely

| Branch | String | Currently pinned? |
| --- | --- | --- |
| No-count, smartd-only mounted | `acknowledged current alerts` | Yes -- `braid-smartd-alert.py:51` |
| No-count, cleanup-only retry | `acknowledged current alerts` | Yes -- `braid-ack-cleanup-pending.py:112` |
| **No-count, offline non-empty (MissingDevice)** | `acknowledged current alerts` | **No -- nothing** (offline reports no count even with causes -- the trap) |
| **Counted, mounted** | `acknowledged {N} alert{s}` | **No -- nothing** |

The counted branch *is executed end-to-end* today (both `monitor-lifecycle.py:94`
and `monitor-hot-unplug.py:101` run a mounted ack over exactly one latched
`MissingDevice` cause -> `acknowledged 1 alert`), but both use bare
`machine.succeed("braid ack")` and assert nothing about stdout.

## Approach

Three complementary pins, all structure-insensitive except the unit test: a VM
assertion for the counted mounted branch (N=1), a VM assertion for the offline
non-empty no-count branch (the trap below), and a pure-helper unit test for the
count/pluralization matrix that no VM scenario reaches (N>=2 needs >=2 simultaneous
causes, which no test sets up).

### 1. Extract a pure confirmation formatter (`cli/src/ack.rs`)

Add a private helper beside the existing `format_systemctl_stop_failure`, plus a
shared const for the no-count line so all four message sites have a single source
of truth and the offline no-count choice becomes self-documenting:

```rust
/// The no-count ack confirmation. Used wherever ack completed real cleanup but
/// has no meaningful latch count to report: the cleanup-only retry, the mounted
/// smartd-only / corrupt-latch fall-through, and *every* offline success --
/// offline ack reports no count even for a latched MissingDevice, because the
/// count is only meaningful on the mounted path that re-baselines counters.
const ACK_NO_COUNT_LINE: &str = "acknowledged current alerts";

/// Builds the mounted-ack confirmation line for `latched_count` latched causes.
/// A count is meaningful only here; `0` (smartd-only / corrupt-latch acks) falls
/// back to the shared no-count line. The count is strictly the latched-cause
/// count -- it must never fold in the synthesized smartd / cleanup-pending
/// signals, which have no latch and are surfaced separately by `status`.
fn format_ack_confirmation(latched_count: usize) -> String {
    if latched_count == 0 {
        ACK_NO_COUNT_LINE.to_owned()
    } else {
        format!(
            "acknowledged {latched_count} alert{}",
            if latched_count == 1 { "" } else { "s" }
        )
    }
}
```

Rewrite the four call sites:

- `ack.rs:107-115` (mounted) -> `println!("{}", format_ack_confirmation(causes.len()));`
- `ack.rs:60` (cleanup-only) -> `println!("{ACK_NO_COUNT_LINE}");`
- `ack.rs:169` (offline, in `ack_offline`) -> `println!("{ACK_NO_COUNT_LINE}");`

**Trap to preserve (do not regress):** the offline site must keep printing the
no-count line even though `causes` may be non-empty. Route it through
`ACK_NO_COUNT_LINE`, never `format_ack_confirmation(causes.len())` -- the latter
would change an offline missing-device ack to `acknowledged 1 alert`. The const +
doc comment exist precisely so a future reader sees this is intentional, and step 4
pins it with a test so the regression cannot land silently.

Signature/naming/placement follow existing house style: `format_*(primitive) -> String`
beside its call site (cf. `format_systemctl_stop_failure` same file,
`format_add_missing_devices_warning(missing_count: u64)` in `add.rs`,
`format_balance_progress(...)` in `progress.rs`).

### 2. Unit test the count/pluralization matrix (`cli/src/ack.rs` `mod tests`)

Add one `assert_eq!` test (the only deterministic way to cover N>=2 pluralization),
with the `//` Intent / Why it exists / Scenario preamble the repo requires:

```rust
#[test]
fn format_ack_confirmation_pins_count_and_pluralization() {
    assert_eq!(format_ack_confirmation(0), "acknowledged current alerts");
    assert_eq!(format_ack_confirmation(1), "acknowledged 1 alert");
    assert_eq!(format_ack_confirmation(2), "acknowledged 2 alerts");
    assert_eq!(format_ack_confirmation(3), "acknowledged 3 alerts");
}
```

This catches the `0 -> "acknowledged 0 alerts"` and dropped-pluralization
regressions the finding worried about. Model the preamble on the existing
`//`-style tests (e.g. `cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats`)
and the simple-assert style of `util.rs` `format_duration_secs_disambiguates_boundaries`.

### 3. Pin the counted branch end-to-end in the VM lane (`tests/module/monitor-lifecycle.py`)

In the existing subtest "braid ack clears alert and stops alert service" (line ~93),
replace `machine.succeed("braid ack")` with a stdout-capturing assertion, matching
the redirect-to-file pattern already used in `braid-smartd-alert.py`:

```python
machine.succeed("braid ack >/tmp/ack.out 2>/tmp/ack.err")
stdout = machine.succeed("cat /tmp/ack.out")
assert stdout == "acknowledged 1 alert\n", (
    f"expected counted ack confirmation for one latched cause, got: {stdout!r}"
)
```

Keep the existing follow-on assertions (`systemctl is-active braid-alert.service`
fails; `alert-latch.json` removed). The exact count is safe: the degraded mount
(disk3 mapper closed on a 3-disk RAID1) latches exactly one `MissingDevice{devid:3}`
cause (no btrfs-error / smartd / computation causes), and `braid-alert.service` is
active at that point so `systemctl stop` succeeds with clean stderr (verified by
tracing `compute_alert_state` in `cli/src/alert.rs` and the probe in `cli/src/probe.rs`).

### 4. Pin the offline non-empty no-count branch in the VM lane (`tests/cli/braid-monitor.py`)

The offline success path (`ack.rs:169`) prints the no-count line even when `causes`
is non-empty -- the trap above -- and nothing currently asserts its stdout. The
existing subtest "MissingDevice alert acked offline does not re-fire on remount"
(line ~274) already runs that exact path: the pool is fully offline (all three LUKS
mappers closed) with a latched `MissingDevice{devid:2}`, then `braid ack` (line ~288).
It asserts `missing_acked` persistence but not stdout, so a regression routing the
offline site through `format_ack_confirmation(causes.len())` (-> `acknowledged 1
alert`) would pass. Replace that bare `machine.succeed("braid ack")` with a
stdout-capturing assertion:

```python
machine.succeed("braid ack >/tmp/ack-offline.out 2>/tmp/ack-offline.err")
stdout = machine.succeed("cat /tmp/ack-offline.out")
assert stdout == "acknowledged current alerts\n", (
    f"offline MissingDevice ack must report no count, got: {stdout!r}"
)
```

Keep the existing follow-on assertions (latch removed, `acked-stats.json` present,
`missing_acked=true`, no re-fire on remount). The redirect is required here (unlike a
plain `succeed`): `braid-monitor.nix` installs only the `braid` binary -- no braid
NixOS module, so `braid-alert.service` does not exist and `stop_beeper`'s `systemctl
stop` writes a `warning: ...` line to stderr (same reason `braid-smartd-alert.py` /
`braid-ack-cleanup-pending.py` capture streams to files). This is the third no-count
code site (`ack.rs:169`); together with the two already-pinned sites it covers every
no-count branch.

## Files to modify

- `cli/src/ack.rs` -- add `ACK_NO_COUNT_LINE` const + `format_ack_confirmation` helper,
  rewrite the 4 message sites, add the unit test.
- `tests/module/monitor-lifecycle.py` -- tighten the existing mounted ack subtest to
  assert `acknowledged 1 alert\n` (counted branch).
- `tests/cli/braid-monitor.py` -- tighten the existing offline MissingDevice ack
  subtest to assert `acknowledged current alerts\n` (offline non-empty no-count branch).

No `flake.nix` checks registration is needed (no new test file; the unit test runs
under the existing `test-rust` lane, the VM assertion lives in an already-registered
test).

## Out of scope

- `status::resolve_alert_state`'s separately-synthesized count is correct as-is; the
  helper's doc comment records why ack's count (latched causes only) intentionally
  differs. No status change.
- A second VM pin in `monitor-hot-unplug.py:101` (also a mounted N=1 ack) is
  redundant with the `monitor-lifecycle.py` pin for the count contract; skip it
  unless that file is being touched anyway.
- No new multi-cause (N>=2) VM scenario -- pluralization is covered deterministically
  by the unit test; a bespoke 2-cause VM scenario is not worth the cost.

## Verification

1. `just test-rust` -- runs `format_ack_confirmation_pins_count_and_pluralization`
   plus the existing `ack.rs` suite. Sanity-check failure direction first: temporarily
   break pluralization (force `"s"`) and confirm the new test goes red on
   `format_ack_confirmation(1)`.
2. `just test-vm monitor-lifecycle braid-monitor` -- exercises the real `braid ack`
   binary over a degraded mounted pool (now asserting the exact counted stdout) and
   the fully-offline MissingDevice path (now asserting the no-count stdout). (Runs on
   `aarch64-darwin` via `nix.linux-builder`.)
3. Confirm the other no-count branches still pass unchanged:
   `just test-vm braid-smartd-alert braid-ack-cleanup-pending`.
4. ASCII-output gate: `scripts/docs/check-output-ascii.py` over the new echo strings
   (all plain ASCII -- no change risk, but it runs in CI).
