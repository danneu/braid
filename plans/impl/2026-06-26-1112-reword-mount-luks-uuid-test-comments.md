# Plan: Reword stale "stored luks_uuid" comments in mount UUID-mismatch tests

## Context

Decision 024 (`docs/design/decisions/024-luks-uuid-identity.md`) makes the LUKS
UUID the `pool.json` map key and **deletes** any value-side `luks_uuid` field.
`DiskMember` (`cli/src/membership.rs#DiskMember`) now carries only
`name`/`by_id`/`devid`/`added_at`, and `#[serde(deny_unknown_fields)]` exists
specifically to reject a "resurrected `luks_uuid`" at load time.

The mount UUID-mismatch tests still describe disk1 as having "a stored
luks_uuid" that the test "overrides." That is exactly the pre-024 mental model
the decision warns against: a future maintainer trusting the comment could look
for -- or reintroduce -- a value-side field. The **test bodies are correct**:
they override the *live probe* UUID (the `CryptsetupLuksUuid` seed in
`base_two_disk_runner`, `cli/src/test_fixtures/mount.rs#base_two_disk_runner`)
so it diverges from the *membership map key* seeded by
`two_disk_membership` (`cli/src/test_fixtures/mount.rs#two_disk_membership`).
Only the wording is stale.

This is a comments-only change. No logic, no behavior, no public API.

## Why this is the right (and contained) fix

- `rg -ni "stored (luks_)?uuid" cli/src/` returns **only** these mount.rs
  comments -- the stale idiom is contained to one file. No cross-file refactor.
- The **correct idiom already lives in the same file** ~140 lines up:
  `mount.rs#plan_open_pool_emits_events_before_uuid_mismatch_on_later_member`
  says "its stored membership key" and "mismatches its stored 2222 membership
  key." So this aligns the mismatch tests with an established local convention
  rather than inventing new vocabulary -- a pure project-fit/consistency fix.

## Scope correction vs. the original finding

The finding cited four lines across two functions but its line list omitted
`mount_luks_uuid_mismatch_already_open`, whose stale inline comment is the
fifth instance. It also flagged only "stored luks_uuid," missing a second stale
clause ("from a prior unlock") on the same comment. The ideal fix covers all
five instances and drops the bad provenance clause.

## Changes -- `cli/src/mount.rs` (comments only)

All edits are confined to the three LUKS UUID-mismatch tests
(`mount_luks_uuid_mismatch_closed`, `mount_luks_uuid_mismatch_already_open`,
`mount_luks_uuid_mismatch_refused_even_with_allow_degraded`). Two kinds of edit.
Keep ASCII (`--`, plain quotes).

### A. Normalize each test's preamble to the `//` line-comment form

`docs/dev/testing.md#preamble-literal--line-comment-form` mandates that each
test's preamble be "a contiguous block of `//` line comments directly above the
test item." These three tests currently use `///` doc-comment preambles, which
is the codebase outlier: across `cli/src` test files, `//` Intent/Why/Scenario
lines outnumber `///` ones ~16:1 (4854 vs 306). Test fns are not `pub`, so the
AGENTS.md `///`-on-public-items rule does not apply -- `testing.md`'s `//` rule
governs, and there is no conflict.

While editing these blocks, convert each **complete** Intent/Why/Scenario
preamble from `///` to `//`, wording unchanged except the Scenario rewords in
section B. Additionally, in `_already_open`'s preamble, fix the stray Unicode
em-dash in the `Why:` line to ASCII `--` (we are rewriting that line for the
style conversion anyway):

Before:
```rust
/// Why: The check must fire in both PresentLuks branches — mapper_open
/// status doesn't make a swapped drive safe.
```
After:
```rust
// Why: The check must fire in both PresentLuks branches -- mapper_open
// status doesn't make a swapped drive safe.
```

Out of scope (do **not** touch): the many other `///` test preambles elsewhere
in `mount.rs` (including the adjacent `on_later_member` sibling cited above).
mount.rs is broadly `///`-preambled; a file-wide normalization to `//` is a
separate cleanup, not this finding's job. Per the reviewer, leave unrelated
preambles alone.

### B. Reword the stale Scenario + inline comments (the five instances)

The two Scenario diffs below are shown in their final `//` form, so each diff
folds in both the style conversion (section A) and the wording reword.

**1. `mount_luks_uuid_mismatch_closed` -- Scenario.** Drops both the
value-side-field framing **and** the inaccurate "from a prior unlock" provenance
(the key is written at add/replace time, not cached by unlock).

Before:
```rust
/// Scenario: 2-disk RAID1. disk1 has a stored luks_uuid from a prior
/// unlock, but the device now reports a different UUID (drive was swapped).
/// Both LUKS devices are closed.
```
After:
```rust
// Scenario: 2-disk RAID1. disk1 is keyed under one LUKS UUID in pool.json,
// but the device now reports a different UUID (drive was swapped). Both LUKS
// devices are closed.
```

**2. `mount_luks_uuid_mismatch_closed` -- inline override comment** (already `//`):

Before:
```rust
// Override base's disk1 UUID seed with a value that mismatches the
// stored luks_uuid (HashMap insert semantics on `with_output`).
```
After:
```rust
// Override base's disk1 probe-UUID seed with a value that mismatches its
// pool.json membership key (HashMap insert semantics on `with_output`).
```

**3. `mount_luks_uuid_mismatch_already_open` -- inline trailing comment** (the
instance the finding missed; already `//`):

Before:
```rust
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
```
After:
```rust
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // differs from membership key
```

**4. `mount_luks_uuid_mismatch_refused_even_with_allow_degraded` -- Scenario:**

Before:
```rust
/// Scenario: 2-disk RAID1. disk1's device reports a UUID that differs from
/// the stored luks_uuid (swapped/cloned/reformatted drive), but the operator
/// reaches for `--allow-degraded` -- the wrong guess after seeing a present
/// disk refused.
```
After:
```rust
// Scenario: 2-disk RAID1. disk1's device reports a UUID that differs from
// its pool.json membership key (swapped/cloned/reformatted drive), but the
// operator reaches for `--allow-degraded` -- the wrong guess after seeing a
// present disk refused.
```

**5. `mount_luks_uuid_mismatch_refused_even_with_allow_degraded` -- inline
override comment** (already `//`):

Before:
```rust
// Override base's disk1 UUID seed with a value that mismatches the
// stored luks_uuid (HashMap insert semantics on `with_output`).
```
After:
```rust
// Override base's disk1 probe-UUID seed with a value that mismatches its
// pool.json membership key (HashMap insert semantics on `with_output`).
```

## Deliberate non-change (wording)

`mount_luks_uuid_mismatch_closed`'s **Intent** line ("...doesn't match
pool.json's stored UUID...") keeps its wording. It attributes the UUID to
*pool.json* (where it genuinely lives, as the key), not to a member field, so it
does not evoke the deleted value-side field. (Its `///` still converts to `//`
under section A -- that is a style change, not a wording change.)

## Verification

Comments don't affect compilation, but confirm the edits didn't mangle the code
line at instance 3 (a trailing comment) and that nothing structural broke:

1. Run the three tests (package is `braid-cli`; mount unit tests live under
   `--lib` via `cli/src/lib.rs#mount`):
   ```sh
   cargo test --manifest-path cli/Cargo.toml --lib mount_luks_uuid_mismatch
   ```
   The substring filter matches all three mismatch tests; all pass.
2. Confirm no stale phrasing remains (returns zero hits post-edit; the two
   legit "...prior unlock attempt..." mapper-cleanup scenarios at the bottom of
   the file lack the trailing comma / line-end and are correctly excluded):
   ```sh
   rg -ni "stored luks_uuid|different from stored|from a prior$|from a prior unlock," cli/src/mount.rs
   ```
3. Spot-check that the reworded Scenarios read consistently (wording, not style)
   with the `on_later_member` sibling's "stored membership key" phrasing.
