# Plan: document and pin the unreachable duplicate-devid arm in remove-missing

## Context

A Low-severity testing finding asked for a unit test proving that a
`MembershipError::DuplicateDevid` (two UUIDs sharing the target devid)
surfaces from `cmd_remove_missing` as `RemoveMissingError::Membership`
("pool membership corruption"), fail-closed, before any mutation.

Verification showed the **proposed test is unwritable as specified** and the
finding is misframed:

- `plan_remove_missing` calls `membership::load_membership` (`remove_missing.rs`
  load step) **before** `resolve_removal_target`. `load_membership_from` runs a
  devid-uniqueness sweep (`membership.rs`, the `seen_devid` loop) that rejects a
  duplicate-devid `pool.json` with `MembershipError::Conflict`, which
  `plan_remove_missing` maps to `RemoveMissingError::Validation("failed to load
  pool membership: ...")`.
- Therefore the membership handed to `resolve_removal_target` is always
  load-validated, `by_devid` can never return `DuplicateDevid` there, and
  `RemoveMissingError::Membership` is **unreachable via the command**. You cannot
  seed a corrupt membership (`for_corruption_tests`) and reach that variant
  through `cmd_remove_missing` -- `load_membership` intercepts first.
- The "picked the first match" regression the finding feared is already pinned at
  its source by `membership.rs::by_devid_returns_duplicate_devid_on_corruption`.

`status::build_devid_names` documents this exact invariant correctly for the same
`by_devid` call: *"`membership` is `load_membership`-validated, so `by_devid`'s
`DuplicateDevid` is unreachable here ... and `load_membership` owns the refusal."*
`remove_missing.rs` does **not** -- its doc comments frame the propagation as a
live operator-facing path ("so the operator-facing remediation reads consistently
with the rest of the membership errors"), which is what led the reviewer to
mistake unreachable defensive plumbing for an uncovered live path.

**Intended outcome:** make the unreachability explicit in the two `remove_missing.rs`
doc comments, mirroring the accurate framing already in `status::build_devid_names`,
so this class of finding dissolves -- and pin the premise those comments now rely on
(`plan_remove_missing` refuses a duplicate-devid `pool.json` at the load gate, before
resolution, fail-closed) with one command-level regression test. No behavior change,
no design-doc change.

## Tests and code: one premise pin, two rejected, no code change

- **Add one command-level premise test** (see "### 3" below). The new doc comments
  make a load-bearing claim: `plan_remove_missing` loads membership *before*
  `resolve_removal_target`, so `load_membership` owns the duplicate-devid refusal and
  the `Membership` arm is unreachable. Nothing pins that claim at the command level
  today -- a regression that reordered resolution before load, or dropped/relaxed the
  load gate, would leave the docs looking correct while letting remove-missing act on
  a corrupt map. `membership.rs::load_membership_rejects_duplicate_value_side_devid`
  pins the load *sweep* in isolation; it does **not** pin the `plan_remove_missing`
  ordering or the fail-closed wrapping through `cmd_remove_missing`. This test is the
  reachable, behavioral counterpart to the original (unwritable) finding.
- **Do not add the `Membership`-asserting command test.** Unwritable: the corrupt
  membership can never reach `resolve_removal_target` through the command (load
  intercepts), so `RemoveMissingError::Membership` cannot be observed from
  `cmd_remove_missing`.
- **Do not add a helper-level `resolve_removal_target(corrupt)` -> `Err(Membership)`
  test.** It exercises an unreachable arm -- structure-sensitive, not behavioral --
  and is redundant with `membership.rs::by_devid_returns_duplicate_devid_on_corruption`.
  Fails the project's "behavioral and structure-insensitive" bar.
- **Keep the `?` propagation and the `Membership` variant.** For a *mutating*
  command, fail-closed propagation with an accurate "pool membership corruption"
  message is the correct defensive choice if the load-validation invariant is ever
  bypassed -- strictly better than `status.rs`'s read-only `.ok().flatten()` (which
  would collapse a hypothetical duplicate into a misleading `NoMemberForDevid`) or
  than deleting the variant. This matches the project's fail-closed safety ethos
  (`docs/dev/safety-heuristics.md`). The only defect is the misleading *doc*, not
  the code.

## The change

Two doc-comment rewrites in `cli/src/remove_missing.rs`. The runtime
`#[error("pool membership corruption: {0}")]` string and the variant itself stay
as-is (no test pins the string; it is a reasonable message if the unreachable case
ever fires). The `NoMemberForDevid` variant doc (with its genuinely load-bearing,
test-pinned wording) is left untouched.

### 1. `RemoveMissingError::Membership` variant doc

Replace the current comment:

```rust
    /// `pool.json` membership corruption surfaced from `by_devid` (two
    /// or more members share the same persisted devid). Wraps
    /// `MembershipError::DuplicateDevid` so the operator-facing
    /// remediation reads consistently with the rest of the membership
    /// errors.
```

with framing that states the unreachability and why the variant is retained:

```rust
    /// Defense-in-depth refusal for `pool.json` membership corruption (two
    /// or more members carry the same persisted devid). `by_devid` returns
    /// `MembershipError::DuplicateDevid` only on such a corrupt snapshot, but
    /// `plan_remove_missing` resolves against a `load_membership`-validated
    /// membership whose devid-uniqueness sweep (`membership::load_membership_from`)
    /// already rejects that corruption -- surfaced as `Validation`, not this
    /// variant. So on the production path this arm is unreachable and
    /// `load_membership` owns the duplicate-devid refusal, exactly as
    /// `status::build_devid_names` documents. It is kept (rather than swallowed
    /// like that read-only display) because remove-missing mutates: a future
    /// caller that ever resolved against an unvalidated membership would stay
    /// fail-closed with an accurate corruption message instead of acting on a
    /// device chosen from a corrupt map.
```

### 2. `resolve_removal_target` doc

Replace the `DuplicateDevid` sentence so it names the propagation as unreachable
defense-in-depth rather than a live corrupt-membership path:

```rust
/// ... Returns `RemoveMissingError::NoMemberForDevid` when no member carries
/// the persisted devid (so the operator can decide whether enrichment ever ran
/// on the pool). The `?` propagates `MembershipError::DuplicateDevid` as
/// fail-closed defense-in-depth only: the sole caller (`plan_remove_missing`)
/// passes a `load_membership`-validated snapshot whose devid-uniqueness sweep
/// already refuses duplicate devids, so that arm is unreachable in practice
/// (see `RemoveMissingError::Membership`). This is the single point of identity
/// resolution for `remove-missing` -- callers thread the returned UUID straight
/// into the journal and the persisted-member removal.
```

Keep ASCII-only per project convention (`--`, `...`).

### 3. New regression test: duplicate-devid `pool.json` refused at load, fail-closed

Add to `cli/src/remove_missing.rs`'s `#[cfg(test)] mod tests`, following the
established overwrite pattern in
`cmd_remove_missing_never_enriched_refusal_returns_structured_error` (build a
membership, `membership::save_membership(&m, &f.paths)`, run `cmd_remove_missing`).

- **Fixtures:** `PoolFixture::three_disk_devids_pinned()` +
  `RemoveMissingPool::three_disk_one_missing().install(MockRunner::default())` +
  `MockFs::storage(vec![])`. Three-disk/one-missing avoids the 2-disk reject so
  execution reaches the load step; the probe reports devid 3 MISSING so
  `validate_missing_id_target` passes (it reads the probe, not membership).
- **Corrupt membership:** `PoolMembership::for_corruption_tests(...)` with two
  members that both carry `devid: Some(3)` but have **distinct** UUIDs, names, and
  by-id paths -- so the load sweep's devid post-loop check (not the earlier name/by-id
  checks) is what rejects. `save_membership(&m, &f.paths)` (save does not validate).
- **Run:** `cmd_remove_missing(&runner, &fs, &f.remove_missing_params().missing_id(3).build())`.
- **Assert (behavioral, fail-closed -- mirrors `single_survivor_rejected_at_preflight`):**
  - `RemoveMissingError::Validation(msg)` with `msg.contains("failed to load pool membership")`
    AND `msg.contains("devid '3' already in use")` -- pins that the refusal is the
    duplicate-devid load sweep surfaced *through the command*, not an incidental load failure.
  - `f.inhibitor.acquire_count() == 0`.
  - `journal::load_journal(&f.paths).unwrap().is_none()`.
  - no `CmdRequest::BtrfsDeviceRemove` in `runner.requests()`.
  - pool.json byte-for-byte unchanged across the call (pre/post `std::fs::read`).
- **Preamble** (per `docs/dev/testing.md`): *Intent* -- a corrupt pool.json (two members
  sharing the missing devid) is refused by remove-missing at the load gate, fail-closed,
  before resolution or mutation. *Why it exists* -- pins the premise the new doc comments
  rely on (load owns the duplicate-devid refusal; the `Membership` arm is unreachable); a
  reorder or relaxed load gate would fail here first. *Scenario* -- an operator's pool.json
  is corrupted so two UUIDs both claim devid 3 (the dead disk); `braid remove-missing
  --missing-id 3` must refuse cleanly, not mutate.

## Files

- `cli/src/remove_missing.rs` -- the two doc comments above (`RemoveMissingError::Membership`
  variant; `resolve_removal_target` fn) plus the new regression test in the file's
  `#[cfg(test)] mod tests`. No other file changes.

## Out of scope (verified, no action)

- `docs/` design pages and ADRs: grep found no page describing the remove-missing
  duplicate-devid / "membership corruption" handling -- nothing to sync.
- `status.rs` / `membership.rs`: already correct; this change aligns
  `remove_missing.rs` to them, not the reverse. The added test reuses
  `membership.rs`'s existing `for_corruption_tests` constructor -- no production
  code changes there.

## Verification

1. `just test-rust` -- whole CLI suite green, including the new regression test. The
   test pins *existing* behavior, so it passes against current code on first run (it is
   a regression guard, not TDD for new behavior). Sanity-check it actually exercises the
   load gate: it asserts both the `failed to load pool membership` wrapper and the
   `devid '3' already in use` inner cause.
2. `cargo doc -p braid-cli --no-deps` -- builds clean; the referenced items
   (`membership::load_membership_from`, `status::build_devid_names`,
   `RemoveMissingError::Membership`) are spelled correctly as code spans. (Package is
   `braid-cli`; `braid` is only the `[[bin]]` name -- `cargo doc -p braid` would fail.)
3. Manual read: the two new comments accurately state "unreachable on the production
   path; `load_membership` owns the refusal; retained as fail-closed defense-in-depth
   because remove-missing mutates," and read consistently with
   `status::build_devid_names`.
