# Plan: pin the pool-scoped, single-shot scrub probe in the multi-btrfs idle check

## Context

ADR 016 documents a deliberate asymmetry in the `braid idle` autosuspend gate
([`016-auto-suspend.md#scrub-probe-is-scoped-to-the-pool-mount-point`](../../docs/design/decisions/016-auto-suspend.md)):

- The **exclusive-op scan** is host-wide -- a busy op on *any* btrfs filesystem
  blocks suspend. Pinned by `idle_any_busy_blocks_suspend_multi_btrfs`
  (`cli/src/idle.rs`).
- The **scrub probe** is deliberately *not* host-wide -- `cmd_idle` runs
  `btrfs scrub status` against only the configured pool mount point, so "a scrub
  on a non-pool btrfs ... is not detected and does not block suspend."

The scrub side of that asymmetry has **no regression test**. The gap is real and
specific: every test that actually reaches the scrub probe seeds a *single* fsid
(via `IdleMockFs::with_exclop` -> `seed_btrfs_listing(&[IDLE_FSID])`), and every
existing *multi*-fsid test short-circuits at the sysfs scan *before* scrub is
reached (`idle_any_busy_blocks_suspend_multi_btrfs` returns `Busy(Exclop)`;
`idle_unknown_entry_notfound_is_fail_closed` returns `Busy(Unknown)`). So a future
change that made scrub host-wide -- spawning a `btrfs scrub status` probe per fsid
-- would compile and keep every current idle test green while silently changing
the documented suspend behavior.

`no_balance_or_replace_subprocess_calls` already asserts the request log is exactly
`[BtrfsScrubStatus { mount_point: idle_mp() }]`, but only with a single fsid, so it
does not exercise the multi-fsid fan-out dimension.

Intended outcome: one behavioral, structure-insensitive test that mirrors the
already-pinned exclop sibling for the scrub dimension -- proving the scrub probe
stays single-shot and pool-scoped even when the host exposes multiple btrfs
filesystems.

## Change

Add one `#[test]` to `cli/src/idle.rs` `mod tests`, placed immediately after its
sibling `idle_any_busy_blocks_suspend_multi_btrfs` so the two multi-btrfs tests
sit together.

No new fixtures, no new imports, no ADR edit. Every symbol used is already
`pub(crate)` and already imported in the test module (`IDLE_FSID`,
`IDLE_FSID_OTHER`, `IdleMockFs`, `idle_mp`, `idle_runner_with_scrub_finished`,
and `CmdRequest` via `use super::*`). The body is the sibling's fixture with the
second fsid flipped from `"balance"` to `"none"`, plus the request-log assertion.

The preamble is a contiguous block of `//` line comments per
[`testing.md#preamble-literal-line-comment-form`](../../docs/dev/testing.md) --
not the `/* */` block form the sibling test happens to use (that predates the
guidance).

```rust
// Intent: when the host exposes multiple btrfs filesystems that are all
//   idle, `cmd_idle` issues exactly one `btrfs scrub status` probe, scoped
//   to the configured pool mount point -- never one probe per fsid.
// Why it exists: the scrub probe is deliberately pool-scoped, not host-wide
//   (ADR 016, "Scrub probe is scoped to the pool mount point"): a scrub on a
//   non-pool btrfs is not detected and does not block suspend. The sibling
//   exclop rule (host-wide) is pinned by
//   `idle_any_busy_blocks_suspend_multi_btrfs`; this is its scrub-side
//   mirror. Every other test that reaches the scrub probe seeds a single
//   fsid, and every existing multi-fsid test short-circuits at the sysfs
//   scan before scrub is reached -- so a future change that made scrub
//   host-wide (a probe per fsid), or scoped it to the wrong mount point,
//   would compile and keep all current idle tests green while silently
//   changing the documented suspend behavior. Asserting the exact request
//   log -- one `BtrfsScrubStatus` keyed to `idle_mp()` -- fails closed on
//   both regressions: MockRunner records every request before dispatch, so
//   a second per-fsid probe lands in the log even when unmocked.
// Scenario: NixOS host with a btrfs root alongside the braid pool; both are
//   idle. autosuspend must conclude Idle after a single pool-scoped scrub
//   probe, ignoring the non-pool filesystem entirely.
#[test]
fn idle_scrub_probe_stays_pool_scoped_multi_btrfs() {
    let runner = idle_runner_with_scrub_finished();
    let fs = IdleMockFs::mounted_btrfs_only()
        .seed_btrfs_listing(&[IDLE_FSID, IDLE_FSID_OTHER])
        .seed_exclop(IDLE_FSID, "none")
        .seed_exclop(IDLE_FSID_OTHER, "none");

    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_eq!(result, IdleResult::Idle);
    assert_eq!(
        runner.requests(),
        vec![CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp()
        }]
    );
}
```

## Why this shape is ideal (and what is *not* needed)

- **Standalone test, not a merge into `no_balance_or_replace_subprocess_calls`.**
  The project's idle module is one-intent-per-test with a rich preamble; the
  exclop side already has its own dedicated multi-btrfs test. A dedicated
  scrub-side mirror keeps the symmetry legible and the regression rationale
  self-documenting. Folding the two fsid counts into one parameterized test would
  blur two distinct intents (no-extra-subprocess vs. scrub-stays-pool-scoped).
- **Assert the request log, not just the result.** `result == Idle` alone does not
  bite: a per-fsid fan-out where every fs is idle still returns `Idle`. The
  `requests() == [one probe @ idle_mp()]` assertion is what pins single-shot,
  pool-scoped behavior. It is robust because `MockRunner::run` pushes the request
  before dispatch (`cli/src/cmd.rs`), so an unmocked second probe still shows up.
- **No ADR change.** ADR 016 already documents the invariant, and its existing doc
  style does not cite test names back (the sibling exclop section does not cite
  `idle_any_busy_blocks_suspend_multi_btrfs`). The citation direction is
  test-preamble -> ADR, which this test follows.
- **No flake.nix registration.** This is a Rust `#[test]` unit test (`cargo test
  --lib`), not a NixOS VM test in `tests/`, so the `checks` registration rule does
  not apply.
- **Scope is exactly this one test.** A sweep of the idle test module against ADR
  016 shows every other documented branch (PoolOffline, all 7 exclops,
  short-circuit ordering, fail-closed sysfs/scrub/mountinfo paths, pseudo-dir skip,
  empty-listing, list_dir error, multi-btrfs any-busy) is already pinned. This is
  the sole uncovered invariant -- there is no sibling gap to bundle.

## Files

- `cli/src/idle.rs` -- add the one test in `mod tests` (only file changed).

## Verification

1. **Passes against current code:**
   ```
   cargo test --lib idle_scrub_probe_stays_pool_scoped_multi_btrfs
   ```
   then the full suite via `just test-rust`.

2. **Confirm the test actually bites (proves it closes the gap, not just passes).**
   Simulate the host-wide-scrub regression *conditionally on multiple fsids* --
   in `cmd_idle`, gate an extra
   `runner.run(&CmdRequest::BtrfsScrubStatus { mount_point: MountPoint::new("/other".into()) })`
   on `fs.list_dir("/sys/fs/btrfs")` reporting more than one fsid (so single-fsid
   fixtures keep issuing exactly one scrub request). Re-run `just test-rust`: the
   single-fsid request-log tests (`no_balance_or_replace_subprocess_calls`,
   `busy_unknown_on_scrub_*`) stay green, and only the new two-fsid test records a
   second request and goes red -- demonstrating it is the isolated pin for the
   fan-out regression. The gate is load-bearing: an *unconditional* second probe
   would break those single-fsid request-log tests too, so "only the new test
   fails" would not hold. Revert the injection.
