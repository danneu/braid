# Plan: simplify add's name/by-id conflict pre-check

## Context

`plan_add` runs a fail-fast gate that refuses an add when a new disk's name
or by-id already belongs to a *different* existing pool member, before any
probe / passphrase / inhibitor / journal side effect. Today that gate
(`cli/src/add.rs`, the block under "Route conflict-detection through
`PoolMembership::insert` on a throwaway clone", ~lines 1698-1734) fabricates
per-spec sentinel UUIDs (`fffffff{n}-...`) purely to drive the four-axis
`PoolMembership::insert`, so that only its name and by-id axes can fire.

This is over-built and has a latent wart:

- **Non-obvious construct.** A reader must reason about why fake UUIDs are
  safe, why they're seeded distinctly, and why the exact-existing-member skip
  is needed. The comment even defends an *impossible* case ("two specs hitting
  the same existing name/by-id"): the in-invocation duplicate-name and
  duplicate-by-id checks already ran above (~lines 1650-1676), so every spec
  is already mutually distinct.
- **Sentinel leak.** `insert`'s messages embed the UUID being inserted, so a
  real conflict currently renders operator-facing text like
  `membership error: name 'disk1' already in use under UUID <real> while
  inserting UUID fffffff0-ffff-ffff-ffff-000000000001` -- a synthetic UUID in
  user output.
- **Untested.** No add-layer test exercises either conflict arm (name match /
  different by-id, or by-id match / different name). The exact-re-add *skip*
  is covered (many tests re-add the seeded `disk1`); the conflict arms are
  not (0 assertions of the conflict message outside `membership.rs`).

The full four-axis invariant (UUID + name + by-id + devid) is **not** weakened
by this change: it is enforced at commit by the real
`target_membership.insert(uuid.clone(), ...)` with the actual per-target UUID
(`cli/src/add.rs` ~lines 1281-1292, "defense-in-depth backstop"). This block
is purely a pre-probe fail-fast.

Outcome: replace the placeholder dance with two direct lookups that say
exactly what they mean, drop the sentinel from operator output, and add the
missing behavioral coverage.

## Decision: inline direct lookups (no new `PoolMembership` API)

Express the gate inline in `plan_add` using the dedicated `by_name` /
`by_by_id` axes. Rejected alternative: a `PoolMembership::check_*` helper --
it would be single-caller, would force either a double `by_name` scan or
bake add's "exact re-add is a tolerated no-op" *policy* into the otherwise
policy-free membership primitive, and there is no identical string to
deduplicate (the pre-probe message is legitimately shorter -- no
"while inserting UUID" because no UUID has been assigned yet). Inline is the
single-scan shape and folds the skip into the `by_name` match naturally.

## Change 1 -- replace the pre-check block (`cli/src/add.rs#plan_add`)

Replace the comment + placeholder loop (~lines 1698-1734) with:

```rust
    // Fail-fast name/by-id conflict gate. Runs before any probe, passphrase
    // read, inhibitor acquisition, or journal write so a name/by-id collision
    // with an existing member fails with zero side effects. In-invocation
    // duplicate names and by-ids were already rejected above, so each spec is
    // mutually distinct and only needs checking against existing members.
    //
    // Only the name and by-id axes are checked here: no LUKS UUID has been
    // assigned yet (it is generated/probed per target later) and devid is
    // enrichment-only. The full four-axis uniqueness invariant is enforced by
    // the real `PoolMembership::insert` that builds `target_membership` at
    // commit time -- this is the early fail-fast, that is the backstop.
    for (name, by_id) in &parsed {
        if let Some((existing_uuid, existing)) = pool_membership.by_name(name) {
            // Exact existing member (same name AND by-id): re-specifying a
            // disk already in the pool is the documented already-in-pool
            // no-op, classified downstream -- not a conflict.
            if &existing.by_id == by_id {
                continue;
            }
            return Err(PlanFailure::empty(
                membership::MembershipError::Conflict(format!(
                    "name '{name}' already in use under UUID {existing_uuid}"
                ))
                .into(),
            ));
        }
        if let Some((existing_uuid, _)) = pool_membership.by_by_id(by_id) {
            return Err(PlanFailure::empty(
                membership::MembershipError::Conflict(format!(
                    "by_id '{by_id}' already in use under UUID {existing_uuid}"
                ))
                .into(),
            ));
        }
    }
```

Notes for the implementer:
- Keep the `MembershipError::Conflict` error type (so it still maps via
  `AddError::Membership(#[from] membership::MembershipError)` at
  `cli/src/add.rs:139`); match the file's existing `membership::` reference
  style. Drop the `while inserting UUID {uuid}` suffix -- there is no UUID yet.
- `by_name`/`by_by_id` are at `cli/src/membership.rs#by_name` /
  `#by_by_id` (O(n) over the tiny pool). `DiskName`/`ByIdPath`/`LuksUuid` all
  impl `Display`, already used for these messages elsewhere in the file.
- The `continue` on exact match is load-bearing: without it, an exact re-add
  would fall through to the `by_by_id` arm and falsely conflict on its own
  by-id.
- Do **not** touch `PoolMembership::insert` or `load_membership_from` wording
  (out of scope; their "while inserting UUID" suffix is correct there).

## Change 2 -- add the two missing conflict-arm tests (`cli/src/add.rs` tests)

Structure each test on `duplicate_by_id_rejected` (~line 6760) -- `cmd_add`,
`dry_run: false`, `yes: true`, the fail-fast assertions (offender named, no
journal, `acquire_count() == 0`) -- plus the passphrase-reader sentinel from
`cmd_add_refuses_when_pool_locked_with_membership` (~line 8326, see below).
Use `add_test_setup()`, which seeds a member `disk1` =
`/dev/disk/by-id/virtio-disk1`. Each opens with the required `//` Intent /
Why it exists / Scenario preamble.

**Test A -- name collision, different by-id.** `disk_specs:
&["disk1=/dev/disk/by-id/virtio-disk9".into()]`. Assert the error names
`disk1` and "in use"; assert behavioral fail-fast invariants; regression-guard
the sentinel leak:

```rust
    let err = result.expect_err("name collision must be rejected").to_string();
    assert!(err.contains("disk1") && err.contains("in use"),
        "error must name the colliding disk name, got: {err}");
    assert!(!err.contains("fffffff"),
        "operator output must not leak a synthetic placeholder UUID, got: {err}");
    assert!(journal::load_journal(&paths).unwrap().is_none(),
        "no journal after pre-probe validation failure");
    assert_eq!(inhibitor.acquire_count(), 0,
        "validation failure must NOT acquire the sleep inhibitor");
    assert_eq!(tty.remaining(), 1,
        "conflict refused before passphrase read");
```

**Test B -- by-id collision, different name.** `disk_specs:
&["disk9=/dev/disk/by-id/virtio-disk1".into()]`. Same assertions, but assert
the error names `/dev/disk/by-id/virtio-disk1` (+ "in use", `!fffffff`, no
journal, `acquire_count() == 0`, `tty.remaining() == 1`).

Mocks for both (identical to the template): `fs = AddMockFs(vec![])`,
`runner = MockRunner::default()` (the gate fires before the first
`runner.run()` at `probe_pool`, so an empty runner never returns `MissingMock`
-- add the same explanatory comment as the template),
`crate::inhibit::RecordingInhibitor::new()`, `crate::confirm::RecordingConfirm::new()`,
`backing_path_resolver: crate::test_fixtures::mock_virtio_offset_backing_path_resolver()`.

Pin the "fails before credentials are read" half of the fail-fast contract the
way `cmd_add_refuses_when_pool_locked_with_membership` (~line 8326) does, NOT
with a real `passphrase_file`: set `passphrase_file: None`, construct
`let tty = ScriptedPassphraseReader::new(["SENTINEL"]);`, pass
`passphrase_reader: &tty`, and assert `tty.remaining() == 1`. A valid
`passphrase_file` would NOT pin this -- a regression that moved the gate after
credential reading would still error and still pass, since a file read is
silent. The scripted reader makes "the queue was never popped" an explicit
assertion. `ScriptedPassphraseReader` is already imported in the add.rs test
module (`use crate::luks::{RealTty, ScriptedPassphraseReader};`, ~line 2441);
destructure `add_test_setup()`'s passphrase path as `_pass_path` (unused).

These are behavioral and structure-insensitive: they assert the operator
contract (offender named, no sentinel, no journal, no inhibitor, no passphrase
read), not the placeholder mechanism, so they survive the refactor and guard
the fix.

## Out of scope / non-goals

- No change to the four-axis `PoolMembership::insert` or to
  `load_membership_from` -- the real invariant + backstop are untouched.
- No new `PoolMembership` method (see Decision).
- No change to downstream already-in-pool classification (the skip preserves
  the exact-re-add no-op path, already covered by existing tests such as
  `cmd_add_mixed_already_in_pool_and_fresh_verifies_each_disk_once`).

## Verification

- `just test-rust` -- full Rust lib/bin lane; must stay green (guards the
  unchanged exact-re-add skip and the four-axis backstop).
- Targeted (`cargo test` takes a single `[TESTNAME]` filter, so one command
  per test): `cargo test --lib add_rejects_name_collision_with_existing_member`,
  then `cargo test --lib add_rejects_by_id_collision_with_existing_member`.
- `cargo clippy --all-targets` -- no new warnings (the first `if let` binds
  `existing_uuid`, used on the conflict path).
- Confirm no regression in the conflict-message golden/substring expectations
  elsewhere: `rg "while inserting UUID" cli/src` should still match only
  `membership.rs` (insert + load), never `add.rs`.

## Behavior-preservation argument

In-invocation dedup (~1650-1676) guarantees specs have mutually-distinct names
and by-ids, so the old loop's cumulative inserts into the throwaway clone could
only ever conflict against *original* members -- exactly what direct lookups
against `pool_membership` do. Check order (name axis before by-id) matches
`insert`. The only operator-visible difference is the removed sentinel UUID in
the message, which is the intended improvement.
