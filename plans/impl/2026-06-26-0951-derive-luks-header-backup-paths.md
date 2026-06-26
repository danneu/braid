# Refactor: derive LUKS header-backup paths on demand (drop the cached field)

## Context

`braid add` and `braid replace` cache a `header_backup_path: HeaderBackupPath`
on their planning structs so the dry-run preview (`render_steps`) can print the
`LUKS header backup -> <path>` line without re-deriving it from `StatePaths`.
The field's own doc admits the smell: *"computed at plan time so render_steps
does not need access to `paths` ... Unused when `enroll_key_file` is `None`."*

Investigation showed the field is more vestigial than it looks:

- **`execute()` never reads it.** Both `ReplacePlan::execute` and
  `AddPlan::execute` destructure the field away (`..`) and re-derive the path at
  mutation time via `backup_luks_header_post_mutation(.., params.paths)`
  (`cli/src/replace.rs` lines ~670/738, `cli/src/add.rs` lines ~1390/1470). The
  field's *only* consumer is `render_steps` (dry-run preview).
- **It is dead when no enroll happens.** For the existing/recoverable targets the
  backup step is gated behind `if let Some(kf) = enroll_key_file`, so the
  always-populated path is unused on the common no-`--enroll` path.
- **The value is deterministically re-derivable** from
  `(paths.luks_headers_dir(), mapper)` via the single source of truth
  `cli/src/luks.rs#luks_header_backup_path`. Unlike the fresh-format UUID (minted
  per-invocation, genuinely un-re-derivable, legitimately cached per ADR 022),
  the path needs no caching.
- **There is already an in-repo precedent for the right shape.**
  `cli/src/recover.rs#RecoverWorkPlan` stores only `luks_headers_dir: PathBuf`
  and derives the path on demand in its renderer
  (`luks::luks_header_backup_path(&plan.luks_headers_dir, &mapper)`).

This contradicts the ADR 022 directive that plan structs *"store semantic work,
not rendered steps"* and that previews *derive* `Vec<Step>` on demand
(`LockPlan::preview()` is cited as the model). The ideal fix makes `add`/`replace`
consistent with `recover`: store the one semantic input (`luks_headers_dir`) and
derive the path where it's rendered.

**Outcome:** remove the 5 cached `header_backup_path` fields, store
`luks_headers_dir: PathBuf` once per work plan, and derive the path on demand in
`render_steps`. Dry-run output and execute behavior are byte-for-byte unchanged.

## Approach (mirror `RecoverWorkPlan`)

Pattern, applied identically in `replace.rs` and `add.rs`:

1. **Add** `luks_headers_dir: PathBuf` to the work-plan struct
   (`ReplaceWorkPlan`, `AddWorkPlan`), with a short `///` noting it exists so
   `render_steps` can derive header-backup paths without `StatePaths` (mirrors
   `RecoverWorkPlan::luks_headers_dir`).
2. **Remove** `header_backup_path` from every planning variant/target:
   `ReplaceTargetPrep::{FreshLuks,ExistingLuks}` and
   `add.rs#{FreshLuksTarget,RecoverableBraidTarget,ClosedPresentLuksCandidate}`.
3. **Set** the new field once at build time from the same source the old field
   used: `luks_headers_dir: input.paths.luks_headers_dir()` in
   `build_replace_work_plan` (`cli/src/replace.rs`) and `build_add_work_plan`
   (`cli/src/add.rs`).
4. **Derive on demand** in `render_steps`, replacing each
   `<target>.header_backup_path` read with
   `luks_header_backup_path(&self.luks_headers_dir, &<mapper>)`:
   - `replace.rs`: mapper is `self.new_mapper` (already on the work plan).
   - `add.rs` Fresh / ClosedPresentLuks: mapper is `target.mapper_name`.
   - `add.rs` OpenRecoverable: this target has only `mapper_path`, not
     `mapper_name`, so derive `mapper_name(&target.name)` -- the exact value used
     at build time (every build site computes `mn = mapper_name(name)`). Bind it
     to a local and pass `&hbp` into `push_returned_disk_enrollment_steps`
     (that helper already takes the path as a `&HeaderBackupPath` parameter, so
     its signature is unchanged).

`render_steps`/`preview` **signatures stay `(&self)`** -- so the ~59 dry-run
call sites do not change.

### Reused helpers (no new code)
- `cli/src/luks.rs#luks_header_backup_path` -- the derivation (already imported in
  both files).
- `cli/src/state_paths.rs#luks_headers_dir` -- yields the dir.
- `cli/src/cmd.rs` / `add.rs#push_returned_disk_enrollment_steps` -- unchanged.

## Files to change

**`cli/src/replace.rs`**
- `ReplaceWorkPlan`: add `luks_headers_dir`.
- `ReplaceTargetPrep::{FreshLuks,ExistingLuks}`: drop `header_backup_path`
  (field + the two `render_steps` reads, replaced by the derivation).
- `build_replace_work_plan`: set `luks_headers_dir`; drop the two
  `header_backup_path: luks_header_backup_path(..)` initializers.
- Test construction site (`FreshLuks` literal, ~line 8006): drop the field init.
  `replace_work_plan_for_test` routes through `build_replace_work_plan`, so it
  needs no change.

**`cli/src/add.rs`**
- `AddWorkPlan`: add `luks_headers_dir`.
- `FreshLuksTarget`, `RecoverableBraidTarget`, `ClosedPresentLuksCandidate`:
  drop `header_backup_path`.
- `build_add_work_plan`: set `luks_headers_dir` on the returned `AddWorkPlan`;
  drop the local `header_backup_path` var and the three target initializers.
- `render_steps`: three derivation sites (Fresh unconditional; OpenRecoverable
  and ClosedPresentLuks enroll-gated).
- `execute` (`verified` reconstruction, ~line 1233): drop the
  `header_backup_path: target.header_backup_path.clone()` line (the rebuilt
  target feeds journaling + pool-add, neither of which reads it).
- Test helpers `fresh_target` / `recoverable_target` (~lines 3000/2987): drop the
  field init. The one manual `AddWorkPlan { .. }` test literal (~line 3062): add
  `luks_headers_dir` (use a tempdir `StatePaths` via the existing `test_paths()`
  helper, or a literal `PathBuf` as `recover.rs` tests do).

## What does NOT change (and why that's safe)

- **Dry-run output:** identical -- the rendered path is the same function of the
  same inputs (`luks_headers_dir` from the same `StatePaths`, `mapper` from the
  same disk name), routed through the same `luks_header_backup_path`.
- **Execute behavior:** untouched. It already re-derives via
  `backup_luks_header_post_mutation(.., params.paths)`; that line stays.
- **Journal schema:** `AddJournalTarget` / `ReplaceJournalTarget` and their
  builders never referenced `header_backup_path` -- no serialized-format change.
- **`render_steps`/`preview` signatures and their ~59 call sites:** unchanged.
- **Recovery messaging / header-backup workflow:** unaffected. This refactor
  touches only internal path derivation, not user-facing recovery advice, so the
  `luks-unlock.md#header-backup-workflow-and-messaging` invariant is not in play.
- **No design-doc change required:** no behavior or invariant changes. The change
  brings `add`/`replace` *into* compliance with ADR 022. (Optional: add them
  beside `LockPlan` in ADR 022's "derive on demand" examples.)

## Tests

This is a representation-only change, so the bar is "existing behavioral coverage
still passes," not new tests:

- The existing dry-run render tests already assert the literal
  `LUKS header backup -> /var/lib/braid/luks-headers/<mapper>.luksheader` line in
  the preview (e.g. `replace.rs` dry-run tests around lines 2509+/4644+, `add.rs`
  around 3769+/7980+). These are structure-insensitive behavioral assertions and
  are the regression guard for the derivation -- if they pass unchanged, the
  rendered path was preserved. No new tests needed.
- Confirm the two-render reproducibility tests (`add.rs` ~7980-7982,
  `replace.rs` ~4786-4787) still pass (derivation is pure, so they will).

## Verification

1. `cargo build -p braid` (or `just build`) -- catches every dropped/added field
   at the construction sites; expect a clean compile once all sites are updated.
2. `cargo clippy --all-targets` -- confirm the `luks_header_backup_path` import
   stays used (it moves from build sites to render sites) and no dead-code warning
   on the new field.
3. `just test-rust` -- runs the unit suite, including the dry-run render and
   reproducibility tests that pin the exact backup-path line.
4. Spot-check a dry run end-to-end if a VM is handy:
   `braid add --dry-run ...` and `braid replace --dry-run ...` and eyeball that
   the `LUKS header backup -> ...` line is byte-identical to `master`.

## Risks / notes

- **Single-source consistency is preserved, not weakened.** Today the path is
  computed in two places (cached field at plan time vs. `params.paths` at
  execute) kept in sync only by the shared helper. After this change it is still
  two derivations (render from `self.luks_headers_dir`, execute from
  `params.paths`) through the *same* helper with the *same* inputs -- identical
  guarantee, minus the dead cached intermediary.
- **OpenRecoverable mapper:** deriving `mapper_name(&target.name)` is exactly the
  build-time value; an alternative is to add a `mapper_name: MapperName` field to
  `RecoverableBraidTarget` for symmetry with its sibling targets. Inline
  derivation is the smaller, recommended change.
