# Plan: pin duplicate-target mountinfo as Busy::Unknown through `cmd_idle`

## Context

`braid idle` is a fail-closed autosuspend gate: any unknowable probe must map to
`Busy`, never to "allow suspend". Its mount-presence check reads
`/proc/self/mountinfo` and can fail in exactly three ways, all surfaced as
`MountInfoError` (`cli/src/mount_check.rs`):

- `Io` -- file unreadable
- `Malformed` -- a non-empty line doesn't parse
- `DuplicateTarget` -- two entries claim the configured mount point

ADR 016 (`docs/design/decisions/016-auto-suspend.md`, "Mount probe reads
`/proc/self/mountinfo` directly") states all three "surface as
`Busy(BusyReason::Unknown)`, exit 1, and block suspend. 'Don't know' never
becomes 'allow suspend'." `docs/commands/idle.md` documents the same
`busy: unknown (mountinfo: ...)` surface.

`cmd_idle` (`cli/src/idle.rs#cmd_idle`) collapses all three into one arm:
`Err(e) => return busy_unknown("mountinfo", e)`. Two `cmd_idle`-level
tests pin that arm today -- `mountinfo_read_failure_is_busy_unknown` (Io) and
`mountinfo_malformed_target_line_is_busy_unknown` (Malformed). **No
`cmd_idle` test pins the `DuplicateTarget` variant**, even though it is the one
mountinfo anomaly with a structurally distinct code path: two parse-clean
matches in `find_unique_target_entry`, not zero/garbage.

Why this gap matters (regression-resilience, not a present bug): a refactor
scoped to `is_btrfs_mounted` or to `cmd_idle`'s match arm that mapped
`DuplicateTarget` to "not mounted" (e.g. "pick the first match" / "an overmount
means the pool is effectively offline") would compile, keep **every** parser
test green (the helper still errors), keep the lone `is_btrfs_mounted` direct
test green (`is_btrfs_mounted_io_error_when_read_fails`), keep both existing
`cmd_idle` mountinfo tests green (Io/Malformed still hit the surviving `Err(e)`
arm), and silently flip a documented block-suspend case to allow-suspend.

This mirrors an established pattern in this file: each distinct wiring path is
pinned at the `cmd_idle` boundary even when a lower-level parser test exists
(see the `idle_when_scrub_{never,aborted,interrupted}` trio and especially
`pool_offline_when_non_btrfs_at_mount_point`, whose justification is
structurally identical to this one).

## The fix

Add one Rust unit test to `cli/src/idle.rs`'s `mod tests`, immediately after
`cli/src/idle.rs#mountinfo_malformed_target_line_is_busy_unknown`, so the three
mountinfo tests sit together. Reuse the existing fixtures already imported into
the module -- no new fixture, helper, or constructor is needed:

- `IdleMockFs::with_mountinfo(content)` (`cli/src/test_fixtures/idle.rs`) --
  its doc explicitly covers "parser-failure tests that keep the bad input inline
  at the call site", which is exactly what the malformed sibling does.
- `assert_idle_busy_unknown_prefix(result, "mountinfo:")`
  (`cli/src/test_fixtures/idle.rs`) -- prefix-only, matching both siblings.
  Do not over-assert the full `DuplicateTarget` Display string; the prefix is
  the user-facing contract the siblings pin.
- `idle_mp()` and `MockRunner::default()`.

The preamble follows the documented current form in
`docs/dev/testing.md#preamble-literal-line-comment-form`: a contiguous block of
`//` line comments directly above the `#[test]`, with the exact labels `Intent`
/ `Why it exists` / `Scenario`. This deliberately diverges from the adjacent
mountinfo siblings, which still use the older `/* ... */` block-comment form
with a `Why:` label -- the new test matches the convention rather than
perpetuating that drift.

Seed two well-formed btrfs lines for `/mnt/storage` with distinct mount IDs and
source devices so the body reads as two genuinely different mounts at one target
(only the mount-point field drives `DuplicateTarget`, but distinct fields
self-document the "overmount/rebind" scenario):

```rust
// Intent: a `/proc/self/mountinfo` body with two entries for the
//   configured target must propagate as Busy::Unknown, not silently
//   become PoolOffline or be resolved by picking one entry.
// Why it exists: ADR 016 and idle.md both name "ambiguous duplicate
//   target entries" as a suspend-blocking mountinfo error.
//   DuplicateTarget is the one mountinfo anomaly with a distinct code
//   path (two parse-clean matches, not zero/garbage), so the Io and
//   Malformed siblings above do not stand in for it. A refactor scoped
//   to is_btrfs_mounted or this match arm that mapped DuplicateTarget
//   to "not mounted" (e.g. "pick the first" / "an overmount means
//   offline") would compile, keep every parser test and both sibling
//   cmd_idle mountinfo tests green, and silently flip a documented
//   block-suspend case to allow-suspend.
// Scenario: an overmount or rebind landed a second mount at
//   /mnt/storage alongside the pool; autosuspend must refuse to guess
//   and block.
#[test]
fn mountinfo_duplicate_target_is_busy_unknown() {
    let runner = MockRunner::default();
    let fs = IdleMockFs::with_mountinfo(
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n\
         37 35 0:33 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk2 rw\n",
    );
    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_idle_busy_unknown_prefix(result, "mountinfo:");
    assert!(runner.requests().is_empty(), "{:?}", runner.requests());
}
```

Both lines parse cleanly and both have mount-point `/mnt/storage`, so
`find_unique_target_entry` sets `hit` on the first and returns
`Err(MountInfoError::DuplicateTarget { target: "/mnt/storage" })` on the second.
`fstype_at_mount` and `is_btrfs_mounted` propagate it via `?`; `cmd_idle` maps it
to `Busy(Unknown("mountinfo: ..."))`. The mount check fails before any
subprocess, so `runner.requests()` is empty.

## Why this shape and not more

- Adding this one test makes `cmd_idle` cover **all three** `MountInfoError`
  variants (Io, Malformed, DuplicateTarget) -- a complete, bounded set, since
  the enum has exactly three variants. Nothing else is missing on this axis.
- No `mount_check.rs`-level duplicate test is warranted: `is_btrfs_mounted` is a
  one-line wrapper that propagates the error unchanged via `?`, and
  `fstype_at_mount_errors_on_duplicate_target_entries` already pins the helper.
  The new `cmd_idle` test exercises that propagation end-to-end, so a direct
  `is_btrfs_mounted` duplicate test would be strictly redundant.
- Out of scope: other `find_unique_target_entry` callers (`probe.rs` via
  `fstype_at_mount_via_fs`, `preflight.rs` via `mount_entry_at_via_fs`). They
  have their own fail-closed contracts, but the finding and the ADR-016 /
  idle.md documentation are scoped to the idle autosuspend gate. Auditing every
  caller's duplicate-target wiring shares no root cause with this gap and would
  be scope creep.

## Files modified

- `cli/src/idle.rs` -- add the one `#[test] fn mountinfo_duplicate_target_is_busy_unknown`
  inside the existing `mod tests`. No other file changes (all fixtures already exist).

## Verification

1. **Run the new test plus the whole idle module** (confirms it passes green and
   doesn't disturb siblings):
   - `just test-rust` (project recipe), or a targeted run from `cli/`:
     `cargo test mountinfo_duplicate_target_is_busy_unknown` and
     `cargo test idle::tests`.
2. **Prove the guard actually bites** (TDD-style red check, per AGENTS.md
   "confirm they fail for the right reason"): temporarily edit
   `cli/src/idle.rs#cmd_idle` to special-case the variant, e.g.
   `Err(crate::mount_check::MountInfoError::DuplicateTarget { .. }) => false,`
   (treat duplicate as not-mounted -> `PoolOffline`). Confirm
   `mountinfo_duplicate_target_is_busy_unknown` fails with "got PoolOffline",
   that both sibling mountinfo tests and all parser tests still pass (proving the
   gap was real and only this test catches it), then revert the mutation.
3. **Lint/format gates**: `cargo fmt`/`cargo clippy` (or the project's `just`
   equivalents) clean; the test comment is ASCII-only per the repo convention.
