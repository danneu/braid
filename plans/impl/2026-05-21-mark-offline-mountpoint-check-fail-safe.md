# Plan: Fail-safe `mark_offline` on mountpoint-check error

## Context

`cli/src/online_state.rs` exposes two finalizers that bracket a pool-lock
window: `mark_online` (post-mount lifecycle fixups) and `mark_offline`
(post-`cmd_lock` deactivation of `braid-online.service`).

Both consult `is_mountpoint` to gate their work. They handle the `Err`
case asymmetrically:

- `mark_online` (lines 266-272) treats `Err` as a hard early-return.
  When mountpoint state is unknown, it warns and does nothing -- the
  fail-safe direction is "don't activate."
- `mark_offline` (lines 338-357) warns on `Err` but falls through and
  still calls `systemctl_stop(BRAID_ONLINE_UNIT, false)`. The fail-safe
  direction here should be "don't deactivate" -- but the current code
  treats unknown the same as `Ok(false)` and proceeds.

The race the asymmetry creates: `cmd_lock_orchestrate_impl`
(`cli/src/lock.rs:1025-1027`) writes `done\n` to the stop coordinator
before calling `mark_offline`. ExecStop reentry treats `done\n` as
authoritative and exits 0. If `is_mountpoint` errors mid-shutdown
(`OnlineError::Spawn`) and the pool somehow remained mounted, the stop
transition fires anyway, ExecStop sees `done\n`, exits 0, and the unit
transitions to inactive over a still-mounted pool -- exactly the
corruption the coordinator protocol exists to prevent. The compound
likelihood is low (a successful `cmd_lock` already validated the
unmount), but the asymmetry contradicts both the sibling function and
the principle pinned in
[`docs/decisions/026-pool-lock-rust-owned.md:86-87`]: "Unknown snapshot
results warn instead of starting."

Intended outcome: `mark_offline` fails safely on `is_mountpoint` `Err`
the same way `mark_online` does, with a regression test pinning the
behavior.

## Recommended fix

### 1. Restore fail-safe symmetry in `mark_offline`

`cli/src/online_state.rs:338-357`. In the existing `match
ops.is_mountpoint(path)`, add `return Ok(())` to the `Err` arm after
the existing `eprintln!`. The new arm:

```rust
Err(e) => {
    eprintln!(
        "braid: WARNING: failed to check mountpoint {}: {e}",
        path.display()
    );
    return Ok(());
}
```

No other branch changes. The function's doc comment (lines 335-337)
already cites ADR 026 and the `done\n` protocol; extend it with one
line noting that an unknown mountpoint state is treated as
still-mounted to mirror `mark_online`'s fail-safe behavior.

### 2. Extend the test mock so `is_mountpoint` can fail

`cli/src/online_state.rs:404-465`. `RecordingOnlineStateOps::mounted`
is currently `Cell<bool>` and `is_mountpoint` always returns `Ok`.
Mirror the existing `bound_by` pattern
(`RefCell<Result<Vec<String>, StagedOnlineFailure>>`) so a staged
failure can be injected without growing the surface:

- Change the field type to `RefCell<Result<bool, StagedOnlineFailure>>`,
  initialized to `Ok(true)` in `new()`.
- `set_mounted(mounted: bool)` wraps in `Ok(mounted)`.
- Add `set_mountpoint_err(failure: StagedOnlineFailure)` that stores
  `Err(failure)`.
- `is_mountpoint` clones and maps via the existing
  `StagedOnlineFailure::into_online_error`.

Reuse the existing `StagedOnlineFailure::Spawn(String)` variant for
the test -- no new variant is needed, since the finding's described
scenario is exactly "the runner returns Spawn error mid-shutdown."

External callers of `set_mounted(false)` (`cli/src/lock.rs:1191`,
`cli/src/lock.rs:1248`, plus the seven plain `RecordingOnlineStateOps::new()`
sites in `cli/src/lock.rs`) keep working unchanged because the
signature stays the same.

### 3. Add the regression test

`cli/src/online_state.rs` test module (alongside the two existing
`mark_offline_*` tests at lines 760-784). One test, the standard
three-section preamble:

```rust
// Intent: mark_offline must not stop braid-online.service when the
// mountpoint check itself fails.
// Why it exists: cmd_lock_orchestrate writes `done\n` to the stop
// coordinator before mark_offline; ExecStop reentry would treat that
// marker as authoritative and exit 0, so a stop transition over an
// unknown mount state could leave the unit inactive over a live pool.
// Scenario: cmd_lock succeeded and the done marker is set, then the
// mountpoint check returns a Spawn error mid-shutdown.
#[test]
fn mark_offline_skips_systemctl_when_mountpoint_check_fails() {
    let cfg = cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#);
    let ops = RecordingOnlineStateOps::new();
    ops.set_mountpoint_err(StagedOnlineFailure::Spawn(
        "mountpoint spawn failure".into(),
    ));

    mark_offline(&cfg, &ops).unwrap();

    assert!(
        !ops.calls()
            .contains(&format!("stop {BRAID_ONLINE_UNIT} no_block=false")),
        "expected no systemctl stop after mountpoint check failure, got {:?}",
        ops.calls(),
    );
}
```

### 4. Document the stop-side fail-safe in ADR 026 and ADR 018

The new invariant -- "unknown post-lock mountpoint state leaves the
lifecycle owner active" -- is a load-bearing safety rule for plain
`braid lock`. Both ADRs currently describe the stop path as a
two-step "write `done\n`, then synchronously stop
`braid-online.service`" sequence with no failure branch. Without an
ADR update the rule is easy to regress later by a reviewer who
re-reads the docs and "fixes" what looks like a missing stop call.

**`docs/decisions/026-pool-lock-rust-owned.md`**, "Stop Coordinator +
Done Protocol" section (lines 108-128). After the existing paragraph
that describes writing `done\n` and then synchronously stopping
`braid-online.service`, add a short paragraph stating that
`mark_offline` re-checks `mountpoint -q` between those two steps and
treats a check failure (e.g. `OnlineError::Spawn` mid-shutdown) as
still-mounted: it warns and skips the stop, leaving
`braid-online.service` active. The operator can re-run `braid lock`
or `systemctl stop braid-online.service` to recover. This mirrors the
"unknown snapshot results warn instead of starting" rule already
documented in the "Snapshot Rule On `systemctl start`" section
(lines 73-87) for the activation side.

**`docs/decisions/018-systemd-lifecycle.md`** -- two touch points,
both required:

1. "On `lock`:" step 6 (line 138). Augment the step to capture the
   branch: the synchronous `systemctl stop braid-online.service`
   happens only when the post-cleanup mountpoint check confirms the
   mount is gone; if the check fails, Rust warns and skips the stop,
   leaving the unit active for the operator to retry. One sentence
   is enough -- the step-by-step is intentionally terse.
2. "`systemctl start/stop` inside held-resource windows" section,
   the `mark_offline` exception (line 176). The current text says
   "plain `braid lock`'s post-success `mark_offline` runs a
   synchronous `systemctl stop braid-online.service` without a
   stop-side snapshot. It is safe because [coordinator + done
   protocol]." Update the exception to also note that
   `mark_offline` skips the synchronous stop when the post-cleanup
   mountpoint check fails; the unit stays active and the operator
   retries. This is the normative rule future lifecycle edits will
   look up, so leaving it stale risks a later reviewer "fixing" the
   missing stop call.

The deeper rationale stays in ADR 026; ADR 018's edits just need to
be specific enough that a future reader can't re-derive the
fall-through behavior from the rule.

## Out of scope

- Updating `mark_online`'s already-correct `Err` handling.
- Restructuring `mark_offline` to mirror `mark_online`'s `let mounted = match ...; if mounted { return }` shape. Not required for the fix and would obscure the diff.
- Adding a `Mountpoint` variant to `StagedOnlineFailure`. `Spawn` covers the finding's scenario; non-spawn `is_mountpoint` errors are not required for the regression test.
- A NixOS VM test. Injecting `is_mountpoint` failure in a real VM would require mocking the binary; the unit test is the right altitude for a Low-severity fail-safe.

## Files touched

- `cli/src/online_state.rs` -- the `mark_offline` `Err` arm, doc comment, `RecordingOnlineStateOps::mounted` field + setters + `is_mountpoint` impl, one new test.
- `docs/decisions/026-pool-lock-rust-owned.md` -- "Stop Coordinator + Done Protocol" section gains a paragraph on the post-lock mountpoint-check fail-safe.
- `docs/decisions/018-systemd-lifecycle.md` -- "On `lock`:" step 6 gains one sentence on the fail-safe, and the `mark_offline` exception under "`systemctl start/stop` inside held-resource windows" (line 176) gains a sentence noting the mountpoint-check fall-through.

No other files change.

## Verification

1. `just test-rust` -- runs `cargo test`, including the new regression
   test and the two existing `mark_offline_*` tests. The pre-existing
   `cmd_lock_success_writes_done_then_calls_mark_offline_in_order` and
   `cmd_lock_failure_does_not_write_done_or_stop_online`
   (`cli/src/lock.rs:1240`, `cli/src/lock.rs:1205`) must still pass --
   they exercise the same mock setter surface this plan modifies.
2. Confirm the new test fails before the `return Ok(())` is added in
   step 1 (sanity check the test is exercising the right path), then
   passes after the fix.
3. Optional: `just test-vm lock-stops-bound-consumers braid-lock-coordinator-race`
   to confirm the existing locked-stop integration tests still pass.
   These don't simulate `is_mountpoint` failure but do exercise the
   `mark_offline` -> `systemctl stop braid-online.service` path
   end-to-end, so any regression in the happy path would show up.

No CLI manual page or README update is required: the user-visible
behavior of `braid lock` is unchanged in the success case, and the
warning text on `is_mountpoint` failure is already the same string the
function emitted before.
