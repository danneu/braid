# Fix clippy `too_many_arguments` on `plan_enroll`

## Context

`just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) warns
`this function has too many arguments (8/7)` on `plan_enroll` in
`cli/src/enroll_key_file.rs:620`. The default clippy threshold is 7; the repo
sets no `too-many-arguments-threshold` override and no `[lints]` config, so the
fix is to refactor, not to raise the threshold or `#[allow(...)]` it.

The intended outcome: drop `plan_enroll` to 3 arguments by grouping its
non-generic args into a struct, with no behavioral change.

## Decision: reuse the existing `EnrollKeyFileParams`

`cli/src/enroll_key_file.rs:410` already defines:

```rust
pub struct EnrollKeyFileParams<'a> {
    pub membership: &'a PoolMembership,
    pub key_file_path: &'a Path,
    pub generate: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub dry_run: bool,
    pub paths: &'a StatePaths,
    pub backing_path_resolver: &'a dyn BackingPathResolver,
}
```

Its fields are a superset of `plan_enroll`'s 6 non-generic args (all but
`passphrase_stdin` / `passphrase_file`), and the sole production caller,
`cmd_enroll_key_file` (`:691`), already holds a `&EnrollKeyFileParams<'_>` and
currently spreads it into the 8 positional args.

Reuse it rather than minting a new `PlanEnrollParams`, because:

- **Matches braid's revealed pattern.** `recover.rs` decomposes its command
  into sub-helpers (`execute_replace_pool_mutation_recovery`,
  `execute_replace_post_maintenance_recovery`) that take the full
  `&RecoverParams<'_>` even though they don't read every field. `plan_enroll`
  is the same shape (a planner sub-function of the enroll command), so it
  shares the command's params struct. Here it lands at 3 args with no
  `#[allow(clippy::too_many_arguments)]` needed -- strictly cleaner than the
  recover.rs precedent, which still needs the allow.
- **No new type surface.** A second `*Params` for one command is overhead a
  reader must reconcile; braid's doc rule discourages boundaries that lose
  nothing when removed.
- **Production callsite collapses** to `plan_enroll(runner, fs, params)`,
  deleting the 6-field spread.
- The "planning is pre-passphrase" property (the only argument for a tight
  6-field struct) is a code-smell guard, not a state-corruption/journal
  invariant, so by braid's Mutation Safety rules it does not warrant type-level
  enforcement. It is instead captured as documented intent (below).

`runner` / `fs` stay positional generics, mirroring
`cmd_enroll_key_file(runner, fs, params)`; folding them into the struct would
make it generic over `R`/`F` and break that established shape.

## Changes (all in `cli/src/enroll_key_file.rs`)

### 1. Signature (`:620`)

Replace the 6 positional fields with one struct ref; keep the generics:

```rust
pub fn plan_enroll<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &EnrollKeyFileParams<'_>,
) -> Result<EnrollPlan, PlanFailure<EnrollKeyFileError>> {
```

### 2. Body (`:630`-`:688`) -- mechanical `name` -> `params.name`

- `:630` `check_no_pending_operation(paths)` -> `params.paths`
- `:634` `if generate` -> `params.generate`
- `:635` `validate_generated_keyfile_target(runner, key_file_path, false)` -> `params.key_file_path`
- `:637` `validate_key_file_path(key_file_path, false)` -> `params.key_file_path`
- `:644` `discover_enrollment_candidates(runner, fs, membership, backing_path_resolver)` -> `params.membership`, `params.backing_path_resolver`
- `:652` `if dry_run` -> `params.dry_run`
- `:653` `if generate` -> `params.generate`
- `:660` `plan_single_disk_enrollment(runner, name, by_id, key_file_path, mode)` -> `params.key_file_path`
- `:676` `compile_enroll_steps(&needs_enroll, key_file_path, generate, paths)` -> `params.key_file_path`, `params.generate`, `params.paths`
- `:687` the `EnrollPlan { ... generate, }` field shorthand -> `generate: params.generate,` (easy to miss -- shorthand no longer resolves)

### 3. Doc comment (`:600`-`:619`) -- document the ignored fields

Append one sentence converting the unused-fields wrinkle into stated intent,
e.g.: "Takes the command's `EnrollKeyFileParams`; `passphrase_stdin` /
`passphrase_file` are intentionally unread here because planning is
pre-passphrase (execution consumes them after the prompt)."

### 4. Production callsite (`:696`-`:705`)

Collapse the spread to:

```rust
let plan = match plan_enroll(runner, fs, params) {
```

### 5. Test callsites (18, all in this file's `mod tests`)

The full inventory is **1 production call + 18 direct test calls**. The 18 test
calls are at lines 779, 964, 1017, 1068, 1125, 1186, 1229, 1273, 1320, 1420,
1475, 1563, 1654, 1715, 1779, 1843, 1902, 1966. Re-derive with `rg -n
'plan_enroll\(' cli/src/enroll_key_file.rs`: it returns 20 lines = these 18 test
calls + the production call (`:696`) + one prose comment (`:746`); the
definition (`:620`) is not matched because the generics sit between the name and
`(`. Convert each call to build an `EnrollKeyFileParams` and pass `&params`.
Example (the `:779` form):

```rust
let params = EnrollKeyFileParams {
    membership: &membership,
    key_file_path: &kf,
    generate: true,
    passphrase_stdin: false,
    passphrase_file: None,
    dry_run: false,
    paths: &paths,
    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
};
let plan = plan_enroll(&runner, &fs, &params).expect("...");
```

Notes:
- `passphrase_stdin: false` / `passphrase_file: None` are always safe filler
  here: the planner ignores them, so they cannot change any test's outcome
  regardless of what that test exercises. Carry over each callsite's existing
  `generate` / `dry_run` literals (verify which bool was which against the old
  positional order -- this is itself a readability win of the change).
- Use inline struct literals, not a positional `enroll_plan_params(.., true,
  false, ..)` helper: a positional helper would reintroduce the exact
  generate-vs-dry_run bool ambiguity this refactor removes. (`EnrollKeyFileParams`
  is already in scope via `mod tests`' `use super::*;`.)

### 6. Fixture comment (`cli/src/test_fixtures/enroll_key_file.rs:17`-`20`)

Omission #2 in that module's doc comment reads: "No
`EnrollKeyFileParamsBuilder`. Only 3 of 47 tests build an `EnrollKeyFileParams`;
they are heterogeneous (recovery-mode gate / wrong-passphrase abort / dry-run
short-circuit) and configure different fields per scenario."

It is **already stale** before this change (actual today: 9 builds across 56
`#[test]`s, not 3 of 47), and this change makes the 18-test `plan_enroll` cohort
build the struct too -- roughly half the module (about 27 of 56) afterward. The
"few + heterogeneous" premise no longer holds, so a bare number-bump would leave
a false rationale. Rewrite omission #2 to drop the brittle exact count and state
the real reason the builder stays omitted, e.g.:

> No `EnrollKeyFileParamsBuilder`. Many enroll tests build an
> `EnrollKeyFileParams` inline. The `plan_enroll` planner cohort sets
> `passphrase_stdin: false` / `passphrase_file: None` uniformly and varies only
> generate / dry_run / membership / keyfile / paths; inline literals keep each
> test's planning inputs explicit at the callsite, which a positional builder
> would obscure.

## Out of scope / non-goals

- No behavioral change. Pure signature/callsite refactor.
- No new struct, no `#[allow(clippy::too_many_arguments)]`, no clippy.toml.
- No `EnrollKeyFileParamsBuilder` test fixture. The change pushes ~half the
  enroll tests (about 27 of 56) to build the struct inline, weakening omission
  #2's premise (handled in step 6), but adding a builder is a separate refactor;
  inline literals match the 9 existing builds and keep this a focused clippy fix.
- Leave the `plan_enroll` prose mentions in `credential_verify.rs:119,476` and
  `test_fixtures/enroll_key_file.rs:347,356` -- they describe behavior, not the
  signature. (The `:17` builder-omission comment IS updated; see step 6.)

## Verification

This is a Rust-level refactor with no systemd/mount/lock/module blast radius, so
VM tests are not required. Focused scope:

1. `just clippy` -- confirm the `too many arguments (8/7)` warning on
   `plan_enroll` is gone and no new warning was introduced. (`--tests` is in the
   recipe, so the edited test code is linted too.)
2. `just test-rust` -- confirm the crate compiles and the 18 `plan_enroll`
   tests still pass (behavior is unchanged, so all should remain green).
