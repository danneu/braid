# Plan: prove `require_lock_preflight` keys off the given fsid

## Context

`braid lock` is teardown (unmount + close LUKS). Before it proceeds it calls
`require_lock_preflight(fs, fsid)` (`cli/src/preflight.rs:627`), which builds
`/sys/fs/btrfs/{fsid}/exclusive_operation` in the shared reader
`read_exclop_for_fsid` (`cli/src/preflight.rs:146-153`) and hard-fails if any
exclusive op is in flight. The `fsid` is probed dynamically at the call site
(`cli/src/lock.rs:774` `probe_fsid` -> `:791`/`:809`), so the per-fsid path
derivation is load-bearing: reading the wrong filesystem's busy state could let
lock unmount mid balance/replace.

A review finding noted this derivation has no behavioral assertion. The
lock-level tests `lock_refuses_when_exclusive_op_active` (`cli/src/lock.rs:3389`)
and `lock_refuses_when_balance_paused` (`:3414`) drive the busy branch through
the fixture MockFs in `cli/src/test_fixtures/shared.rs:169`, which matches **any**
path ending in `/exclusive_operation` and ignores the fsid segment -- so they
pass regardless of which fsid the code reads.

Verification refined the gap: the `preflight.rs` unit MockFs
(`cli/src/preflight.rs:638-700`) is exact-path-keyed, so its existing tests
(`:1569-1633`) already catch an empty/hardcoded/wrong fsid (a mismatched path
returns `NotFound` -> "cannot read exclusive operation status", failing the
busy-op assertions). What no test covers is fsid **insensitivity** -- a
regression where the path stops varying with the `fsid` argument (caching the
first fsid, reading a captured outer fsid, or otherwise selecting a fixed
path). With only one fsid file ever seeded, such a bug is invisible. The
intended outcome: one cheap discriminating test that fails iff the path stops
tracking the `fsid` it was given.

## The fix

Two edits, both in `cli/src/preflight.rs`, both test-only. No production code
changes.

### 1. Let the unit MockFs model multiple fsids

In the `#[cfg(test)] mod tests` `impl MockFs` block (`cli/src/preflight.rs:643-654`),
add a chainable additive seeder and have the existing constructor delegate to it,
so the `/sys/fs/btrfs/{fsid}/exclusive_operation` path format stays in one place
and all current `with_sysfs(FSID, ...)` call sites keep identical behavior:

```rust
fn with_sysfs(fsid: &str, content: &str) -> Self {
    Self::empty().with_sysfs_entry(fsid, content)
}

fn with_sysfs_entry(mut self, fsid: &str, content: &str) -> Self {
    self.files.insert(
        format!("/sys/fs/btrfs/{fsid}/exclusive_operation"),
        content.to_owned(),
    );
    self
}
```

Match the surrounding test-helper style: the sibling builders (`with_mountinfo`,
`with_mountinfo_error`) carry no doc comment, and `#[cfg(test)]` items are
exempt from the doc-comment rule, so omit one here.

### 2. Add the discriminating test

Insert after `lock_preflight_rejects_on_unrecognized_value` (ends
`cli/src/preflight.rs:1633`), before the `--- require_mutation_preflight tests ---`
divider. Seed one busy fsid and one idle fsid in a single MockFs, then assert
each fsid resolves to its own file:

```rust
#[test]
// Intent: require_lock_preflight reads the exclusive_operation file for the
//   exact fsid it is given, not a fixed or sibling fsid's file.
// Why it exists: the path is /sys/fs/btrfs/{fsid}/exclusive_operation; a
//   regression that stopped tracking the fsid argument (hardcoded, cached, or
//   captured-outer fsid) would read the wrong filesystem's busy state. Lock
//   teardown is fail-closed precisely to avoid unmounting mid balance/replace,
//   so the per-fsid derivation is a real safety gate. The lock fixtures
//   (test_fixtures/shared.rs) match any path ending in /exclusive_operation and
//   cannot prove this -- assert it here in the fsid-keyed unit lane.
// Scenario: two btrfs filesystems present -- one mid-balance, one idle. Locking
//   the idle pool must pass; locking the balancing pool must refuse.
fn lock_preflight_keys_off_given_fsid() {
    const OTHER_FSID: &str = "11111111-2222-3333-4444-555555555555";
    let fs = MockFs::with_sysfs(FSID, "balance\n").with_sysfs_entry(OTHER_FSID, "none\n");

    // Idle fsid -> Ok proves it read OTHER_FSID's "none", not FSID's "balance".
    assert!(
        require_lock_preflight(&fs, OTHER_FSID).is_ok(),
        "expected idle fsid to pass preflight"
    );

    // Busy fsid -> refusal proves it read FSID's "balance", not OTHER_FSID's "none".
    let err = require_lock_preflight(&fs, FSID).unwrap_err();
    assert!(
        err.contains("in progress"),
        "expected busy refusal for the balancing fsid, got: {err}"
    );
}
```

Why this discriminates: a correct implementation reads `OTHER_FSID` -> "none" ->
`Ok` and `FSID` -> "balance" -> `Err`. A fsid-insensitive implementation reads
the same fixed file for both calls, so one of the two opposite-outcome assertions
must fail. The two existing single-fsid busy tests cannot distinguish these
cases; this one does.

## Why this shape (rejected alternatives)

- **Do not tighten the `shared.rs` fixture mock to be fsid-aware.** The
  per-fsid invariant is owned by preflight, not lock orchestration (matches the
  project's "check the invariant at the layer that owns it" heuristic). The
  fixture's `ends_with` match is also relied on by `replace.rs`, `remove.rs`,
  and `remove_missing.rs` (`with_excl_op(...)`), which deliberately do not model
  fsid; making it fsid-aware would force those unrelated tests to thread a
  correct fsid for no behavioral reason.
- **Do not change the lock-level tests** (`lock_refuses_when_*`). They correctly
  cover lock orchestration (refuse without unmounting/closing) and should keep
  using the fsid-blind fixture.
- **Idle-vs-busy over busy-vs-absent for the two fsids.** Seeding `OTHER_FSID`
  as "none" (idle) rather than leaving it absent makes the negative path a
  successful read+parse with the semantically opposite outcome, which reads as
  "keys off the fsid" rather than "fails to find a file."
- **One test over two.** A single test asserting both directions on one shared
  MockFs expresses the selection invariant most directly.

## Files

- `cli/src/preflight.rs` -- add `with_sysfs_entry` + delegate `with_sysfs`
  (test module, ~`:643`); add `lock_preflight_keys_off_given_fsid` (~`:1633`).

## Verification

- `just test-rust` -- runs the CLI unit tests (crate `braid-cli`), including the
  new test and the existing `require_lock_preflight` suite. This is the only
  required lane: the change is test-only, touches no parser and no production
  behavior, so VM tests (`just test-vm`) and the parser canary
  (`just test-parsers`) are not implicated.
- Optional TDD confidence check before finalizing: temporarily hardcode the
  path in `read_exclop_for_fsid` to ignore its `fsid` argument and confirm the
  new test fails (the `is_ok()` assertion breaks), then revert.
