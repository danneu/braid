# Make `enrich_from_pool_state` infallible and drop its dead report

## Context

A review finding (Low/Simplicity) flagged that
`membership::enrich_from_pool_state` is `?`-propagated post-commit at
`cli/src/replace.rs` (`ReplacePlan::execute`) even though the function body
is infallible by construction. The fallible `Result<EnrichmentReport,
MembershipError>` signature implies a post-commit failure mode that does not
exist: if the body ever grew a real `Err` arm, a completed (irreversible)
`btrfs replace start` would turn into a hard `ReplaceError`, contradicting the
best-effort intent the surrounding comment promises.

Investigation widened the picture:

- The `?` trap recurs at **three** post-commit sites, not one:
  `ReplacePlan::execute`, and **both** enrichment calls in `AddPlan::execute`
  (bootstrap and live-pool). A fourth caller, `cmd_unlock`, already wraps the
  call in a `match` with warn-and-continue -- so today the four sites handle a
  never-occurring error three different-but-equivalent ways, and a reader
  cannot tell from the call sites whether enrichment can fail.
- The `EnrichmentReport` return value is **dead in production**: it is
  discarded at all four call sites (`let _ = ...` at three, `Ok(_report)` at
  unlock). The only `.foreign` reads are in comments. `braid doctor` obtains
  foreign UUIDs from the sibling pure helper `foreign_luks_uuids`, not from
  this report. The doc comment's rationale for returning the report ("rather
  than routing it through a thread-local") is therefore obsolete -- a later
  doctor refactor routed around it.

**Intended outcome:** make the type tell the truth. `enrich_from_pool_state`
becomes infallible (`-> ()`) and stops returning a report no one reads. This
dissolves the latent post-commit trap at all four sites at once, deletes a
vestigial struct, and unifies foreign-detection on the single
`foreign_luks_uuids` helper. The key invariant established: if a future editor
ever needs enrichment to fail, they must restore `Result`, which forces the
compiler to surface every call site for an explicit handling decision --
converting today's silent trap into a checked one.

Zero runtime behavior change: the per-UUID foreign warning and the membership
mutation are preserved. The only code path removed (`unlock`'s "failed to
enrich" warning arm) is unreachable today.

## Approach

Drop both the `Result` wrapper and the `EnrichmentReport` return from
`enrich_from_pool_state`; delete the now-unused struct; collapse the four call
sites; re-point the three unit tests onto `foreign_luks_uuids`.

### Core change -- `cli/src/membership.rs`

- **`enrich_from_pool_state`**: change signature to
  `pub fn enrich_from_pool_state(membership: &mut PoolMembership, pool: &PoolState)`
  (no return). Keep the body's two loops verbatim -- compute `foreign` via
  `foreign_luks_uuids` for the per-UUID `eprintln!` warning, then the
  `by_uuid_mut` devid/added_at refresh. Delete the final
  `Ok(EnrichmentReport { foreign })`.
- **Delete the `EnrichmentReport` struct** (no remaining consumer).
- **Rewrite the `enrich_from_pool_state` doc comment** to braid's
  why-at-the-boundary standard, stating the invariant precisely (NOT as "no
  I/O" -- the helper does write to stderr): it has no recoverable error path
  because it does no pool probing, output parsing, or state-file I/O (those
  fallible steps live in `probe_pool` / `save_membership` at the call sites)
  and never inserts into membership (fail-closed, update-only); it may emit
  transient foreign-UUID warnings via `eprintln!` and otherwise only refreshes
  in-memory `devid` / `added_at`. That is why it returns nothing and is safe to
  call best-effort post-commit without `?`. Drop the obsolete "report alongside
  the mutation rather than a thread-local" paragraph, and note that foreign
  UUIDs are surfaced transiently here and persistently by `braid doctor` via
  `foreign_luks_uuids`.
- Leave `foreign_luks_uuids` itself unchanged (still the shared
  enrichment+doctor join); touch its doc comment only if it references the
  report.

### Call sites

All three `?` sites become plain statements; the `unlock` match collapses.

- `cli/src/replace.rs` (`ReplacePlan::execute`):
  `membership::enrich_from_pool_state(&mut target_membership, &pool_after);`
  Remove the stale "EnrichmentReport.foreign is intentionally discarded here"
  comment (there is no report). The outer best-effort/probe comment stays
  accurate.
- `cli/src/add.rs` (`AddPlan::execute`, both the bootstrap and live-pool
  calls): same statement form against `final_membership`; remove the two
  "EnrichmentReport.foreign is intentionally discarded here" comments.
- `cli/src/unlock.rs` (`cmd_unlock`): collapse the inner
  `match enrich_from_pool_state { Ok/Err }` to a single call followed by the
  existing `save_membership` error handling; delete the now-impossible
  `Err(e) => "Warning: failed to enrich pool membership"` arm. Update the
  "Three outcomes are tolerated" comment: the third bullet "enrich/save
  failure" becomes "save failure" (enrichment can no longer fail); the
  probe-error and `mounted: false` bullets are unchanged.

### Tests -- `cli/src/membership.rs`

The three `enrich_from_pool_state_*` unit tests
(`..._known_uuid_with_new_devid_updates_in_place`,
`..._known_uuid_stamps_missing_added_at`,
`..._foreign_live_uuid_does_not_admit`):

- Call `enrich_from_pool_state(&mut m, &pool);` as a statement (drop
  `.expect("enrichment succeeds")` and the `let report = ...` binding).
- Replace `report.foreign` assertions with `foreign_luks_uuids(&m, &pool)`
  (invariant across the call -- enrichment never inserts, so the foreign set
  is identical before and after). This asserts the same property against the
  real production surface doctor uses.
- Keep every existing assertion on `m` (devid refreshed in place, `added_at`
  stamped, foreign UUID not admitted / membership otherwise unchanged) --
  those are the load-bearing behavioral checks and they are unaffected.

## Explicitly out of scope

- `foreign_luks_uuids` stays as the one foreign-detection API; doctor's
  `foreign_luks_uuid` check is untouched.
- The per-UUID foreign `eprintln!` warning behavior is preserved (the
  `2026-05-19-doctor-foreign-luks-uuid` plan explicitly calls for retaining
  it).
- `MembershipError` stays -- it is still returned by `load_membership`,
  `save_membership`, and `PoolMembership::insert`.
- Historical plan docs that quote the old signature or the deleted warning
  string (`plans/impl/2026-05-18-...`, `.../2026-05-12-luks-uuid-as-identity`,
  `.../2026-05-19-doctor-foreign-luks-uuid`, `.../2026-05-21-...`) are dated
  records of prior work and are not rewritten. ADR
  `017-runtime-disk-membership` only lists the function name (no signature
  claim) and needs no change.

## Verification

- **Compiler enforces completeness.** Removing `Result` makes every missed
  `?` or `.expect` a build error: `just test-rust` (or `cargo build -p`
  the CLI crate) must compile clean. This is the primary guard that all four
  call sites and three tests were converted.
- **Unit tests:** run the three `enrich_from_pool_state_*` tests plus the
  membership suite via `just test-rust`; confirm the rewritten
  `foreign_luks_uuids`-based assertions pass.
- **Behavior-pin tests still green (unchanged paths):**
  `unlock_warns_when_post_mount_probe_errors`,
  `unlock_tolerates_post_mount_probe_mounted_false`,
  `cmd_replace_warns_when_post_mount_probe_errors`,
  `cmd_add_bootstrap_warns_when_post_mount_probe_errors` -- all pin the
  probe-error / mounted-false branches, which this change does not touch.
- **Dead-string check (done):** `rg "failed to enrich pool membership"` hits
  only `cli/src/unlock.rs` (the line being deleted) and historical plan docs
  -- no Rust or Python VM test asserts it, so deleting the arm breaks nothing.
- **ASCII lint:** `scripts/docs/check-output-ascii.py` over the edited
  comments/strings (all edits stay ASCII).

## Implementation notes

- The test-suite header comment above the `enrich_from_pool_state_*` tests
  in `cli/src/membership.rs` referenced "surfaced in the report" / "the
  report content" -- stale once `EnrichmentReport` is deleted. Re-pointed it
  onto `foreign_luks_uuids` to match the rewritten assertions; the plan's
  Tests section did not call this comment out.
