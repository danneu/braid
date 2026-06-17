# Collapse the duplicate btrfs-profile classifier to a single source of truth

## Context

`braid status` reports the per-block-group-type btrfs profile (Data / Metadata /
System) on two surfaces: the JSON `profile` object and the human `Profile:` block.
Today these are computed by **two different builders over two different inputs**
that must stay byte-equivalent with nothing enforcing it:

- JSON: `summarize_df` builds a `ProfileSummary` via `profile_summary::from_df_entries`
  (parsed `BtrfsDfEntry`s), projects it to `ProfileJson`, and drops the summary
  (`cli/src/status.rs#summarize_df`, `#build_status`).
- Human: `format_status_human` re-derives a second `ProfileSummary` via
  `profile_summary::from_allocation`, which **re-parses the already-stringified**
  `AllocationEntry.profile` rows (`cli/src/status.rs#format_status_human`).

They agree today only by coincidence (both route through `summarize_profiles`, and
`AllocationEntry.profile == bg_profile.to_string()`). Any change to allocation-row
construction can silently desync the two surfaces. There is even a **latent live
divergence**: `summarize_df` sorts entries before building `allocation`, while
`from_df_entries` reads `df.entries` in original order -- so for 2+ distinct
*unknown* profile names in one block-group type, the human and JSON surfaces would
already order them differently (`summary_preserves_unknown_tail_order` pins the
order only on the `from_df_entries` side).

The deeper observation: `report.profile` (a `ProfileJson`, already the JSON
contract) **already carries the canonical deduped/ordered profile names**. The
human renderer ignores it and rebuilds the data from `allocation`. The redundancy
label it needs (`Mirrored`, `SameDisk`, ...) is a *pure function of those names*
(`classify_profiles`), not data that must be stored or threaded.

**Outcome:** make `report.profile` the single profile representation both surfaces
read. Delete the second builder and the `ProfileSummary`/`TypeProfile` types
entirely; compute the redundancy label at render time. This dissolves the
cross-surface invariant (one stored copy, read by both) rather than enforcing it,
and fixes the latent ordering divergence for free. This matches the codebase's own
stated principle two lines above the JSON profile build: "one classification pass
projects each disk into the JSON, verbose, and compact surfaces at once ... no
bridge map to keep them in sync" (`cli/src/status.rs#build_status`).

This was chosen over the finding's literal proposal (keep `ProfileSummary`, thread
it to the human formatter via `MountedExtras`) because it yields the simpler
end-state: fewer types, no `format_status_human` signature change, and a single
stored copy of the profile names instead of two kept in sync by `build_status`.

## Design

One representation, one classifier function:

- `ProfileJson` (in `cli/src/profile_summary.rs`) = the deduped, domain-ordered
  profile names per block-group type. Already serialized as the JSON `profile`
  field; now also the source the human `Profile:` block reads. (Optional polish,
  out of scope: rename to drop the `Json` suffix since it is no longer JSON-only.)
- `classify_profiles(&[String]) -> Redundancy` = pure presentational classifier,
  called only by the human renderer to choose the parenthetical label.
- `ProfileSummary`, `TypeProfile`, and the eager per-type `class` storage are
  deleted -- nothing reads them once the human side derives the label on demand.

## Changes

### `cli/src/profile_summary.rs`

- Delete `struct ProfileSummary`, `struct TypeProfile`, and
  `impl From<&ProfileSummary> for ProfileJson`.
- Keep `enum Redundancy` (now only the return of `classify_profiles`), `ProfileJson`
  (+ its `#[cfg(test)] uniform` helper), and `profile_display_order`.
- `summarize_profiles`: change return type from `TypeProfile` to `Vec<String>` --
  return just the deduped/ordered names (drop the `classify_profiles` call and the
  `TypeProfile` wrap). Rename to reflect it now only normalizes names
  (e.g. `normalize_profiles`).
- `classify_profiles`: make `pub(crate)`; add a `///` (new public boundary) saying
  it is the shared redundancy classifier the human renderer applies to the JSON
  profile names. Logic unchanged.
- `from_df_entries`: change return type to `ProfileJson`; build it directly from
  three `normalize_profiles(...)` calls (same per-`bg_type` filter as today).
  Update its `///`.
- Delete `from_allocation` and the now-unused `use crate::status::AllocationEntry;`.
- Tests: split the existing `from_df_entries` cases (single / RAID1 / mixed /
  GlobalReserve-omit / empty / RAID0 / RAID5 / unknown / unknown-tail-order) into
  (a) assertions on the returned `ProfileJson` name vectors and (b) direct
  `classify_profiles(&[...]) -> Redundancy` unit tests for each variant. Delete
  `from_allocation_matches_from_df_entries` (the invariant it guarded no longer
  exists). Keep the `entry(...)` helper.

### `cli/src/status.rs`

- Import (`cli/src/status.rs:21`): drop `ProfileSummary` and `TypeProfile`; keep
  `self`, `ProfileJson`, `Redundancy`.
- `struct DfSummary`: replace `profile_summary: ProfileSummary` with
  `profile: ProfileJson`; `summarize_df` sets `profile: profile_summary::from_df_entries(&df.entries)`.
  The `allocation` field is unchanged (still feeds the `Allocation:` size table).
- `build_status` report build: `profile: df_summary.as_ref().map(|s| s.profile.clone())`
  (replacing the `ProfileJson::from(&summary.profile_summary)` projection).
- `format_type_profile_human`: change the parameter from `&TypeProfile` to
  `&[String]`; compute the class internally via `profile_summary::classify_profiles(names)`.
  Behavior identical (`Mirrored | Unknown` render bare names; the other variants
  append their existing suffixes).
- Human `Profile:` block in `format_status_human`: gate on `report.profile` (instead
  of `report.allocation`) being present with at least one non-empty type vector, and
  iterate `profile.data` / `.metadata` / `.system`, passing each name slice to
  `format_type_profile_human`. This is the gate-flip from `allocation` to `profile`;
  in production both are co-populated by `summarize_df`, so output is unchanged.

### New behavioral test in `cli/src/status.rs` (guards the source-of-truth change)

The existing Profile-asserting tests supply *agreeing* `report.profile` and
`allocation`, so they pass whether the human block reads `report.profile` (new) or
rebuilds from `allocation` (old) -- they do not pin the contract this plan
establishes, leaving it open to silent reintroduction. Add one discriminating
formatter test whose two inputs **deliberately disagree**, asserting the human
output follows `report.profile`:

- A `StatusReport` with `status: StatusCode::Intact` (so the formatter does not
  early-return before the `Profile:` block),
  `profile: Some(profile_json(&["RAID1", "XENO", "FOOBAR"], &["RAID1"], &["RAID1"]))`,
  and `allocation: Some(...)` whose Data rows are ordered `FOOBAR, RAID1, XENO` -- a
  conflicting order, since rebuilding the Data row from allocation would yield
  `RAID1, FOOBAR, XENO`.
- `let human = format_status_human(&report, None, None, None);`
- Assert it follows `report.profile`:
  `assert!(human.contains("Data:      RAID1, XENO, FOOBAR (not fully redundant)"))`.
- Assert it does *not* follow allocation order:
  `assert!(!human.contains("RAID1, FOOBAR, XENO"))`.

This is structure-insensitive (asserts on rendered output, not on which function is
called), fails on the pre-change implementation, and passes only when the human
block's authority is `report.profile`. It doubles as the regression guard for the
latent unknown-tail ordering divergence noted in Context (`from_df_entries`
first-seen order vs `summarize_df`'s sorted allocation).

### Test fallout in `cli/src/status.rs` (minimal)

- The five Profile-asserting tests (`status_human_healthy_single`, the RAID1 case,
  `status_human_mixed_data_profile`, `status_human_unrecognized_profile_renders_verbatim`,
  `status_human_missing_type_renders_unknown`) **already set `report.profile`** to the
  exact `ProfileJson` they assert on, so they pass unchanged.
- `status_json_healthy` (`cli/src/status.rs:1932`): change
  `profile: Some(ProfileJson::from(&df_summary.profile_summary))` to
  `profile: Some(df_summary.profile.clone())`.
- Expected harmless ripple: ~28 disk-focused tests set `profile: Some(...)` with
  `allocation: None`; under the gate-flip they now render a `Profile:` block. No
  assertion breaks -- the only negative profile assertions are
  `!contains("Profile:")` (test sets `profile: None`, returns early) and
  `!contains("RAID1")` (test's profile is `single`/`DUP`, already renders a Profile
  block today). Verified by grep over every `!*contains(*Profile|RAID|single|DUP*)`.

## Verification

- `just test-rust` -- the full CLI unit suite. Confirms: the new profile-authority
  regression test (human follows `report.profile`, not `allocation`), the five
  existing human Profile render tests, the new `classify_profiles` unit tests and
  `ProfileJson`-shape tests in `profile_summary.rs`, `status_json_healthy`, and that
  the ~28 disk tests still pass with the gate-flip ripple.
- `just clippy` -- catches any now-dead imports/items (removed `AllocationEntry`,
  `ProfileSummary`, `TypeProfile`) and unused-variant warnings.
- Spot-check end-to-end equivalence: the human `Profile:` lines must byte-match the
  pre-change output for single (`Data: single (no redundancy)` etc.), RAID1, mixed
  (`single, RAID1 (not fully redundant)`), RAID5 (verbatim), and missing-type
  (`unknown`) -- all asserted by the existing tests above.
