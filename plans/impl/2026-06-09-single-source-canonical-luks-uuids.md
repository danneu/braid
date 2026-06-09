# Plan: fix divergent drift fixture + single-source the canonical test LUKS UUIDs

## Context

`disk2_disk3_membership()` in `cli/src/remove.rs` (test module) carries a comment
claiming its seed UUIDs "match the disk-number mirroring used in
`PoolFixture::three_disk_healthy`" so the two yield "bit-equal pool.json bodies."
That claim is **false now**. The helper keys disk2/disk3 under
`test_uuid(2)`/`test_uuid(3)` = `00000000-...-0002/0003`, while
`three_disk_healthy` keys the same logical disks under the canonical
repeated-digit UUIDs `22222222-...`/`33333333-...` (via `luks_uuid_for_disk_name`).

**History (why it's stale, not wrong-from-birth):** the comment was added in
`bc64bb8f` when `three_disk_healthy` itself still keyed by `test_uuid(seed)` --
true at the time. Commit `9c23a15a` ("finish luks uuid identity migration")
rekeyed `three_disk_healthy` to the canonical UUIDs so its pool.json keys align
with the live `CryptsetupLuksUuid` probe (`luks_uuid_for_device` returns
`11111111/22222222/33333333` for `/dev/vd{b,c,d}`), but left
`disk2_disk3_membership` on `test_uuid`. The comment went stale and the two
fixtures diverged.

**Why it currently passes anyway:** both drift tests
(`cmd_remove_dry_run_rejects_when_target_absent_from_pool_json`,
`execute_rejects_when_pool_json_drifts_after_planning`) remove `disk1`, which is
absent from the drifted pool.json; the rejection fires on disk1's absence
regardless of the disk2/disk3 keys. The survivor assertion compares
`load_membership(&f.paths)` against the in-memory `drifted` (the helper compared
to itself), so the UUID mismatch is inert. The comment is load-bearing for
nobody, and a future drift test that removed a *surviving* disk and relied on
membership<->live-probe correlation would be built on a false premise.

**Proven siblings of the same pattern:** `drifted_member_remove_closes_observed_mapper`
and `post_commit_close_uuid_probe_demotes_to_skip_on_mismatch` (both in
`cli/src/remove.rs`) key their survivor disks disk1/disk3 under
`test_uuid(410/411)` and `test_uuid(420/421)` while their own live probes return
canonical `11111111`/`33333333` -- the same latent incidental-pass mismatch,
inert only because survivors are correlated by devid/name, not UUID.

**Root architectural smell:** the canonical `disk N -> N-repeated UUID` mapping is
hand-encoded in many places (`luks_uuid_for_disk_name` and `luks_uuid_for_device`
-- two parallel maps that must agree; inline `LuksUuid::parse("11111111-...")` in
`two_disk_healthy`, `one_live_one_missing`, `three_disk_membership_with_pinned_disk2`),
with one copy already drifted to a second convention. This is the test-fixture
analog of the ambiguity ADR 024 (LUKS UUID Is Disk Identity) exists to eliminate.

**Intended outcome:** the drift fixture can never re-encode disk identity under a
second UUID convention; across the in-scope remove + shared fixture surface the
canonical UUIDs gain a single source of truth (`canonical_luks_uuid`) with
documentation that prevents this class from recurring there. Other
`test_fixtures/*` helpers keep their own canonical literals for now (tracked in
the follow-up inventory below) -- this change does not claim a codebase-wide
single source. No production behavior changes -- this is a Rust unit-test fixture
fix; the added tests are a fixture-contract regression guard (step 3a) and the
generator-contract tests (step 1).

## Scope (chosen: "Fix + single source")

In scope: the `remove` test surface + `test_fixtures/shared.rs` and
`test_fixtures/remove.rs`. **Out of scope** (separate cleanup initiative): the 4
copy-pasted private `test_uuid` definitions (`probe_mapper_uuid.rs`, `cmd.rs`,
`membership.rs`, `main.rs`), `synth_test_uuid` in `replace.rs`, and the parallel
device->UUID maps in `add.rs` / `replace.rs` / `remove_missing.rs`.

## Changes

### 1. Add a single-source canonical-UUID generator

In `cli/src/test_fixtures/shared.rs`, next to `test_uuid`, add:

```rust
/// Canonical repeated-digit fixture LUKS UUID for disk `n`: `n` is the hex
/// digit repeated across all 32 positions (`canonical_luks_uuid(2)` ->
/// `22222222-2222-2222-2222-222222222222`). This is the UUID the present-pool
/// `RemovalPool` live probe (`luks_uuid_for_device`) reports for `/dev/vd{b,c,d}`.
///
/// Use `canonical_luks_uuid(n)` for any fixture entry modeling the canonical
/// `diskN` identity -- present OR temporarily missing (e.g. the replace target
/// in `one_live_one_missing`). For a PRESENT, live-probed disk the pool.json key
/// MUST equal the probed UUID or the membership<->live-UUID correlation is a
/// silent incidental pass; for a missing `diskN` it keeps the row recognizable
/// and future-proof if that disk later becomes present.
///
/// Reserve `test_uuid(seed)` (`00000000-...-{seed}`) for identities that are NOT
/// a canonical `diskN`: arbitrary unique values and deliberately custom-mocked
/// sentinels (drift / foreign targets whose `CryptsetupLuksUuid` probe is
/// overridden to that exact value).
pub(crate) fn canonical_luks_uuid(n: u64) -> LuksUuid {
    // Fail closed: n == 0 would silently build the nil UUID, and n > 15 (or a
    // value that truncates under `as u32`) would alias another disk -- both
    // defeat the single-source purpose. Assert the full u64 before casting.
    assert!((1..=15).contains(&n), "canonical disk index must be 1..=15, got {n}");
    let d = std::char::from_digit(n as u32, 16).expect("1..=15 is a single hex digit");
    let g = |len: usize| -> String { std::iter::repeat(d).take(len).collect() };
    LuksUuid::parse(&format!("{}-{}-{}-{}-{}", g(8), g(4), g(4), g(4), g(12)))
        .expect("canonical repeated-digit UUID is valid")
}
```

Output is byte-identical to the existing literals (verify `n=1,2,3` ->
`11111111-...`, `22222222-...`, `33333333-...`), so every refactor below is a
pure substitution with zero behavioral change.

**Pin the generator's contract** (in `cli/src/test_fixtures/shared.rs`'s existing
`#[cfg(test)] mod tests`). Step 2 routes BOTH `luks_uuid_for_device` and
`luks_uuid_for_disk_name` through this one function, so membership and live-probe
agree by construction -- a wrong generator would corrupt both sides in lockstep
with no cross-check. These tests are that independent cross-check, and they pin
the `1..=15` domain guard (the `n=0` nil-UUID trap):

```rust
// Intent: canonical_luks_uuid(n) yields the exact repeated-digit literal for
//   each disk index (1/2/3 -> 11111111-.../22222222-.../33333333-...) the
//   inline fixtures used before this change.
// Why it exists: step 2 routes BOTH the membership map (luks_uuid_for_disk_name)
//   and the live-probe map (luks_uuid_for_device) through this one generator, so
//   a wrong generator corrupts both sides in lockstep with no cross-check; this
//   pins the output byte-for-byte against the literals it replaces.
// Scenario: a refactor tweaks a segment length or the digit and silently shifts
//   every canonical fixture UUID; this byte-for-byte tripwire fails closed.
#[test]
fn canonical_luks_uuid_pins_repeated_digit_literals() {
    assert_eq!(canonical_luks_uuid(1).as_str(), "11111111-1111-1111-1111-111111111111");
    assert_eq!(canonical_luks_uuid(2).as_str(), "22222222-2222-2222-2222-222222222222");
    assert_eq!(canonical_luks_uuid(3).as_str(), "33333333-3333-3333-3333-333333333333");
}

// Intent: canonical_luks_uuid(0) panics instead of silently returning the nil
//   UUID (00000000-...).
// Why it exists: n == 0 builds the nil UUID, which aliases an "absent/zero"
//   identity and defeats the fail-closed 1..=15 domain guard; this pins that
//   guard so the n=0 trap cannot regress.
// Scenario: a caller passes a 0-based disk index by mistake; the generator must
//   fail closed, not mint a nil-UUID pool member.
#[test]
#[should_panic(expected = "canonical disk index must be 1..=15")]
fn canonical_luks_uuid_rejects_disk_index_zero() {
    // n == 0 silently built the nil UUID before the guard -- the alias trap.
    let _ = canonical_luks_uuid(0);
}
```

Re-export it from `cli/src/test_fixtures.rs` alongside `test_uuid` (the
`pub(crate) use shared::{...}` list) so per-command test modules can reach it.

### 2. Route the existing canonical maps through the generator

- `cli/src/test_fixtures/remove.rs` `luks_uuid_for_disk_name` and
  `luks_uuid_for_device`: keep the explicit `disk1/disk2/disk3` (resp.
  `/dev/vd{b,c,d}` + `virtio-disk{1,2,3}`) arms to preserve the current
  `None`-for-anything-else behavior, but map each arm to a disk number and return
  `canonical_luks_uuid(n)` instead of a hardcoded string literal. Return type
  becomes `Option<LuksUuid>`; update the two call sites (`three_disk_healthy`
  drops its now-redundant `LuksUuid::parse(...)`; `target_device` keeps a
  `canonical_luks_uuid`/explicit fallback). The `CryptsetupLuksUuid` handler in
  `RemovalPool::install` still `format!("{uuid}\n")`s fine (LuksUuid Displays as
  its string).
- `cli/src/test_fixtures/remove.rs` `three_disk_healthy` doc comment: rewrite the
  now-stale "Each disk's UUID seed encodes its disk number (`disk1` -> seed 1,
  etc.) so fixture UUIDs read at a glance and stay in disk-number order" line. The
  drift helper (step 3) derives from this fixture, so its comment must be accurate:
  the membership is keyed by `canonical_luks_uuid(n)` (via `luks_uuid_for_disk_name`)
  in disk-number order, and the `disk_member_with` seed-derived UUID is discarded
  (the seed feeds only that now-unused UUID -- the `DiskMember` fields come from
  name/by_id/devid -- so it is vestigial and may be dropped if the implementer
  prefers a member-only constructor).
- `cli/src/test_fixtures/shared.rs` `two_disk_healthy`, `one_live_one_missing`:
  replace the inline `LuksUuid::parse("11111111-...")` / `"22222222-..."` with
  `canonical_luks_uuid(1)` / `canonical_luks_uuid(2)`.
- `cli/src/remove.rs` (test module) `three_disk_membership_with_pinned_disk2`:
  replace the inline `11111111-...` / `33333333-...` literals for disk1/disk3
  with `canonical_luks_uuid(1)` / `canonical_luks_uuid(3)` (disk2 stays the
  caller-supplied sentinel).

### 3. Fix `disk2_disk3_membership` by deriving the drift from the fixture (the pivot)

Replace the hand-built helper in `cli/src/remove.rs` (test module) with one that
derives the drift from what `three_disk_healthy` already saved -- so disk2/disk3
*inherit* the canonical keys and there is no second encoding to drift:

```rust
/// pool.json drift for the membership-drift rejection tests: the
/// `three_disk_healthy` membership with `disk1` removed. Derived from the
/// fixture's own saved pool.json, so the surviving disk2/disk3 keep the
/// canonical LUKS UUIDs the live `RemovalPool` probe returns -- the drift
/// cannot re-encode disk identity under a second UUID convention.
fn three_disk_healthy_without_disk1(paths: &StatePaths) -> PoolMembership {
    let mut m = membership::load_membership(paths).expect("three_disk_healthy pool.json");
    let (uuid, _) = m
        .by_name(&DiskName::parse("disk1").expect("valid fixture name"))
        .expect("disk1 present in three_disk_healthy");
    let uuid = uuid.clone();
    m.remove_by_uuid(&uuid);
    m
}
```

Reuses existing API: `membership::load_membership`, `PoolMembership::by_name`
(`cli/src/membership.rs`), `PoolMembership::remove_by_uuid`. Delete the false
comment entirely (the derivation is self-evident).

Update both call sites
(`cmd_remove_dry_run_rejects_when_target_absent_from_pool_json`,
`execute_rejects_when_pool_json_drifts_after_planning`):
`let drifted = three_disk_healthy_without_disk1(&f.paths);` then the existing
`save_membership(&drifted, &f.paths)`. This is valid in both because
`plan_remove` is read-only and `execute` writes membership only after the
journal + device-remove (confirmed), so `f.paths` still holds the unmodified
healthy 3-disk membership at the derivation point. The closing
`assert_eq!(load_membership(&f.paths), drifted)` still holds (round-trip) and is
now a *stronger* assertion: survivors are pinned under the canonical UUIDs.

### 3a. Pin the drift-survivor contract with a regression test

The existing drift tests remove `disk1` and so stay green even if the survivors
are keyed under the wrong UUIDs -- that insensitivity is the very bug being
fixed, so the suite cannot guard the fix. Add one committed contract test (in
`cli/src/remove.rs` `mod tests`, beside the helper) that asserts the derived
drift keeps disk2/disk3 under the canonical keys:

```rust
// Intent: the drift fixture keeps its surviving disks under the SAME
//   canonical LUKS UUIDs `three_disk_healthy` assigns -- it derives the
//   drift from the saved pool, never re-encoding disk identity under a
//   second UUID convention.
// Why it exists: the drift-rejection tests remove `disk1`, so they stay
//   green even when disk2/disk3 are keyed under the wrong UUIDs -- the
//   incidental-pass bug this change fixes. This contract test fails closed
//   on that regression: revert `three_disk_healthy_without_disk1` to a
//   hand-built `test_uuid(2/3)` membership and this is the only test red.
// Scenario: `three_disk_healthy` saves disk1+disk2+disk3; the drift drops
//   disk1; disk2/disk3 must remain keyed by canonical_luks_uuid(2/3).
#[test]
fn drift_fixture_keeps_survivors_under_canonical_uuids() {
    let f = PoolFixture::three_disk_healthy();
    let drift = three_disk_healthy_without_disk1(&f.paths);

    assert_eq!(drift.len(), 2, "drift drops exactly disk1");
    assert!(
        drift.by_name(&DiskName::parse("disk1").unwrap()).is_none(),
        "disk1 must be absent from the drift",
    );
    for n in [2u64, 3] {
        let member = drift.by_uuid(&canonical_luks_uuid(n));
        assert!(
            member.is_some_and(|m| m.name.as_str() == format!("disk{n}")),
            "disk{n} must be keyed under canonical_luks_uuid({n}), as in three_disk_healthy",
        );
    }
}
```

This is structure-insensitive (it asserts the fixture's observable contract --
which disks exist under which keys -- not how the helper builds it) and directly
guards both the derivation (disk1 dropped) and the single-source keys (disk2/3
canonical). Add `canonical_luks_uuid` to the remove test module's
`crate::test_fixtures::{...}` import.

### 4. Make the proven sibling fixtures internally consistent

In `cli/src/remove.rs` (test module), for the two present-pool tests whose
*survivor* disks are keyed under `test_uuid` but live-probed as canonical:
`drifted_member_remove_closes_observed_mapper` and
`post_commit_close_uuid_probe_demotes_to_skip_on_mismatch`. Key disk1/disk3 under
`canonical_luks_uuid(1)` / `canonical_luks_uuid(3)` so the membership key equals
the probed UUID, following the existing `three_disk_healthy` idiom
(`let (_, member) = disk_member_with(...); m.insert(canonical_luks_uuid(n), member)`,
discarding the seed-derived UUID). **Leave the deliberate sentinels as
`test_uuid`** (`u_r`, `u_foreign` -- the drifted/target/foreign disks whose
`CryptsetupLuksUuid` probe is custom-overridden to those exact values). Their
assertions are about the target mapper/close, not survivor UUIDs, so this is a
no-behavior-change consistency fix.

## Verification

- `just test-rust` -- full Rust unit suite. The dedup in steps 1-2 feeds many
  fixtures (`two_disk_healthy` / `one_live_one_missing` back add/replace/status
  tests too), so a green full run is the real proof the substitution is
  byte-identical.
- Targeted, before and after:
  `cargo test -p braid-cli -- remove::tests` (crate name per `cli/Cargo.toml`),
  focusing on the named tests: the new contract test
  `drift_fixture_keeps_survivors_under_canonical_uuids` (step 3a), the two drift
  tests (step 3), and the two sibling tests (step 4).
- Mutation check (proves the contract test is load-bearing, per the review):
  temporarily revert `three_disk_healthy_without_disk1` to a hand-built
  `test_uuid(2/3)` membership and confirm `drift_fixture_keeps_survivors_under_canonical_uuids`
  is the ONLY test that goes red -- the two drift tests stay green, which is the
  incidental-pass failure mode this change closes. Revert the mutation after.
- The `canonical_luks_uuid_pins_repeated_digit_literals` and
  `canonical_luks_uuid_rejects_disk_index_zero` unit tests (step 1) pin the
  generator's output and domain guard directly -- an independent cross-check that
  does not lean on the membership<->probe agreement (which step 2 makes both
  sides derive from the same source).
- No fixture-refresh / NixOS VM test impact: this touches only Rust unit-test
  fixtures, not parser fixtures or module config.

## Out of scope (note for a follow-up)

The codebase-wide dedup (4 shadow `test_uuid` copies, `synth_test_uuid`, and the
add/replace/remove_missing device->UUID maps) is a larger, separate initiative.
`canonical_luks_uuid` is deliberately placed in `shared.rs` so that follow-up can
adopt it without rework.

Concretely, under the step-1 rule the `remove_missing` fixtures
(`three_disk_devids_pinned`, `two_disk_devids_pinned`) are the known laggards:
they key modeled disk1/2/3 under `test_uuid(seed)` and *should* move to
`canonical_luks_uuid(n)`. Deferring is safe because remove-missing always targets
a missing / devid-correlated disk that is never live-UUID-probed, so the
key-vs-probe mismatch stays inert today (the same incidental-pass reasoning, but
without a misleading "bit-equal" comment to correct). They convert in the
follow-up, not here.

Full inventory of `test_fixtures/*` helpers that still hardcode canonical
repeated-digit *per-disk LUKS* literals and would adopt `canonical_luks_uuid` in
the follow-up (re-verified by grep -- everything except the in-scope `remove.rs`
+ `shared.rs`): `mount.rs` (`two_disk_membership`, `three_disk_membership`,
`base_two_disk_runner`), `status.rs`, `unlock.rs`, the device->UUID map in
`remove_missing.rs` (`luks_uuid_for_device`), and in `replace.rs` both the
`canonical_dev_to_uuid` map and `PoolFixture::one_live_only`. Each converts
independently against the `shared.rs` generator without rework. (`ack.rs`,
`doctor.rs`, and `lock.rs` are deliberately NOT in this list: their only
repeated-segment UUID literal is the btrfs *filesystem* UUID
`aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee`, not a per-disk LUKS identity, so the
single-repeated-hex-digit `canonical_luks_uuid(n)` does not apply to them.)

## Implementation notes

- `canonical_luks_uuid`'s segment builder uses `std::iter::repeat_n(d, len)`
  rather than the plan's `std::iter::repeat(d).take(len)`. Clippy's
  `manual_repeat_n` lint (warning-by-default, and `just clippy` is warning-clean)
  flags the latter; the two are behaviorally identical, and the byte-for-byte
  pin test `canonical_luks_uuid_pins_repeated_digit_literals` passes unchanged.
