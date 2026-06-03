# Plan: rationale comment for the zero-missing precedence in `plan_remove_missing`

## Context

A code-review finding proposed reordering `plan_remove_missing` so
`validate_missing_id_target` runs *before* the `pool.missing_count == 0`
guard, on the theory that a live devid passed to `--missing-id` on a healthy
pool should be steered to `braid remove` instead of getting "no missing
devices detected."

That ordering is **deliberate, documented, and test-pinned**, not an
oversight:

- Commit `961e4bd0` (2026-04-27, "fix(cli): tighten transient validation
  wording") added a design doc (`2026-04-27-remove-missing-validation-wording.md`)
  whose Context section names this exact scenario and explicitly chooses to
  keep zero-missing precedence: *"In a healthy pool, if the operator passes a
  live devid as `--missing-id`, current behavior reports 'no missing devices'
  before the live-device validation can fire. Keep that branch explicit."*
- `plan_remove_missing_zero_missing_precedes_live_device_validation` pins the
  precedence (asserts the message contains "no missing devices detected" and
  *not* "live device").
- `plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing`
  pins the companion subtlety: the wording is keyed on `missing_count`, not
  `missing_devids.is_empty()`, so null-underlying hot-unplug pools aren't
  mislabeled as healthy.

The problem is purely **discoverability**: the rationale lives in a test
preamble and a (non-authoritative) plan doc, while the guard itself
(`remove_missing.rs:384`) carries no comment -- unlike its immediate neighbor,
the 2-disk RAID1 reject at `:401`, which has a full explanatory block. A fresh
reviewer reading `plan_remove_missing` top-to-bottom sees an uncommented
guard sitting in front of a more specific classifier and reasonably reads it
as a bug. The fix is a short rationale comment that dissolves this class of
finding. **No behavior change.**

(The reordering itself is rejected: besides reverting a deliberate decision,
the naive reorder also degrades the *unknown*-devid message -- `validate_missing_id_target`
returns "devid N is not a device in this pool" for unknown devids rather than
falling back to "no missing devices" -- which would break
`plan_remove_missing_preserves_preflight_notes_on_no_missing_devices`.)

## Change

Single file: `cli/src/remove_missing.rs`, inside `plan_remove_missing`.

Insert a `//` rationale comment immediately above the `if pool.missing_count == 0 {`
guard (currently line 384, between the UPS preflight block ending at `:382`
and the guard). Recommended text, 4-space indented to match the function body:

```rust
    // Ordered before validate_missing_id_target deliberately: on a healthy pool
    // there is no btrfs-MISSING device to remove, so any --missing-id (even a
    // live member's devid) reports "no missing devices" instead of falling
    // through to validate's "use `braid remove`" live-device hint. Keyed on
    // missing_count, not missing_devids.is_empty(), so null-underlying hot-unplug
    // pools (missing_count > 0, missing_devids empty) are not mislabeled healthy.
    // Pinned by plan_remove_missing_zero_missing_precedes_live_device_validation
    // and plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing.
    if pool.missing_count == 0 {
        return Err(PlanFailure::with_notes(
```

### Why this wording

- **Leads with the precedence** -- the exact thing the finding misread (guard
  intentionally ahead of the classifier, and *why*: healthy pool = no MISSING
  target, so the live-device hint is deliberately not reached).
- **Covers the `missing_count` vs `missing_devids` keying** -- the second
  non-obvious subtlety at this spot, and the natural "why this field?" question.
- **Points at both pinning tests by symbol name** (no line numbers), per the
  AGENTS.md "File References" convention -- greppable and drift-proof; one
  `rg plan_remove_missing_zero_missing_precedes_live_device_validation` lands on
  both the comment and the test.
- **ASCII `--`, no em-dashes**, matching the repo's writing-style rule.
- ~8 lines, consistent with the density of the adjacent `:401` block comment.

### Out of scope / do not do

- Do not reorder the guard and `validate_missing_id_target`.
- Do not change any error string, control flow, or test.
- Do not cite the plan doc path in the comment (plan docs are non-authoritative
  and may be cleaned up; the test names are the durable anchor).
- Do not run a formatter over the file (AGENTS.md "Formatting"); the edit is a
  hand-inserted comment only.

## Verification

- `just test-rust` -- confirms the crate still compiles and the two pinning
  tests (and the rest of the `remove_missing` suite) stay green. A
  comment-only change should not alter any test outcome; this is a
  regression guard that the edit was purely additive.
- Quick visual diff: the only change is the inserted comment block above
  `if pool.missing_count == 0 {`.

No VM tests, fixtures, or docs updates are required -- this touches neither
behavior, parser surface, nor any `docs/` page.
