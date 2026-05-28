# Plan: warn on keyfile asymmetry for returning disks in `add` / `replace`

## Context

The keyfile-asymmetry warning (`luks::format_keyfile_asymmetry_warning`) nudges
operators when they add a drive to a pool whose existing members have slot-1
keyfile auto-unlock enrolled, but the new drive will not -- auto-unlock will
silently break on the new drive at next boot. Passphrase unlock still works, so
this is a UX gap, not a safety bug.

Today both planners gate the warning on "any target needs fresh format":

- `cli/src/add.rs:1757-1773` -- `any_needs_format` is true only when at least
  one target is `PresentConfigDiskState::PresentNotLuks`.
- `cli/src/replace.rs:1358-1369` -- single-target equivalent on `new_probed`.

The gap: a returning braid disk (`PresentLuks`) re-added or replaced without
`--enroll` against a keyfile-enrolled pool emits no warn, even when the
returning disk's slot 1 is empty (e.g. it was evicted before keyfiles were
rolled out). The proper test is "will any surviving target end up without
slot 1 post-op?" -- which covers both the existing fresh-format case and the
returning-disk-empty-slot-1 case.

## Approach

Replace the fresh-format-only gate with a per-target check that runs against
the actual work targets, not the raw probe input. The decision per target:

- `AddTargetWork::Fresh(_)` -> will lack slot 1 post-op.
- `AddTargetWork::OpenRecoverable(_)` or `AddTargetWork::ClosedPresentLuks(_)`
  -> probe slot 1 via `luks::check_key_slot(by_id, LUKS_SLOT_KEYFILE)`:
  - `Ok(Empty)` -> will lack.
  - `Ok(Occupied)` -> will have.
  - `Err(_)` -> surface as a target-side probe-failure note (mirrors the
    existing pool-side uncertainty channel).
- `--enroll` provided -> skip the gate entirely; `resolve_existing_luks_enroll`
  already covers that path.

If any target will lack slot 1, run the existing pool-side
`probe_pool_keyfile_enrollment(runner, &pool.devices)` and emit either the
asymmetry warn or the pool-side probe-failure notes -- unchanged from today.

**Placement (add):** the new block must run AFTER `build_add_work_plan` and
BEFORE the no-op Info note (between `add.rs:1799` and `add.rs:1806`). The
current location (line 1761, before `build_add_work_plan`) runs against the
raw probe input, which still contains already-in-pool `SameBacking` targets
that `build_add_work_plan` filters out via `continue` at `add.rs:2062` and
`add.rs:2127`. Deriving the check from `work_plan.targets` avoids the
"new drive will not" warn on a no-op `braid add diskN` where diskN is already
in the pool with an (asymmetric) empty slot 1.

Trade-off: if `build_add_work_plan` returns Err (e.g. identity-check
failure on a returning disk), the keyfile-asymmetry note is no longer
emitted on the failure path. Today's code emits it because the note is
inserted before `build_add_work_plan`. Accept the regression -- the
identity-check error is the more important message on the failure path,
and the keyfile-asymmetry warn is informational. Missing-devices warn is
unaffected; it stays at its current pre-build position.

Note order in `notes` remains: missing-devices warn (pre-build), then
target-side probe-failure notes, then either the asymmetry warn or
pool-side probe-failure notes (post-build), then the no-op Info note (if
work plan is empty).

**Placement (replace):** keep the new block at the current location
(`replace.rs:1358-1369`). Replace has no SameBacking-no-op filtering path
on the new disk -- the new target is always going to be adopted (or the
plan errors). The single-target check is correct against `new_probed`
directly.

Do not extract a shared helper. The predicate is small at each callsite, and
`add` (work_plan.targets iteration with three variant arms) vs `replace`
(single-target `match` against `new_probed.state`) have different natural
shapes. A shared helper would also have to encode the
"AddTargetWork::Fresh-is-always-true" and "Err-becomes-a-note" policies
inside its signature, which obscures more than it deduplicates.

Do NOT route through `enroll_key_file::plan_single_disk_enrollment` for the
warning decision: it hard-errors on `Slot1 Occupied` via
`check_slot_one_available` (`enroll_key_file.rs:152`), which is precisely the
no-warn case. Use `check_key_slot` directly.

`mapper_open` is not gated in replace. A `PresentLuks { mapper_open: true }`
returning target whose slot 1 is empty against a keyfile-enrolled pool
reflects real asymmetric state and the replace is a real adoption, not a
no-op -- the warn is correct. In add, the `mapper_open: true` SameBacking
case is the no-op path filtered by `build_add_work_plan`; the post-build
placement handles it implicitly without an explicit `mapper_open` gate.

## Critical files

- `cli/src/luks.rs`: add `format_target_keyfile_probe_failure(by_id: &ByIdPath,
  err: &LuksError) -> String` next to `format_keyfile_enrollment_probe_failure`
  (around line 1076). Wording target-specific: "could not check whether new
  disk {by_id} already has a keyfile (slot 1): {err}; proceeding without the
  asymmetry check for this disk." Distinct from the pool-side body, which
  talks about an existing pool member.
- `cli/src/add.rs`: delete the `any_needs_format` block at lines 1757-1773.
  Insert the new per-target check between `build_add_work_plan` (line 1799)
  and the no-op Info note (line 1806). The block iterates
  `work_plan.targets`, matching on `AddTargetWork::{Fresh, OpenRecoverable,
  ClosedPresentLuks}` to decide whether to probe `check_key_slot` on the
  target's `by_id`. Add `check_key_slot, KeySlotState, LUKS_SLOT_KEYFILE,
  format_target_keyfile_probe_failure` to the existing `use crate::luks::{...}`
  import block at lines 10-14.
- `cli/src/replace.rs:1358-1369`: replace in place with the single-target
  `match` shape over `new_probed.state` and `new_by_id` (no placement
  change). Same import additions to its `use crate::luks::{...}` block.

Existing helpers reused (no changes):

- `luks::check_key_slot` (`cli/src/luks.rs:1020`), `KeySlotState`,
  `LUKS_SLOT_KEYFILE`.
- `luks::probe_pool_keyfile_enrollment` (`cli/src/luks.rs:1050`),
  `format_keyfile_asymmetry_warning`, `format_keyfile_enrollment_probe_failure`.
- `PresentConfigDiskState` (`cli/src/types.rs:523`).

## Tests

### Rust unit tests in `cli/src/add.rs`

Extend `AddPlanTestRunner` (around `add.rs:8253`):

- Add `target_probes: HashMap<String /* by_id */, AddPlanTargetProbe>` field
  and `with_target_probe(by_id, probe)` builder. Variants encode the
  `AddTargetWork` shape, the slot-1 state, AND (where it matters) the
  live-pool match outcome so tests can cover each branch the new gate
  has to handle:
  - `ClosedSlot1Empty` / `ClosedSlot1Occupied` / `ClosedDumpFails`
    (mapper inactive; UUID outside pool members; resolves to
    `AddTargetWork::ClosedPresentLuks` via the `NoMatch` arm at
    `add.rs:2131`).
  - `OpenRecoverableSlot1Empty` (mapper active; FSID matches the live
    pool; UUID outside pool members so
    `classify_live_pool_match` -> `NoMatch`; resolves to
    `AddTargetWork::OpenRecoverable`). Only the Empty variant is needed
    for the warn-fires assertion; Occupied/DumpFails behavior is
    structurally identical between Open and Closed branches and is
    already pinned by tests 2 and 3.
  - `AlreadyInPoolSlot1Empty` (mapper active; target's LUKS UUID equals
    the corresponding pool member's UUID; backing path resolves to the
    same `/dev/vdN` as the pool member; resolves to the no-op
    `LivePoolMatch::SameBacking` arm at `add.rs:2066` that `continue`s
    out of `build_add_work_plan`). Required for test #5: without this
    variant the target's fresh UUID lands in the `NoMatch` arm and the
    no-op regression test would actually exercise the
    `OpenRecoverable` adoption path it's meant to disprove.
- Default empty map -> existing tests unaffected.
- Extend handler arms:
  - `CryptsetupStatus { mapper }` (line 8316): map `braid-diskN` ->
    inactive (exit 4, stderr "is inactive.") for `Closed*` variants;
    return active (`type: LUKS2`, `device: /dev/vdX`) for
    `OpenRecoverable*` and `AlreadyInPool*` variants so
    `probe_mapper_open` returns `Ok(true)`.
  - `CryptsetupLuksUuid { device }` (line 8337): for `Closed*` and
    `OpenRecoverable*` target by-ids, return a fresh UUID distinct
    from pool UUIDs (e.g. `aaaaaaaa-...`); the same UUID must also
    resolve against the underlying `/dev/vdX` for `OpenRecoverable*`
    so `probe_mapper_open`'s cross-check passes. For `AlreadyInPool*`,
    return the corresponding pool member's UUID (e.g. for disk2 ->
    `11111111-1111-1111-1111-111111111111`, matching
    `AddPlanTestRunner`'s per-index assignment at ~line 8351) on
    BOTH the target by-id and the underlying `/dev/vdX` so
    `classify_live_pool_match`'s UUID + backing-path filter
    (`add.rs:251, 261`) selects `SameBacking`.
  - `CryptsetupLuksDumpText { device }`: return `"LUKS header
    information\nVersion:\t2\nLabel:\tbraid-diskN\n"` where `N` matches
    the target's disk name. The `Label` line is required:
    `probe_config_disk` feeds the parsed label into
    `ConfigDiskState::PresentLuks { label, .. }` and
    `validate_braid_preconditions` (`add.rs:144`) rejects PresentLuks
    add targets whose label is not `braid-<name>`. Without the Label
    line the new tests would fail at precondition validation, not at
    the gate under test.
  - `BtrfsFilesystemShowTarget { target }`: for `OpenRecoverable*` and
    `AlreadyInPool*` variants targeting `/dev/mapper/braid-diskN`,
    return a `HasBtrfs` body whose FSID matches the live pool's FSID.
    This is the `classify_braid_disk_fsid` -> `SamePool` precondition
    that the open-target branches at `add.rs:2054-2087` both depend
    on; the `LivePoolMatch` arm (NoMatch vs SameBacking) then
    distinguishes `OpenRecoverable` from the no-op `continue`.
  - `CryptsetupLuksDump { device }` (line 8366): for target by-ids,
    return `{"keyslots":{"0":{}}}` (Empty), `{"keyslots":{"0":{},"1":{}}}`
    (Occupied), or exit 5 (DumpFails) -- shared between Closed and Open
    variants.

UUID strategy summary:

- `Closed*` and `OpenRecoverable*` -> fresh UUID outside the pool's
  per-index range so `classify_live_pool_match` returns `NoMatch` and
  the target reaches the `ClosedPresentLuks` push at `add.rs:2141` or
  the `OpenRecoverable` push at `add.rs:2104`.
- `AlreadyInPool*` -> the matching pool-member UUID on both the target
  by-id and the underlying `/dev/vdX` so the SameBacking arm at
  `add.rs:2062` (mapper_open=true) `continue`s out of
  `build_add_work_plan` and the target never lands in
  `work_plan.targets`.

New tests, clustered after the existing `plan_add_keyfile_*` block
(~lines 8519-8703). Each test asserts `plan_add` returns `Ok(_)` before
inspecting notes -- a precondition-validation failure must surface as a
test failure, not as an assertion that misreads an `Err`-path note list:

1. `plan_add_keyfile_asymmetry_emits_warn_for_returning_disk_with_empty_slot_1`
   -- pool with `with_keyfile` + target `ClosedSlot1Empty`, no `--enroll`
   -> exactly one `PreviewNote::Warn` body == `format_keyfile_asymmetry_warning()`.
2. `plan_add_keyfile_no_warn_for_returning_disk_with_occupied_slot_1`
   -- pool with `with_keyfile` + target `ClosedSlot1Occupied`, no
   `--enroll` -> zero warns.
3. `plan_add_keyfile_emits_target_probe_failure_for_returning_disk_dump_error`
   -- pool with `with_keyfile` + target `ClosedDumpFails`, no `--enroll`
   -> exactly one `PreviewNote::Warn` body == `format_target_keyfile_probe_failure(...)`.
4. `plan_add_keyfile_pool_probe_failure_for_returning_disk_with_empty_slot_1`
   -- `with_keyfile_probe_failure` + target `ClosedSlot1Empty`, no
   `--enroll` -> exactly one `PreviewNote::Warn` carrying the existing
   pool-side `format_keyfile_enrollment_probe_failure` body. Pins that the
   pool-side uncertainty channel is still reachable through the new gate.
5. `plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1`
   -- regression for the post-build placement. Two-device pool via
   `with_keyfile_probes(vec![Occupied, Empty])` so disk1 (Occupied)
   proves pool enrollment and disk2 is a real member of `pool.devices`.
   Target disk2 supplied as `AlreadyInPoolSlot1Empty` (mapper active,
   target by-id and underlying `/dev/vdc` both return disk2's pool
   UUID `11111111-1111-1111-1111-111111111111`, target's btrfs probe
   resolves to the pool FSID, slot 1 empty), no `--enroll`. The
   matching UUID + backing path drives `classify_live_pool_match` to
   `SameBacking` (`add.rs:2062`), so `build_add_work_plan`
   `continue`s past disk2 and `work_plan.targets` is empty. Assert:
   zero `PreviewNote::Warn` (no keyfile-asymmetry warn) and exactly
   one `PreviewNote::Info` (the no-op message via `format_add_noop`).
   `with_keyfile()`'s one-disk pool would NOT exercise this branch:
   without disk2 in `pool.devices` the
   `device.luks_uuid == target_uuid` filter at `add.rs:251` finds no
   match and the target lands in `NoMatch` -> the `OpenRecoverable`
   adoption path instead of the no-op `continue`.
6. `plan_add_keyfile_asymmetry_emits_warn_for_open_returning_disk_with_empty_slot_1`
   -- coverage for the `AddTargetWork::OpenRecoverable` arm at
   `add.rs:2054-2087`. Pool with `with_keyfile` + target
   `OpenRecoverableSlot1Empty`, no `--enroll`. Mapper braid-disk2 is
   active, its LUKS UUID is not in `pool.devices` (`NoMatch`), and
   `classify_braid_disk_fsid` resolves to `SamePool` against the live
   pool FSID, so `build_add_work_plan` pushes an
   `AddTargetWork::OpenRecoverable`. Assert: exactly one
   `PreviewNote::Warn` body == `format_keyfile_asymmetry_warning()`.
   Without this test an implementation that handled only
   `AddTargetWork::ClosedPresentLuks` (tests 1-4) would still pass.

### Rust unit tests in `cli/src/replace.rs`

Fixture change: extend the canonical `ReplacementPool::install` handler
(`cli/src/test_fixtures/replace.rs:215-296`) to return slot0-only JSON for
`/dev/vdb` and `/dev/vdc` on `CryptsetupLuksDump` (alongside the existing
`virtio-disk3` arm at line 268-275). Today the canonical handler only
mocks JSON luksDump for `virtio-disk3`; with the new gate the pool-side
`probe_pool_keyfile_enrollment` now runs whenever the target's slot-1 is
empty (including the canonical `with_mapper_closed("braid-disk3")` case),
and missing /dev/vdb /dev/vdc mocks would generate spurious probe-failure
notes. Existing zero-note tests like
`plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render`
(`replace.rs:5100`) would regress without this fixture extension.

The slot0-only canonical default keeps `has_enrollment=false, failures=[]`
on the unmodified path so existing tests still see zero notes; new
keyfile tests override these arms to Occupied or DumpFails as needed.

New tests, clustered after the existing keyfile cluster (~lines 5705-5829):

6. `plan_replace_keyfile_asymmetry_emits_warn_for_returning_disk_with_empty_slot_1`
   -- override `/dev/vdb` and `/dev/vdc` to Occupied (pool has keyfile),
   leave `virtio-disk3` at canonical Empty, `with_mapper_closed("braid-disk3")`,
   no `--enroll` -> exactly one warn body ==
   `format_keyfile_asymmetry_warning()`.
7. `plan_replace_keyfile_no_warn_for_returning_disk_with_occupied_slot_1`
   -- same plus override `virtio-disk3` to Occupied -> zero warns.
8. `plan_replace_keyfile_emits_target_probe_failure_for_returning_disk_dump_error`
   -- same plus override `virtio-disk3` to exit 5 -> exactly one
   target-side probe-failure warn.

### NixOS VM test

Append Test 4d to `tests/cli/braid-add-enroll.py` after the existing 4c
block (~line 220). Shape:

- Pool already has disk1 + disk2 with slot 1 enrolled (from Tests 1-3).
- After Test 4c, disk3 is in the pool with slot 0 only (added without
  `--enroll`).
- New subtest:
  - `braid remove disk3 --yes` -- evicts disk3 from the pool. `braid
    remove` runs only `cryptsetup close` on the mapper (see
    `cli/src/remove.rs:218`) and does not erase the LUKS header, so
    disk3 stays as a `PresentLuks` returning braid disk with slot 0 only.
    No passphrase flags: `RemoveArgs` (`main.rs:308`) has only `disk` +
    `common`, and `remove_commands_reject_passphrase_flags`
    (`main.rs:1738`) pins this contract.
  - `add_cmd_disk3('--dry-run')` -- re-adds disk3 without `--enroll`,
    piping the passphrase via the existing `add_cmd_disk3` helper.
  - Assert `[warn] Existing pool drives have a keyfile (keyslot-1)` in stdout
    and stderr empty -- same canonical block as Test 4a.

No `cryptsetup luksKillSlot` step is needed: after Test 4c, disk3's slot
1 is already empty (Test 4c added without `--enroll`), and
`luksKillSlot` against an inactive slot returns -EINVAL (see
`reference/cryptsetup/src/cryptsetup.c:1969`).

## Verification

1. `just test-rust` -- the six new add tests (including the
   `OpenRecoverable` arm coverage) and three new replace tests pass;
   existing `plan_add_keyfile_*`, `plan_replace_keyfile_*`, and
   `plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render`
   (the byte-equivalence pin at `replace.rs:5100` that depends on the
   slot0-only canonical fixture extension) still pass.
2. `just test-vm braid-add-enroll` -- Tests 4a-4c stay green and the new
   Test 4d passes.
3. Manual spot-check of `braid add --dry-run` against a 2-disk
   keyfile-enrolled pool with both a fresh and a returning-disk target:
   note-ordering invariant (missing-devices first, keyfile second) holds and
   the keyfile-asymmetry warn body is byte-identical to today's output.
4. Confirm no stderr leakage from the new target-side probe-failure note:
   it routes through `PreviewNote::Warn -> Preview::render -> stdout` on
   dry-run and `emit_notes_to_stderr` on real-run -- the same channel as the
   existing pool-side probe-failure note.

## Implementation notes

- The replace dump-error unit test lets the capacity preflight's first target
  JSON `luksDump` succeed, then fails the later target slot probe. The
  production path legitimately consumes both probes before producing the
  target-side warning under test.
