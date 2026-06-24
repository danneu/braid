# Plan: correct the stale single-survivor ENOSPC-skip comment

## Context

In `cli/src/remove_missing.rs`, the `plan_remove_missing` function guards the
relocation-space preflight (`check_relocation_space`) behind
`if pool.devices.len() >= 2`, skipping it when only one present device survives.
The comment above that guard justifies the skip with:

> Skip when only 1 present device survives: in 2-device RAID1, the survivor
> already has all data (every chunk is mirrored). This does not match the
> reproduced relocation-failure mode.

That justification is **stale**. Git archaeology shows why:

- `bf2dff40` (Feb 26) added the skip + comment. At that time there was no
  2-device reject above it, so a 2-device pool with 1 present + 1 missing
  genuinely reached the skip and the "survivor already has all data" claim was
  accurate.
- `00395563` (May 11, "fix(remove-missing): reject 2-disk RAID1 + 1 missing at
  preflight") inserted the `pool.total_devices == 2 && pool.devices.len() == 1
  && pool.missing_count == 1` reject **above** the skip, plus an authoritative
  scope block describing the multi-missing case -- but did **not** update the
  skip comment.

Net effect today: the 2-device case the comment describes is rejected before it
ever reaches the skip, so the **only** single-survivor state that reaches the
`devices.len() >= 2` guard is the multi-missing one (e.g. `total_devices == 3`,
`devices.len() == 1`, `missing_count == 2`). In that state the survivor does
**not** hold every chunk -- data mirrored only across the simultaneously-dead
devices is already lost -- so the comment's stated reason for skipping is false
for the exact branch that exercises it.

The **behavior is correct and intentional** (documented in the scope block at the
2-device reject, and delegated to the kernel min-devices check +
`device_remove_error`); only the comment misdescribes why the skip is safe.
This is a comment-only correction -- no behavior change, no new tests.

## The change

Single edit in `cli/src/remove_missing.rs`, function `plan_remove_missing`,
immediately above the `if pool.devices.len() >= 2` relocation-space guard.

Keep the first paragraph (the "why the check exists" lines) unchanged. Replace
only the second paragraph (currently lines 456-458) -- the stale "in 2-device
RAID1, the survivor already has all data" justification.

**Current:**

```rust
    // Skip when only 1 present device survives: in 2-device RAID1, the
    // survivor already has all data (every chunk is mirrored). This does
    // not match the reproduced relocation-failure mode.
```

**Replacement:**

```rust
    // Skip when only 1 present device survives (devices.len() == 1). The
    // clean 2-device RAID1 + 1 missing case -- where the survivor mirrors
    // every chunk and no relocation is needed -- is already rejected by the
    // 2-device guard above, so the only single-survivor state that reaches
    // here is multi-missing (total_devices > 2, >= 2 missing). There the
    // survivor is NOT guaranteed to hold every chunk: data mirrored only
    // across the simultaneously-dead devices is already lost and cannot be
    // relocated, so a relocation-space preflight has nothing to prove. Per
    // the scope block above, that case is deliberately delegated to the
    // kernel min-devices check and device_remove_error (pool.rs), not
    // proven safe here.
```

### Why this wording

- **Names the actual reachable branch** (multi-missing, `total_devices > 2`),
  not the pre-empted 2-device branch -- the core correction the finding asks for.
- **Cross-references the reject above descriptively** ("the 2-device guard
  above" / "the scope block above") rather than by line number, per the project
  citation convention ([doc-citations.md](../../docs/dev/doc-citations.md)), so a
  future maintainer who moves or edits that guard sees the dependency and the
  comment cannot silently re-stale the same way.
- **Aligns with the existing authoritative scope block** at the 2-device reject,
  which already states the survivor "is not guaranteed to mirror every chunk"
  and names `device_remove_error (pool.rs)` as the runtime backstop. The skip
  comment now tells the same story instead of contradicting it.
- **ASCII only** (`--`, `>=`, `NOT`), matching the surrounding comments. Rust
  comments are exempt from `check-output-ascii.py`, but the house style here is
  already ASCII.

## Scope boundaries (deliberately excluded)

- **No behavior change.** The guard, the reject, and the multi-missing
  delegation are all correct and intentional; only the comment is touched.
- **No new test.** The skipped-preflight outcome is internal control flow;
  asserting "the preflight was skipped" would be structure-sensitive and is not
  warranted for a comment fix. The observable multi-missing behavior is already
  covered: the parallel user-facing message in `format_remove_missing_confirm`
  is gated on `missing_count == 1` and routes multi-missing to "Pool will remain
  degraded", pinned by the `format_remove_missing_confirm` multi-missing test
  (the `(1 present, 2 missing)` / `(2 present, 2 missing)` cases that assert the
  "Surviving disk already has all data" string is absent). `device_remove_error`
  has its own decode tests in `pool.rs`.
- **First comment paragraph and the `(see tests/repro/)` reference** are
  accurate and left as-is.
- **`remove.rs`** uses a separate `check_single_survivor` path on a healthy
  (non-degraded) pool where the multi-missing concern cannot arise -- not a
  sibling of this comment, left untouched.

## Verification

Comment-only, so verification confirms nothing else drifted and existing
behavior is intact:

1. `just test-rust` -- full Rust suite passes unchanged (proves no accidental
   code edit; the `remove_missing` and `format_remove_missing_confirm` tests in
   particular stay green).
2. `cargo clippy` (or the repo's clippy recipe) -- still clean.
3. Manual read-back: confirm the new comment's claims hold against the control
   flow -- the 2-device reject precedes the guard, and the multi-missing
   1-survivor case (`total_devices > 2`, `devices.len() == 1`) is the only
   single-survivor state reaching `if pool.devices.len() >= 2`.
