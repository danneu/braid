# Plan: gate `discover --write` against a healthy UUID-keyed `pool.json`, and pin the rebuild contract by test

## Context

A code-review finding (Severity: Medium, Category: Testing) flagged that
`braid discover --write` has no unit-test coverage for the case where
`pool.json` already exists as `ValidUuidKeyed` or `Corrupt`, and that
the manual claims a refusal the code does not implement. The
`/verify-issue` investigation confirmed:

1. **There IS a real behavior bug for `ValidUuidKeyed`.**
   `write_discovered_membership` (`cli/src/discover.rs:538-566`) only
   refuses for `LegacyNameKeyed`; an existing healthy UUID-keyed
   `pool.json` is silently overwritten by `save_membership`, dropping
   every persisted `DiskMember.devid`. Per decision 024
   (`docs/decisions/024-luks-uuid-identity.md:21-24, 115-117,
   143-148`), those devids are the authorized fallback identity for
   `null_underlying` mappers and btrfs `missing_devids`, so a clobber
   here breaks `recover` / `remove-missing` / `replace` for any member
   that is currently observable only by devid. The wrapper lock at
   `/run/braid-pool.lock` (per decision 018 and
   `modules/braid/braid-wrapper.sh:53-56`) prevents concurrent braid
   operations but does not prevent an operator from running
   `discover --write` sequentially against an already-healthy pool;
   that is the hazard this gate closes.

2. **For `Corrupt`, the current allow-and-rebuild behavior is correct,
   not a bug.** Decision 017
   (`docs/decisions/017-runtime-disk-membership.md:99`) names
   `braid discover --write` as the explicit recovery path for lost or
   corrupt `pool.json`. The same wording is pinned in four
   operator-facing sites: `MembershipError::Corrupt` Display
   (`cli/src/membership.rs:30-36`), the `refresh_pool_metadata`
   warning, the bare-discover refusal arm at
   `cli/src/main.rs:803-808`, and `docs/luks-unlock.md:146`. The
   finding's prescription of asserting refusal for `Corrupt` would
   contradict that contract.

3. **The introducing plan acknowledged the `--write` gap.**
   `plans/impl/2026-05-13-discover-valid-pool-json-refusal.md:131-133`:
   "the `Corrupt`/`ValidUuidKeyed` variants are only consulted by the
   read-only path." The bare arm at `cli/src/main.rs:782-811` was
   wired up to distinguish all four shapes; the `--write` arm was
   not.

This plan supersedes the sibling
`plans/wip/plan-the-ideal-pivot-bright-duckling.md`, adopting its
design and adding the missing `Corrupt`-proceeds unit test so the
rebuild contract is pinned by code instead of by plan history alone.

## Pivot from the finding's prescription

The finding asked for two refusal tests: (a) `ValidUuidKeyed`, (b)
`Corrupt`. The right work pivots on two axes:

- **Test alone isn't enough for (a).** The behavior is buggy. A test
  pinning current behavior locks in the bug; a test pinning desired
  behavior fails. Fix the behavior first (add a `ValidUuidKeyed`
  gate), then add the test.
- **Flip the assertion on (b).** `Corrupt` is the documented rebuild
  path. Assert `discover --write` PROCEEDS against a `Corrupt`
  `pool.json`, not that it refuses.

The finding's "preserve devid" alternative is rejected: `discover
--write` is a "rebuild from scratch" surface, and preserving some
persisted fields while re-keying others would be asymmetric and
surprising.

## Changes

### 1. Add the `ValidUuidKeyed` error variant

**File:** `cli/src/discover.rs`

In the `DiscoverWriteError` enum (lines 155-189), insert after
`NameKeyedPoolJson` (after line 171):

```rust
/// Existing `pool.json` on disk is already a healthy UUID-keyed
/// membership. `discover --write` would clobber persisted
/// `DiskMember.devid` bindings, which are decision 024's authorized
/// fallback identity for `null_underlying` mappers and btrfs
/// `missing_devids`. The operator must move the file aside; `discover`
/// is not the surface for mutating an established pool.
#[error(
    "discover refusing to write pool.json: existing file at {path} is already a healthy UUID-keyed membership -- back it up and move it aside before retrying, or use 'braid add' / 'braid remove' / 'braid replace' to mutate membership (see docs/luks-unlock.md)"
)]
ValidUuidKeyed { path: String },
```

The `back it up and move it aside before retrying` phrase mirrors
`NameKeyedPoolJson`'s wording at line 169 so the same runbook in
`docs/luks-unlock.md:148-152` applies. The `add` / `remove` /
`replace` pointer mirrors the bare-discover refusal at
`cli/src/main.rs:798`.

### 2. Promote the gate to an exhaustive match

**File:** `cli/src/discover.rs:550-555`

Replace:

```rust
let pool_json_path = paths.pool_json();
if classify_pool_json(&pool_json_path) == PoolJsonShape::LegacyNameKeyed {
    return Err(DiscoverWriteError::NameKeyedPoolJson {
        path: pool_json_path.display().to_string(),
    });
}
```

with:

```rust
let pool_json_path = paths.pool_json();
match classify_pool_json(&pool_json_path) {
    PoolJsonShape::LegacyNameKeyed => {
        return Err(DiscoverWriteError::NameKeyedPoolJson {
            path: pool_json_path.display().to_string(),
        });
    }
    PoolJsonShape::ValidUuidKeyed => {
        return Err(DiscoverWriteError::ValidUuidKeyed {
            path: pool_json_path.display().to_string(),
        });
    }
    // `Missing` is the normal first-write path. `Corrupt` is the
    // documented rebuild remediation per decision 017 -- canonical
    // membership load failed, and `discover --write` is intentionally
    // a rebuild-from-attached-disks surface that does not salvage
    // fields from invalid state.
    PoolJsonShape::Missing | PoolJsonShape::Corrupt => {}
}
```

The exhaustive `match` and the inline comment capture the design
decision in code so a future reader does not have to reconstruct it
from plan history.

### 3. Update the `write_discovered_membership` doc comment

Extend the gates list at `cli/src/discover.rs:524-537` from two items
to three. After the existing item 2 (`NameKeyedPoolJson`), add:

> 3. Existing `pool.json` must not be a healthy UUID-keyed membership
>    (covered by `ValidUuidKeyed`). `Corrupt` is intentionally allowed
>    -- it is the documented rebuild remediation per decision 017.

### 4. Add Rust unit tests

**File:** `cli/src/discover.rs` (test module, after line 1643)

Two unit tests, modeled on
`discover_write_refuses_when_pool_json_is_name_keyed`
(`cli/src/discover.rs:1617-1643`) for fixture pattern and on
`discover_write_proceeds_when_no_gates_fire`
(`cli/src/discover.rs:1782-1810`) for proceed-with-save shape. Both
include the project's `Intent / Why / Scenario` preamble per
`AGENTS.md` Test Conventions.

**a.** `discover_write_refuses_when_pool_json_is_valid_uuid_keyed`

```rust
/// Intent: write_discovered_membership refuses when on-disk pool.json
/// is a healthy UUID-keyed membership; no save happens; the existing
/// file is byte-for-byte unchanged.
/// Why: protects persisted DiskMember.devid bindings (decision 024
/// fallback identity) from a stray `braid discover --write` against
/// an already-built pool.
/// Scenario: an operator who knows their pool.json is fine reflexively
/// runs `braid discover --write` to "refresh"; the gate refuses
/// instead of clobbering the file and dropping every devid.
```

Body pattern (reuses the UUID-keyed fixture from
`classify_pool_json_returns_valid_uuid_keyed_for_loadable_pool_json`
at lines 1683-1703):

- Build a single-member `PoolMembership` and persist it via
  `save_membership` to seed a real loadable `pool.json` whose bytes
  the test can capture.
- Read the on-disk bytes into `pool_json_pre`.
- Build a separate `DiscoverOutcome` whose members are NOT the same
  set (e.g., one different UUID) so a regression that calls
  `save_membership` before the gate clobbers visibly.
- Call `write_discovered_membership(outcome, &paths, None)`; expect
  `DiscoverWriteError::ValidUuidKeyed`.
- Assert the error string contains
  `is already a healthy UUID-keyed membership`.
- Assert `std::fs::read_to_string(paths.pool_json()).unwrap() ==
  pool_json_pre`. Byte equality (not just error-type) catches a
  regression where the gate fires but `save_membership` was already
  partially applied.

**b.** `discover_write_proceeds_when_pool_json_is_corrupt`

This is the inverse of the finding's case (b). Pins decision 017's
"`discover --write` is the explicit recovery path for lost/corrupt
`pool.json`" contract as a test:

```rust
/// Intent: write_discovered_membership proceeds when on-disk pool.json
/// is corrupt; the corrupt file is replaced with a loadable
/// UUID-keyed membership.
/// Why: decision 017 names `braid discover --write` as the explicit
/// recovery path for lost/corrupt pool.json, and four operator-facing
/// sites instruct operators to use it; a future regression adding a
/// `Corrupt`-refusal gate would silently break that rebuild path.
/// Scenario: power loss truncates pool.json into non-JSON bytes; the
/// operator follows the documented `braid discover --write` rebuild
/// remediation and gets a working pool.json.
```

Body pattern (reuses the corrupt fixture from
`classify_pool_json_returns_corrupt_for_unparseable` at lines
1709-1719 and the discover outcome pattern from
`discover_write_proceeds_when_no_gates_fire` at 1782-1810):

- Seed `paths.pool_json()` with `"not-json"`.
- Build a single-member `DiscoverOutcome`.
- Call `write_discovered_membership(outcome, &paths, None)`; expect
  `Ok(_)`.
- Assert `classify_pool_json(&paths.pool_json()) ==
  PoolJsonShape::ValidUuidKeyed` after the call -- the corrupt file
  was rebuilt into a loadable UUID-keyed membership.

### 5. Add a VM subtest for the `ValidUuidKeyed` refusal

**File:** `tests/cli/braid-discover.py`

After the existing `with subtest("discover fails when pool.json
already exists")` block at lines 57-59, insert:

```python
with subtest("discover --write also refuses healthy UUID-keyed pool.json"):
    before = machine.succeed("cat /var/lib/braid/pool.json")
    out = machine.fail("braid discover --write 2>&1")
    assert "is already a healthy UUID-keyed membership" in out, (
        "expected ValidUuidKeyed refusal; got:\n" + out
    )
    after = machine.succeed("cat /var/lib/braid/pool.json")
    assert before == after, "pool.json must be byte-for-byte unchanged"
```

Reuses the `pool.json` written at lines 38-41 -- no extra fixture
setup. Byte-equality (not just error-substring) prevents a regression
where the gate fires but `save_membership` was already partially
applied.

### 6. Add a VM subtest for the `Corrupt`-proceeds rebuild path

**File:** `tests/cli/braid-discover-migration.py`

The unit test in 4(b) pins the `Corrupt`-proceeds behavior at the
function boundary, but the plan changes operator-facing docs to
promise `discover --write` rebuilds a corrupt `pool.json`. Without
end-to-end coverage, a future or mistaken refusal arm added in
`cli/src/main.rs` (the bare-discover shape match at lines 782-811) or
elsewhere on the CLI path could block the documented rebuild flow
while every unit test still passes. The migration test already seeds
a 3-disk fixture with `DISK_UUIDS` and already writes corrupt /
off-schema payloads at lines 138-146 for the bare-discover refusal
assertion, so it is the natural home.

After the existing
`assert_corrupt_pool_json_refuses_preview(...)` calls at lines
138-146, append:

```python
with subtest("discover --write rebuilds corrupt pool.json"):
    write_pool_json("not-json-at-all")
    out = machine.succeed("braid discover --write --expect-count 3 2>&1")
    assert "pool membership written to /var/lib/braid/pool.json" in out, (
        "expected rebuild success in output:\n" + out
    )
    pool = json.loads(read_pool_json())
    assert set(pool["disks"].keys()) == set(DISK_UUIDS.values()), (
        "expected UUID-keyed disk set after rebuild: " + json.dumps(pool, sort_keys=True)
    )
    for name, uuid in DISK_UUIDS.items():
        assert pool["disks"][uuid]["name"] == name, (
            "expected " + uuid + " to carry name " + name + ": " + json.dumps(pool)
        )
```

Pins decision 017's "explicit recovery path for lost/corrupt
`pool.json`" contract at the CLI boundary. Re-uses `write_pool_json`,
`read_pool_json`, and `DISK_UUIDS` already defined at the top of the
file (lines 18-66).

### 7. Tighten the manual

**File:** `manual/commands/discover.md`

Three edits: clarify the four-shape decision matrix and reconcile the
"only when missing" line with the corrupt-rebuild contract.

Replace line 13 ("When to use it" tail):

> The normal path for adding disks is `braid add`. Use `discover`
> only when `pool.json` is missing.

with:

> The normal path for adding disks is `braid add`. Use `discover`
> when `pool.json` is missing or corrupt, or to migrate the legacy
> name-keyed shape -- see the runbook in `docs/luks-unlock.md`.

Replace lines 55-57 (step 2 of "What happens under the hood"):

> 2. Refuses if `pool.json` already exists in the new UUID-keyed shape
>    or an unrecognized shape. If the existing file is the legacy
>    name-keyed shape, bare read-only `discover` prints a migration
>    hint and continues as a preview.

with:

> 2. Refuses on a healthy UUID-keyed `pool.json` (bare and `--write`).
>    Refuses on a legacy name-keyed `pool.json` for `--write`; bare
>    `discover` prints a migration hint and continues as a preview.
>    A corrupt or off-schema `pool.json` is the documented rebuild
>    path: bare `discover` prints the rebuild remediation, and
>    `discover --write` proceeds.

Replace the "Safety checks" bullet at line 70:

> Refuses if `pool.json` already exists in the new UUID-keyed shape
> or an unrecognized shape; legacy name-keyed files are allowed only
> for read-only preview.

with:

> Refuses any operation on a healthy UUID-keyed `pool.json`. Corrupt
> or off-schema files are allowed for `--write` rebuild only (with
> all intended pool members attached; see `docs/luks-unlock.md`).
> Legacy name-keyed files are allowed only for read-only preview.

### 8. Refresh the bare-discover gate comment in main.rs

**File:** `cli/src/main.rs:773-779`

The comment at lines 774-779 enumerates the `--write` gates that live
inside `write_discovered_membership` so a reader of `Commands::Discover`
sees the gate boundary without re-reading `discover.rs`. After the new
`ValidUuidKeyed` gate lands, the existing wording omits the new
refusal arm. Replace:

```rust
// Note: the pre-save fail-closed gates for `--write`
// (pending-op presence + name-keyed pool.json sniff) live
// inside `discover::write_discovered_membership`. The
// bare read-only path reuses the shape classifier so
// operators can preview legacy cutovers before moving the
// old state file aside.
```

with:

```rust
// Note: the pre-save fail-closed gates for `--write`
// (pending-op presence + pool.json shape check that refuses
// both `LegacyNameKeyed` and `ValidUuidKeyed`; `Corrupt` is
// the documented rebuild path per decision 017) live inside
// `discover::write_discovered_membership`. The bare read-only
// path reuses the shape classifier so operators can preview
// legacy cutovers before moving the old state file aside.
```

### 9. Update the pool-lock-discover-contention positive control

**File:** `tests/module/pool-lock-discover-contention.py:63-65`

The final subtest (`with subtest("discover succeeds after lock
release")`) currently runs `braid discover --write --expect-count 1`
against the UUID-keyed `pool.json` written at line 25 of the
precondition subtest. After the new `ValidUuidKeyed` gate lands, that
command will fail with the new refusal instead of succeeding. The
subtest's lock-release intent is preserved by asserting two things:
the wrapper-contention message is absent (proves the wrapper lock
check no longer fires) and the new `ValidUuidKeyed` gate message is
present (proves the CLI was reached).

Replace lines 63-65:

```python
with subtest("discover succeeds after lock release"):
    machine.succeed("braid discover --write --expect-count 1")
    machine.succeed("test -e /var/lib/braid/pool.json")
```

with:

```python
with subtest("discover reaches CLI after lock release and refuses healthy UUID-keyed pool.json"):
    rc, out = machine.execute("braid discover --write --expect-count 1 2>&1")
    assert rc != 0, "expected ValidUuidKeyed refusal; out=" + out
    assert "another braid operation is already in progress" not in out, (
        "wrapper lock check must not fire after release; out=" + out
    )
    assert "is already a healthy UUID-keyed membership" in out, (
        "expected ValidUuidKeyed refusal at the gate; out=" + out
    )
    assert machine.succeed("cat /var/lib/braid/pool.json") == pool_before, (
        "pool.json must be byte-for-byte unchanged after refusal"
    )
```

`pool_before` is already in scope from the precondition subtest at
line 26. The combined "no contention message AND ValidUuidKeyed at the
gate" assertion is a stronger lock-release proof than the original
`succeed` because it pins exactly where the command failed.

The test's top-level "Intent" preamble at lines 3-9 mentions
"`discover --write` ... must fail fast" under contention, which stays
true; it does not promise success after release, so the preamble does
not need rewording. (If a future change to the file's purpose
warrants a preamble rewrite, do it in a separate commit.)

## What is intentionally NOT changed

- **`Corrupt` is still allowed for `--write`.** Documented rebuild
  remediation across four sites. The `Corrupt` shape covers any
  canonical membership load failure that the `LegacyNameKeyed` sniff
  did not peel off first (unparseable JSON, parseable but off-schema,
  value-side uniqueness violations, non-NotFound I/O); `discover
  --write` is intentionally a rebuild-from-attached-disks surface
  that does not salvage fields from invalid state.
- **`cli/src/main.rs:782-811` shape-match block.** Stays guarded by
  `if !args.write`. The gate lives where the mutation lives (inside
  `write_discovered_membership`); duplicating it in `main.rs` would
  split one precondition across two sites.
- **No new pool-lock acquisition on `Commands::Discover`.** The
  wrapper at `modules/braid/braid-wrapper.sh:53-56` already serializes
  `discover` (decision 018:140). The new `ValidUuidKeyed` gate
  addresses the orthogonal hazard of an operator running
  `discover --write` sequentially against an already-healthy pool;
  concurrency is already covered.
- **`--expect-count` semantics.** The finding noted that
  `--expect-count` does not protect against re-running against the
  same N-disk pool; the new `ValidUuidKeyed` gate IS that protection.

## Critical files

- `cli/src/discover.rs:155-189` -- `DiscoverWriteError` enum (new
  `ValidUuidKeyed` variant).
- `cli/src/discover.rs:524-566` -- `write_discovered_membership` gate
  (exhaustive `match`; doc-comment update).
- `cli/src/discover.rs:1577-1897` -- test module (two new tests after
  the existing `discover_write_*` and `classify_pool_json_*` suite).
- `cli/src/main.rs:773-779` -- refresh the bare-discover gate-list
  comment so it lists the `ValidUuidKeyed` arm.
- `tests/cli/braid-discover.py:57-61` -- new end-to-end refusal
  subtest after the existing bare-discover refusal.
- `tests/cli/braid-discover-migration.py:138-146` -- new end-to-end
  `Corrupt`-rebuild subtest appended after the existing
  rebuild-remediation assertions.
- `tests/module/pool-lock-discover-contention.py:63-65` -- rewrite
  the post-release positive control as a `ValidUuidKeyed` refusal
  assertion.
- `manual/commands/discover.md:13, 55-56, 70` -- doc text now matches
  the four-shape decision matrix and the corrupt-rebuild contract.

## Existing patterns reused

- `crate::membership::save_membership` (`cli/src/membership.rs`) --
  drives both the seed-an-existing-pool.json setup in test 4(a) and
  the proceed-with-save path inside `write_discovered_membership`.
  Already used by `discover_write_proceeds_when_no_gates_fire`.
- `DiscoverWriteError::NameKeyedPoolJson` variant style + the
  "back it up and move it aside before retrying" phrase
  (`cli/src/discover.rs:165-171`) -- the new `ValidUuidKeyed` variant
  is a direct sibling.
- `classify_pool_json` four-shape return
  (`cli/src/discover.rs:218-233`) -- already covers exactly the four
  states the new exhaustive `match` discriminates.
- UUID-keyed seed fixture from
  `classify_pool_json_returns_valid_uuid_keyed_for_loadable_pool_json`
  (`cli/src/discover.rs:1683-1703`) for test 4(a).
- `"not-json"` corrupt fixture from
  `classify_pool_json_returns_corrupt_for_unparseable`
  (`cli/src/discover.rs:1709-1719`) for test 4(b).
- `DiscoverOutcome` build pattern from
  `discover_write_proceeds_when_no_gates_fire`
  (`cli/src/discover.rs:1782-1810`) for both new tests.
- VM-subtest pattern from `tests/cli/braid-discover.py:57-59`
  (machine.fail + substring + state assertion). Byte-equality
  assertion borrowed from
  `tests/cli/braid-discover-migration.py:87, 94`
  (`read_pool_json() == legacy_raw`).

## Verification

1. `just test-rust` -- exercises the two new unit tests
   (`discover_write_refuses_when_pool_json_is_valid_uuid_keyed`,
   `discover_write_proceeds_when_pool_json_is_corrupt`) alongside the
   existing `discover_write_*` and `classify_pool_json_*` suite.
2. `just test-vm braid-discover` -- exercises the new VM subtest
   end-to-end. The existing block at lines 38-41 writes the
   UUID-keyed `pool.json` that the new subtest then attempts to
   overwrite.
3. `just test-vm braid-discover-migration` -- exercises the new
   `Corrupt`-rebuild subtest end-to-end. Existing legacy-shape,
   missing, count-mismatch, and rebuild-remediation subtests stay
   unchanged.
4. `just test-vm pool-lock-discover-contention` -- exercises the
   rewritten post-release subtest. The contention subtests at lines
   33-58 stay unchanged; the rewritten subtest at lines 63-65 pins
   that the wrapper lock release is observable as a downstream
   `ValidUuidKeyed` refusal.
5. `git grep "pool.json already exists\|is already a healthy
   UUID-keyed"` after the change should show:
   - `cli/src/main.rs:798` -- surviving bare-discover refusal-arm
     format string.
   - `cli/src/discover.rs` -- new `ValidUuidKeyed` variant.
   - `tests/cli/braid-discover.py` -- existing bare assertion and the
     new `--write` one.
   - `tests/cli/braid-discover-migration.py:134` -- the
     post-migration UUID-keyed subtest.
   - `tests/module/pool-lock-discover-contention.py` -- the rewritten
     post-release subtest assertion.
6. Manual sanity check on a dev VM:
   - First boot, `pool.json` missing: `braid discover --write`
     succeeds.
   - Re-run: refuses with `is already a healthy UUID-keyed membership`,
     `pool.json` byte-for-byte unchanged.
   - `printf garbage > /var/lib/braid/pool.json; braid discover
     --write`: succeeds, replaces the corrupt file with a loadable
     membership.
