# Plan: drop reconstructed-name filter from lock "already closed" prelude

## Context

`LockPlan::execute` (`cli/src/lock.rs:608-640`) currently uses the helper
`already_closed_names` (`cli/src/lock.rs:441-460`) to decide which
members to emit as "disk <name>: already closed". The helper filters
membership by comparing a reconstructed `mapper_name(member.name)`
string against `planned_mappers` and `skipped_mappers`.

That reconstruction is the only line of defense between the planner's
truth and the operator-visible status. It works on the happy path
because every member with a live presence is either in
`planned_members`, in `planned_mappers`, or has its expected mapper name
match the skipped set. It breaks the moment a member's mapper has
drifted AND the planner could not classify it -- three real paths:

1. **Pass 2 `MembershipError::DuplicateDevid`** (`cli/src/lock.rs:855-869`).
   A `null_underlying` entry's persisted devid resolves to multiple
   membership UUIDs. The drifted mapper goes into `skipped_mappers`;
   neither member is in `planned_*`. The prelude reconstructs
   `braid-aaa` / `braid-bbb`, finds nothing in any set, and emits
   "disk aaa: already closed" and "disk bbb: already closed" while
   `braid-WRONG` is still open. At least one of the two claims is
   provably false.
2. **Pass 3 classify failure** (`push_uuid_classified_candidate` Err
   at `cli/src/lock.rs:303-309`, called from `cli/src/lock.rs:896-907`).
   A stranded `braid-WRONG` exists; `cryptsetup status` fails so the
   mapper lands in `skipped_mappers`. Every unaccounted member gets a
   false "already closed" line.
3. **Full-arm `/dev/mapper` scan failure** (`cli/src/lock.rs:884-893`).
   `scan_braid_mapper_candidates` errors (e.g. EACCES); the planner
   warns and returns the Pass 1/2 close set without populating
   `skipped_mappers` at all. A drifted live mapper backing an
   unaccounted member is unenumerated, the reconstructed filter has
   nothing to suppress against, and every unaccounted member gets a
   false "already closed" line. (Today this branch also does not set
   `cleanup_uncertain`, which compounds the misleading output.)

All three paths contradict decision 024
(`docs/decisions/024-luks-uuid-identity.md:78-81`, `109-117`): "a mapper
opened as `braid-WRONG` but owned by `disk1` is closed as `braid-WRONG`;
braid does not merely try `braid-disk1` and leave the real mapper open."
The current code does worse -- it leaves the real mapper open (correct
under cleanup-uncertain) AND prints a contradictory "already closed"
line.

Probability is low: drift + a classification or enumeration failure is
required. But the fix is structural -- making the planner the source
of truth removes the reconstruction anti-pattern entirely, which is what
doc 024 calls for.

## Approach

Move the "already closed" decision into the planner. `LockPlan` gains a
`members_known_closed: Vec<DiskName>` field, pre-sorted in DiskName
order. `LockPlan::execute` emits the prelude from that field. The
`already_closed_names` helper, the `planned_members` / `planned_mappers`
/ `skipped_mappers` plumbing inside execute, and the
`mapper_name(member.name)` reconstruction all go away.

### Predicate (computed at plan time)

Track `members_potentially_present: HashSet<DiskName>` (owned, not
borrowed -- Pass 3's `classify_candidate_mapper` returns
`LockMapperCloseKind::MemberOwned { display_name: DiskName }`, an
owned name that does not borrow from `membership`, so an owned set
keeps insertion uniform across passes) and
`has_unclassified_skip: bool` while building the close set. Updates
all clone `member.name` (or the destructured `display_name`) into the
set:

- **Pass 1** (`pool.devices`, by UUID at `cli/src/lock.rs:823-838`):
  on `by_uuid` Some, `members_potentially_present.insert(member.name.clone())`.
- **Pass 2** (`pool.null_underlying`, by devid at
  `cli/src/lock.rs:840-871`):
  - `Ok(Some((_uuid, member)))`: insert `member.name.clone()`.
  - `Ok(None)`: no-op (orphan).
  - `Err(DuplicateDevid { members, .. })`: for each UUID in `members`,
    resolve via `membership.by_uuid` and insert that member's
    `name.clone()`.
- **Pass 3** (stranded scan at `cli/src/lock.rs:884-907`, helper
  `push_uuid_classified_candidate` at `cli/src/lock.rs:286-311`):
  - `MemberOwned { display_name }`: destructure and insert
    `display_name.clone()` before reconstructing the
    `LockMapperClose` for `member_owned.push(...)`.
  - `Orphan`: no-op.
  - `Err(_)`: set `has_unclassified_skip = true`.
- **Full-arm `mapper_scan_warn`** (`cli/src/lock.rs:884-893`):
  scan failure means we cannot enumerate stranded mappers. A drifted
  member mapper could be live but unenumerated, so the same
  false-closed class applies. Set `has_unclassified_skip = true` in
  the `Err(e) =>` branch before the early `return`. (This is also a
  good spot to set `*cleanup_uncertain = true` -- today it is not set
  here, which contradicts the operator-facing meaning of "cleanup
  uncertain" when we cannot enumerate /dev/mapper. Doing so keeps the
  "pool already locked" summary at `cli/src/lock.rs:675` suppressed
  in this branch as well.)
- **Fallback arm** (`build_close_sets_uuid_scanned_fallback` at
  `cli/src/lock.rs:916-950`): same Pass 3 rules. Scan failure
  (`cli/src/lock.rs:929-933`) already sets `cleanup_uncertain`; also
  set `has_unclassified_skip = true` so an unscannable fallback does
  not produce false "already closed" lines.

After both arms finish:

```text
if has_unclassified_skip:
    members_known_closed = []  // no unaccounted member is confidently closed
else:
    members_known_closed =
        membership.iter_by_name()
            .filter(|(_, m)| !members_potentially_present.contains(&m.name))
            .map(|(_, m)| m.name.clone())
            .collect()
```

`iter_by_name()` already produces DiskName order (decision 024,
exercised by `already_closed_names_returned_in_name_order_independent_of_uuid_order`
at `cli/src/lock.rs:1023`).

### Files

- `cli/src/lock.rs`
  - `LockPlan` struct (line ~466): add
    `pub members_known_closed: Vec<DiskName>` with a `///` doc
    comment per the project rules ("Planner-derived set of members
    confidently absent from every observed live state; the prelude
    rendering source so execute does not reconstruct
    `mapper_name(member.name)`.").
  - `build_close_sets_full` (line ~811): thread two new outputs
    (potentially-present set and unclassified-skip flag) alongside the
    existing `notes` / `skipped_mappers` / `cleanup_uncertain`
    out-params.
  - `build_close_sets_uuid_scanned_fallback` (line ~916): same threading.
  - `push_uuid_classified_candidate` (line ~286): same threading
    (gains a way to mark unclassified-skip and to insert members on
    success).
  - `plan_lock` (line ~690): compute `members_known_closed` after the
    close-set construction, populate the new `LockPlan` field.
  - `LockPlan::execute` (line ~494): replace the
    `already_closed_names`-based prelude (lines ~608-640) with a
    direct loop over `self.members_known_closed`. Remove the
    `planned_mappers` / `planned_members` / `skipped_mappers`
    HashSet builds (lines ~608-614) since they were only inputs to
    the filter.
  - Delete `already_closed_names` (line ~443) and its supporting
    `member_names`/`mapper_names` callers if no longer used (verify
    `LockCloseSet::mapper_names` and `LockCloseSet::member_names` --
    they may still be useful for other call sites; do not delete
    blindly, just stop calling them from execute).

The exploration agent confirmed `LockPlan` has only one struct
constructor (in `plan_lock` at `cli/src/lock.rs:794`); no other call
sites need updating.

### Docs

Per project rule "Any change to behavior or invariants must update
those docs" (`CLAUDE.md` -> `AGENTS.md`, "Architecture Authority"):

- `docs/decisions/024-luks-uuid-identity.md` -- update point 7 in
  "Runtime Handles And Labels" (currently lines 109-117) to record
  two new invariants this change introduces:
  1. `lock` reports `disk <name>: already closed` only for members
     the planner has positively proved absent from every observed
     live state (the new `members_known_closed` set), not by
     reconstructing `mapper_name(&member.name)` and comparing
     against the skipped set. Drift plus a classification or
     enumeration failure must never produce a contradictory "already
     closed" line.
  2. A `/dev/mapper` scan failure (either arm of close-set
     construction) marks cleanup uncertain, warns, and suppresses
     all `already closed` claims for unobserved members. This brings
     the full-arm scan-failure branch (`cli/src/lock.rs:884-893`,
     which previously did not set `cleanup_uncertain`) in line with
     the fallback-arm behavior.
- Cross-check the "Tests That Enforce This" section of doc 024
  (currently around lines 147-191) and append the new VM test
  (`tests/cli/luks-lock-skipped-no-false-closed.py`) alongside the
  existing `tests/cli/luks-mapper-drift.py` reference.

No changes to `docs/principles.md` -- this fix refines lock's
implementation of an existing principle (Stable Identifiers via LUKS
UUID), not the principle itself.

### Tests

Unit tests (`cli/src/lock.rs`):

1. Extend `full_arm_pass2_duplicate_devid_skips_and_warns_with_cleanup_uncertain`
   (line ~3197): assert that the resulting `LockPlan.members_known_closed`
   contains neither `aaa` nor `bbb` (both are DuplicateDevid claimants).
   Drive `plan_lock` end-to-end (not just `build_close_sets_full`) so
   the new field is computed.
2. New test: Pass 3 classify-failure with a third unaccounted member.
   Membership has `aaa`/`bbb`/`ccc`; `pool.devices` contains
   `aaa`/`bbb`; a stranded `braid-stranded` fails classify. Assert
   `members_known_closed` is empty (the planner cannot confirm `ccc`
   is closed because the stranded mapper could be `ccc` drifted).
3. New test: Full-arm scan failure with an unaccounted member.
   Membership has `aaa`/`bbb`/`ccc`; `pool.devices` contains
   `aaa`/`bbb`; `fs` is built via `with_dev_mapper_error()` so the
   stranded scan returns `Err`. Assert `members_known_closed` is empty
   -- the planner cannot enumerate /dev/mapper, so a drifted live
   mapper backing `ccc` cannot be ruled out.
4. Extend `dry_run_preview_warns_when_list_dir_fails` (line ~1530) --
   the existing fallback (unmounted) scan-failure test -- with an
   assertion that `plan.members_known_closed.is_empty()`. This pins
   the fallback arm's `has_unclassified_skip = true` so a regression
   that drops that line in `build_close_sets_uuid_scanned_fallback`
   (line ~929-933) fails the test rather than producing false
   `already closed` rows from a real `braid lock` run.
5. New test: happy path -- two-member pool, no live mappers, both
   members in `members_known_closed` in DiskName order. Replaces the
   intent of `already_closed_names_returned_in_name_order_independent_of_uuid_order`
   (which can be removed since the helper goes away). Verifies
   `LockPlan.members_known_closed` directly.
6. New test: drift happy path (Pass 1 classifies `braid-WRONG` as
   member-owned). Assert that the drifted-member is in
   `planned_members` (via `close_set.member_names()`) and NOT in
   `members_known_closed`.

VM test (`tests/cli/`) -- mandatory; the bug is in execute-side
stderr, and unit tests prove the planner field but cannot prove
execute consumes it correctly:

7. New `tests/cli/luks-lock-skipped-no-false-closed.py` (and matching
   `.nix`), modeled on `tests/cli/luks-mapper-drift.py`. Scenario:
   build a normal two-disk pool (`disk1`, `disk2`), `braid lock` to
   close both mappers, then `touch /dev/mapper/braid-WRONG` to create
   an unclassifiable Pass 3 candidate. Run `braid lock` again and
   capture stderr.

   Assertions:
   - `"skipping mapper braid-WRONG"` appears (planner emitted the
     skip warn).
   - `"disk disk1: already closed"` is absent.
   - `"disk disk2: already closed"` is absent.
   - `"pool already locked"` is absent (cleanup_uncertain suppresses
     the summary).
   - `braid lock` exits 0 (skip warns do not fail the command; pool
     was already locked).

   This pins the execute-side wiring against the planner's
   `members_known_closed` and catches any regression that drops it
   back to the reconstructed-name filter. The `touch` works because
   `scan_braid_mapper_candidates` only filters by basename and
   `fs.exists`, and `cryptsetup status` on a regular file fails,
   producing the Pass 3 classify-failure path.

## Verification

Local:
- `just test-rust` -- the six unit tests above pass; the
  `full_arm_pass2_duplicate_devid_skips_and_warns_with_cleanup_uncertain`
  extension now also asserts no false "already closed" on the prelude,
  and `dry_run_preview_warns_when_list_dir_fails` newly asserts the
  fallback arm produces an empty `members_known_closed`.
- `just test-vm luks-mapper-drift braid-lock braid-lock-name-order luks-lock-skipped-no-false-closed`
  -- the new VM regression test passes alongside existing prelude
  ordering and drift coverage.

End-to-end behavior on the bug scenario:
- With drift + classify failure (Pass 3 Err, Pass 2 DuplicateDevid,
  or full-arm scan failure), stderr should contain the planner's
  `[warn]` line for the skipped/unscannable state and the `[wait]/...`
  lines for any positively-classified members, but NO
  `disk <name>: already closed` line for any unaccounted member.
- With `cleanup_uncertain` set and no positively-closable members, the
  "pool already locked" summary (`cli/src/lock.rs:675`) stays
  suppressed -- now including the full-arm scan-failure branch, which
  also sets `cleanup_uncertain` under this plan.
