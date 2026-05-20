# Plan: delete dead `pool::evict_present_device`

## Context

`pool::evict_present_device` was the shared "evict a live device" helper used by
`braid remove`. Commit `bc64bb8` ("wip: migrate remove + remove-missing +
membership enrich (phase 3a)") inlined the helper into `RemovePlan::execute`
and added a defense-in-depth UUID-probe gate on the trailing `cryptsetup
close`. The helper was left in place at that stage, presumably to keep its
two unit tests as a behavioral pin during the migration. The migration has
now shipped through several follow-up commits (`41f0462`, `db3627d`,
`b301300`, `2c6fc40`) -- the helper has no callers, on any branch in this
tree, and the inlined path is now the only production code that exercises
this sequence.

The current state leaves three problems:

1. ~140 lines of `pub` production code (`evict_present_device` plus the
   `EvictRunner` fixture and two `evict_present_device_*` tests) compiled
   into the binary and maintained against behavior that nothing dispatches
   to.
2. Seven stale prose references to `evict_present_device` scattered across
   `remove.rs`, `recover.rs`, `docs/principles.md`, and three test files.
   Future readers will assume the helper is load-bearing for `braid
   remove`.
3. The two `evict_present_device_*` tests assert behavior (close-failure
   warn-row sequencing, busy-retry on EBUSY) that is **partially**
   covered at the helper boundary in `cli/src/mapper_close.rs`. The
   existing `close_mapper_best_effort_*` tests pin the *return-value
   and request-count contracts* (success bool, retry counts) but
   discard captured stderr (`run_best_effort` in
   `cli/src/mapper_close.rs:135-147` invokes `capture_with_color` then
   throws away the captured string). Before the evict tests can be
   deleted without coverage loss, the *row sequencing* assertions
   (`[wait]`...`[warn]` on close failure; `[wait]`...`[ok]` after
   busy-retry success) need to migrate from the evict tests onto the
   matching `close_mapper_best_effort_*` tests at the real owner.

Intended outcome: the helper, its fixture, and its tests are gone; prose
references are either deleted (when they're vestigial) or rewritten to
point at the live code (when the surrounding sentence still has a real
point to make).

## Scope and edits

### `cli/src/mapper_close.rs` (do this first)

Before deleting the evict tests, migrate the *row sequencing* assertions
they carry onto the matching `close_mapper_best_effort_*` tests at the
real owner. Two changes:

- **Refactor `run_best_effort` (`cli/src/mapper_close.rs:135-147`) to
  return the captured stderr alongside the bool.** Today it discards
  the captured string. Change the signature to e.g. `fn
  run_best_effort(runner: &MockRunner) -> (bool, String)` so callers
  can assert on the row contents.
- **Extend two existing tests with sequencing assertions:**
  - `close_mapper_best_effort_returns_false_without_retry_on_non_busy`
    (`cli/src/mapper_close.rs:212-220`) -- the 1:1 successor of the
    deleted `evict_present_device_close_failure_emits_warn_row`. Add
    assertions that the captured output contains
    `[wait] disk disk2: locking...` and a matching
    `[warn] disk disk2: lock failed (...)` row, and that the wait row
    precedes the warn row. Pattern-match the existing deleted test at
    `cli/src/pool.rs:1822-1830`.
  - `close_mapper_best_effort_retries_busy_then_succeeds`
    (`cli/src/mapper_close.rs:171-182`) -- the 1:1 successor of the
    deleted `evict_present_device_retries_on_busy_then_succeeds`. Add
    `wait` and `ok` string variables (`[wait] disk disk2: locking...`
    and `[ok]   disk disk2: locked`), assert both are present in the
    captured output, and assert `captured.find(wait) <
    captured.find(ok)` so the helper-level wait/ok sequence after
    busy-retry is actually pinned (the deleted evict test only
    checked `[ok]`, but the Context section above promises full
    `[wait]` -> `[ok]` sequencing migration, and the helper is the
    right owner for it).

These additions preserve the behavioral pin at the helper that actually
emits the rows. Only after they're in place can the
`evict_present_device_*` tests be deleted without coverage loss.

### `cli/src/pool.rs`

- **Delete** the helper and its leading doc comment: lines 577-653
  (`/// Shared helper: evict a live (present) device from the pool.` ...
  through the closing `}` of `pub fn evict_present_device`).
- **Delete** the `EvictRunner` fixture in the test module:
  lines 1728-1796 (the doc comment "Custom runner for the trimmed
  `evict_present_device` test." through the closing `}` of
  `impl CommandRunner for EvictRunner`).
- **Delete** the two tests:
  - `evict_present_device_close_failure_emits_warn_row` (lines 1798-1831).
  - `evict_present_device_retries_on_busy_then_succeeds` (lines 1833-1872).
- **Cleanup** at the top of the file (deleting the helper makes these
  unused -- the compiler will surface them as `unused_imports`):
  - `cli/src/pool.rs:2` -- delete the line `use
    crate::mapper_close::close_mapper_best_effort;` entirely. The
    `close_mapper_best_effort` call at the old `pool.rs:650` was its
    sole consumer; no other code in `pool.rs` uses it.
  - `cli/src/pool.rs:4-7` -- remove the `Sleeper` token from the
    `progress::{...}` use group. The bare `Sleeper` bound at the old
    `pool.rs:604` was its sole consumer; the surviving `progress::Sleeper`
    references at lines 506 and 961 work through the existing `self`
    import. Keep `self, ProgressOutput,
    run_device_remove_with_progress, run_replace_with_progress,
    run_with_progress`.
- **Cleanup** any test-module `use` lines that become unused after the
  fixture is gone (e.g. `std::sync::atomic::{AtomicU32, Ordering}`,
  `std::sync::{Arc, Mutex}`, `crate::progress::NoopSleeper` -- only
  remove ones the compiler flags). Let `cargo build`/`cargo clippy`
  drive this.

### `cli/src/remove.rs`

- **Line 385-390** -- trim the stale comment. The current text reads:

  ```
  // Execute -- inlined so the close has a UUID-probe gate (the
  // defense-in-depth double-drift probe specified in the plan's
  // "Double-drift defense-in-depth UUID probe" section). Original
  // `evict_present_device` did balance + remove + close as one
  // call; we keep balance + remove inline and gate the close on a
  // probe of the journaled identity.
  ```

  Replace with a comment that retains the UUID-probe rationale but
  drops the historical reference *and* the non-durable "in the plan"
  pointer. Suggested form -- cite the local `probe_observed_mapper_uuid`
  call site, not external prose:

  ```
  // Execute. The trailing close is gated on the
  // `probe_observed_mapper_uuid` check below -- a defense-in-depth
  // re-probe of the journaled identity at the observed mapper, so
  // we don't tear down a foreign dm slot an operator opened under
  // the same mapper between plan and execute.
  ```

- **Line 2493-2502** -- the test comment at `pre_journal_target_hot_unplug_message`
  references `evict_present_device_target_null_underlying_classifies_hot_unplug`,
  a test that no longer exists. Drop the "replaces the old helper-level"
  sentence; keep the `Intent` / `Scenario` body untouched.

### `cli/src/recover.rs`

- **Line 14943-14957** -- the multi-line `Intent` comment for
  `cmd_recover_remove_with_null_underlying_target_preserves_membership`
  describes "Layer 1 (evict_present_device fail-closed)" as a sibling of
  the recover-layer fix. With Layer 1 inlined into `RemovePlan::execute`,
  the parenthetical is misleading. Rephrase to "the same phantom-success
  class that `RemovePlan::execute` fail-closes at the helper boundary"
  or just drop the parenthetical. Preserve the rest of the preamble --
  the `Intent` / `Why` / `Scenario` framing is still accurate.

### `docs/principles.md`

- **Line 89** -- the `[wait]`-row closing rules cite
  ``pool::evict_present_device``'s trailing LUKS close as the canonical
  example of a `[warn]`-closed best-effort. The example needs a durable
  location. Replace with `mapper_close::close_mapper_best_effort`, which
  is the helper that actually emits the `[warn]` row (see
  `cli/src/mapper_close.rs:84-105`). The surrounding sentence about
  `wait_for_kernel_replace_to_finish`'s status-poll error stays.

### Test-file prose comments

These are *comment-only* edits; no behavior change.

- **`tests/cli/braid-remove-disk.py:169-171`** -- the "Principle 13:
  `pool::evict_present_device` closes the LUKS mapper after the
  device-remove succeeds." comment can be rewritten to point at the
  inlined sequence, e.g. "Principle 13: `braid remove` closes the LUKS
  mapper after the device-remove succeeds (see the
  `close_mapper_best_effort` call inside `RemovePlan::execute` in
  `cli/src/remove.rs`)." Keep the `[wait]`/`[ok]` assertions below
  untouched.
- **`tests/cli/remove-inhibits-suspend.py:14-27`** -- the "Topology
  choice" preamble says "forces cli/src/pool.rs::evict_present_device
  to run pool_balance_single() *before* btrfs device remove." Rewrite to
  reference the inlined sequence in `cli/src/remove.rs`'s
  `RemovePlan::execute` (which still runs `pool_balance_single` first
  in the `remaining == 1` branch). The remaining/skipped rationale
  stays.
- **`tests/cli/remove-inhibits-suspend.nix:13-27`** -- mirror of the
  Python file's topology comment. Apply the same rewrite.
- **`tests/cli/braid-remove-disk-busy.py:9`** -- preamble says
  "pool.rs intentionally treats `cryptsetup close` as best-effort".
  Rewrite to cite the real owner:
  `mapper_close::close_mapper_best_effort` (the helper that emits the
  warn-row and exits 0), invoked from `RemovePlan::execute` in
  `cli/src/remove.rs`.
- **`tests/cli/braid-remove-disk-busy.nix:7`** -- mirror "pool.rs
  treats `cryptsetup close` as best-effort". Apply the same rewrite.

## Files to be modified

- `cli/src/mapper_close.rs`
- `cli/src/pool.rs`
- `cli/src/remove.rs`
- `cli/src/recover.rs`
- `docs/principles.md`
- `tests/cli/braid-remove-disk.py`
- `tests/cli/braid-remove-disk-busy.py`
- `tests/cli/braid-remove-disk-busy.nix`
- `tests/cli/remove-inhibits-suspend.py`
- `tests/cli/remove-inhibits-suspend.nix`

## Reused functions / pin locations

These continue to be the durable owners of the behavior the deleted code
duplicated:

- `cli/src/mapper_close.rs:70-105` -- `close_mapper_best_effort` (and its
  `close_mapper_with_retry` helper). This is the actual production code
  that emits the `[wait]`/`[ok]`/`[warn]` close-mapper rows, and the
  retry-on-EBUSY logic the deleted `evict_present_device_retries_on_busy_then_succeeds`
  test asserted.
- `cli/src/mapper_close.rs:155-220` -- the four `close_mapper_best_effort_*`
  unit tests. After the augmentation described in the
  `cli/src/mapper_close.rs` section above:
  - `_returns_true_on_success` (no augmentation needed).
  - `_retries_busy_then_succeeds` (1:1 supersedes
    `evict_present_device_retries_on_busy_then_succeeds` once the
    `[ok]   disk disk2: locked` row assertion is added).
  - `_returns_false_after_persistent_busy` (no augmentation needed).
  - `_returns_false_without_retry_on_non_busy` (1:1 supersedes
    `evict_present_device_close_failure_emits_warn_row` once the
    `[wait]` -> `[warn]` sequencing assertions are added).
- `cli/src/remove.rs:391-450` -- the live inlined balance + remove +
  UUID-probe + close sequence. Behavior pinned end-to-end by the
  `braid-remove-disk` VM test.
- `cli/src/probe_mapper_uuid.rs::probe_observed_mapper_uuid` -- the
  defense-in-depth UUID probe gating the close; the `remove.rs` comment
  rewrite retains the rationale for why this exists.

## Verification

1. `just test-rust` -- compiles cleanly (no `unused_imports` or
   `dead_code` warnings in `pool.rs`'s test module), and all
   `mapper_close` and `pool::tests` cases pass.
2. `cargo clippy --all-targets` -- no new warnings.
3. `just test-vm braid-remove-disk remove-inhibits-suspend` -- both VM
   tests still pass (comments-only edits, no behavior change expected).
4. `git grep -F "evict_present_device" -- cli docs tests/cli` --
   returns zero matches. The grep is scoped to the in-scope tracked
   trees: `plans/impl/` is a historical record that is intentionally
   left untouched, and `plans/wip/humble-nibbling-chipmunk.md` is an
   unrelated WIP plan whose stale reference is out of scope for this
   change.
