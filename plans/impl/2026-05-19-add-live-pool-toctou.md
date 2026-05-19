# Close the plan-to-execute live-pool TOCTOU in `braid add`

## Context

A code-review finding flagged a TOCTOU race in `cli/src/add.rs` for
`ClosedPresentLuks` add targets: the planner's `classify_live_pool_match`
sees the cached LUKS UUID absent from the live pool (`NoMatch`), queues
the target, and then `AddPlan::execute` proceeds without re-checking
live-pool membership. Between plan and execute, an external actor can
open a cloned-LUKS-header disk under a non-conflicting mapper name and
`btrfs device add` it to the pool. When `braid add` then runs
`pool_add_device` against the legitimate disk, the canonical
"duplicate LUKS UUID" defense never fires; the operator gets whatever
non-canonical error btrfs happens to surface (or, worst case, two
devices share a LUKS UUID in the pool).

Investigation in this conversation confirmed two things the original
finding partially missed:

1. **Same race in OpenRecoverable.** The Pass-1 OpenRecoverable loop
   at `cli/src/add.rs:969-983` performs **zero** execute-time re-checks
   — it just pushes straight to `needs_pool_add`. Its
   `initial_journal_targets` entries entered via the planner's
   `LivePoolMatch::NoMatch` branch (`cli/src/add.rs:1923`), so the same
   TOCTOU window applies. Fixing only ClosedPresentLuks would leave a
   parallel hole.

2. **Documented invariant already claims this defense exists.** The
   planner comment at `cli/src/add.rs:1964-1973` advertises a "Two-tier
   defense" whose execute-time tier is supposed to reject foreign disks.
   The existing execute-time helper
   `probe_closed_present_luks_target_uuid`
   (`cli/src/add.rs:277-304`, commit `09bfa79`) only catches a by-id
   swap on the **target itself**, not a live-pool collision under a
   different mapper. The code does not match its own documented
   invariant.

The intended outcome: any cached-UUID target reaching `AddPlan::execute`
gets a single fresh-pool re-classification pass before any irreversible
mutation, so a live-pool clone race surfaces as the same canonical
`duplicate_live_pool_uuid_error` the planner raises.

## Approach

**One fresh `probe_pool` + one re-classification pass** over the final
`journal_targets` map, placed between the end of Pass-1 (so
`ClosedPresentLuks` `SamePool` inserts are already merged in) and
journal write / sleep-inhibitor acquisition (so an abort can rely on
`LuksCleanupGuard::drop` and no pending-op.json exists yet).

Why this shape:

- **Single probe.** `probe_pool` (`cli/src/probe.rs:probe_pool`) is the
  same call used elsewhere; one invocation costs one
  `BtrfsFilesystemShow` + one `CryptsetupStatus` per live device, which
  is cheap relative to the LUKS opens already done in Pass-1.
- **One pass over `journal_targets`.** This map is the authoritative
  final set of "things we are about to add" by the time Pass-1
  finishes: `initial_journal_targets.clone()` (Fresh + OpenRecoverable
  from planning) plus ClosedPresentLuks `SamePool` insertions
  (`cli/src/add.rs:1043-1048`). Re-classifying the map covers Fresh,
  OpenRecoverable, and ClosedPresentLuks in one loop. Fresh entries
  have random UUIDs and will trivially return `NoMatch`, but they cost
  nothing to include and the code stays uniform.
- **Reuse, don't fork.** `classify_live_pool_match`
  (`cli/src/add.rs:228-272`) already takes `&PoolState` by reference;
  calling it with a fresh `PoolState` Just Works. The mismatch error
  reuses `duplicate_live_pool_uuid_error` (`cli/src/add.rs:2084`) so
  the operator-visible refusal is byte-identical to the planner's.
- **No re-planning.** Per `docs/decisions/022-dry-run-preview-model.md`:
  "execute may still perform execution-time validation that dry-run
  intentionally cannot do." A targeted live-pool re-check fits that
  exception; a full re-plan would be an architectural change out of
  scope for this fix.

### Placement

After Pass-1 ClosedPresentLuks loop ends (`cli/src/add.rs:1058`), past
the noop short-circuit at `cli/src/add.rs:1060-1064`, before the sleep
inhibitor acquisition at `cli/src/add.rs:1079`. At this point:

- All ClosedPresentLuks `ensure_luks_open` mappers are tracked by
  `LuksCleanupGuard` (`cli/src/add.rs:1005`) so an abort closes them.
- `journal_targets` is finalized.
- No `pending-op.json` exists yet (`journal::write_journal` is at
  `cli/src/add.rs:1116`).
- No sleep inhibitor held yet, so abort path stays clean.

### Pool-identity guard (runs first)

Before any per-target re-classification, validate that the fresh
`PoolState` describes the **same pool** the plan was built against.
Without this gate, an unmount or pool-replace between plan and execute
would yield `fresh_pool.devices == []` (or a different FSID's devices),
which makes every `classify_live_pool_match` call trivially return
`NoMatch`, silently passing the per-target check and proceeding to
`journal::write_journal` + `pool_add_device` with stale planning state.

`PoolState` carries `mounted: bool` (cli/src/types.rs:380) and
`fsid: Option<String>` (cli/src/types.rs:385). The guard rejects
**any** mount-state drift -- both directions -- because either drift
invalidates planning. The asymmetric "only `mounted -> unmounted`" form
considered earlier leaves the bootstrap path exposed:
`pool_bootstrap_mount` runs `mkfs.btrfs` **before** `mount`
(`cli/src/pool.rs:655-686`), so if a foreign pool appears at
`/mnt/storage` between an `unmounted` plan and execute, the destructive
`mkfs.btrfs` on candidate disks (and the corresponding `journal::write_journal`)
still runs ahead of the failing mount step.

```text
if fresh_pool.mounted != self.pool.mounted {
    return Err(AddError::Validation(<mount-drift-msg>));
}
if self.pool.mounted && fresh_pool.fsid != self.pool.fsid {
    return Err(AddError::Validation(<fsid-drift-msg>));
}
```

FSID comparison is gated on **both** snapshots being mounted because an
unmounted-pool snapshot has `fsid: None` by definition
(`cli/src/types.rs:384-385`); the mount-state drift gate above already
covers the asymmetric cases.

Wording (operator-facing, follows the AGENTS.md `--` convention):

- Mount drift, planned-mounted now unmounted:
  `"pool unmounted between planning and execution -- aborting before journal write. Re-mount /mnt/storage and re-run \`braid add\`."`
- Mount drift, planned-unmounted now mounted:
  `"a pool appeared at /mnt/storage between planning and execution -- aborting before \`mkfs.btrfs\`. braid will not bootstrap on top of a live filesystem; identify the mounted pool and unmount it (or unify your config) before re-running \`braid add\`."`
- FSID drift (both mounted, different FSIDs):
  `"pool fsid changed between planning and execution (was <plan-fsid>, now <live-fsid>) -- aborting before journal write. The pool you planned against is no longer the same filesystem."`

### Re-classification semantics (runs after the pool-identity guard)

For each `(uuid, target)` in `journal_targets`, call
`classify_live_pool_match(uuid, &target.by_id, &fresh_pool, params.backing_path_resolver)`
and dispatch:

- `LivePoolMatch::NoMatch` → expected case, continue. Planner saw
  `NoMatch` (or generated a fresh random UUID) and the live pool still
  agrees.
- `LivePoolMatch::DifferentBacking { device }` → return
  `duplicate_live_pool_uuid_error(uuid, &target.name, &target.by_id, device)`.
  This is the canonical clone refusal, identical wording to the
  planner's at `cli/src/add.rs:1982`.
- `LivePoolMatch::SameBacking { .. }` → return an `AddError::Validation`
  with wording along the lines of: "pool state changed between
  planning and execution -- disk '\<name\>' (UUID `<uuid>`) is now a
  live pool member. Re-run `braid add` to converge." This shouldn't
  happen under normal operation (the planner saw `NoMatch`; reaching
  `SameBacking` at execute means our own backing was added between
  plan and execute, e.g. by recovery replay). Fail closed and tell
  the operator to re-run.

All abort arms return cleanly; `LuksCleanupGuard::drop` closes any
Pass-1 mappers we opened. The pool-identity guard runs before any
per-target loop iteration, so a drift abort still goes through the
same cleanup path.

## Files to modify

- **`cli/src/add.rs`**
  - Add the fresh `probe_pool` + pool-identity guard +
    per-target re-classification block between the noop short-circuit
    (`:1064`) and the sleep-inhibitor acquisition (`:1079`). Order
    inside the block: probe → identity guard → per-target loop.
  - Reuse `probe_pool` -- already imported at `cli/src/add.rs:24`
    (`use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};`).
  - Reuse `classify_live_pool_match` (`:228`), `LivePoolMatch` (`:125`),
    and `duplicate_live_pool_uuid_error` (`:2084`) -- all in-file.
  - Pool mount point: use `self.config.mount_point()` -- the canonical
    access path already used downstream at `cli/src/add.rs:1277` for
    the same purpose. Filesystem trait: thread the `fs: &F` parameter
    already on `AddPlan::execute` (`cli/src/add.rs:867`) into the
    `probe_pool` call.
  - Update the planner comment at `cli/src/add.rs:1964-1973` to
    describe the new execute-time tier accurately (currently claims
    "execute-time live-UUID re-probe before `ensure_luks_open`" --
    expand to also note the post-Pass-1 live-pool re-classification
    that covers OpenRecoverable as well).

No other files need code changes for the fix itself. (Test file is new;
see below.)

## Tests

### Rust unit tests (`cli/src/add.rs` `#[cfg(test)]`)

All cases below MUST build an `AddPlan` whose `work_plan.is_noop()`
returns false. `is_noop` checks
`work_plan.targets.is_empty()` (`cli/src/add.rs:489-491`) -- **not**
`journal_targets` -- so every test must include at least one
`AddTargetWork` entry in `work_plan.targets`. Otherwise the
early-return at `cli/src/add.rs:884-886` short-circuits BEFORE the
re-check block and the test would pass even with the gate removed.

The `initial_journal_targets` map is a separate field
(`cli/src/add.rs:482`) and must be set up consistently with the chosen
`AddTargetWork` variant, because execute clones it into the working
`journal_targets` at `cli/src/add.rs:961`:

- **Fresh** -- planner inserts at `cli/src/add.rs:1886-1888`, so the
  test must populate `initial_journal_targets` with the matching
  `LuksUuid` -> `AddJournalTarget` entry alongside the
  `AddTargetWork::Fresh(...)` entry in `targets`. (This is the shape
  test #3 uses below.)
- **OpenRecoverable** -- planner inserts at
  `cli/src/add.rs:1951-1952`, same dual-field setup as Fresh.
- **ClosedPresentLuks** -- planner does NOT insert at planning time;
  execute's Pass-1 (`cli/src/add.rs:1043-1048`) inserts after
  FSID verification. Tests targeting this variant populate
  `targets` but leave `initial_journal_targets` empty for that
  entry -- the re-check sees the entry added by Pass-1 because the
  re-check runs after Pass-1.

The existing `add_pre_write_uniqueness_assert_*` cluster shows how to
assemble a non-noop work plan with one journal target.

Add six table-style cases under the existing `classify_live_pool_match`
test cluster (`cli/src/add.rs:2545+`):

1. **`execute_live_pool_recheck_rejects_different_backing`** -- pins
   the **OpenRecoverable** branch of the TOCTOU race so the
   plan's "same race in OpenRecoverable" claim is not only described
   but tested. Build a non-noop `AddPlan` whose
   `work_plan.targets` contains exactly one
   `AddTargetWork::OpenRecoverable(RecoverableBraidTarget { ... })`
   entry and whose `work_plan.initial_journal_targets` carries the
   matching `LuksUuid -> AddJournalTarget { mode:
   AddJournalMode::RecoverableBraidLabeled { verified_pool_fsid, ...
   }, name, by_id }` row (per the planner's insert at
   `cli/src/add.rs:1951-1952` and the journal shape at
   `cli/src/journal.rs:62-71`). OpenRecoverable is the right variant
   to pin because:
   - It has the dual-field plan-time setup the preamble describes.
   - Its execute path (the Pass-1 loop at `cli/src/add.rs:969-983`)
     does zero re-checking, so the only thing standing between a
     plan-time `NoMatch` and an irreversible `pool_add_device` is
     the new re-check block. A Fresh-target test would not exercise
     this signal because Fresh UUIDs are random and the same-UUID
     race is structurally impossible for them; a ClosedPresentLuks
     test runs through Pass-1 work the VM test already covers.
   - Pass-1 OpenRecoverable runs before the re-check
     (`cli/src/add.rs:969-983` precedes the new block), so the
     OpenRecoverable entry is in `journal_targets` by the time the
     re-check iterates the map.

   Mock a fresh `probe_pool` whose returned `PoolState` has the same
   `mounted` and `fsid` as `self.pool` (so the pool-identity guard
   passes) and one `PoolDevice` carrying the target's `luks_uuid` with
   `underlying` set to a path that canonicalizes to something
   different from the target's by-id (so
   `classify_live_pool_match` returns `LivePoolMatch::DifferentBacking`).
   Inject a `BackingPathResolver` whose `canonicalize` returns
   deterministic distinct paths for the target by-id and the live
   `underlying` so the comparison at `cli/src/add.rs:258` resolves to
   not-equal. Assert `AddPlan::execute` returns
   `AddError::DuplicateUuid` with the canonical live-pool rendering
   (target's by-id, placeholder live by-id `/dev/disk/by-id/`, the
   synthesized live mapper name as `braid-<name>`, the target's name
   as `braid-<name>`, and the shared UUID). See F3 details in the
   VM-test section for the rendering template.

2. **`execute_live_pool_recheck_rejects_same_backing`** -- mock a fresh
   pool that contains our UUID under our by-id, assert the
   pool-drift `AddError::Validation` fires with the "Re-run" hint.

3. **`execute_live_pool_recheck_no_match_invokes_resolver_for_target`**
   -- the NoMatch arm is the silent-pass arm, so its test needs an
   execute-time sentinel that uniquely fires when the re-check ran.
   Just reaching `journal::write_journal` does not work as a sentinel:
   `write_journal` (`cli/src/journal.rs:229`) writes through
   `StatePaths`, not the `Filesystem` mock, and journal write is
   downstream of the re-check block regardless of whether the block
   exists. Use a counting `BackingPathResolver` instead:

   - Wrap `BackingPathResolver` (`cli/src/luks.rs:724-728`) in a
     test impl that records every `canonicalize(path)` call.
     `classify_live_pool_match` (`cli/src/add.rs:228-272`) calls
     `resolver.canonicalize(target_by_id.as_str())` exactly once
     per invocation (line 234-235) -- so a count of canonicalize
     calls keyed on the target's by-id is a direct proof the
     re-check executed `classify_live_pool_match` against the
     fresh pool.
   - Build a non-noop `AddPlan` directly with one Fresh target (Fresh
     keeps the test small: no LUKS-format / luks-open mocking is
     required before the re-check, so the counting resolver's
     observation window starts empty -- Pass-1 Fresh loop is empty
     and Pass-1 OpenRecoverable / ClosedPresentLuks loops have no
     targets to iterate). Allocate a temp `StatePaths` so the
     journal file lands on a real path the test can inspect.
   - Mock downstream commands so the re-check passes (fresh
     `probe_pool` returns a pool with no matching UUID and the same
     `mounted` + `fsid` as `self.pool`), the sleep inhibitor
     acquires, `journal::write_journal` succeeds, and Pass-2
     `luks_format` (`cli/src/add.rs:1148`) **fails** with a
     synthetic `CmdRequest::CryptsetupLuksFormat` error. Pass-2's
     ensure_luks_open is downstream of `luks_format` (line 1195),
     so failing on `luks_format` keeps every canonicalize call out
     of Pass-2 and isolates the observation to the re-check.
   - Assertions:
     - Counting resolver recorded at least one
       `canonicalize(target_by_id)` call during `execute`. If the
       entire re-check block were deleted, this count for the
       target's by-id would be zero (Fresh's Pass-2 path was cut
       off before ensure_luks_open, which is the only other site
       that would canonicalize the target by-id during execute).
     - `journal::load_journal(&paths)` (`cli/src/journal.rs:242`)
       returns `Ok(Some(_))` from the temp `StatePaths`, proving
       execute progressed past `journal::write_journal` (i.e. the
       re-check did not abort).
     - `execute` returned `Err(_)` with the synthetic
       `luks_format` failure (so we know the test exercised the
       intentional-fail leg rather than completing the happy path
       and removing pending-op.json).

4. **`execute_pool_identity_guard_rejects_planned_mounted_now_unmounted`**
   -- `self.pool` has `mounted: true`, fresh `probe_pool` returns
   `mounted: false`. Assert `AddError::Validation` fires with the
   mount-drift "pool unmounted" wording BEFORE any per-target
   `classify_live_pool_match` call. This is the regression guard
   against the F1 finding for the mounted-plan path.

5. **`execute_pool_identity_guard_rejects_planned_unmounted_now_mounted`**
   -- `self.pool` has `mounted: false` (bootstrap path),
   `journal_targets` has one Fresh entry, fresh `probe_pool` returns
   `mounted: true`. Assert `AddError::Validation` fires with the
   "a pool appeared at /mnt/storage" wording, and crucially that
   the abort happens BEFORE the downstream `mkfs.btrfs` sentinel
   would record. This pins the bootstrap-side leg of F1 -- the
   destructive `mkfs.btrfs` in `pool_bootstrap_mount` MUST NOT run.

6. **`execute_pool_identity_guard_rejects_fsid_drift`** -- both
   `self.pool` and fresh `probe_pool` have `mounted: true`, but
   `fsid` values differ (e.g. `Some("A")` vs `Some("B")`). Assert
   `AddError::Validation` fires with the "pool fsid changed"
   wording. Pin that both FSID values appear in the message so the
   operator can correlate.

The existing test scaffolding for `classify_live_pool_match` (the
`live_pool_match_*` cluster at `cli/src/add.rs:2545+`) shows the right
shape: build `PoolDevice` fixtures with `luks_uuid` + `underlying`,
construct a stub `BackingPathResolver`, and assert the returned variant.

### VM test (new)

**`tests/cli/braid-add-cloned-luks-header-race-rejected.py`** (with
sibling `.nix` config). Pattern adapted from
`tests/cli/braid-add-uuid-swap-rejected.py` (the only existing test
that drives the interactive `braid add` confirmation prompt via a
FIFO).

Preamble (per `docs/testing.md` and AGENTS.md `Test Conventions`):

```
# Intent: `braid add` rejects a returning-disk add when a cloned
# LUKS header is added to the live pool between the confirmation
# prompt and the irreversible pool-add step.
#
# Why it exists: protects the execute-time live-pool re-classification
# that closes the plan-to-execute TOCTOU window for ClosedPresentLuks
# and OpenRecoverable targets. Without that gate, an external
# clone-add during the confirmation pause slips past the canonical
# "duplicate LUKS UUID" defense and surfaces as a non-canonical
# btrfs error.
#
# Scenario: disk3 is a removed-but-returnable braid disk. Operator
# starts `braid add disk3=...` without `--yes`. While braid waits at
# the confirmation prompt, an external actor clones disk3's LUKS
# header onto disk4, opens disk4 under `clone-foreign`, and
# `btrfs device add` adds it to the pool. Feeding "yes\n" +
# passphrase to the waiting `braid add` must trigger the execute-time
# live-pool re-check and surface the canonical duplicate-UUID refusal,
# leaving pool.json and pending-op.json untouched.
```

Skeleton (mirroring `braid-add-uuid-swap-rejected.py:80-181`):

1. **Setup** -- build pool with disk1+disk2+disk3, write a marker file,
   unmount, close disk3's mapper, mount degraded, `braid remove-missing`
   disk3, snapshot `pool.json` to `/tmp/pool-before.json`. (Mirrors
   `braid-add-cloned-luks-header-rejected.py:45-63`.)

2. **Background `braid add`** -- `mkfifo /tmp/braid-in`, spawn
   `braid add disk3=/dev/disk/by-id/virtio-disk3 ... --passphrase-stdin`
   (no `--yes`, no `--passphrase-file`) with stdin from `/tmp/braid-in`
   and stdout/stderr captured to `/tmp/braid-out`. Poll
   `grep -q "Type 'yes' to continue" /tmp/braid-out` to detect prompt.

3. **Race injection** -- while braid is paused, run as root:
   `cryptsetup luksHeaderBackup .. disk3` then `luksHeaderRestore` onto
   disk4, `cryptsetup open` disk4 under a non-conflicting mapper name
   (e.g. `clone-foreign`), then `btrfs device add
   /dev/mapper/clone-foreign /mnt/storage` to inject the clone into
   the live pool.

4. **Resume add** -- write `yes\n<passphrase>\n` into `/tmp/braid-in`,
   wait for the background process to exit, capture exit code.

5. **Assertions** -- precise to the live-pool arm of
   `duplicate_live_pool_uuid_error` (`cli/src/add.rs:2084-2104`). That
   helper synthesizes the live side with a **placeholder by-id**
   (`/dev/disk/by-id/`, no device suffix) and parses the live mapper
   name into a `DiskName` (`live_device.mapper.0`). Combined with
   `duplicate_uuid_error`'s by-id-lexicographic sort
   (`cli/src/add.rs:2106-2138`) and the `AddError::DuplicateUuid`
   format string (`cli/src/add.rs:59-61`), the rendered message looks
   like:

   ```
   duplicate LUKS UUID: braid-<live-mapper-name> (/dev/disk/by-id/) and braid-disk3 (/dev/disk/by-id/virtio-disk3) share UUID <UUID> -- detach the cloned or unintended disk before retrying (this typically indicates a dd-cloned disk)
   ```

   where `<live-mapper-name>` is the mapper string the test used in
   `cryptsetup open ... <mapper>` (e.g. `clone-foreign` -- pick a
   name that does NOT match `braid-disk3` so we exercise the
   live-pool arm, not the mapper-conflict arm; also pick a string
   that satisfies `DiskName::parse` at `cli/src/types.rs:125-132`
   so the renderer uses the parsed value rather than the `foreign`
   fallback at `cli/src/add.rs:2095`).

   The test must assert:
   - `exit_code != 0`.
   - Output contains:
     - the literal substring `duplicate LUKS UUID`;
     - `braid-<live-mapper-name>` (the parsed synth name);
     - `(/dev/disk/by-id/)` -- the placeholder by-id, exactly, as a
       parenthesized substring (so it is not confused with the real
       `/dev/disk/by-id/virtio-disk3`);
     - `braid-disk3 (/dev/disk/by-id/virtio-disk3)` -- the target side;
     - the shared UUID (the value the test captured from
       `cryptsetup luksUUID /dev/disk/by-id/virtio-disk3`).
   - Output does NOT contain `is open but backed by` (that wording
     belongs to `MapperConflict` / `MapperBackingMismatch` from
     `cli/src/luks.rs:103-125`; matching it would mean we wandered
     into the existing mapper-conflict path instead).
   - `pool.json` byte-equal to `/tmp/pool-before.json` (no membership
     mutation -- abort was clean).
   - `pending-op.json` does not exist (abort before journal write).

6. **Recovery** -- close `clone-foreign`, remove disk4 from pool via
   `btrfs device remove` (or just leave the test machine in its
   shutdown state -- the existing
   `braid-add-cloned-luks-header-rejected.py` does not bother
   cleaning up before `machine.shutdown()`).

**VM config (`.nix`)** -- model on
`tests/cli/braid-add-cloned-luks-header-rejected.nix`. Allocate
disk1-disk4 only (sufficient: disk4 receives the clone, no extra
disks needed). Register the new check in `flake.nix` per
`docs/testing.md`.

### Why no test for OpenRecoverable race

The same defense covers OpenRecoverable, but VM-testing that branch
adds complexity: the test would need to leave disk3's mapper open
after `remove-missing`, which is less natural to set up than the
closed-mapper path the existing rejection test already exercises.
The unit tests above pin the execute-time helper directly against
the `journal_targets` map, which is variant-agnostic -- so an
OpenRecoverable target reaches the same code via the same map. The
unit tests cover OpenRecoverable via the abstract path; the VM test
covers ClosedPresentLuks via the concrete user-visible path. This
is the precedent the repo uses elsewhere (e.g.
`classify_live_pool_match` itself is exhaustively unit-tested but
has one VM test that exercises only the most operator-relevant arm).

## Verification

1. **Rust unit tests:** `just test-rust` -- the new
   `execute_live_pool_recheck_*` and `execute_pool_identity_guard_*`
   cases must pass; the existing `classify_live_pool_match`,
   `add_pre_write_uniqueness_assert_*`, and add planner tests must
   remain green.

2. **VM tests:** `just test-vm braid-add-cloned-luks-header-rejected
   braid-add-cloned-luks-header-race-rejected
   braid-add-uuid-swap-rejected add-returned-disk-after-remove-missing
   braid-add-disk` -- existing rejection paths still fire, the new
   race test pins the new gate, and the happy-path add flows still
   succeed.

3. **Manual sanity (optional, on the new VM test):** confirm via
   `-v` output that the abort path runs `luks_guard.drop` and the
   Pass-1 ClosedPresentLuks mapper is closed (or never opened, if the
   re-check fires before Pass-1 opens it -- depending on final
   placement detail, see below).

## Out of scope

- **Re-planning at execute time.** A full `build_add_work_plan` rerun
  with fresh state would catch wider drift (e.g. an operator
  removed-missing in a parallel session) but is an architectural
  change that this fix does not need.
- **`replace.rs` analogous gap.** Replace has a similar pattern at
  `cli/src/replace.rs:983-1027` (`verify_existing_luks_open_mapper_target`)
  that re-checks mapper ownership but not live-pool collision. The
  same fix shape could apply, but the replace race surface is
  narrower (replace targets one specific devid; btrfs replace
  serializes against pool state at start) and is left as a follow-up.
- **Hardening against post-re-check drift.** A residual window remains
  between the fresh `probe_pool` and the actual `btrfs device add`
  (`pool_add_device` at `cli/src/add.rs:1323`). Eliminating it would
  require a btrfs-level atomic guard that does not exist. The fix
  shrinks the window from "entire user-typing time + Pass-1" down to
  "single re-classification pass plus journal write," which is the
  practical limit without a kernel-level primitive.
- **Removing or rewriting `probe_closed_present_luks_target_uuid`.**
  That gate (commit `09bfa79`) catches a different threat (by-id swap
  -- the disk at the configured by-id path is suddenly a different
  LUKS volume). Keep it; the new gate is additive.
