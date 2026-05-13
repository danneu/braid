# Unify orphan emission in `build_close_sets_full` and surface `DuplicateDevid`

## Context

`command-findings/lock.md` raises a paired Medium finding against
`cli/src/lock.rs::build_close_sets_full` (lines 781-874):

- **Finding (1) -- Correctness.** Pass 2 (the `pool.null_underlying`
  loop) collapses `Err(MembershipError::DuplicateDevid)` and
  `Ok(None)` into the same silent orphan demote (lock.rs:829-838).
  The corrupt-membership signal that decision 024 introduced the typed
  error specifically to surface is discarded; `braid lock` then closes
  the device without warning and the operator never learns
  `pool.json` has two members claiming the same devid.
- **Finding (2) -- Testing.** Three classification branches have no
  direct unit coverage: Pass 1 orphan (UUID absent from membership),
  Pass 2 `null_underlying` orphan, and Pass 2 `DuplicateDevid`. All
  existing orphan coverage goes through the Pass 3 stranded scan
  (`push_uuid_classified_candidate`) or the fallback.

Verification (verify-issue investigation) showed Pass 1 and Pass 2
also silently push orphans -- only Pass 3 emits the
`orphan_mapper_warn_body` warn. The proposed test (a) ("emits the
expected warn note") implicitly demands Pass 1 stop being silent,
which the finding pair cannot achieve without a small unification
refactor.

This change lands the correctness fix, dissolves the silent-Pass-1/2
inconsistency by extracting one shared push helper, and adds the
proposed (a)/(b)/(c) regression tests on top of the now-uniform
contract.

## Approach

Extract a single `push_orphan_close` helper that all three passes
call to emit an orphan. Promote Pass 2's `_ =>` collapse to three
explicit match arms so `DuplicateDevid` becomes a typed warn + skip
+ `cleanup_uncertain` rather than a silent orphan close. Refactor
Pass 3's `push_uuid_classified_candidate` to delegate its Orphan arm
to the same helper so there is one source of truth for the orphan
warn body across the full and fallback paths.

Helper choice (vs. introducing a `Classification` enum that every
pass returns): the three passes have genuinely different lookup
shapes (`by_uuid -> Option`, `by_devid -> Result<Option, _>`,
`classify_candidate_mapper -> Result<LockMapperCloseKind, CmdError>`
after a cryptsetup round-trip). A unifying enum would force each
call site to lift its lookup into the enum, then dispatch -- more
code than the duplication it removes. A 5-line `push_orphan_close`
keeps each lookup readable while pinning the orphan emission in one
place.

## Files to change

- `cli/src/lock.rs` -- helpers, Pass 1 + Pass 2 + Pass 3 rewires,
  doc-comment refresh, three new unit tests, one new fixture helper.
- (No code change required in `cli/src/membership.rs` or
  `cli/src/types.rs` -- the variant, fields, and `format_uuid_list`
  helper already exist.)

## Step-by-step

### 0. Imports

Three names are referenced by the new helpers and the Pass 2 match
that are not yet in scope in `cli/src/lock.rs:1-12`:

- `MembershipError` -- extend `use crate::membership::PoolMembership;`
  (line 4) to `use crate::membership::{MembershipError, PoolMembership};`.
- `LuksUuid` and `format_uuid_list` -- extend
  `use crate::types::{DiskName, MapperName, MountPoint, PoolState};`
  (line 11) to add `LuksUuid` and `format_uuid_list`.

(Test-only `use crate::types::{...}` blocks inside test fns are
unaffected; this is the file-level import update.)

### 1. Add `push_orphan_close` helper

Insert next to the existing `*_warn_body` helpers
(`cli/src/lock.rs:223-256`). Signature:

```rust
fn push_orphan_close(
    notes: &mut Vec<PreviewNote>,
    orphan_mappers: &mut Vec<LockMapperClose>,
    mapper: MapperName,
    disk_name: String,
) {
    notes.push(PreviewNote::Warn(orphan_mapper_warn_body(&mapper)));
    orphan_mappers.push(LockMapperClose {
        mapper,
        kind: LockMapperCloseKind::Orphan { disk_name },
    });
}
```

Doc comment: "Shared orphan emission so Pass 1, Pass 2, and the
Pass 3 stranded path produce byte-identical `[warn] orphaned mapper
...` rendering and route through one append-to-orphan_mappers
site."

### 2. Add `duplicate_devid_warn_body`

Insert next to `skipped_mapper_warn_body`
(`cli/src/lock.rs:243-245`). Signature:

```rust
/// Operator-facing body for the Pass 2 skip when persisted devid
/// resolution surfaces corrupt membership (two members claiming the
/// same devid). Centralizes the warning so the mapper name, the
/// colliding devid, and the offending UUID set all render through
/// one format and stay grep-compatible with `skipping mapper`.
fn duplicate_devid_warn_body(
    entry: &MapperName,
    devid: u64,
    members: &[LuksUuid],
) -> String {
    format!(
        "skipping mapper {entry}: pool.json corrupt -- devid {devid} \
         claimed by multiple members [{}] (resolve before locking)",
        format_uuid_list(members),
    )
}
```

`format_uuid_list` is already `pub(crate)` in
`cli/src/types.rs:67` (also drives `MembershipError::DuplicateDevid`'s
`Display` body at `cli/src/membership.rs:47`); the file-level import
was added in Step 0. The `"skipping mapper {entry}:"` prefix matches
`skipped_mapper_warn_body` so operator-side grep on `skipping mapper`
keeps working. The doc comment satisfies the AGENTS.md "new top-level
function" rule (justify the boundary, not the signature).

### 3. Rewire Pass 1 (`cli/src/lock.rs:805-817`)

Replace the inline `else` block with:

```rust
} else {
    let disk_name = name_from_mapper(dev.mapper.as_str())
        .unwrap_or(dev.mapper.as_str())
        .to_owned();
    push_orphan_close(notes, &mut orphan_mappers, dev.mapper.clone(), disk_name);
}
```

This is the change that makes Pass 1 emit the orphan warn for the
first time. No existing test asserts `notes.is_empty()` after a
Pass 1 invocation (`full_arm_classifies_drifted_member_by_uuid_into_member_owned`
at `:2955` only checks `member_summaries` / `orphan_summaries`).

### 4. Rewire Pass 2 (`cli/src/lock.rs:820-839`)

Replace the two-arm match with three explicit arms:

```rust
for nu in &pool.null_underlying {
    match membership.by_devid(nu.devid) {
        Ok(Some((_uuid, member))) => member_owned.push(LockMapperClose {
            mapper: nu.mapper.clone(),
            kind: LockMapperCloseKind::MemberOwned {
                display_name: member.name.clone(),
            },
        }),
        Ok(None) => {
            let disk_name = name_from_mapper(nu.mapper.as_str())
                .unwrap_or(nu.mapper.as_str())
                .to_owned();
            push_orphan_close(notes, &mut orphan_mappers, nu.mapper.clone(), disk_name);
        }
        Err(err) => match err {
            MembershipError::DuplicateDevid { devid, members } => {
                notes.push(PreviewNote::Warn(duplicate_devid_warn_body(
                    &nu.mapper, devid, &members,
                )));
                skipped_mappers.push(nu.mapper.clone());
                *cleanup_uncertain = true;
            }
            // `by_devid` only constructs `DuplicateDevid` today
            // (`cli/src/membership.rs:284-302`); the other
            // `MembershipError` variants come from load/parse paths.
            // Listing them explicitly makes adding a future variant
            // a compile error here rather than a silent fall-through.
            // The `other @` binding re-binds the inner-match value so
            // it can be formatted in `unreachable!` (the outer `err`
            // is moved by the inner `match err`).
            other @ (MembershipError::Corrupt { .. }
                | MembershipError::Conflict(_)
                | MembershipError::Io { .. }
                | MembershipError::Save { .. }) => {
                unreachable!(
                    "by_devid cannot return this MembershipError variant: {other:?}"
                );
            }
        },
    }
}
```

The outer match is exhaustive on `Result<Option<_>, MembershipError>`;
the inner `match err` is exhaustive on `MembershipError`'s five
variants (`cli/src/membership.rs:28-66`). The `unreachable!` arm is
preferred over `_ =>` so a future `MembershipError` variant added in
`membership.rs` produces a compile-time error in `lock.rs` rather than
silently routing through whichever fall-through the author wrote.

### 5. Refactor `push_uuid_classified_candidate` (Pass 3) (`cli/src/lock.rs:260-286`)

Replace the `Ok(kind @ LockMapperCloseKind::Orphan { .. })` block
(`:274-277`) with:

```rust
Ok(LockMapperCloseKind::Orphan { disk_name }) => {
    push_orphan_close(notes, orphan_mappers, mapper, disk_name);
}
```

Net behavior identical -- the helper produces the same
`PreviewNote::Warn(orphan_mapper_warn_body(&mapper))` push and the
same `orphan_mappers.push(LockMapperClose { ... })`. After this
step `orphan_mapper_warn_body` has one caller
(`push_orphan_close`).

### 6. Tighten the Pass 3 exclusion set (`cli/src/lock.rs:841-849`)

Today `already_observed` is built from `member_owned` and
`orphan_mappers`:

```rust
let already_observed: HashSet<&str> = member_owned
    .iter()
    .map(|m| m.mapper.as_str())
    .chain(orphan_mappers.iter().map(|o| o.mapper.as_str()))
    .collect();
```

This works only as long as every `pool.devices` and
`pool.null_underlying` mapper lands in one of those two vectors --
which is true pre-change but no longer true once Step 4 routes a
Pass 2 `DuplicateDevid` mapper into `skipped_mappers`. Pass 3 would
then re-scan that mapper through `push_uuid_classified_candidate`,
either duplicating the skip warning, double-inserting into
`skipped_mappers`, or (if the backing UUID has since become
readable) closing the very mapper Pass 2 refused.

Rebuild the exclusion set from the source `PoolState` directly so it
matches the existing Pass-3 comment ("slots... that did NOT appear in
pool.devices or pool.null_underlying") regardless of how Pass 1/2
classified each mapper:

```rust
let already_observed: HashSet<&str> = pool
    .devices
    .iter()
    .map(|d| d.mapper.as_str())
    .chain(pool.null_underlying.iter().map(|n| n.mapper.as_str()))
    .collect();
```

This change is safe across every Pass 1/2 outcome: member, orphan,
skip, or future classes. Test (c) (Step 10 below) exercises the
exact regression -- include `/dev/mapper/braid-dup` in the test's
`lock_fs` so `scan_braid_mapper_candidates` would surface it as a
Pass-3 candidate, and assert `runner.requests()` contains no
`CryptsetupStatus { mapper: braid-dup }` entry.

### 7. Update `build_close_sets_full` doc comment (`cli/src/lock.rs:777-783`)

Rewrite to describe all three passes' uniform contract:

> "All three passes emit `orphan_mapper_warn_body` when they
> classify a mapper as orphan, and Pass 2 surfaces
> `MembershipError::DuplicateDevid` as a typed warn that skips the
> close and sets `cleanup_uncertain` (so the operator must
> reconcile corrupt pool.json before lock can run cleanly)."

### 8. Cross-check the fallback (`cli/src/lock.rs:879-917`)

`build_close_sets_uuid_scanned_fallback` only iterates through
`push_uuid_classified_candidate`, which calls
`classify_candidate_mapper` and never invokes `by_devid`. So the
fallback cannot hit `DuplicateDevid` -- finding (1) has no fallback
analog. No code change; consider a one-line comment near the
fallback definition pointing out this asymmetry for the next
reviewer.

### 9. Add fixture helper next to `synthetic_pool_state` (`cli/src/lock.rs:2914-2940`)

```rust
fn synthetic_pool_state_with_null_underlying(
    mapper_aaa: &str,
    null_mapper: &str,
    null_devid: u64,
) -> crate::types::PoolState {
    use crate::types::{LuksUuid, MapperName, NullUnderlyingDevice, PoolDevice, PoolState};
    PoolState {
        mounted: true,
        devices: vec![PoolDevice {
            mapper: MapperName(mapper_aaa.into()),
            luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bc").unwrap(),
            devid: 1,
            underlying: "/dev/disk/by-id/a".into(),
        }],
        missing_count: 1,
        total_devices: 2,
        fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
        missing_devids: vec![null_devid],
        null_underlying: vec![NullUnderlyingDevice {
            mapper: MapperName(null_mapper.into()),
            devid: null_devid,
        }],
    }
}
```

`NullUnderlyingDevice` is defined at `cli/src/types.rs:413-419`.

### 10. Add the three unit tests

Insert after the existing `full_arm_*` tests (around
`cli/src/lock.rs:3134`). Each preamble follows the literal `//`
line-comment form from `docs/testing.md:11-22` (Intent / Why it
exists / Scenario), not `///` doc comments.

**Test (a) -- Pass 1 unknown UUID:**
```rust
// Intent: a PoolDevice whose luks_uuid is absent from membership is
//   classified as orphan AND emits the orphan_mapper_warn_body warn
//   -- not silently demoted as it was pre-unification.
// Why it exists: pins finding (2a) so the silent-Pass-1 orphan path
//   cannot regress; without this test, Pass 1 could be reverted to
//   its silent shape and only Pass 3 (stranded scan) would still warn.
// Scenario: post-migration `braid lock` sees a live mapper whose
//   backing UUID does not match any pool.json member (leftover from a
//   prior `braid replace` that died mid-flight before pool.json was
//   rewritten).
#[test]
fn full_arm_pass1_unknown_uuid_classifies_as_orphan_and_warns() { ... }
```
Builds: a custom `PoolState` with one `PoolDevice {
mapper: "braid-leftover", luks_uuid: ORPHAN_UUID }`
(`ORPHAN_UUID` constant at `cli/src/lock.rs:976`).
Asserts: `orphan_summaries` contains `("braid-leftover", "leftover")`;
`member_summaries` empty; `notes` contains a `PreviewNote::Warn`
whose body contains `"orphaned mapper braid-leftover"`; `skipped`
empty; `cleanup_uncertain` is false (orphans are normal cleanup).

**Test (b) -- Pass 2 unknown devid:**
```rust
// Intent: a null_underlying entry whose devid is absent from
//   membership is classified as orphan AND emits the orphan warn.
// Why it exists: pins finding (2b) -- Pass 2's Ok(None) arm was
//   previously silent.
// Scenario: a hot-unplugged device whose mapper is still open but
//   whose devid never landed in pool.json (e.g. yanked mid-`braid
//   add` before pool.json was committed).
#[test]
fn full_arm_pass2_null_underlying_unknown_devid_classifies_as_orphan_and_warns() { ... }
```
Builds: `synthetic_pool_state_with_null_underlying("braid-aaa", "braid-ghost", 99)`
+ `lock_test_membership` (which has no member with devid 99).
Asserts: `orphan_summaries` contains `("braid-ghost", "ghost")`;
`notes` carries a `PreviewNote::Warn` whose body contains
`"orphaned mapper braid-ghost"`; `skipped` empty;
`cleanup_uncertain` false.

**Test (c) -- Pass 2 DuplicateDevid:**
```rust
// Intent: a null_underlying entry whose devid is claimed by two
//   members in the in-memory PoolMembership surfaces a typed
//   DuplicateDevid warn, lands in skipped_mappers, does NOT land in
//   orphan_mappers or member_owned, and sets cleanup_uncertain =
//   true. Pass 3 must NOT rescan the skipped mapper.
// Why it exists: pins finding (1) -- defense-in-depth against the
//   silent-demote-to-orphan path. `load_membership` already rejects
//   duplicate value-side devids at load time
//   (`cli/src/membership.rs:475-489`, surfaced as
//   `MembershipError::Conflict`), so `by_devid`'s `DuplicateDevid`
//   is unreachable from a real pool.json today. This test guards
//   against a future code path bypassing the load-time sweep and
//   confirms `build_close_sets_full` would still refuse to silently
//   close. Also pins the Pass-3 rescan gap (already_observed must
//   include every pool.devices and pool.null_underlying mapper, not
//   just those that landed in member_owned / orphan_mappers).
// Scenario: an in-memory PoolMembership built via the test-only
//   `PoolMembership::for_corruption_tests` constructor (which
//   bypasses the four-axis uniqueness check, mirroring
//   `membership.rs::by_devid_returns_duplicate_devid_on_corruption`)
//   with two members sharing devid 7.
#[test]
fn full_arm_pass2_duplicate_devid_skips_and_warns_with_cleanup_uncertain() { ... }
```
Builds:
- A `PoolMembership` carrying members `aaa`/`bbb` both with
  `devid = Some(7)` via the existing test-only constructor
  `PoolMembership::for_corruption_tests` (`cli/src/membership.rs:395`,
  `pub(crate)`, `#[cfg(test)]`). That constructor takes
  `Vec<(LuksUuid, DiskMember)>` and inserts directly without the
  four-axis sweep -- it exists for exactly this scenario, mirroring
  how `membership.rs:939-954` builds its own corruption fixture
  in-module. Building each `DiskMember` via `DiskMember::new(name,
  by_id)` then setting `dm.devid = Some(7)` (the `devid` field is
  `pub` at `membership.rs:244`, so no piercing of private state is
  needed). Do NOT attempt `PoolMembership { disks: LuksUuidMap(...) }`
  directly from `lock.rs` -- both the field and the inner tuple are
  private outside `membership.rs`.
- `synthetic_pool_state_with_null_underlying("braid-aaa", "braid-dup", 7)`.
- `lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-dup"])` --
  including `braid-dup` is deliberate: it forces
  `scan_braid_mapper_candidates` to surface `braid-dup` as a Pass-3
  candidate, so the Step-6 exclusion fix is what stops the rescan.
- `MockRunner::default()` -- no `CryptsetupStatus { mapper: braid-dup }`
  mock; if Pass 3 incorrectly rescanned it, `dispatch` would record
  the request and return `CmdError::MissingMock`, producing a spurious
  `skipped_mapper_warn_body` warn and a second `skipped_mappers` entry.

Asserts:
- `member_summaries(&close_set)` and `orphan_summaries(&close_set)`
  both exclude `braid-dup`.
- `skipped == vec![MapperName("braid-dup".into())]` (exactly one
  entry; a Pass-3 rescan would produce two).
- `cleanup_uncertain == true`.
- `notes` contains a `PreviewNote::Warn` whose body contains all four
  pinned substrings: `"braid-dup"`, `"devid 7"`, the AAA UUID
  `"...02bc"`, and the BBB UUID `"...02bd"`.
- `notes` contains no `PreviewNote::Warn` whose body contains
  `"cannot verify backing LUKS UUID"` (the `skipped_mapper_warn_body`
  marker), which would indicate a Pass-3 rescan.
- `runner.requests()` (exposed by `MockRunner` at `cli/src/cmd.rs:1104`)
  contains no `CmdRequest::CryptsetupStatus { mapper }` whose mapper
  string equals `"braid-dup"`. This is the direct assertion that Pass
  3 honored the Step-6 exclusion.

## Existing helpers reused

- `orphan_mapper_warn_body` (`cli/src/lock.rs:236`)
- `skipped_mapper_warn_body` (`cli/src/lock.rs:243`) -- style
  precedent for the new `duplicate_devid_warn_body`
- `format_uuid_list` (`cli/src/types.rs:67`) -- also drives
  `MembershipError::DuplicateDevid`'s `Display` body at
  `cli/src/membership.rs:47`
- `PoolMembership::for_corruption_tests`
  (`cli/src/membership.rs:395`, `pub(crate)`, `#[cfg(test)]`) --
  bypasses the four-axis uniqueness sweep so test (c) can build an
  in-memory membership with two members sharing a devid without
  touching `LuksUuidMap`'s private inner field
- `name_from_mapper` (`cli/src/config.rs:71-79`)
- `lock_test_membership` (`cli/src/test_fixtures/lock.rs:127`)
- `synthetic_pool_state` (`cli/src/lock.rs:2917`) -- new
  null_underlying helper sits adjacent
- `ORPHAN_UUID` constant (`cli/src/lock.rs:976`)
- `member_summaries`, `orphan_summaries` test helpers (already
  present in the `lock.rs` test module)

## Risks and existing-test impact

- **Pinned wording**: `lock_dry_run_warns_about_orphan_mapper`
  (`cli/src/lock.rs:1559`) and the umount-failed dry-run
  (`cli/src/lock.rs:1606`) literal-compare
  `"[warn] orphaned mapper braid-ccc (not in pool.json -- likely a
  prior crash)\n"`. The refactor must keep
  `orphan_mapper_warn_body`'s body byte-identical -- it does, the
  helper is moved not rewritten.
- **No silent-Pass-1 assertion exists**: `grep -n "notes.is_empty"`
  in `cli/src/lock.rs` shows zero hits, and the only Pass-1 test
  (`full_arm_classifies_drifted_member_by_uuid_into_member_owned`,
  `:2955`) checks only the member/orphan vectors. Adding the warn
  to Pass 1 is invisible to existing tests.
- **`DuplicateDevid` is defense-in-depth today**: `load_membership_from`
  enforces a secondary uniqueness sweep on value-side devids at load
  time (`cli/src/membership.rs:475-489`), surfacing duplicates as
  `MembershipError::Conflict` before `cmd_lock` ever runs. So a real
  pool.json with duplicate devids cannot reach `by_devid`'s
  `DuplicateDevid` branch today. The Pass-2 typed handling and test
  (c) protect against a future code path (e.g. journal replay, in-memory
  mutation, or partial state reconstruction) that builds a
  `PoolMembership` without going through the load-time sweep. The
  `pub(crate) for_corruption_tests` constructor exists precisely to
  exercise this kind of "constructed state, not loaded state" scenario.
- **Pass-3 exclusion regression risk**: rebuilding `already_observed`
  from `pool.devices` + `pool.null_underlying` (Step 6) is broader
  than the current "everything in member_owned + orphan_mappers" set
  only when a Pass-1/2 mapper does NOT land in either of those
  vectors. Today that never happens; after Step 4 it happens for
  `DuplicateDevid` skips. The two sets agree on every pre-existing
  test (and continue to agree for tests (a) and (b)); only test (c)
  distinguishes them, which is its purpose.
- **`MembershipError` exhaustiveness**: today `by_devid` only ever
  returns `DuplicateDevid` (`cli/src/membership.rs:284-302`), but the
  static return type is `Result<_, MembershipError>` -- a flat
  `Err(MembershipError::DuplicateDevid {..})` arm would fail to
  compile because the four load/parse variants (`Corrupt`,
  `Conflict`, `Io`, `Save`) are not covered. The Step-4 shape uses a
  nested `Err(err) => match err { ... }` with the impossible variants
  enumerated and routed to `unreachable!`. If a future
  `MembershipError` variant is added in `membership.rs`, this match
  fails to compile in `lock.rs` -- the right outcome (forces a
  conscious decision rather than a silent fall-through).
- **Decision-doc anchor (optional polish)**:
  `docs/decisions/024-luks-uuid-identity.md` does not currently
  reference `DuplicateDevid` at all (`grep` confirms zero hits in
  the docs/decisions/ tree). A one-line addition pointing to
  `build_close_sets_full` Pass 2 is welcome but not required for
  this change; flag it for follow-up.

## Verification

1. `just test-rust` -- runs the three new unit tests plus the full
   `lock::tests` module. Expected: all pass.
2. `just test-vm braid-lock-orphan` -- guards the byte-identical
   wording at `cli/src/lock.rs:1559` against the helper extraction.
3. `just test-vm braid-lock` -- umbrella happy-path integration.
4. `cargo clippy --workspace -- -D warnings` -- catches an
   accidental unreachable `_` arm in the new Pass 2 match, or any
   unused import after the refactor.

After the change, finding (1) and finding (2) in
`command-findings/lock.md` are both fully addressed and can be
struck.
