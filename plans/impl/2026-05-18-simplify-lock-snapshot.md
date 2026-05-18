# Plan: simplify `LockSnapshot` in `plan_lock`

## Context

`plan_lock` (cli/src/lock.rs:690) currently uses a public 2-variant enum
`LockSnapshot { Full(PoolState), FsidOnly { fsid, probe_error } }` to thread
its probe-state decision into the close-set construction match. The shape
has three problems that compound:

1. The unmounted branch (lock.rs:749-756) reuses `FsidOnly` by stuffing
   dummies: `fsid: String::new()` and a synthetic `ProbeError::PoolDevice`
   built solely to satisfy the variant's shape. Neither field is ever
   read -- the consuming match only touches them inside
   `if pool_was_mounted { ... }`, which the unmounted construction can't
   satisfy by construction. The synthetic error is unreachable, not
   ill-typed; the `mapper` field of `ProbeError::PoolDevice` is already
   overloaded across the codebase (`probe_pool` at probe.rs:412 and
   `probe_fsid` at probe.rs:515 / probe.rs:532 also set it to a
   mount-point string for FSID and mountinfo failures), so the
   "synthetic value goes unobserved" argument is what carries the
   simplification -- not a claim that the field is globally
   `/dev/mapper/...`-only.
2. Because both dummy fields are unreachable in the consuming match
   (`fsid` and `probe_error` are only read inside `if pool_was_mounted`
   at lock.rs:777-781, and the synthetic value is only built when
   `pool_was_mounted == false`), the enum carries
   `#[allow(dead_code)]` at lock.rs:38. The attribute is hiding genuine
   dead writes, not nudging the compiler about deliberately reserved
   fields.
3. The match arm at lock.rs:776 immediately re-checks `pool_was_mounted`
   to decide what to do, even though the snapshot variant already
   encodes mounted-ness by construction. The type system has the answer;
   the code asks the runtime anyway.

`LockSnapshot` is also declared `pub` despite having no consumer outside
`cli/src/lock.rs` (confirmed by full-tree grep across `cli/src/` and
`cli/tests/`; the only outside-of-`lock.rs` reference is one doc comment
in `cli/src/test_fixtures/lock.rs`).

Goal: replace the 2-variant `pub` enum with a private 3-variant enum that
encodes the three real states distinctly, drop the dead-code synthesis,
and let the consuming match drop its `pool_was_mounted` re-check. Net
effect: less code, fewer footguns, type-level proof of which preflight
path runs.

## Critical files

- `cli/src/lock.rs` -- definition, construction, consumer match.
- `cli/src/test_fixtures/lock.rs` -- one doc comment refers to the
  `FsidOnly` arm by name.

## Changes

### 1. Replace the enum definition (cli/src/lock.rs:34-50)

Replace the existing `pub enum LockSnapshot { Full | FsidOnly }` block
with a private 3-variant enum. Drop the `#[allow(dead_code)]` attribute.

```rust
/// Snapshot of the pool's live state at lock-planning time. Variants
/// encode the three real branches: a successful per-device probe, a
/// mounted pool whose per-device probe failed (FSID still proved
/// ownership), and an unmounted pool that bypasses mounted-pool
/// probing and FSID preflight (per-candidate UUID probing still runs
/// during mapper cleanup).
enum Snapshot {
    /// Per-device probe succeeded; close-set classification routes
    /// through observed LUKS UUIDs.
    Probed(PoolState),
    /// Pool is mounted and FSID matched, but per-device probing
    /// failed. `fsid` feeds preflight; `probe_error` is quoted in the
    /// fallback warning.
    ProbeFailed { fsid: String, probe_error: ProbeError },
    /// Pool is not mounted. Skips the mounted-pool `probe_pool` call
    /// and the FSID preflight gate; UUID-scanned mapper cleanup still
    /// runs via `build_close_sets_uuid_scanned_fallback` to close any
    /// orphan braid-* mappers left behind from a previous unlock
    /// (each candidate is verified by `cryptsetup status` +
    /// `luksUUID` before being added to the close set).
    Unmounted,
}
```

Rename rationale:
- `Snapshot` (unqualified, private) reads cleanly inside `lock.rs` and
  there are no other `Snapshot` types in the crate (verified by
  grep).
- `Probed` / `ProbeFailed` / `Unmounted` describe what happened, not the
  data shape, so the variants are self-documenting against the doc
  comment.

### 2. Rewrite the snapshot construction (cli/src/lock.rs:713-756)

Collapse the current `if pool_was_mounted { match probe_pool(...) } else
{ LockSnapshot::FsidOnly { ...dummies... } }` into a structurally
identical block that returns the new variants:

```rust
let snapshot = if pool_was_mounted {
    match probe_pool(runner, fs, &mount_point) {
        Ok(pool) => Snapshot::Probed(pool),
        Err(ProbeError::NotBtrfs { mount_point: mp, fstype }) => {
            return Err(LockError::Failed(format!(
                "{mp} is mounted but fstype is {fstype}, not btrfs"
            )));
        }
        Err(
            probe_error @ (ProbeError::Cmd(_)
            | ProbeError::Parse(_)
            | ProbeError::PoolDevice { .. }
            | ProbeError::UnsupportedLuksVersion { .. }
            | ProbeError::MapperConflict { .. }
            | ProbeError::MapperBackingMismatch { .. }
            | ProbeError::MapperBackingResolveError { .. }
            | ProbeError::MountInfo(_)),
        ) => {
            let fsid = probe_fsid(runner, fs, &mount_point)
                .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
            Snapshot::ProbeFailed { fsid, probe_error }
        }
    }
} else {
    Snapshot::Unmounted
};
```

The unmounted branch becomes a single literal -- no synthesized
`ProbeError::PoolDevice`, no empty `fsid`. Both dead writes vanish.
Preserve the existing explicit per-variant routing of `ProbeError`
(no catch-all) and the comment block at lock.rs:716-720 that explains
the routing, but update its trailing reference to `FsidOnly path` as
part of the text-edit sweep in change 4 below (the rename target is
`ProbeFailed path`).

### 3. Rewrite the consuming match (cli/src/lock.rs:761-792)

The three variants now map 1:1 to the three behaviors. The
`pool_was_mounted` re-check at lock.rs:763 (`if pool_was_mounted && let
Some(fsid)`) and at lock.rs:777 (`if pool_was_mounted`) both drop -- the
variant already proves mounted-ness.

```rust
let close_set = match &snapshot {
    Snapshot::Probed(pool) => {
        if let Some(fsid) = &pool.fsid {
            preflight::require_lock_preflight(fs, fsid)
                .map_err(LockError::Failed)?;
        }
        build_close_sets_full(
            runner, fs, pool, membership,
            &mut notes, &mut skipped_mappers, &mut cleanup_uncertain,
        )
    }
    Snapshot::ProbeFailed { fsid, probe_error } => {
        notes.push(PreviewNote::Warn(uuid_scanned_fallback_warn_body(
            probe_error,
        )));
        preflight::require_lock_preflight(fs, fsid)
            .map_err(LockError::Failed)?;
        build_close_sets_uuid_scanned_fallback(
            runner, fs, membership,
            &mut notes, &mut skipped_mappers, &mut cleanup_uncertain,
        )
    }
    Snapshot::Unmounted => {
        build_close_sets_uuid_scanned_fallback(
            runner, fs, membership,
            &mut notes, &mut skipped_mappers, &mut cleanup_uncertain,
        )
    }
};
```

Behavior preservation argument:
- `Probed` is only constructed when `pool_was_mounted == true`, so
  dropping the outer `pool_was_mounted &&` guard cannot change behavior;
  the inner `Some(fsid)` check is unchanged.
- `ProbeFailed` is only constructed when `pool_was_mounted == true`, so
  dropping the `if pool_was_mounted` guard makes the warn + preflight
  unconditional, matching today's mounted-branch behavior.
- `Unmounted` is only constructed when `pool_was_mounted == false`. In
  the current code that hits the `FsidOnly` arm with `pool_was_mounted
  == false`, which skips the warn + preflight and proceeds directly to
  `build_close_sets_uuid_scanned_fallback`. The new arm is identical.

`pool_was_mounted` remains a local variable because `LockPlan` still
carries the field (lock.rs:794-799, read by `compile_lock_steps` at
lock.rs:219 and the clean-state print at lock.rs:263). The simplification
is local to the match -- the plan struct does not change.

### 4. Update doc-comment references to the old variant names

These are pure text edits inside doc/comment lines; no code semantics.

- `cli/src/lock.rs:716-720` -- the inline routing comment inside
  `plan_lock` currently ends "...a real configuration error cannot be
  silently masked by the FsidOnly path." Change `FsidOnly path` to
  `ProbeFailed path`.
- `cli/src/lock.rs:804` -- the doc comment on `build_close_sets_full`
  currently reads "Close-set construction for the `LockSnapshot::Full`
  arm." Change to "Close-set construction for the `Snapshot::Probed`
  arm."
- `cli/src/lock.rs:3001` -- test preamble "Intent: in
  LockSnapshot::Full, a drifted member mapper ..." Change to "Intent:
  in `Snapshot::Probed`, a drifted member mapper ...".
- `cli/src/lock.rs:3047` -- test preamble "Intent: in
  LockSnapshot::Full, the forget_devs set ..." Change to "Intent: in
  `Snapshot::Probed`, the forget_devs set ...".
- `cli/src/test_fixtures/lock.rs:155-158` -- the doc comment for
  `lock_with_fsid_probe_mocks` mentions "the FsidOnly probe surface" and
  "the FsidOnly fallback". Rewrite to refer to "the probe-failed
  fallback" (the fixture seeds the mounted-but-probe-failed path).

### 5. Confirm `LockSnapshot::Full` test in mounted-and-probe-failed
seeding still drives the new `Snapshot::ProbeFailed` arm

No code change here -- this is a verification note. The fixture
`lock_with_fsid_probe_mocks` seeds per-device probes; tests that
intentionally override one to fail trigger the `Snapshot::ProbeFailed`
construction at lock.rs:739-742 today. The construction guard set
(matched `ProbeError` variants) is preserved verbatim in change 2, so
those tests continue to land on the renamed variant with no behavior
delta. Confirm during `just test-rust` (see verification).

## Out of scope (deliberate)

- `build_close_sets_full` function name (lock.rs:811) and its 8
  `super::build_close_sets_full(...)` test call-sites. These reference
  the function, not the variant. Renaming to `build_close_sets_probed`
  for naming consistency is a separate sweep; the current plan keeps
  the function name to bound the diff.
- The 9 `full_arm_*` test function names (lock.rs:3013, 3056, 3093,
  3151, 3197, 3402, 3453, 3519, plus one more under `tests`). Same
  reasoning -- these are stable test labels, not variant references.
- `unmounted_fallback_*` test at lock.rs:1791 -- unaffected, still
  exercises the unmounted code path through `plan_lock`.

If the user wants the broader naming sweep, run it as a separate
follow-on commit so this plan stays focused on dissolving the
dead-code / type-fib problem.

## Verification

1. **Type check + lint**: `cargo check -p braid-cli` (compiler will
   surface any missed rename or syntax issue). `cargo clippy -p
   braid-cli -- -D warnings` to catch any newly unused imports.
2. **Rust unit tests**: `just test-rust` -- exercises the
   `full_arm_*`, `unmounted_fallback_*`, and FSID-fallback tests
   inside `cli/src/lock.rs` (including the dedicated
   `uuid_scanned_fallback_warn_body_contains_pinned_substrings` test at
   lock.rs:3344, which still receives a real `ProbeError::PoolDevice`
   constructed inside the test body at lock.rs:3346-3349 -- unaffected).
3. **VM coverage for end-to-end behavior**: `just test-vm` (lock-related
   suite). The behavior-preservation argument under change 3 is the
   substantive claim; VM tests of `braid lock` against mounted +
   unmounted pools verify it externally.
4. **Confirm dead-code attribute is gone**: `grep -n
   '#\[allow(dead_code)\]' cli/src/lock.rs` should not return the
   former line-38 hit (other unrelated `#[allow(dead_code)]` may exist;
   target the lock-snapshot context).
5. **Confirm no stale variant names remain**:
   `rg -n 'LockSnapshot|FsidOnly' cli/src/lock.rs cli/src/test_fixtures/lock.rs cli/tests`
   should return zero hits after the rename. An empty result confirms
   both that `pub enum LockSnapshot` is gone (public-surface check) and
   that no `FsidOnly`-named string survives in code, comments, or test
   preambles.
