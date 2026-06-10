# Make `CredentialVerifyTarget` construct-safe via smart constructors

## Context

The shipped fix `plans/impl/2026-06-09-credential-verify-uuid-join.md` routed the
four live-member credential-verify sites through the ADR-024 UUID->`DiskName`
join (`cli/src/membership.rs#present_device_name`), so member display names now
survive mapper drift. That closed the *active* bug, but only by convention +
tests: `cli/src/credential_verify.rs#CredentialVerifyTarget` still has two
**public `String` fields**, so any future call site can put a mapper-derived
name straight into a target and silently re-violate
[ADR-024](../../docs/design/decisions/024-luks-uuid-identity.md) /
[principle 5](../../docs/design/principles.md#5-stable-identifiers) ("No code
path may decide membership, target a device, or correlate live pool state by
parsing a name out of a mapper path").

This plan converts that rule from documented-and-tested to **structurally
enforced**: make the fields private and expose only two smart constructors, so
the only ways to build a target are the UUID join (for live members) or an
already-validated `DiskName` (for operator input). A raw mapper `String` becomes
unconstructable. This applies [principle 3, safe-by-construction
operations](../../docs/design/principles.md#3-safe-by-construction-operations)
at the type level, following `docs/dev/safety-heuristics.md`'s rule to "put
invariant checks at the layer that owns the invariant" -- here, the constructor
that mints the display name. It is a cosmetic-display refactor only -- no
identity or control-flow change: cryptsetup verification still runs against
`device` (the live `underlying` path or the `by_id` path), untouched.

## Goal / non-goals

- **Goal:** private `name`/`device` fields behind `name()`/`device()`
  accessors; two constructors (`existing_pool_member`, `named_candidate`); all
  11 construction sites and the error-formatter read sites migrated; the
  invariant pinned by behavioral tests; ADR-024 "Tests That Enforce This" notes
  the structural guarantee.
- **Non-goal:** changing which disk is verified, the `device` value, control
  flow, or any identity check. `name` is cosmetic.
- **Non-goal:** touching `lock.rs`/`discover.rs` mapper parsing (sanctioned
  ADR-024 carve-outs) or relocating the `present_*` join helpers.

## Design

Two associated functions on `cli/src/credential_verify.rs#CredentialVerifyTarget`,
fields made private, `#[derive(Debug, Clone, PartialEq, Eq)]` **retained**
(derive is in-module, so private fields keep `==` working):

```rust
/// ADR-024 present-device display rule, enforced at the type boundary:
/// resolve a live pool member's display name through the UUID->DiskName
/// join so a drifted mapper can never leak into the credential-verify line.
pub fn existing_pool_member(membership: &PoolMembership, device: &PoolDevice) -> Self {
    Self {
        name: membership::present_device_name(membership, device),
        device: device.underlying.clone(),
    }
}

/// Operator-attested target: the name is an already-validated DiskName
/// (never a mapper basename), the device the by-id setup handle.
pub fn named_candidate(name: &DiskName, device: &ByIdPath) -> Self {
    Self { name: name.as_str().to_owned(), device: device.as_str().to_owned() }
}

pub fn name(&self) -> &str { &self.name }
pub fn device(&self) -> &str { &self.device }
```

No test-only raw constructor is needed: Rust private fields stay readable and
literal-constructible inside the defining module and its descendant `#[cfg(test)]
mod tests`, so `credential_verify.rs`'s own fixtures keep building literals
unchanged. Privacy only blocks the *external* modules.

**Constructor placement is deliberate.** Both live on the type, not in
`membership.rs`: a member builder elsewhere would need a public/`pub(crate)` raw
constructor, re-opening the hole. The new edge `credential_verify` ->
`membership`/`types` is cycle-free (`membership.rs` never imports
`credential_verify`).

`cli/src/credential_verify.rs` adds `use crate::membership::{self, PoolMembership};`
(the `PoolMembership` type lives in `membership.rs`, **not** `types.rs`) and
`use crate::types::{ByIdPath, DiskName, PoolDevice};`. All names are used
(clippy-clean): `membership`/`PoolMembership`/`PoolDevice` in
`existing_pool_member`, `DiskName`/`ByIdPath` in `named_candidate`.

## Implementation steps

This is one cohesive edit set. Making the fields private breaks only the five
*external* files (add/replace/recover/mount/enroll) -- in-module
`credential_verify.rs` code (production reads and its `mod tests`) compiles
unchanged. One commit.

**1. `cli/src/credential_verify.rs` -- type + API.** Add the imports; drop `pub`
on the two fields; add the two constructors + two accessors (each with a `///`
per AGENTS.md). The in-module reads in `verify_credential_for_targets` /
`probe_keyfile_enrollment` compile against the now-private fields (same-module
access), so no in-module migration is required; converting them to
`name()`/`device()` is optional style, left out to keep the diff minimal.

**2. Migrate the 11 production construction sites.** (A repo-wide
`rg "CredentialVerifyTarget \{"` over `cli/` returns exactly these 11 + 3 test
literals + the def; there are zero `match`/`let` destructures.)

Live-member -> `CredentialVerifyTarget::existing_pool_member(<membership>, device)`:
- `cli/src/add.rs#build_add_credential_prelude` (first `.map` over
  `input.pool.devices`; membership = `input.pool_membership`).
- `cli/src/replace.rs#build_member_verify_targets` (membership = `pre_membership`).
- `cli/src/recover.rs#verify_recover_passphrase_for_add_replay` (first `.map`).
- `cli/src/recover.rs#verify_replace_fresh_prep_passphrase` (first `.map`).

Operator-input -> `CredentialVerifyTarget::named_candidate(<name>, <by_id>)`:
- `cli/src/add.rs#build_add_credential_prelude` (second construction):
  `named_candidate(&input.names[i], input.by_ids[i])`. **Ref-level gotcha:**
  `AddStepsInput.by_ids` is `&[&ByIdPath]`, so `input.by_ids[i]` is *already*
  `&ByIdPath` -- pass it bare (no `&`); `input.names` is `&[DiskName]`, so
  `&input.names[i]` is required.
- `cli/src/replace.rs#execute` (`ReplaceTargetPrep::ExistingLuks` arm):
  `named_candidate(&new_name, &new_by_id)`. **Ref-level:** `execute` destructures
  `ReplaceWorkPlan` by value, so `new_name`/`new_by_id` are *owned*
  `DiskName`/`ByIdPath` here -- borrow them (passing by value would move bindings
  reused later in `execute`).
- `cli/src/recover.rs#verify_recover_passphrase_for_add_replay` (the
  `verify_targets.push`): `named_candidate(&target.name, &target.by_id)` --
  here `target` is `&journal::AddJournalTarget` with owned fields.
- `cli/src/recover.rs#verify_replace_fresh_prep_passphrase` (the `push`):
  `named_candidate(new_name, new_by_id)`.
- `cli/src/mount.rs#credential_verify_targets`: `named_candidate(name, by_id)`.
- `cli/src/enroll_key_file.rs#plan_single_disk_enrollment`:
  `named_candidate(name, by_id)`.
- `cli/src/enroll_key_file.rs#plan_enrollment` (`.map(|c| ...)`):
  `named_candidate(&c.name, &c.by_id)`.

**3. Migrate the external (non-`credential_verify.rs`) read sites to accessors.**
These are the only field reads privacy actually breaks. The production
error-formatter reads carry the `target` in a `CredentialVerifyError`:
- `cli/src/add.rs#execute` rejection arm: keep `position(|t| t == &target)`
  (PartialEq); `target.name` -> `target.name()`, `target.device` ->
  `target.device()`.
- `cli/src/replace.rs#execute` rejection arm: keep
  `new_disk_target.as_ref() == Some(&target)` (PartialEq); migrate `target.name`
  / `target.device` to accessors.
- `cli/src/mount.rs#open_disks_with_credential` Rejected + Luks arms: migrate
  `target.name`/`target.device` to accessors (incl. the value passed to
  `luks::probe_luks_header`).
- `cli/src/recover.rs#verify_recover_passphrase_for_add_replay` **map_err arms
  only** -> `target.name()`. **Trap:** the same function's earlier loop binds a
  *different* `target: &AddJournalTarget` (used at `probe_config_disk`,
  `config::luks_label_for`, the UUID-mismatch messages) -- those reads must
  **not** change. Do not mass-replace `target.name` in this file.
- `cli/src/recover.rs#verify_replace_fresh_prep_passphrase` map_err arms ->
  `target.name()`.
- `cli/src/enroll_key_file.rs#plan_enrollment` rejection arm -> `target.name()`.

Plus one external **test** read that also breaks compile under private fields
(the prelude drift test ADR-024 enumerates):
- `cli/src/add.rs#add_credential_prelude_names_drifted_member_via_membership`:
  `prelude.verify_targets[0].name` -> `.name()` and
  `prelude.verify_targets[0].device` -> `.device()`. Assertion values
  (`"disk1"`, `/dev/vda`) unchanged -- still a real regression check on the join.

**4. New behavioral tests (`cli/src/credential_verify.rs`).** Add the
constructor tests below in the in-file `mod tests`. No fixture rewrite: the
existing `targets()` literals and the `expected_wait_ok_pairs` /
`passphrase_runner` / `key_file_runner` reads keep compiling against the private
fields (same module), and the `target == targets[1]` matches still rely on the
retained `PartialEq`.

## Files to modify

- `cli/src/credential_verify.rs` -- type change, constructors, accessors,
  new behavioral tests (in-module reads/fixtures untouched).
- `cli/src/add.rs`, `cli/src/replace.rs`, `cli/src/recover.rs`,
  `cli/src/mount.rs`, `cli/src/enroll_key_file.rs` -- construction +
  external error-read migration (plus the `add.rs` prelude drift-test asserts).
- `docs/design/decisions/024-luks-uuid-identity.md` -- one enforcement note.

## Tests (behavioral, structure-insensitive)

New unit tests in `credential_verify.rs` (use the hand-padded-UUID `parse`
idiom from `membership.rs` tests; allocate seed band **600-609** -- 300-399 is
already owned by `cmd.rs`, and 100-599 are otherwise taken; add a
`// Test-module seed allocation: cli/src/credential_verify.rs uses 600-609.`
comment):

- **`existing_pool_member` resolves drift:** membership `U -> "disk1"`,
  `PoolDevice { mapper: braid-WRONG, luks_uuid: U, underlying: /dev/vdb }` ->
  `name() == "disk1"` AND `device() == "/dev/vdb"` (proves `underlying`, not
  mapper, not by_id). This is exactly what the original bug failed.
- **Foreign UUID falls back to full basename:** UUID absent from membership,
  `mapper: braid-WRONG` -> `name() == "braid-WRONG"` (not stripped to `WRONG`).
- **`named_candidate` round-trips:** typed `DiskName` + `ByIdPath` ->
  both accessors return the input strings.

The existing per-site drift tests that ADR-024 "Tests That Enforce This"
enumerates regression-guard the migration, all with **assertion values
unchanged**: the replace/recover/membership ones assert emitted lines / error
strings (untouched); the lone exception is
`add.rs#add_credential_prelude_names_drifted_member_via_membership`, which
asserts struct fields directly -- its two reads become `name()`/`device()` per
step 3 (the asserted `"disk1"` / `/dev/vda` values stay).

## ADR-024 update

Add one bullet to "Tests That Enforce This" (then `just docs-build`): note that
`CredentialVerifyTarget`'s fields are private and the only constructors are
`existing_pool_member` (UUID-joins the display name) and `named_candidate`
(operator-attested `DiskName`), so no call site can put a mapper-derived name
into a credential-verify target; unit tests pin the drifted-mapper join and
foreign-UUID fallback at the constructor. `principles.md` needs no change (it
states the rule, not the mechanism).

## Verification

- `just test-rust` -- new constructor tests pass; the existing
  add/replace/recover/membership decision-024 drift tests still pass unchanged.
- `just clippy` (lints `--tests`) -- no unused imports; no `needless_borrow`
  (the `add.rs` `by_ids[i]` ref-level is the one to get right).
- `just docs-build` -- mdbook + linkcheck2 clean after the ADR note.
- `scripts/docs/check-output-ascii.py` -- unaffected (constructors return
  names; no Unicode).
- Spot-read the five `cli/` diffs: confirm every `device:` resolves to
  `underlying` (member) or a `ByIdPath` (candidate) and no message wording
  changed.

## Risks

- **`recover.rs` `target` aliasing** (highest): the `AddJournalTarget` loop
  binding vs the `CredentialVerifyError` binding share the name `target`. Step 3
  enumerates the exact arms; do not blanket-replace.
- **Ref-levels (two opposite traps):** in `add.rs` `input.by_ids[i]` is already
  `&ByIdPath` -- pass it bare (over-borrowing trips `needless_borrow`); in
  `replace.rs#execute` `new_name`/`new_by_id` are *owned* -- borrow them (passing
  by value moves bindings reused later). The compiler catches a wrong level
  immediately.
- **PartialEq:** three production/test consumers rely on the derive
  (`add.rs#execute`, `replace.rs#execute`, two tests). Keep `#[derive(... PartialEq, Eq)]`;
  private fields do not break it.
- **Fallback string** (`WRONG` vs `braid-WRONG`) is already the shipped
  behavior of `present_device_name`; this plan does not change it.

## Implementation notes

- Added a `///` doc on the `CredentialVerifyTarget` struct itself (the plan
  only specified docs for the constructors/accessors): the struct previously
  had no doc comment, and AGENTS.md requires one on every `pub` item; it now
  states the construct-safety invariant the private fields enforce.
- Dropped the per-site `// Decision-024 display join: ...` comments at the
  three migrated `existing_pool_member` construction sites (`add.rs`,
  `recover.rs` x2): they described the inline construction that no longer
  exists, and the rationale now lives in the constructor's doc comment.
