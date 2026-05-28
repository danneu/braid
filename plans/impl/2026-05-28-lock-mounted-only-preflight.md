# lock.md: mark mounted-only steps as conditional

## Context

`docs/commands/lock.md` step 2 says

> Checks that no btrfs exclusive operation (balance, device remove, etc.) is running

without noting that this gate runs only when the pool is mounted. In
`cli/src/lock.rs`, `plan_lock` builds a `Snapshot` (lock.rs:746-779):
`Snapshot::Probed` and `Snapshot::ProbeFailed` (mounted) both call
`preflight::require_lock_preflight` (lock.rs:789, 807); `Snapshot::Unmounted`
goes straight to `build_close_sets_uuid_scanned_fallback` (lock.rs:819-828)
without ever reading `/sys/fs/btrfs/<fsid>/exclusive_operation`. The skip is
correct -- there is no FSID to scope the sysfs read to on an unmounted
filesystem, and the kernel cannot have an exclusive op in flight without
one -- but the doc reads as if the gate is always applied. An operator who
runs `braid lock` on an already-unmounted pool during what looks like a
balance could plausibly be confused by the asymmetry.

The same conditionality applies to step 3 (`Unmounts ...`) and step 4
(`Runs btrfs device scan --forget ...`). Commit `fe81d5c` (`docs(lock):
document conditional btrfs forget`) already made step 4 conditional-aware
with the trailing clause `Skipped when there is nothing left to forget.`
`docs/commands/doctor.md:74` uses the same `Skipped when the pool is not
mounted` pattern. The ideal fix mirrors that established style for the
remaining asymmetric step (2). Step 3 does not need a clause because the
verb `Unmounts` is self-evidently conditional on mounted state -- adding
"Skipped when the pool is already unmounted" to "Unmounts the btrfs
filesystem ..." would be over-explicit. Step 4 is already done.

This is a documentation-only change. No code behavior changes.

## Change

`docs/commands/lock.md:36` -- step 2. Replace the current single-sentence
step with the same sentence plus a trailing "Skipped when ..." clause that
mirrors the fe81d5c / doctor.md style:

Before:

```
2. Checks that no btrfs exclusive operation (balance, device remove, etc.) is running
```

After:

```
2. Checks that no btrfs exclusive operation (balance, device remove, etc.) is running. Skipped when the pool is not mounted.
```

No other steps need updating:

- Step 1 (mountpoint check) always runs -- correct as-is.
- Step 3 (`Unmounts ...`) -- verb already conveys the conditional.
- Step 4 (`After a successful unmount, ...`) -- already conditional-aware
  (commit fe81d5c).
- Steps 5-6 (mapper classify + close, orphan scan) always run -- correct
  as-is.

The footer line 42 (`If the pool is already unmounted and all mappers are
already closed, lock reports "pool already locked"`) covers the no-op case
and does not need changes; the new step-2 clause closes the gap for the
"unmounted with open mappers" case where lock still does useful work.

## Files touched

- `docs/commands/lock.md` -- one line (line 36).

## Verification

- `mdbook build docs` from the repo root must succeed (mdbook-linkcheck
  validates the cross-link graph per `docs/book.toml`).
- Skim the rendered "What happens under the hood" section to confirm step
  2 reads cleanly alongside the existing step-4 "Skipped when ..." clause
  -- the two should feel like the same pattern.
- No code, tests, or fixtures are touched. No VM test or Rust test run is
  required.
