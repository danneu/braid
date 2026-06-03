# Plan: clarify and properly cover `assert_target_uuid_unique` arm 2b

## Context

A review finding flagged that `assert_target_uuid_unique`'s live-pool arm
("arm 2b", the `live_pool.devices.iter().find(...)` block in
`cli/src/add.rs#assert_target_uuid_unique`) is unreachable for every
`PresentLuks` target. Verification confirmed the claim and surfaced a second,
sharper defect the finding did not mention.

What is actually true in `cli/src/add.rs`:

- Both `PresentLuks` call sites in `build_add_work_plan` call
  `classify_live_pool_match` (the stronger, backing-aware check) *before* the
  assert. `SameBacking` -> `continue`; `DifferentBacking` -> early return;
  only `NoMatch` falls through to `assert_target_uuid_unique`. `NoMatch` is
  returned iff no live device has the target UUID, so by the time the assert
  runs on a `PresentLuks` path, arm 2b's scan is **necessarily empty**.
- Arms 1 (in-flight) and 2a (membership) *are* reached on `PresentLuks`-`NoMatch`
  paths and are covered (`add_cloned_disk_duplicate_uuid_refusal`,
  `add_pre_write_uniqueness_assert_membership_collision`). Only arm 2b is dead
  on those paths.
- Arm 2b is reachable only from the `FreshLuks` caller (no preceding
  `classify_live_pool_match`), and even there only on an astronomically
  unlikely `LuksUuid::new_v4()` collision with a live-but-unmembered device.
- **The defect:** `add_pre_write_uniqueness_assert_live_pool_collision` claims
  (in its preamble) to lock down arm 2b, but its input is a `PresentLuks`
  open target whose UUID collides with a live device at a *different* backing.
  It therefore refuses via `classify_live_pool_match`'s `DifferentBacking` arm
  at the open call site and **never reaches the assert** -- duplicating
  `add_open_present_luks_same_uuid_different_backing_rejects_clone`. Arm 2b has
  **zero** real coverage: deleting arm 2b today leaves the whole suite green.

### Outcome intended

Make the code self-documenting about which gate owns live-pool clone rejection,
and give arm 2b genuine coverage so a future refactor cannot silently strip a
fail-closed safety guard. Arm 2b is **kept** -- it is a residual invariant
check per AGENTS.md ("Residual invariant checks must be hard errors in all
builds"; "set fail-closed policy from the downstream failure mode"), consistent
with the repo's defense-in-depth backstops (e.g.
`target_uuid_map_conflict_to_validation`).

## Approach (recommended: keep arm 2b, document it, cover it for real)

Three edits, all in `cli/src/add.rs`. No production behavior change.

### 1. Document arm 2b's reachability (doc comment + inline comment)

- **`assert_target_uuid_unique` doc comment** (the `///` block above the fn):
  add one or two lines stating that arm 2b is a fail-closed residual backstop
  with no deterministically reachable production trigger. Both `PresentLuks`
  callers pre-reject live-pool UUID collisions via the stronger backing-aware
  `classify_live_pool_match` before this assert runs (reaching it on a
  `PresentLuks` path implies `NoMatch` -- no live device with this UUID -- so
  arm 2b's scan is empty), and the only other caller, `FreshLuks`, would reach
  it only on an astronomically unlikely `LuksUuid::new_v4()` collision. Do not
  frame it as an operationally-meaningful guard. Reference the sibling by
  backticked symbol name (`` `classify_live_pool_match` ``), per the
  `path#symbol` / no-line-number convention in AGENTS.md.
- **Arm 2b inline comment** (the existing `// (2b) Live-pool collision. ...`
  block): prepend a sentence that it fires only for the `FreshLuks` caller, and
  only on a random `new_v4()` collision with a live device -- `PresentLuks`
  callers are intercepted upstream by `classify_live_pool_match`. Frame it as a
  fail-closed residual backstop, **not** a "membership-vs-live divergence"
  guard (that wording implies an operational trigger the code provably
  intercepts). Keep the existing text describing how the error is rendered.

### 2. Mirror the "two-tier defense" comment onto the open `PresentLuks` call site

The closed branch already carries the `// Two-tier defense for the cached `uuid`:`
comment block; the open branch (`if *mapper_open` -> `AddLuksBtrfsProbe::SamePool`)
has no equivalent. Add a concise comment before the open branch's
`classify_live_pool_match` call stating the same layering: the backing-aware
match proves same-backing no-ops and rejects different-backing clones, and the
subsequent `assert_target_uuid_unique` then only catches in-flight (arm 1) and
membership (arm 2a) collisions -- its live-pool arm is dead here. This removes
the asymmetry the finding's maintenance concern is about ("a future reader edits
one of the two checks and assumes the other compensates").

### 3. Replace the misattributed test with a direct arm-2b unit test

Replace `add_pre_write_uniqueness_assert_live_pool_collision` (preamble + body)
with a direct unit test of the private helper -- the same pattern already used
for `classify_live_pool_match` (its unit tests live in this file). Suggested
name: `assert_target_uuid_unique_refuses_live_pool_uuid`.

- Build inputs directly (no `build_add_work_plan`):
  - `uuid` = a fixed UUID;
  - `live_pool` = `pool_with_live_devices(vec![live_pool_device("braid-foreign", &uuid, "/dev/vdb")])`
    (the same helpers `add_open_present_luks_same_uuid_different_backing_rejects_clone`
    uses);
  - `membership` = `PoolMembership::empty()`;
  - `in_flight` = `LuksUuidMap::new()`;
  - `name` / `by_id` = parsed test values.
- Assert `assert_target_uuid_unique(&uuid, &membership, &live_pool, &in_flight, &name, &by_id)`
  returns `Err(AddError::DuplicateUuid { uuid, .. })`. Empty membership +
  empty in-flight isolate arm 2b (arms 1 and 2a cannot fire). Backing is
  irrelevant -- arm 2b is UUID-only.
- Preamble (Intent / Why it exists / Scenario), matching the repo convention:
  - *Intent:* the assert refuses a target UUID matching a live `pool.devices`
    row when membership and the in-flight map are empty (arm 2b in isolation).
  - *Why it exists:* arm 2b is a fail-closed residual backstop that is
    unreachable through `build_add_work_plan` (PresentLuks callers pre-reject
    via `classify_live_pool_match`; FreshLuks would need a non-deterministic
    `new_v4()` collision), so a direct call is the only way to lock the guard
    against a silent refactor removal.
  - *Scenario:* there is no operationally reachable scenario -- arm 2b is a
    residual fail-closed guard. `PresentLuks` UUID collisions are intercepted
    upstream by `classify_live_pool_match` (which scans the live pool directly,
    regardless of membership), and the `FreshLuks` path would require a random
    `new_v4()` collision, so the test constructs the colliding state directly --
    a live `pool.devices` row whose UUID equals the target, with arms 1 and 2a
    neutralized via empty in-flight and empty membership -- and asserts the
    refusal.

No new assertion is lost: the old test's "no `CryptsetupLuksFormat` issued"
check is near-vacuous (planning never formats), and the
open-different-backing call-site path it actually exercised is already covered
by `add_open_present_luks_same_uuid_different_backing_rejects_clone`.

### Style notes

- Use `--` (not em-dash) in the new comment text, matching the doc-comment
  convention and the closed-branch two-tier comment being mirrored. Do not
  reformat pre-existing em-dashes in adjacent lines (out of scope; narrow edits
  only -- do not run a formatter).
- Reference sibling functions by backticked symbol name, never by line number.

## Critical files

- `cli/src/add.rs`
  - `assert_target_uuid_unique` -- doc comment + arm 2b inline comment (edits 1).
  - `build_add_work_plan`, open `PresentLuks` / `SamePool` branch -- mirrored
    two-tier comment (edit 2).
  - tests module -- replace `add_pre_write_uniqueness_assert_live_pool_collision`
    with the direct unit test (edit 3). Reuse existing helpers
    `pool_with_live_devices`, `live_pool_device`, `PoolMembership::empty`,
    `LuksUuidMap::new`.

No other files change. No production logic changes.

## Verification

1. **Prove the gap exists first** (validates the plan's premise that arm 2b had
   zero prior coverage): on the current tree -- old test still present, new test
   not yet added -- temporarily comment out arm 2b in `assert_target_uuid_unique`
   and run `just test-rust`. Confirm the suite stays GREEN: no existing test,
   including the soon-to-be-replaced
   `add_pre_write_uniqueness_assert_live_pool_collision`, exercises arm 2b.
   Restore arm 2b.
2. Make the edits (comments + test swap), then `just test-rust` -- the new
   `assert_target_uuid_unique_refuses_live_pool_uuid` passes; the suite is green
   with the old test removed. (Rust unit tests only; no VM/systemd blast radius,
   so no `just test-vm` needed.)
3. **Prove the new test is a real guard** (the whole point): temporarily comment
   out arm 2b again, run `just test-rust`, and confirm the new test now FAILS --
   the same edit that left the suite green in step 1 now turns it red. Restore
   arm 2b.
4. `cargo build` / clippy clean -- comment-only edits plus one test; no warnings
   expected. (No `mdbook build docs` needed: all edits are in `.rs`, not the docs
   tree.)

## Implementation notes

- The plan's central premise -- "Arm 2b has **zero** real coverage: deleting arm
  2b today leaves the whole suite green" -- is false. The same commit the finding
  targets (`c36ead87`) already added a *direct* unit test of the helper,
  `assert_target_uuid_unique_live_pool_collision_omits_foreign_mapper` (empty
  membership + empty in-flight + a live `pool.devices` row carrying the colliding
  UUID), which exercises arm 2b in isolation. Verified empirically per the plan's
  Step 1: with arm 2b commented out, that test goes **red** (`unwrap_err()` on
  `Ok(())`), while `add_pre_write_uniqueness_assert_live_pool_collision` stays
  green (confirming it never reaches arm 2b).
- Because arm 2b is already covered, edit 3 was **pivoted** (user-approved):
  instead of adding the redundant `assert_target_uuid_unique_refuses_live_pool_uuid`
  and deleting the misattributed test, the misattributed test was relabeled. It
  was renamed `add_pre_write_uniqueness_assert_live_pool_collision` ->
  `add_live_pool_collision_omits_braid_prefixed_mapper` and its `/* */` preamble
  rewritten (in the conventional `//` three-section form) to describe what it
  actually verifies: the open-branch `classify_live_pool_match` `DifferentBacking`
  refusal plus the `braid-`-prefixed double-prefix message regression. Its body
  (all assertions) is unchanged, so no coverage is lost -- including the
  `braid-braid` double-prefix and `braid-foreign` leak checks the plan's
  delete-and-replace would have dropped.
- Edits 1 (doc + arm-2b inline comment) and 2 (mirrored two-tier comment on the
  open call site) were done as written; their substance is independent of the
  coverage premise.
- The plan's edit-3 assertion target was also slightly wrong (`Err(AddError::DuplicateUuid {..})`);
  arm 2b raises `DuplicateUuidLivePool`. Moot given the pivot.
- Historical references to the old test name in `plans/impl/*.md` were left
  untouched (frozen point-in-time records).
