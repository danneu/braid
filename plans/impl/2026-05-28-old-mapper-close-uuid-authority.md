# Plan: recovery closes the old mapper by UUID authority, not path existence

## Context

`braid replace` of a present disk ("Live" source) does a post-commit
best-effort close of the OLD disk's LUKS mapper. The normal execute path
guards that close with a defense-in-depth UUID probe
(`cli/src/replace.rs:899-925`): it calls
`probe_observed_mapper_uuid(runner, &old_mapper, &journaled_old_uuid)` and
only issues `cryptsetup close` if the mapper's live backing LUKS UUID still
equals the journaled `old_uuid`. On mismatch or probe failure the helper
warns and skips, so braid does not tear down a *foreign* dm slot an operator
opened under the same `braid-<old>` name between plan and close.

The crash-recovery replay of that same post-commit close does **not** have
this guard. `close_old_mapper_best_effort` (`cli/src/recover.rs:3157-3178`)
gates only on `fs.exists("/dev/mapper/<name>")` and then closes -- no UUID
probe. So if a Live replace crashes after the btrfs commit but before the
old-mapper close, and the operator opens a foreign disk under `braid-<old>`
before recovery runs, recovery closes the foreign slot. The execute path
would have detected the UUID drift and skipped.

This is documented-but-unfinished migration work:

- `cli/src/probe_mapper_uuid.rs:11-17` says the helper was lifted into a
  shared module so "the upcoming recovery-side callers ... addressed in
  Phase 4b ... Phase 4b will reuse this body." The migration was later
  declared finished, but this caller was never wired up.
- ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md:62-65`) says
  recovery should "re-check live UUIDs before replaying ... close steps,"
  and Consequences (`:230-232`) says recovery "verifies live UUIDs again at
  mutation boundaries."
- AGENTS.md mutation-safety heuristic: "Query the authoritative source of
  state directly; do not pre-gate it with a cheaper but weaker observable
  such as path existence."

**Why `fs.exists` must go entirely (revised after review).** An earlier
draft kept `fs.exists` as a "silent no-op guard" in front of the probe,
arguing the two were decision-equivalent. That premise is false.
`cryptsetup status` is invoked with the bare mapper *name*
(`cli/src/cmd.rs:465-468`), and cryptsetup only `stat()`s an argument that
contains `/` (`reference/cryptsetup/lib/libdevmapper.c:1935`), resolving a
bare name through a name-based dm ioctl (`dm_status_dmi`). So an *active dm
mapping whose `/dev/mapper` node is missing* (udev race, a manually removed
node, early-boot devtmpfs state) is reported `Active` by the probe but
`false` by `fs.exists`. Keeping the path check would therefore still skip a
close the execute path performs -- the exact weaker-observable divergence the
heuristic forbids. The path node is not the authority; the dm/cryptsetup
state is.

Outcome: bring the recovery close into parity with the execute close by
making the dm/LUKS-UUID probe the sole close authority, and refactor the
shared probe so recovery can treat an already-closed (inactive) mapper as a
silent no-op while mismatch/probe-failure warnings stay shared.

Severity is Low (narrow trigger window plus a deliberate operator action;
a wrongly-closed mapper is recoverable), but the fix closes a real
divergence the command's own non-recovery path already follows, removes a
stale "upcoming" doc claim, and fixes a probe-result modeling defect.

## The fix

### 1. Refactor the shared probe into an ownership result

`probe_observed_mapper_uuid` (`cli/src/probe_mapper_uuid.rs:31-118`) today
returns `bool` and emits *every* non-match warning -- including the inactive
case, which it mislabels as `probe failed (mapper is inactive)`. An inactive
mapper is not a probe failure: `cryptsetup status` on a closed name prints
"... is inactive." with exit 4 (`reference/cryptsetup/src/cryptsetup.c:949-954`),
which `parse_cryptsetup_status` deterministically maps to `Ok(Inactive)`,
*not* `Err` (`cli/src/parse/cryptsetup_status.rs:38-50`). So inactive is
cleanly separable from genuine probe failure.

Change the return type to an ownership result so callers decide how to treat
absence:

```rust
/// Outcome of re-probing the live backing LUKS UUID at an observed mapper
/// against the journaled identity. Separates a clean "no active mapping"
/// result -- which recovery treats as the normal already-closed no-op --
/// from active-but-unverifiable/wrong results, which are always a warned
/// skip. Replaces the prior `bool`, which mislabeled an inactive mapper as a
/// probe failure and forced every caller to treat absence identically.
pub(crate) enum MapperOwnership {
    /// Active; backing LUKS UUID equals the journaled identity. Close it.
    Owned,
    /// No active dm mapping for this name. No warning emitted; the caller
    /// decides whether absence is normal (recovery) or surprising (execute/remove).
    Inactive,
    /// Active but the backing UUID differs, or the probe could not be
    /// completed (status/luksUUID command or parse error, null backing). A
    /// warning was already emitted; skip the close.
    Unverified,
}
```

- The `Match` path returns `Owned`. The mismatch / status-error /
  parse-error / null-backing / luksUUID-error paths emit their warning and
  return `Unverified`. The inactive path returns `Inactive` **without**
  warning.
- **Route warnings through `emit_status`** (`cli/src/status_tag.rs:66`)
  instead of raw `eprintln!`, keeping identical text plus the trailing
  newline: `emit_status(&format!("Warning: ...\n"))`. Production output is
  byte-identical (`emit_status`'s non-test arm is `eprint!`), but the lines
  now flow through the `capture_line` test seam so `capture_with_color` can
  assert them. (This is the Finding-2 fix and is what makes the new
  foreign-mapper test able to observe the warning.)
- Add a shared inactive-note emitter in the same module for the
  execute/remove callers, so the text is not duplicated and recovery can
  simply omit it:

```rust
/// Operator-facing note that a post-commit close was skipped because the
/// observed mapper had no active mapping. Used by the execute/remove close
/// sites, where an absent mapper is surprising; recovery omits it because an
/// already-closed old mapper is its normal post-crash state.
pub(crate) fn warn_close_skipped_inactive(mapper: &MapperName, expected_uuid: &LuksUuid)
```

### 2. Recovery close: drop `fs.exists`, match on ownership

`cli/src/recover.rs` -- `close_old_mapper_best_effort`. Drop the `fs`
parameter and its `Filesystem` bound entirely (it was used only for the
`fs.exists` gate), add `old_uuid: &LuksUuid`, and gate the close solely on
the probe:

```rust
match probe_observed_mapper_uuid(runner, mapper, old_uuid) {
    MapperOwnership::Owned => {
        let color_enabled = color_enabled_for_stderr();
        let old_label = mapper.as_str().strip_prefix("braid-").unwrap_or(mapper.as_str());
        if close_mapper_best_effort(runner, sleeper, mapper, old_label, color_enabled) {
            eprintln!("Old device closed. If repurposing the physical disk, wipe it separately.");
        }
    }
    // Already closed -- the normal post-crash / post-remount-cycle state.
    // Stay silent: unlike execute/remove, an absent old mapper is expected here.
    MapperOwnership::Inactive => {}
    // Foreign or unverifiable slot under braid-<old>; helper already warned.
    MapperOwnership::Unverified => {}
}
```

Add the import near `cli/src/recover.rs:11`:
`use crate::probe_mapper_uuid::{MapperOwnership, probe_observed_mapper_uuid};`

Add a comment naming the accepted downside (parity, not new fragility): the
probe means recovery now always issues at least one `cryptsetup status`
(even in the common already-closed case, which previously short-circuited
with no subprocess), and an *unrelated* transient cryptsetup error on a
genuine replay-close demotes that close to a warned skip, leaking the old dm
slot until `braid lock`/reboot. This is the identical best-effort semantics
the execute path already has at `replace.rs:912`; recovery still *completes*
(resize and journal-clear still run), it is only *warned*.

**Call site (`cli/src/recover.rs:3253-3255`):** pull `old_uuid` from
`journal.op` in the same `if let` scope that already destructures `source`
for `old_mapper`, using the file's existing let-else idiom (see
`replace_journal_in_phase` and the test at `recover.rs:10638`), and stop
passing `fs`:

```rust
if let journal::ReplaceJournalSource::Live { old_mapper, .. } = source {
    let journal::OpKind::Replace { old_uuid, .. } = &journal.op else {
        // ReplacePostMaintenance is only constructed for OpKind::Replace.
        unreachable!("post-maintenance recovery runs only for Replace journals");
    };
    close_old_mapper_best_effort(runner, sleeper, old_mapper, old_uuid);
}
```

`OpKind::Replace.old_uuid` is `LuksUuid` (`cli/src/journal.rs:197-210`) and
is in scope via the function's `journal: &Journal` param.

### 3. Update the two existing callers to match the new return type

These keep their current behavior (warn on inactive, since an absent member
mapper is surprising mid-operation), now expressed as explicit arms:

- **`cli/src/replace.rs:899-925`** -- the `if probe(...) && close(...)`
  chain becomes a `match`: `Owned` => close (and print the trailer),
  `Inactive` => `warn_close_skipped_inactive(...)`, `Unverified` => skip.
- **`cli/src/remove.rs:454-462`** -- the `if probe(...) { close }` becomes
  the same three-arm `match`.

This widening to the shared helper is the correct layer per AGENTS.md ("put
invariant checks at the layer that owns the invariant"); the execute/remove
callsite edits are mechanical and behavior-preserving.

## Reused helpers (do not reimplement)

- `probe_observed_mapper_uuid` (`cli/src/probe_mapper_uuid.rs:31`) -- the
  helper being refactored; single source of truth for the status+luksUUID
  round-trip.
- `close_mapper_best_effort` (`cli/src/mapper_close.rs:70`) -- unchanged.
- `emit_status` / `status_tag::testing::capture_with_color`
  (`cli/src/status_tag.rs:66`) -- the capturable stderr seam.
- `parse_cryptsetup_status` -> `CryptsetupStatusOutput::{Active,Inactive}`
  (`cli/src/parse/cryptsetup_status.rs:35`) -- the Active/Inactive split the
  refactor relies on.
- The execute-path precedent to mirror: `cli/src/replace.rs:886-925`.

## Doc and comment corrections

1. **`cli/src/probe_mapper_uuid.rs:11-17` (module doc)** -- (a) the contract
   line "logger-coupled by design -- every failure path emits the
   operator-facing Warning text and returns `false`" is no longer true;
   rewrite it to describe the `MapperOwnership` contract (shared warnings for
   mismatch/unverifiable via `emit_status`; `Inactive` returned silently for
   the caller to classify). (b) It names `finish_uncommitted_replace_recovery`
   as the "Phase 4b" recovery caller, but that function never closes a mapper;
   the real caller is `close_old_mapper_best_effort` (invoked from
   `execute_replace_post_maintenance_recovery`). Rename it and drop the
   "upcoming / Phase 4b will reuse this body" future framing.
2. **`cli/src/journal.rs:121-128`** -- the `ReplaceJournalSource::Live` field
   doc says `old_mapper` is for "the post-commit `close_mapper_best_effort`
   call and the recovery mirror in `finish_uncommitted_replace_recovery`."
   Point the "recovery mirror" at `execute_replace_post_maintenance_recovery`
   / `close_old_mapper_best_effort`.
3. **`cli/src/replace.rs:886-898` and `cli/src/remove.rs:448-452`** -- the
   "demote to a logged-warning skip" comments should note inactive is now a
   distinct (caller-classified) outcome, not folded into the warned skip.
4. **ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`)** -- add one
   "Tests That Enforce This" bullet (after the `replace.rs` bullet at `:197`)
   for the new recovery foreign-mapper-skip test. The "Safer recovery replay"
   benefit (`:62-65`) and Consequences (`:230-232`) already describe the
   intent generically and need no edit.

## Tests

Behavioral, asserted on `runner.requests()` (and, where noted, captured
stderr). The probe-result change touches every direct caller and unit test
of the helper.

**Helper unit tests** (`cli/src/probe_mapper_uuid.rs:120-344` and the
direct-probe tests at `cli/src/replace.rs:6351-6426`): swap the `assert!(matched)`
/ `assert!(!matched)` checks to the corresponding `MapperOwnership` variant
(`probe_returns_false_when_mapper_is_inactive` -> `Inactive`; the
status-error / parse-error / null-backing / mismatch cases -> `Unverified`;
the match case -> `Owned`). The `Unverified` cases may now also assert the
warning text via `capture_with_color`.

**Recovery tests** (`cli/src/recover.rs`):

1. **Update `recover_replace_old_close_retries_on_busy_then_succeeds`
   (`:10594`).** With `fs.exists` gone, the probe runs unconditionally; stub
   it for `Owned`: `CryptsetupStatus{braid-old}` -> active on a backing
   device (`cryptsetup_status_active`, `:4472`) and `CryptsetupLuksUuid{<that
   device>}` -> `uuid_for_name("old")` = `22222222-2222-2222-2222-222222222222`
   (the journal's `old_uuid`). Probe runs once before the retry loop, so one
   Status + one LuksUuid mock covers the 2-attempt close; the `close_count == 2`
   assertion stays. (The `MockFs` braid-old entry is now irrelevant to the
   decision.)

2. **Update `recover_replays_resize_after_replace_via_mount_cycle`
   (`:15196`) -- `braid-old` is ACTIVE here.** This is a
   `ReplacePhase::PoolMutation` recovery, so `mount_membership_for_recover`
   returns the *union* admission membership (`recover.rs:3701-3704`), and
   `RemountCycle.execute` drops `reopen_names` (`{ close_names, .. }` at
   `:467`) and passes that union into `relock_and_remount`, which *replans*
   the open via `mount::plan_open_pool(... membership ...)` (`:3539-3554`).
   The cycle just closed every union mapper, so the replan reopens all of
   them -- including `braid-old`. (The plan-time `cycle_reopen_names` at
   `:1402-1442` is not what execution uses.) So at the post-maintenance close
   `braid-old` is active; dropping `fs.exists` makes the probe issue
   `CryptsetupStatus{braid-old}` against an active mapper -- the harness
   `closed`-set fast path does *not* apply. Stub `CryptsetupStatus{braid-old}`
   -> active on `/dev/disk/by-id/virtio-old` and reuse the existing
   `CryptsetupLuksUuid{/dev/disk/by-id/virtio-old}` -> `2222...` mock
   (`:15218-15226`), so the probe returns `Owned` and the post-maintenance
   close fires. Assert **two** `CryptsetupClose{braid-old}` calls (the cycle's
   plus the post-maintenance close); the `CryptsetupClose{braid-old}` mock at
   `:15346-15351` is reusable.

3. **Add: direct post-maintenance recovery with `braid-old` already closed
   -> silent skip.** The sibling of
   `recover_replace_old_close_retries_on_busy_then_succeeds` for the inactive
   case (models a crash *after* the old-mapper close but before journal-clear,
   so recovery re-runs `execute_replace_post_maintenance_recovery` on a
   `PostReplaceMaintenance` journal -- no mount cycle, so nothing reopens
   `braid-old`). Stub `CryptsetupStatus{braid-old}` -> inactive (stderr
   "... is inactive.", exit 4; cf. `inactive_status` at
   `test_fixtures/recover.rs:187-194`) and provide no
   `CryptsetupClose{braid-old}` mock. Assert: **zero**
   `CryptsetupClose{braid-old}`, no warning emitted (via `capture_with_color`),
   resize fired (devid 2), journal cleared. This is the inactive case the
   earlier plan draft wrongly attributed to 15196.

4. **Add: foreign disk under `braid-old` -> close skipped** (headline
   regression guard for the original finding; fails before the fix). Stub
   `CryptsetupStatus{braid-old}` -> active on `/dev/<foreign>` and
   `CryptsetupLuksUuid{/dev/<foreign>}` -> a non-matching UUID (e.g.
   `9999...9999`, cf. `already_mounted_two_disks_and_foreign_runner` at
   `:4492`); stub the resize. Assert: **zero** `CryptsetupClose{braid-old}`,
   resize fired (devid 2), journal cleared, and (via `capture_with_color`)
   the `Warning: ... observed 9999...` mismatch line is emitted.

5. **Add: missing `/dev/mapper` node but active dm mapping -> close**
   (regression guard for Finding 1 -- proves the path node is not the gate).
   Drive `execute_replace_post_maintenance_recovery` directly with `MockFs`
   *without* `/dev/mapper/braid-old`; `CryptsetupStatus{braid-old}` -> active
   on a backing device; `CryptsetupLuksUuid` -> `uuid_for_name("old")`. Assert
   the `CryptsetupClose{braid-old}` *did* fire. (Under the old
   `fs.exists`-gated code this close would be skipped.)

6. **Audit, do not assume.** `replace_post_maintenance_inhibitor_failure_preserves_journal`
   (`:10998`) fails at the inhibitor gate before the close
   (`runner.requests().is_empty()`), so it is unaffected.
   `replace_pool_mutation_recovery_resolves_credential_and_remount_cycles_when_all_mappers_open`
   (`:12318`) is another `PoolMutation` mount-cycle path, so -- like 15196 --
   the cycle reopens `braid-old` and it is active at the post-maintenance
   close. It will need a `CryptsetupStatus{braid-old}` -> active stub plus a
   matching `CryptsetupLuksUuid` (reuse the test's existing old-UUID mock);
   confirm and wire the `Owned` close.

**Caller-level inactive tests (replace + remove).** The inactive warning
moved out of the helper (now `Inactive` is returned silently and the
execute/remove callers emit `warn_close_skipped_inactive`). Helper/probe
tests assert the variant only, so they would stay green even if replace or
remove dropped the inactive arm. Add a focused unit test on each post-commit
close path -- `replace.rs` and `remove.rs` -- that stubs
`CryptsetupStatus{<old/target mapper>}` -> inactive and asserts **no**
`CryptsetupClose` plus the captured inactive warning (via `capture_with_color`,
now that the text routes through `emit_status`). These are the only tests
that pin the caller-owned inactive behavior.

No VM test is warranted: the behavior is pure decision logic over cryptsetup
probe outputs, fully modeled by the mock seam, and strictly lower-severity
than the existing recover/replace UUID-mismatch VM tests
(`recover-replace-existing-luks-uuid-mismatch.py`,
`replace-cloned-luks-header-rejected.py`). This matches how the execute-path
probe is covered (unit tests at `replace.rs:6351-6426`,
`probe_mapper_uuid.rs:120-344`).

## Out of scope

- The remount-cycle close (`recover.rs:371-378`) closes reconstructed
  *member* mappers it just validated and immediately reopens -- a different
  close-to-reopen operation, not the foreign-disk hazard this fix targets.

## Verification

- `just test-rust` -- runs the refactored helper unit tests, the updated
  recovery close tests, and the two new regression tests. Confirm the
  foreign-mapper test fails if the probe gate is reverted, and the
  missing-node test fails if `fs.exists` is reintroduced as a gate.
- `cargo build` is exercised by `just test-rust`; no parser-critical tool
  versions, fixtures, or Nix module options change, so no fixture refresh or
  parser canary is required.
- Optional sanity (not required for merge): re-run an existing recover
  replace VM test
  (`just test-vm recover-replace-existing-luks-uuid-mismatch`) to confirm no
  regression in the adjacent recovery path.
