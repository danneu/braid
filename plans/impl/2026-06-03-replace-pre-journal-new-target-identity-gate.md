# Pre-journal new-target identity gate for `replace` (both ExistingLuks arms)

## Context

`ReplacePlan::execute` (`cli/src/replace.rs#ReplacePlan::execute`) writes the
pending-op journal (`journal::write_journal`, ~`replace.rs:597`) **before** it
re-checks the new target's LUKS identity. For `ReplaceTargetPrep::ExistingLuks`
targets the identity re-check sits *after* the journal:

- closed-mapper arm: `probe_existing_luks_new_target_uuid` (~`replace.rs:746`),
- open-mapper arm: `verify_existing_luks_open_mapper_target` (~`replace.rs:769`).

The only pre-journal identity gate, `verify_replace_execute_live_pool_uuid`
(~`replace.rs:557`), inspects the **live pool** for a *colliding* `new_uuid`; it
never reads the disk at `new_by_id`. So if the operator swaps the new disk to a
foreign, non-colliding-UUID disk during the post-confirmation window (idle
confirm prompt, passphrase read), the mismatch is caught only post-journal. No
data is corrupted (recovery re-checks identity and refuses destructive replay --
`cli/src/recover.rs#returned_replay_wrong_identity_fails_before_wipe_or_add`),
but the operator is pushed into `braid recover` for a reversible, preflight-class
failure.

This contradicts the governing decision directly. ADR 019 -- the authority for
the inhibitor/journal boundary -- already enumerates the excluded,
pre-journal/pre-inhibitor scope for all four mutating commands as "`--dry-run`,
confirmation prompt, passphrase reads, **reversible validation and identity
checks**" ([`019-inhibit-sleep.md`](../../docs/design/decisions/019-inhibit-sleep.md),
"Current application"). replace's ExistingLuks identity checks at
`replace.rs:746` / `:769` sit in the *protected* (post-journal) scope instead, so
the code is out of step with 019's own enumeration -- and per `AGENTS.md`, "code
that contradicts a principle is wrong." The higher-level invariant is in
[`docs/design/principles.md`](../../docs/design/principles.md) section 3: *"The
journal write is the line of no return ... a ... 'command never started' failure
must not leave a stranded `pending-op.json`."* The sibling
`cli/src/add.rs#AddPlan::execute` corroborates the target shape: it runs its
ExistingLuks-equivalent identity checks (`probe_closed_present_luks_target_uuid`
+ `ensure_luks_open` for ClosedPresentLuks, OpenRecoverable verified at planning)
in **Pass 1, before** `journal::write_journal` (~`add.rs:1245`); only Fresh-disk
format is post-journal.

**Outcome:** an ExistingLuks disk-swap/backing-drift in the post-confirmation
window aborts on the reversible side (before the sleep inhibitor and journal),
leaving no stranded `pending-op.json` -- for **both** ExistingLuks arms.

## Approach

Two-tier defense, mirroring `add.rs` and the existing
`verify_replace_execute_live_pool_uuid` seam:

1. **Add a pre-journal tier** -- hoist the *arm-appropriate* identity check above
   the journal/inhibitor, reusing the two existing helpers (no new probe logic).
2. **Keep the post-journal tier** -- the existing `probe_existing_luks_new_target_uuid`
   (closed, ~746) and `verify_existing_luks_open_mapper_target` (open, ~769) stay
   as the tight pre-open / pre-replace TOCTOU guard. They also cover the narrow
   post-journal window that now contains the slot-1 keyfile enroll (~`replace.rs:713`).

Rejected alternatives:
- *Finding's literal "move" (single-tier).* Drops the deliberately-tight
  "probe right before `ensure_luks_open`" guard documented at `replace.rs:740-745`.
- *Closed-arm only.* Leaves the open-mapper backing-drift / cloned-header swap
  stranding the journal -- the same finding, re-fileable against the open arm.
- *Uniform by-id probe for both arms.* Bolts a by-id UUID check onto the open arm
  whose real invariant is mapper-backing identity; muddled. Arm-appropriate keeps
  each pre-journal check identical to its post-journal counterpart.

## Changes

### `cli/src/replace.rs`

**New private dispatcher** (place near the other execute-time gates,
`verify_replace_execute_live_pool_uuid` ~939 / `probe_existing_luks_new_target_uuid`
~983). Reuses both existing helpers; FreshLuks is a no-op (its journaled UUID is
written by `cryptsetup luksFormat` after the journal, and is gated at
finish-time/recovery instead).

```rust
/// Pre-journal new-target identity gate for `replace`. Hoists the
/// arm-appropriate ExistingLuks identity check above `journal::write_journal`
/// so an operator disk-swap/backing-drift in the post-confirmation window
/// aborts on the reversible side (principles.md "line of no return") instead
/// of stranding pending-op.json. FreshLuks has no pre-existing identity to
/// probe. The post-journal probe/verify in Step 1 remain as the tight
/// pre-open guard (two-tier).
fn verify_existing_luks_new_target_preflight<R: CommandRunner>(
    runner: &R,
    target_prep: &ReplaceTargetPrep,
    new_name: &DiskName,
    new_mapper: &MapperName,
    new_by_id: &ByIdPath,
    new_uuid: &LuksUuid,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<(), ReplaceError> {
    match target_prep {
        ReplaceTargetPrep::ExistingLuks { mapper_open: false, .. } => {
            probe_existing_luks_new_target_uuid(runner, new_by_id, new_uuid)
        }
        ReplaceTargetPrep::ExistingLuks { mapper_open: true, .. } => {
            verify_existing_luks_open_mapper_target(
                runner, new_name, new_mapper, new_by_id, new_uuid, backing_path_resolver,
            )
        }
        ReplaceTargetPrep::FreshLuks { .. } => Ok(()),
    }
}
```

**New call site** -- immediately after the live-pool gate (~`replace.rs:557`) and
**before** the sleep-inhibitor acquisition (~572) so a failure here also occurs
before the inhibitor (`acquire_count == 0`). All bindings are already in scope
from the `ReplaceWorkPlan` destructure (~423-440): `target_prep`, `new_name`,
`new_mn`, `new_by_id`, `new_uuid`, `params.backing_path_resolver`.

```rust
verify_replace_execute_live_pool_uuid(runner, fs, config.mount_point(), &pool, &new_uuid)?;
verify_existing_luks_new_target_preflight(
    runner, &target_prep, &new_name, &new_mn, &new_by_id, &new_uuid,
    params.backing_path_resolver,
)?;
```

Leave the Step-1 post-journal checks (`replace.rs:746`, `:769`) exactly as-is.

### Docs -- ADR 019 is the authoritative home

The fix brings the code into conformance with an invariant ADR 019 *already*
states ("reversible validation and identity checks" are excluded/pre-journal
scope), so no new invariant statement is created. The doc work is verification
plus one optional clarifying note -- not a parallel enumeration.

- **`docs/design/decisions/019-inhibit-sleep.md` (authority).** Verify the
  "braid replace" protected-scope subsection still reads correctly after the
  change. It does: the *primary* identity gate moving pre-journal conforms to the
  excluded-scope enumeration, and the residual second-tier TOCTOU re-probe is
  subsumed under the existing post-journal "new-disk LUKS initialization/open"
  protected-scope line. Optionally add one sentence to that subsection documenting
  the deliberate **two-tier** identity gate (primary check pre-journal per the
  excluded-scope rule; residual post-journal re-probe inside the protected
  LUKS-open scope, guarding the keyfile-enroll window) so a future reader does not
  collapse it to one tier.
- **`docs/design/principles.md` (minimal).** Do **not** append identity to the
  section-3 "Environment-side resource acquisition" bullet: identity validation
  is not resource acquisition (category mismatch), and restating 019's
  enumeration here creates a second home for one fact (authority drift the
  project's docs norms police). At most add a brief cross-reference from that
  bullet to ADR 019 for the full pre-journal excluded scope.

## Tests (`cli/src/replace.rs`, inline `#[cfg(test)]`)

Two axes per `AGENTS.md`: the behavior we add (the new **pre-journal tier**), and
the existing claim we keep load-bearing (the **post-journal tier** -- which the
plan's own rationale calls essential against a swap during the journal->open
keyfile-enroll window). That yields a 2x2 matrix -- {pre-journal, post-journal} x
{closed arm, open arm} -- plus the dispatcher no-op. Every test gets the Intent /
Why it exists / Scenario preamble.

Critically, the post-journal tier (tests #4/#5) is the coverage the change would
otherwise *lose*: today only the open arm has full-execute wiring coverage of the
post-journal verifier (via #2 below), and the pre-journal tier intercepts #2's
scenario before line 769 is reached. So #4/#5 are not redundant -- without them,
deleting the post-journal probe at `replace.rs:746`/`:769` would fail no test.

1. **NEW -- closed-arm foreign swap aborts before journal (full execute).**
   Closed-mapper ExistingLuks target (`ReplacementPool::two_disk_healthy()
   .with_mapper_closed("braid-disk3")`). The new disk's by-id `luksUUID` returns
   `new_uuid` during planning and a foreign UUID at the execute pre-journal probe
   (call-index-keyed handler on the by-id device: call 0 -> `new_uuid`, call 1 ->
   foreign). Assert: `Err(NewTargetUuidMismatchAtOpen { .. })`,
   `journal::load_journal(&f.paths).unwrap().is_none()`, `f.inhibitor.acquire_count() == 0`,
   no `BtrfsReplaceStart`, no `CryptsetupLuksOpen`. Model the journal/inhibitor
   asserts on `wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal`
   (`replace.rs:4276`); model the error shape on
   `replace_existing_luks_open_boundary_probe_mismatch_aborts` (`replace.rs:6707`).

2. **MODIFY -- `mapper_name_drift_does_not_skip_open_mapper_verifier`
   (`replace.rs:4394`).** Proves the pre-journal tier for the open arm. This is
   the only *pre-existing* full-execute test with a call-index-keyed `luksUUID`
   handler (on the mapper backing `/dev/vdd`), so it is the only existing handler
   that must be re-derived (#4/#5 author fresh call-index handlers by design).
   The drift is now caught **pre-journal**. Re-derive the `/dev/vdd`
   call sequence (the pre-journal `verify_existing_luks_open_mapper_target` adds an
   earlier backing read) and update the `disk3_backing_uuid_calls` match arms
   accordingly; if the index bookkeeping is brittle, gate the handler on
   `replace_done` / a phase flag instead of a raw counter. Add
   `journal::load_journal(...).is_none()` and `acquire_count() == 0` asserts; keep
   the existing "no `BtrfsReplaceStart`" assert. This is the behavioral proof for
   the open arm.

3. **NEW (small) -- dispatcher unit test for `verify_existing_luks_new_target_preflight`.**
   FreshLuks -> `Ok(())` with zero issued requests (pins the no-op branch
   structure-insensitively). Closed/open routing is already covered by the
   existing helper unit tests (`replace.rs:6707`, `:6769`, `:6796`); this only
   pins the FreshLuks-skip + dispatch.

4. **NEW -- closed-arm post-journal-tier catch (full execute, `--enroll`).**
   The pre-journal tier *passes* and a swap is detected only at the post-journal
   probe (`replace.rs:746`), inside the keyfile-enroll window the design calls
   load-bearing. Run with `--enroll` so the slot-1 `cryptsetup luksAddKey`
   (`replace.rs:713`) actually sits between the two tiers. Call-index-keyed
   handler on the new disk's by-id `luksUUID`: planning -> `new_uuid`, pre-journal
   probe -> `new_uuid` (pass), post-journal probe -> foreign. Assert:
   `Err(NewTargetUuidMismatchAtOpen { .. })`, `journal::load_journal(...).is_some()`
   (the journal *is* written -- accepted post-journal residual), a
   `CryptsetupLuksAddKey` *was* issued (proves the enroll window was exercised),
   no `CryptsetupLuksOpen` (abort precedes `ensure_luks_open`), no
   `BtrfsReplaceStart` (no pool data routed onto the foreign disk). This is the
   guard that closing-by-deletion of line 746 must fail; it never had full-execute
   wiring coverage before.

5. **NEW -- open-arm post-journal-tier catch (full execute, `--enroll`).**
   Restores what old test #2 covered before its repurpose: the post-journal
   `verify_existing_luks_open_mapper_target` (`replace.rs:769`) rejecting a
   mismatch in the full `cmd_replace` path. Pre-journal verify reads the mapper
   backing -> `new_uuid` (pass); post-journal verify reads it -> foreign (abort).
   Call-index-keyed handler on the backing `luksUUID` (planning + pre-journal ->
   `new_uuid`, post-journal -> foreign). Assert `Err(NewTargetUuidMismatchAtOpen
   { .. })`, `journal::load_journal(...).is_some()`, no `BtrfsReplaceStart`. (No
   `ensure_luks_open` on the open arm -- the mapper is already open.)

**Not affected (verified):** the open-boundary helper unit tests at
`replace.rs:6707 / 6769 / 6796` call the helpers directly (not through `execute`),
so the new pre-journal call does not perturb their `requests[0]` assertions.
Position-based ordering tests (e.g. `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`,
`replace.rs:4492`) use `position()` checks, not exact counts, and tolerate extra
read-only requests. The dry-run/preview path (`plan.preview()` ->
`render_steps()`) performs no probes, so no snapshot changes: the new check is
execute-time and read-only, and 022's actual constraint is "no mutation inside
`plan_*()`" (`docs/design/decisions/022-dry-run-preview-model.md`), which this
respects -- planning issues no new probe.

## Verification

1. `just test-rust` -- primary. Runs the inline mock-backed execute tests
   (pre- and post-journal tier tests for both arms, plus the dispatcher test)
   plus the existing replace suite. This change is execute-time ordering only -- no
   systemd / mount / pool-lock / module blast radius -- so per `AGENTS.md`
   test-scope guidance a full VM run is not required.
2. (Optional, belt-and-suspenders) a focused replace VM test if extra confidence
   is wanted, but the no-stranded-journal behavior is fully covered by the Rust
   mock tests above.
3. No fixture refresh (`just capture-all-fixtures`) -- no parser-critical tool
   version change.

## Files

- `cli/src/replace.rs` -- new dispatcher + call site; modify one test (#2),
  add four (#1 pre/closed, #3 dispatcher, #4 post/closed, #5 post/open).
- `docs/design/decisions/019-inhibit-sleep.md` -- verify the "braid replace"
  protected-scope subsection still reads correctly; optional one-line two-tier
  note.
- `docs/design/principles.md` -- at most a cross-reference to ADR 019 from the
  section-3 resource-acquisition bullet (no enumeration edit).

## Implementation notes

- The plan's test inventory was one commit stale. HEAD commit `2ca36d47`
  ("test(replace): pin closed-mapper open-boundary uuid re-probe wiring") had
  already added `cmd_replace_existing_luks_closed_mapper_open_boundary_swap_aborts`
  -- a closed-arm full-execute test with a call-index-keyed by-id `luksUUID`
  handler, i.e. exactly the scenario the plan's "Test #1 NEW" describes. After
  the pre-journal tier lands, that test's single-swap scenario is intercepted
  before the journal, so its post-journal boundary assertions (journal `Some`,
  `acquire_count == 1`) flip. Test #1 was therefore implemented as a **modify**
  of that existing test (flip to journal `None` / `acquire_count == 0`, update
  the preamble) rather than a new test -- mirroring exactly how the plan treats
  the open arm in Test #2, and avoiding a duplicate scenario. This supersedes
  the plan's claim that "only the open arm has full-execute wiring coverage of
  the post-journal verifier": both arms had it, so Tests #4/#5 are what preserve
  the post-journal coverage the two pre-journal modifications would otherwise
  drop. Final shape: 2 modifies (#1 closed pre-journal, #2 open pre-journal) +
  3 news (#3 dispatcher, #4 closed post-journal, #5 open post-journal).
- Test #2 (`mapper_name_drift_does_not_skip_open_mapper_verifier`) kept its
  existing call-index `/dev/vdd` handler unchanged -- empirically it already
  lands the failing probe at the pre-journal tier (the abort observed value is
  unchanged), so only the journal-`None`/`acquire_count == 0` asserts and a
  preamble note were added. No call-sequence re-derivation was needed.
- Tests #4/#5 use the **phase-flag** approach the plan endorsed as the
  robustness fallback: gate the foreign UUID on "slot-1 `luksAddKey` seen"
  rather than a raw call counter. Planning + the pre-journal probe (both before
  the enroll) observe `U_NEW`; only the post-journal re-probe observes
  `U_FOREIGN` -- independent of the exact pre-enroll probe count.
- Verified the post-journal probes stay load-bearing: temporarily disabling
  both Step-1 checks makes #4 and #5 fail (execution proceeds past the abort to
  `MissingMock` / pool mutation), confirming the post-journal tier is not dead
  coverage. Restored before staging.
- Added the optional two-tier note to ADR 019's `### braid replace` subsection
  (plan-recommended) and a pure pointer (no enumeration) from principles.md
  section 3 to `019-inhibit-sleep.md#current-application`, per the plan's
  authority-drift guidance.
