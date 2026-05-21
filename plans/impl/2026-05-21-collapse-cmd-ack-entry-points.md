# Plan: Collapse the `cmd_ack` / `cmd_ack_impl` dual entry point

## Context

`cli/src/ack.rs` exposes two entry points for the same logic:

- `cmd_ack` (`cli/src/ack.rs:10-17`) -- pub one-line wrapper that forwards
  to `cmd_ack_impl(..., &stop_beeper)`.
- `cmd_ack_impl` (`cli/src/ack.rs:19-25`) -- the real implementation,
  which takes a `&dyn Fn()` stop-beeper hook.

The wrapper exists only so that tests which don't care about beeper
behavior can call `cmd_ack` without supplying a hook. To keep that
shortcut from shelling out to real `systemctl` during `cargo test`,
`stop_beeper` is split with `cfg`:

- `#[cfg(not(test))] fn stop_beeper()` (lines 214-229) -- runs
  `systemctl stop braid-alert.service`.
- `#[cfg(test)] fn stop_beeper() {}` (lines 254-255) -- no-op shim.

The actual smell is the `cfg(test)` behavior shim. It exists only so
the 15 tests that go through the public `cmd_ack` wrapper don't shell
out at `cargo test` time. The dual-entry shape itself is the project's
existing convention -- `cli/src/lock.rs:977-984` and
`cli/src/lock.rs:1036` use exactly the same `cmd_lock` /
`cmd_lock_impl` split for sleeper injection. Diverging from that here
would just substitute a new pattern (publicly exposing the production
hook) for a one-line cleanup.

Goal: kill the `cfg(test)` shim; align with the existing `cmd_lock` /
`cmd_lock_impl` convention; make every test pass an explicit hook so the
two entry points stop being interchangeable from a reader's
perspective.

## Approach

Adopt the `cmd_lock` / `cmd_lock_impl` shape:

1. **Keep public `cmd_ack` as a no-hook wrapper.** Signature unchanged
   from today (`cli/src/ack.rs:10-15`). Body becomes:

   ```rust
   pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
       runner: &R,
       fs: &F,
       mount_point: &MountPoint,
       paths: &StatePaths,
   ) -> Result<(), AckError> {
       cmd_ack_impl(runner, fs, mount_point, paths, &stop_beeper)
   }
   ```

   `main.rs:870` does not change. The public API surface does not
   change. Tests that go through `cmd_ack` today will be migrated to
   call `cmd_ack_impl` directly (step 4), so production is the only
   `cmd_ack` caller.

2. **Keep `cmd_ack_impl` private** (`cli/src/ack.rs:19-25`). Name
   retained to match `cmd_lock_impl` (`cli/src/lock.rs:1036`); the
   existing rustdoc references at `cli/src/ack.rs:167`, `181`, `193`,
   `196` remain accurate.

3. **Collapse the `cfg`-gated `stop_beeper` pair into a single
   non-cfg function.** Production is now the only caller of
   `stop_beeper` (tests don't reach `cmd_ack`'s wrapper anymore after
   step 4), so the `#[cfg(test)] fn stop_beeper() {}` shim has no
   reason to exist. Delete both gated variants; keep one ungated:

   ```rust
   fn stop_beeper() {
       // body moved verbatim from the current `cfg(not(test)) fn stop_beeper`
   }
   ```

   `format_systemctl_stop_failure` (`cli/src/ack.rs:231-249`) and its
   two existing unit tests (`cli/src/ack.rs:1751-1769`, `1781-1789`)
   are untouched -- they continue to cover the production hook's
   failure formatting and do not depend on the `cfg` gating.

4. **Migrate the 15 tests that currently call `cmd_ack` to call
   `cmd_ack_impl`** with an explicit no-op hook. Call sites:
   `cli/src/ack.rs:321, 476, 509, 546, 576, 606, 634, 1246, 1290,
   1334, 1531, 1577, 1656, 1694, 1732`. Each becomes
   `cmd_ack_impl(..., &ack_noop_beeper)`.

   Add the helper to the per-scope ack fixture file at
   `cli/src/test_fixtures/ack.rs` (alongside `ack_mp`, `ack_write_latch`,
   `ack_fs_btrfs`, etc., which already follow `pub(crate)` there):

   ```rust
   pub(crate) fn ack_noop_beeper() {}
   ```

   Then re-export it through the facade by adding `ack_noop_beeper`
   to the `pub(crate) use ack::{ ... }` list at
   `cli/src/test_fixtures.rs:139-144`. Test sites import it
   through the existing `use crate::test_fixtures::{ ... }` block at
   `cli/src/ack.rs:293-299`.

5. **No behavioral changes.** Production still invokes the same
   `systemctl stop braid-alert.service` through the same
   `stop_beeper` body. Test assertions on beeper-call counts remain
   identical -- those 22-ish tests already pass their own counting
   closures to `cmd_ack_impl` and will continue to.

## Files changed

- `cli/src/ack.rs` -- collapse the two `stop_beeper` cfg variants into
  one ungated function; migrate the 15 test sites from `cmd_ack` to
  `cmd_ack_impl(&ack_noop_beeper)`; add the import for `ack_noop_beeper`.
- `cli/src/test_fixtures/ack.rs` -- add `pub(crate) fn ack_noop_beeper() {}`.
- `cli/src/test_fixtures.rs` -- add `ack_noop_beeper` to the `pub(crate)
  use ack::{ ... }` re-export at lines 139-144.

`cli/src/main.rs` does not change.

## Doc-comment obligation

Per `AGENTS.md` ("Doc Comments"), `cmd_ack` and `cmd_ack_impl` should
each have a `///` justifying intent at this boundary. `cmd_ack` is
public; `cmd_ack_impl` already has rustdoc on related items (lines
167-196). Add a one-liner on each:

```rust
/// Production entry point. Wraps `cmd_ack_impl` with the real
/// `stop_beeper` hook; tests call `cmd_ack_impl` directly with their
/// own hook.
pub fn cmd_ack ...

/// Injectable-hook variant used by tests to observe `stop_beeper`
/// firing order. Production goes through `cmd_ack`.
fn cmd_ack_impl ...
```

## Verification

- **Static migration check.** `rg -n 'cmd_ack\(' cli/src/ack.rs` must
  produce zero matches. The `pub fn cmd_ack<R, F>(` definition has `<`
  between `cmd_ack` and `(` so the regex never matches it; the wrapper
  body calls `cmd_ack_impl(`, not `cmd_ack(`. Any match is a missed
  test migration -- the ungated `stop_beeper` is best-effort
  (eprintln on failure, no `Result`), so a leftover `cmd_ack(...)`
  test would still pass `just test-rust` while shelling out to real
  `systemctl` on the developer's host. Today the same query prints 15
  matches (the test call sites at lines 321, 476, 509, 546, 576, 606,
  634, 1246, 1290, 1334, 1531, 1577, 1656, 1694, 1732); after
  migration it must print zero.
- `cargo check -p braid-cli` -- compile-only smoke check.
- `just test-rust` -- covers the full Rust unit-test surface, including
  every `cmd_ack*` test in `cli/src/ack.rs` (both the migrated ones and
  the ~22 tests already on `cmd_ack_impl`) and the
  `format_systemctl_stop_failure` tests. Must pass unchanged.
- `just test-vm monitor-lifecycle` -- the production wiring check.
  `tests/module/monitor-lifecycle.py:78-80` asserts `braid ack`
  stops `braid-alert.service` through real systemd; this is the only
  test that exercises `cmd_ack` -> `stop_beeper` -> `systemctl stop
  braid-alert.service` end-to-end. Required because `just test-rust`
  alone would pass even if the wrapper or `stop_beeper` body
  regressed.

No new tests are needed: behavior is preserved exactly, and the existing
surface already covers both the beeper-fires and beeper-does-not-fire
paths (e.g.
`cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper` at
`cli/src/ack.rs:1367`,
`cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path` at
`cli/src/ack.rs:345`).

## Out of scope

- The wider question of whether `ack` should grow more injected hooks
  (e.g. for `mark_alert_cleanup_pending`). This plan only kills the
  `cfg(test)` shim and unifies test entry points; it does not
  preemptively add hook points.
- Renaming `cmd_ack_impl` to something more descriptive
  (`cmd_ack_with_beeper`, etc.). Keeping `_impl` matches
  `cmd_lock_impl` and preserves the existing comment/test-name
  references at `cli/src/ack.rs:167`, `181`, `193`, `196`, `655`,
  `1019`, `1151`, `1367`, `1435`.
