# Route credential-verify display names through the ADR-024 UUID join

## Context

Four sites build the **display-only** `name` on a `CredentialVerifyTarget` by
parsing the live mapper basename instead of joining the device's LUKS UUID back
to the operator name:

- `cli/src/replace.rs:507` (execute credential block)
- `cli/src/add.rs:1998` (`build_add_credential_prelude`)
- `cli/src/recover.rs:2098` (`verify_recover_passphrase_for_add_replay`)
- `cli/src/recover.rs:2842` (`verify_replace_fresh_prep_passphrase`)

Each reads `name_from_mapper(device.mapper).unwrap_or(device.mapper)` and carries
a comment conceding it is a "display-only fallback." This contradicts
[ADR-024](../../docs/design/decisions/024-luks-uuid-identity.md) item 6 ("Code
must not parse mapper names ... to ... correlate live pool state") and its
"Display code has an explicit join rule" section, which mandates resolving a live
pool device's UUID back to `DiskName` for presentation. Under mapper drift (a
member open as `braid-WRONG`), every credential line shows `WRONG`
(`passphrase: checking against WRONG...`, `accepted by WRONG`, and the
`passphrase does not match existing pool member 'WRONG'` rejection) instead of
the real disk name -- precisely the drift case ADR-024 was written to fix.

The fix is not four local patches. A canonical helper already implements this
exact rule -- `present_display_name` at `cli/src/status.rs:261`, documented as the
"Single source of the decision-024 present-device display-name rule" -- but it is
**private to `status.rs`**, which is the root cause of the duplication. The
intended outcome: promote that helper to a shared home, route all four sites (and
status's own call sites) through it, and make the credential display survive
mapper drift like every sibling surface. `PoolDevice` already carries `luks_uuid`
(`cli/src/types.rs:490`), and every site already has a `PoolMembership` snapshot in
scope, so no new `pool.json` reads are required.

This refactor makes latent-violating code conform to ADR-024, so the ADR text
(which currently claims the TUI Bus column was "the last display correlation to
adopt this rule") must be updated in the same change.

## Goal / non-goals

- **Goal:** one shared join helper; all four credential-verify display names
  resolved via UUID->`DiskName`; status call sites unified onto the same helper;
  ADR-024 corrected; drift-resistance pinned by tests.
- **Non-goal:** changing any identity/control-flow decision. The `name` field is
  cosmetic -- the actual cryptsetup verification uses `device`/`underlying`, which
  is untouched. No behavior change to which disk is verified.
- **Non-goal:** touching `lock.rs` / `discover.rs` mapper parsing -- those are the
  bootstrap/cleanup narrow exceptions ADR-024 item 6/7 explicitly sanctions.
- **Hard constraint:** the deliberately **late** `membership::load_membership` at
  `cli/src/replace.rs:600` (after the sleep inhibitor) is the journal/drift guard
  pinned by `replace_execute_rejects_when_pool_json_drifts_after_planning`. It must
  stay exactly where it is and must **not** be hoisted or reused for display names.

## Design

### 1. Promote and extend the helper (`cli/src/membership.rs`)

Move `present_display_name` out of `status.rs` into `membership.rs` as
`pub(crate)` (its natural home -- pure transformation over `PoolMembership` /
`DiskMember` / `MapperName`, all already in this module), and add a
`PoolDevice`-centric wrapper so the repeated `by_uuid(&d.luks_uuid) + &d.mapper`
pattern lives in one place:

```rust
/// ADR-024 present-device display rule: UUID-join membership to the operator
/// name, falling back to the raw mapper basename for a foreign live device.
pub(crate) fn present_display_name(member: Option<&DiskMember>, mapper: &MapperName) -> String {
    member.map(|m| m.name.as_str().to_owned()).unwrap_or_else(|| mapper.0.clone())
}

/// Common case: resolve a live `PoolDevice`'s operator name through membership.
pub(crate) fn present_device_name(membership: &PoolMembership, device: &PoolDevice) -> String {
    present_display_name(membership.by_uuid(&device.luks_uuid), &device.mapper)
}
```

Add `PoolDevice` to the existing `use crate::types::{...}` line. Note the
deliberate fallback change: today's sites strip via `name_from_mapper`
(`WRONG`); the helper keeps the full mapper basename (`braid-WRONG`), matching
`status`/TUI. This is the intended consistency fix.

### 2. Route the four credential sites through `present_device_name`

- **`add.rs` (`build_add_credential_prelude`, ~1990):** swap the `name:` expression to
  `membership::present_device_name(input.pool_membership, device)`. `pool_membership`
  is already an `AddStepsInput` field. Drop `name_from_mapper` from the `add.rs:3`
  import.

- **`recover.rs` add-replay (`verify_recover_passphrase_for_add_replay`, ~2092):** add a
  `membership: &PoolMembership` parameter; at the call site
  (`execute_add_pool_mutation_recovery`, ~2409) pass `union` (it covers pre union target,
  so a mid-add new disk that already appears in live `pool.devices` resolves too).
  Use `present_device_name`.

- **`recover.rs` replace-replay (`verify_replace_fresh_prep_passphrase`, ~2836):** add a
  `membership: &PoolMembership` parameter; at both call sites in
  `finish_uncommitted_replace_recovery` (~2949, ~3009) pass `&journal.pre_membership`
  (the `pool.devices` here are all pre-existing members). The separately pushed
  new-disk target keeps using `new_name` directly. Use `present_device_name`.

- **`replace.rs`: resolve member targets at PLAN time (mirrors `add`).** Move the
  retained/anchor selection (`replace.rs:484-512`) into `plan_replace`, where the
  plan-time `pre_membership` (`replace.rs:1314`) and probed `pool` are both in scope
  but the membership is currently loaded and discarded. Build
  `member_verify_targets: Vec<CredentialVerifyTarget>` there using
  `present_device_name(&pre_membership, device)`, and carry it on `ReplaceWorkPlan`
  (add the field to `ReplaceWorkPlanInput` + `ReplaceWorkPlan`). In `execute`, replace
  lines 484-512 with `let mut credential_targets = work_plan.member_verify_targets;`.
  The `new_disk_target` construction (513-522) and the `is_new_disk` rejection
  distinction (533-545) stay byte-for-byte. Drop `name_from_mapper` from the
  `replace.rs:3` import. The late line-600 read is untouched.

  Rationale for plan-time (not a second execute read): reuses already-loaded
  membership (zero new reads), stores only resolved display strings (no membership
  snapshot on the plan, consistent with `replace.rs`'s "plan stores no membership
  snapshot" design), preserves the drift guard, and matches how `add`'s planner
  builds its verify targets.

### 3. Unify status's own call sites (`cli/src/status.rs`)

- `status.rs:285` and `:341` (`present_display_name(membership.by_uuid(&pd.luks_uuid),
  &pd.mapper)`) -> `membership::present_device_name(membership, pd)`.
- `status.rs:1066` (passes a precomputed `matched_member`) -> keep on the primitive:
  `membership::present_display_name(matched_member, &pd.mapper)`.

## Files to modify

- `cli/src/membership.rs` -- new `present_display_name` (moved) + `present_device_name`; import `PoolDevice`; unit tests.
- `cli/src/status.rs` -- delete local `present_display_name`; update 3 call sites to the `membership::` path.
- `cli/src/add.rs` -- join at the prelude builder; drop dead import.
- `cli/src/replace.rs` -- plan-time `member_verify_targets`; new plan field; thin execute; drop dead import.
- `cli/src/recover.rs` -- thread membership into the two replay verify fns + their call sites.
- `docs/design/decisions/024-luks-uuid-identity.md` -- correct the "explicit join rule" paragraph + add enforcement bullets.

## Tests (structure-insensitive, behavioral)

Unit tests only -- the resolved `name` is a cosmetic string pinned precisely at
its construction boundary, so a NixOS VM test (the heavier route ADR-024 uses for
`status`) is intentionally omitted here.

- **Helper (`membership.rs`):** member present -> operator name; member absent ->
  full mapper basename (`braid-WRONG`, asserting it is NOT stripped to `WRONG`).
- **Drift-mapper, per site** -- pool device `mapper = braid-WRONG`, `luks_uuid = U`,
  membership `U -> "disk1"`; assert the observable name is `disk1`:
  - `add`: `build_add_credential_prelude` -> built target name == `disk1`.
  - `replace`: an **execute-level** test is required (a plan-builder assertion on
    `member_verify_targets[0].name` does not substitute -- it would pass even if
    `execute` still emitted a mapper-derived name). Drive `ReplacePlan::execute` with a
    retained member whose mapper is `braid-WRONG` but whose UUID maps to `disk1`, and a
    runner that rejects the passphrase for that member; assert the returned
    `ReplaceError::Validation` reads `... pool member 'disk1'` (not `WRONG` /
    `braid-WRONG`). Model: `replace_execute_rejects_when_pool_json_drifts_after_planning`
    drives `execute` and matches on the `Validation` message; the
    `status_compact_names_present_disk_from_membership_uuid` fixture shows the
    drifted-mapper-plus-UUID-membership construction. (Asserting the emitted
    `[wait] passphrase: checking against disk1...` line via the stderr seam is an
    acceptable alternative target.)
  - `recover` (both): the emit closure writes straight to stderr (`|line| eprint!(...)`),
    so the `[wait]` row is not deterministically capturable in a Rust test. Instead drive
    the verify fn with a passphrase-rejecting `MockRunner` over a drifted-mapper member
    (mapper `braid-WRONG`, UUID -> `disk1`), and assert the returned `RecoverError::Failed`
    names `'disk1'` (not `WRONG` / `braid-WRONG`) -- i.e. `... rejected by 'disk1'`. This
    mirrors the existing `... rejected by 'disk2'` / `'new'` assertions.
- **Regression:** the existing recover rejection-message tests (assert `'disk2'` /
  `'new'`, e.g. `add_pool_mutation_replay_verifies_open_journaled_target_passphrase`,
  `replace_pool_mutation_fresh_luks_bad_passphrase_preserves_journal`) must still pass.
  Verify each fixture's `union` / `journal.pre_membership` contains the verified pool
  devices' UUIDs so the join resolves to the same names; adjust fixtures if any test
  currently leans on a mapper-derived name with no membership entry. **This is the main
  migration risk.**

## ADR-024 doc updates (required -- behavior now conforms)

Describe the *contract*, not the implementation. ADR-024 is authority -- keep it
free of private helper names, and scope the claim to exactly what changes.

- Rewrite the "Display code has an explicit join rule" paragraph: the sentence "The
  TUI Data-tab Bus column is the last display correlation to adopt this rule" is now
  false. Replace it with a contract statement only -- that the **passphrase**
  credential-verification display (add/replace/recover) resolves each existing pool
  member's name through the same live-UUID->`DiskName` join as every sibling surface,
  so member names survive mapper drift. Do **not** name `present_display_name` /
  `present_device_name` (private helpers), and do **not** claim keyfile coverage:
  these paths verify passphrases (recovery rejects keyfile credentials outright), and
  keyfile-enrollment display already resolves names and is unchanged.
- Add "Tests That Enforce This" bullets for the new passphrase credential-verify
  drift tests.

## Verification

- `just test-rust` -- new unit tests pass; existing recover/replace/add/status tests pass.
- `just clippy` -- no unused-import warnings after the `name_from_mapper` drops;
  helper is `pub(crate)` and used.
- `just docs-build` -- `mdbook-linkcheck2` passes; ADR edits keep valid links.
- `scripts/docs/check-output-ascii.py` -- unaffected (helper returns names; no Unicode).
- Spot-check by reading the four diffs: confirm `device`/`underlying` (the verified
  path) is unchanged and only the cosmetic `name` resolution moved.

## Risks

- **Recover fixture coverage** (above) -- the one place an existing assertion could
  flip from a mapper-derived string to a full-mapper fallback if a fixture lacks a
  membership entry. Caught by `just test-rust`.
- **Fallback string change** (`WRONG` -> `braid-WRONG`) is intentional and only shows
  for a device genuinely absent from membership (a foreign live device), which is rare
  on these paths and already how `status`/TUI render it.

## Implementation notes

- Extracted the retained/anchor selection into a shared non-test helper
  `build_member_verify_targets(pre_membership, pool, replace_source, old_uuid)`
  in `replace.rs` rather than inlining it in `plan_replace`. `plan_replace`
  calls it (the "build it there" intent), and the three `build_replace_work_plan`
  test callers reuse it, so the selection logic stays single-sourced.
  `member_verify_targets` is still a field on `ReplaceWorkPlanInput` /
  `ReplaceWorkPlan` as specified.
- The render-only test `dry_run_render_existing_luks_replace_with_enroll_renders_addkey_and_backup`
  passes `member_verify_targets: Vec::new()` (`render_steps` never reads them);
  the two execute-capable test plan-builders build them via
  `build_member_verify_targets` so credential verification still issues the same
  commands.
- The two recover drift tests call the module-private verify fns
  (`verify_recover_passphrase_for_add_replay`, `verify_replace_fresh_prep_passphrase`)
  directly from the in-file test module with a one-member drifted pool and no
  journaled targets, isolating the membership join rather than driving the full
  `execute_*_recovery` flow.
