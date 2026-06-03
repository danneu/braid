# Plan: make `build_devid_names` infallible (drop dead `DuplicateDevid` plumbing)

## Context

`cli/src/status.rs#build_devid_names` resolves btrfs-surfaced devids to operator-facing
display names for `braid status`. For null-underlying and btrfs-MISSING rows it looks up the
persisted name via `PoolMembership::by_devid`, which returns
`Result<Option<...>, MembershipError>` and can fail with `MembershipError::DuplicateDevid`.
`build_devid_names` propagates that error through `?` at two sites, which makes its signature
fallible and forces the sole caller (`build_status`) to thread the error and abort read-only
`braid status`.

That `DuplicateDevid` branch is **dead in production**. `build_status` obtains membership only
via `membership::load_membership` (status.rs:469) or `PoolMembership::empty()` (status.rs:474),
and `load_membership` already rejects duplicate value-side devids at load time
(`membership.rs#load_membership_from`, the devid sweep at ~478-488; pinned by
`load_membership_rejects_duplicate_value_side_devid`). So by the time `build_devid_names` runs,
`by_devid` can never find 2+ matches. The only way to construct the duplicate-devid state is the
`#[cfg(test)]`-only `PoolMembership::for_corruption_tests` bypass -- which is exactly what the
current test (`build_devid_names_propagates_duplicate_devid`) uses to reach the branch.

The fallible signature is therefore error-plumbing weight for an unreachable state. Worse, it is
an **inconsistency**: the TUI's parallel devid->name path (`tui/probe.rs#devid_to_name`,
~207-221) is already fully infallible -- it `.collect()`s into a `HashMap<u64, &str>` and trusts
the same `load_membership` guard (TUI loads via `load_membership` at `tui/mod.rs:34`), never
re-checking. `build_devid_names`'s own doc comment names converging with that TUI path as a
future goal. This change removes the contract mismatch (status fails-closed redundantly; the TUI
does not), advancing that goal, with **zero observable production behavior change**.

**Intended outcome:** `build_devid_names` returns a plain `HashMap<u64, String>`; the impossible
`DuplicateDevid` is treated as a skipped (unnamed) join -- the banner renders bare `devid N`
instead of `name (devid N)`, a purely cosmetic degrade that can never actually trigger.
`load_membership` remains the single authoritative refusal for duplicate-devid corruption.

## Why silent swallow (not a loud panic) is correct here

- `build_devid_names` is a pure, read-only display helper. Its downstream failure mode is
  cosmetic: `devid_to_name` (status.rs:1149-1154) renders `"{name} (devid {devid})"` when the
  map has an entry and falls back to `"devid {devid}"` when it does not. No state corruption, no
  journal risk -- so the CLAUDE.md "fail-closed from the downstream failure mode" mandate (scoped
  to branches that can corrupt state or strand a journal) does not apply.
- CLAUDE.md "residual invariant checks must be hard errors" forbids downgrading the *owning*
  guard to `debug_assert!`. The owning guard is `load_membership` (a hard error in all builds),
  and it is untouched. We delete a redundant *downstream* re-check -- the literal application of
  "put invariant checks at the layer that owns the invariant."
- `by_devid`'s only `Err` variant is `DuplicateDevid` (every other `MembershipError` is marked
  `unreachable!` at its real callers in `lock.rs` and `recover.rs`), so `.ok().flatten()` masks
  nothing else. `DuplicateDevid` stays loud where it matters -- the mutating commands in
  `lock.rs`/`recover.rs`/`remove_missing.rs` keep handling it.
- A read-only `braid status` panicking on a state only reachable via a `#[cfg(test)]` bypass is
  strictly worse operator UX than one unnamed devid, and the TUI sibling already swallows.

## Out of scope

The actual TUI/status unification ("collapse into this") is a larger future refactor. This fix
only removes the signature-shape blocker so the two paths share a contract; it does not merge
them. The retained doc-comment line keeps that goal visible.

## Changes

All edits are in `cli/src/status.rs` (line numbers approximate -- anchor on the symbols).

### 1. `build_devid_names` (def ~317-351)

- **Doc comment (~317-322):** add one line capturing the new invariant, e.g.:
  `/// `membership` is `load_membership`-validated, so `by_devid`'s `DuplicateDevid` is`
  `/// unreachable here and is treated as an unnamed join rather than refused -- this`
  `/// read-only display must not abort, and `load_membership` owns the refusal.`
  Keep the existing first three lines and the trailing TUI-unification line.
- **Signature:** drop the `Result`:
  `fn build_devid_names(pool: &PoolState, membership: &PoolMembership) -> HashMap<u64, String>`
- **Both `by_devid` sites** (null_underlying loop ~335, missing_devids loop ~343): replace
  `membership.by_devid(...)?` with `membership.by_devid(...).ok().flatten()`. Bodies
  (`.entry(...).or_insert_with(...)`) unchanged. Use `.ok().flatten()` (not `if let Ok(Some(..))`)
  in both loops -- it states "discard the impossible error, keep the Option, fall through on
  None" without inviting an `else` arm that pretends to handle the impossible error.
- **Return (~350):** `Ok(names)` -> `names`.

`StatusError::Membership(#[from] membership::MembershipError)` (status.rs:368) stays -- still
produced by the `load_membership` error arm at status.rs:476. No change to `StatusError`.

### 2. Callsite in `build_status` (status.rs:538)

`let devid_names = build_devid_names(&pool, &membership)?;` -> drop the `?`.

### 3. Mechanical `.unwrap()` drops at three passing test sites

- `build_devid_names_covers_present_null_underlying_and_missing` (call at status.rs:5605)
- `build_devid_names_present_foreign_live_uses_mapper_basename` (call at status.rs:5635)
- `alert_btrfs_errors_foreign_live_mapper_keeps_basename` (call at status.rs:6144)

Each: drop the trailing `.unwrap()` on the `build_devid_names(...)` call.

### 4. Reframe the duplicate-devid test (status.rs:5640-5673)

Rename `build_devid_names_propagates_duplicate_devid` ->
`build_devid_names_leaves_duplicate_devid_unnamed`. Keep the `for_corruption_tests` duplicate-
devid membership and the `missing_devids: vec![7]` pool unchanged. Replace the
`.unwrap_err()` + `matches!(... DuplicateDevid ...)` assertion with:

```rust
    let names = build_devid_names(&pool, &membership);
    assert_eq!(names.get(&7), None, "duplicate devid must be left unnamed");
```

Rewrite the three-section `//` preamble (Intent / Why it exists / Scenario) to describe the new
contract: given a duplicate-devid corruption that `load_membership` would reject upstream,
`build_devid_names` degrades that devid to unnamed (no panic, no mis-named join) rather than
aborting read-only status; `load_membership` remains the authoritative refusal. Use `--`, not an
em-dash. Keep this test -- it is the only coverage that the swallow is intentional and fences
against a future change re-introducing a panic or silently picking one member.

## Reuse / no new code

- Reuses the existing `PoolMembership::by_devid` (`cli/src/membership.rs#PoolMembership::by_devid`)
  unchanged -- only the caller's handling changes. No new helper, no new infallible `by_devid`
  variant (that would be more code for identical behavior).
- `load_membership`'s devid guard (`cli/src/membership.rs#load_membership_from`) and its test
  `load_membership_rejects_duplicate_value_side_devid` are unchanged -- they remain the
  authoritative enforcement.

## Verification

Run `just test-rust` (the CLI crate is `braid-cli`). The signature change is compiler-enforced:
a missed `?` (now on a non-`Result`) or a stale `.unwrap()`/`.unwrap_err()` (now on a `HashMap`)
fails the build, so the compiler catches any missed site.

Confirm these pass:

- `build_devid_names_leaves_duplicate_devid_unnamed` -- renamed test, new `names.get(&7) == None`
  assertion (the core behavior change).
- `build_devid_names_covers_present_null_underlying_and_missing` -- `.unwrap()` dropped.
- `build_devid_names_present_foreign_live_uses_mapper_basename` -- `.unwrap()` dropped.
- `alert_btrfs_errors_foreign_live_mapper_keeps_basename` -- `.unwrap()` dropped.
- `load_membership_rejects_duplicate_value_side_devid` -- unchanged; proves the owning guard still
  hard-fails (enforcement was not weakened).

Expectation: full `braid-cli` unit suite green. No VM tests are needed -- the change is confined
to a read-only Rust display helper with no module/systemd/lifecycle blast radius. Do not run any
formatter.
