# Ideal salvage: make the non-fatal-status tests pin their own body contract

## Context

Commit `f64aa1e0` ("fix(cli): make status config-probe errors non-fatal") made
`build_status` keep a config-side probe error (hijacked `braid-<name>` mapper,
backing mismatch, LUKS-version drift, unreadable cryptsetup) **non-fatal**: the
report still comes back `Ok` with the pool summary intact, and the fault is
attributed to its disk as an advisory (plus an `Unknown` row for an unpooled
member). The same commit rewrote `status_surfaces_mapper_conflict` to assert the
`Ok`-with-advisory contract.

The preamble of that test promises the non-fatal contract keeps "capacity,
scrub, and per-disk detail" from blanking. **But the assertions don't enforce
it.** Today they cover only `status`, `present_count`, the advisory, and the
present/unknown disk row. Those are not enough:

- `status` and `present_count` are derived from the pool-membership join
  (`probe_pool` + btrfs filesystem show), computed **independently** of the
  df/usage/scrub gather path. Proven by `build_status_df_cmd_failure_tolerant`,
  which asserts `status == Intact` and `present_count == Some(3)` while
  `capacity.is_none()` and `allocation.is_none()`.
- So the real gap is a regression that keeps the pool **mounted and intact**
  (status and counts preserved) but drops the body -- e.g. an early-return or a
  refactor that skips the capacity/allocation/scrub gather when a probe error is
  present. That would leave `StatusCode::Intact`/`present_count` set while
  blanking the body, and **today's test would still pass.** (A regression that
  instead bailed to `not_mounted_status` flips status to `NotMounted` and
  `present_count` to `None`, so the existing assertions already catch it -- it
  is not the case worth guarding here.)

The cited finding proposed adding `assert!(built.report.capacity.is_some())`.
The ideal version is slightly broader and reuses the file's existing idioms:
assert the **whole mounted body** (capacity + allocation + scrub) renders, in
**both** non-fatal tests (the gap is identical in the sibling
`status_unpooled_probe_failure_renders_unknown`), via a small profile-agnostic
helper extracted from an existing one.

Intended outcome: each non-fatal test actually verifies the "body does not
blank" contract its preamble already claims, with zero production-code change
and zero behavior change to existing tests.

## The change (all in `cli/src/status.rs`, test module `mod tests`, test-only)

### Step 1 -- extract a profile-agnostic body helper (pure refactor)

The existing `assert_pool_sections_retained` bundles a **RAID1-specific**
profile assertion with the capacity+allocation checks, so it is not reusable for
the two non-fatal tests (both are single-disk pools, not RAID1). Split the
profile-agnostic part out:

```rust
fn assert_capacity_and_allocation_retained(built: &BuiltStatus) {
    assert!(
        built
            .report
            .allocation
            .as_ref()
            .is_some_and(|allocation| !allocation.is_empty()),
        "allocation: {:?}",
        built.report.allocation
    );
    assert!(built.report.capacity.is_some());
}

fn assert_pool_sections_retained(built: &BuiltStatus) {
    assert_profile_json(&built.report.profile, &["RAID1"], &["RAID1"], &["RAID1"]);
    assert_capacity_and_allocation_retained(built);
}
```

- Place it next to the sibling helpers `assert_scrub_and_balance_retained` /
  `assert_pool_sections_retained`.
- The assertions and their order are **identical** to today's
  `assert_pool_sections_retained`, so its existing callers
  (`build_status_device_stats_cmd_failure_tolerant`,
  `build_status_device_stats_parse_failure_tolerant`) are unaffected.
- Match the surrounding style: these test helpers carry **no** `///` doc comment;
  do not add one (an optional one-line `//` noting "profile-agnostic body check"
  is fine).

### Step 2 -- pin the body in both non-fatal tests

In `status_surfaces_mapper_conflict` and
`status_unpooled_probe_failure_renders_unknown`, right after the existing
`assert_eq!(built.report.present_count, ...)` line, add:

```rust
// The non-fatal contract keeps the whole mounted body rendering, not just
// the status code: capacity, allocation, and scrub survive even though one
// member's config probe errored. status/present_count come from the
// membership join, computed independently of the df/usage/scrub gather (see
// build_status_df_cmd_failure_tolerant), so they alone would not catch a
// mounted report that kept status and counts but dropped the body sections.
assert_capacity_and_allocation_retained(&built);
assert_scrub_and_balance_retained(&built);
```

- `assert_scrub_and_balance_retained` already exists and is profile-agnostic
  (`last_scrub.is_some()` + `balance.is_some()`); reuse it as-is to cover the
  preamble's "scrub".
- Both tests already mock the full mounted stack (`BtrfsFilesystemDfJson`,
  `BtrfsFilesystemUsageRaw`, `BtrfsDeviceUsageRaw`, `BtrfsScrubStatus`,
  `BtrfsDeviceStatsJson`), so capacity/allocation/`last_scrub` are populated on
  the healthy pool. The new assertions pass today and fail only if the body
  blanks -- exactly the regression class being guarded.

## Why this shape

- **Reuse + dedup over a one-off `capacity.is_some()`**: gives the contract a
  name parallel to `assert_scrub_and_balance_retained`, and the new helper is
  reused inside `assert_pool_sections_retained` (3 call sites, clears the
  rule-of-three).
- **Profile-agnostic**: single-disk non-fatal tests can assert the body without
  inheriting the RAID1 profile expectation.
- **Covers the full body** (capacity + allocation + scrub), matching the
  preamble's "capacity, scrub, and per-disk detail" rather than just one field.

## Files

- `cli/src/status.rs` -- test module only: one helper extraction + two two-line
  assertion additions. **No production code changes.**

## Deliberately out of scope

- No change to `build_status` or the non-fatal handling -- the behavior is
  already correct; this is test hardening only.
- Do **not** fold scrub into `assert_pool_sections_retained` -- that would add a
  new assertion to existing RAID1 tests (collateral risk) for no gain here.
- No preamble rewrite required; the existing "Why it exists" wording becomes
  accurate once the assertions enforce it.
- No expansion beyond the two tests that pin the non-fatal contract; other
  mounted tests already assert the body through the helpers.

## Verification

- Primary: `just test-rust`.
- Fast inner loop (from `cli/`):
  - `cargo test status_surfaces_mapper_conflict`
  - `cargo test status_unpooled_probe_failure_renders_unknown`
  - `cargo test build_status_device_stats` -- confirms the
    `assert_pool_sections_retained` refactor didn't break its existing callers
    (`build_status_device_stats_cmd_failure_tolerant` /
    `build_status_device_stats_parse_failure_tolerant`).
- Teeth check (optional, throwaway): temporarily point one non-fatal test's
  `BtrfsFilesystemUsageRaw`/`BtrfsFilesystemDfJson` mock at an error fixture and
  confirm `assert_capacity_and_allocation_retained` now fails -- proving the new
  assertion guards the blanking regression rather than passing vacuously. Revert
  after.

## Risk

Minimal: additive test assertions plus a behavior-preserving helper extraction;
no production code touched.
