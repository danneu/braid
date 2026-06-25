# De-brittle dry-run preview render tests via a shared ordered-line helper

## Context

A Low-severity testing finding flagged
`plan_remove_2to1_preview_omits_confirmation_only_redundancy_warning`
(`cli/src/remove.rs`) for asserting `lines.len() == 6` and exact command rows
at fixed indices (`lines[1]/[3]/[5]`). That pins the absolute rendered layout:
a cosmetic preview reflow (e.g. adding a blank separator line) shifts every
index and breaks the test even though the operator-visible commands are
unchanged. The risk is churn/false-failure, not a missed regression.

Investigation showed this is **cross-cutting**, not a one-off: ~8 dry-run
preview tests across six modules (`add.rs`, `lock.rs`, `replace.rs`,
`remove_missing.rs`, `remove.rs`, `enroll_key_file.rs`) assert on absolute line
indices and/or total line count.
Meanwhile the robust idiom **already exists in-tree**, duplicated as local
closures -- `find()` in `add.rs` (twice) and `pos_of()` in `replace.rs` --
that locate a line by substring and assert *relative* order. The codebase also
already has the right home for shared test scaffolding: `cli/src/test_fixtures.rs`,
whose whole purpose is consolidating per-test setup.

Per AGENTS.md ("reach for the ideal, robust, simple, most correct solution --
regardless of scope") the ideal fix dissolves the whole class behind one shared
helper rather than patching the single cited test. These are test-only changes
with no production-behavior risk: the tests still pin every load-bearing string
and must still pass.

**Outcome:** preview tests assert the contract that actually matters -- which
commands appear, in what order, with what exact argv quoting, and that
confirmation-only `WARNING:` lines stay out -- and survive cosmetic layout
reflows without edits.

## Approach

### 1. Add a shared preview-assertion helper to `test_fixtures`

Place in `cli/src/test_fixtures/shared.rs` -- the `shared` submodule declared at
`test_fixtures.rs:125`, alongside `MockFs` -- since these are command-agnostic.
Mark them `pub(crate)` and re-export them through the existing
`pub(crate) use shared::{...}` facade list in `test_fixtures.rs` (the `shared`
module itself is private, so callers reach fixtures as `crate::test_fixtures::{...}`,
the same path `MockFs` / `PoolFixture` already use). All take the rendered `&str`
and split internally, retiring the
`let lines: Vec<&str> = rendered.lines().collect()` boilerplate.

- `line_index(rendered, needle) -> usize` -- index of the first line containing
  `needle`; panic with the full render on miss. The shared generalization of
  the existing `find()`/`pos_of()` closures.
- `assert_lines_in_order(rendered, &[needle])` -- each needle appears
  (substring) on its own line in strictly increasing order. For ordering /
  presence contracts (LUKS `format < addkey < backup < open`, lock umount
  ordering).
- `assert_exact_lines_in_order(rendered, &[exact])` -- each string matches a
  rendered line **verbatim** (complete line: no leading indent, no trailing
  junk) in strictly increasing order. For the load-bearing `shell_words` argv
  pins. A single-element slice covers the "exact line, position-agnostic" case.

### 2. Migrate the brittle tests (transformation rules)

- `assert_eq!(lines.len(), N)` -> the semantic step count already in scope
  (`steps.len()` / `preview.steps.len() == N_steps`) where "no extra steps" is
  worth guarding; drop it where purely incidental. (The 3->2 test already does
  `preview.steps.len() == 2` -- this is the established idiom.)
- `assert_eq!(lines[K], "$ cmd ...")` (exact) -> `assert_exact_lines_in_order(...)`
  with the exact strings: preserves quoting + order + no-junk, drops the
  absolute index.
- `assert!(lines[K].contains("..."))` (substring) -> `assert_lines_in_order(...)`.
- **Preserve legitimate first-line contracts:** keep `assert_eq!(lines[0], "[warn] ...")`
  where warning-first ordering is the real contract (ADR 022 notes-render-first);
  de-brittle only the follow-on indices.

### 3. Retire the duplicated closures

Replace the local `find()` (`add.rs`) and `pos_of()` (`replace.rs`) closures
with the shared `line_index` / `assert_lines_in_order`. This is the dedup payoff
and keeps one idiom for all future preview tests.

### 4. Pin the helper contract with focused tests

The migration leans on `assert_exact_lines_in_order` enforcing **full-line**
equality: if it silently accepted substring/prefix matches, every migrated
exact-argv pin would stop catching extra trailing text. Add focused unit tests
in the existing `cli/src/test_fixtures/shared.rs#tests` module (negative cases
via `#[should_panic]`):

- exact-line matching **rejects trailing junk** -- needle
  `"$ cryptsetup close braid-disk2"` against a rendered line
  `"$ cryptsetup close braid-disk2 # x"` must fail (proves `==`, not `contains`).
- both ordered helpers **require strictly increasing positions** -- needles
  passed out of render order (or resolving to the same line) must fail.
- (sanity) a needle absent from the render panics with the full render in the
  message.

## Load-bearing constraint (must hold)

The exact `shell_words`-quoted argv strings stay pinned as **full-line** matches:
`remove.rs` balance `'-dconvert=single' '-mconvert=dup'`, `remove_missing.rs`
balance `'-dconvert=raid1,soft'`, and the exact `cryptsetup close braid-diskN`
rows. These tests are the *only* end-to-end pin of that quoting -- no `cmd.rs`
test covers the `BtrfsBalanceSingle` argv (`cmd.rs:771`). `assert_exact_lines_in_order`
preserves this; do not weaken these to loose `contains`.

## Files (pattern repeats; representative tests)

- `cli/src/test_fixtures/shared.rs` -- new shared helpers (`pub(crate)`) plus
  their contract tests in the existing `#[cfg(test)] mod tests`.
- `cli/src/test_fixtures.rs` -- add the three helpers to the existing
  `pub(crate) use shared::{...}` re-export list so migrated tests reach them via
  `crate::test_fixtures::{...}` (the `shared` submodule stays private).
- `cli/src/remove.rs` -- `plan_remove_2to1_...` (the finding),
  `plan_remove_3to2_...`. **Leave** `plan_preview_renders_soft_warn_above_dry_run_steps`
  untouched -- it is already the good model (`lines[0]` warning contract + `contains`).
- `cli/src/remove_missing.rs` -- `dry_run_render_targeted_removal_with_balance`;
  `plan_preview_renders_warn_above_steps` (keep the `lines[0]` warning, de-brittle
  the `lines[1]` follow-on).
- `cli/src/replace.rs` -- `dry_run_render_fresh_disk_live_replace_with_keyfile`
  (incl. exact `lines[11]` close pin); migrate the `pos_of()` / `find()`
  ordering tests to the helper.
- `cli/src/add.rs` -- `dry_run_render_fresh_single_disk_bootstrap`; migrate the
  two `find()` ordering tests.
- `cli/src/lock.rs` -- `dry_run_render_lock_mounted_2_disks`,
  `dry_run_lock_not_mounted_1_open`.
- `cli/src/enroll_key_file.rs` -- `dry_run_render_enroll_generate_3_disks`
  (drop the brittle `output.lines().count() == 13`; it already locates by `find()`)
  and `dry_run_render_enroll_existing_keyfile` (replace `output.lines().count() == 8`
  with the semantic `steps.len() == 4` -- its own comment notes "2x (enroll +
  backup) = 4 steps"; keep the existing `contains("enroll keyfile")` /
  `!contains("generate keyfile")` presence checks).

## Reuse (existing code to lean on)

- `find()` (`cli/src/add.rs#dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey`)
  and `pos_of()` (`cli/src/replace.rs`) -- the prototype for `line_index`.
- `Preview::render` (`cli/src/preview.rs#Preview::render`) / `Step::render_dry_run`
  (`cli/src/cmd.rs#Step::render_dry_run`) -- define the `[risk] desc` / `$ cmd`
  line shape the helpers target.
- `preview.steps` field -- semantic step count for the count assertions.

## Verification

- `just test-rust` -- all migrated tests pass, including the new
  `shared.rs#tests` helper-contract cases (which pin full-line / strictly-increasing
  semantics automatically).
- **Reflow-robustness check (manual, revert after):** temporarily insert a blank
  separator line into `Step::render_dry_run`; confirm the migrated tests still
  pass (robust) where the old `lines.len()`/`lines[K]` assertions would have
  failed. Revert.
- **Load-bearing-coverage check (manual, revert after):** temporarily perturb a
  balance argv quote (drop a `'`); confirm `assert_exact_lines_in_order` fails
  loudly. Revert.
- `just test-parsers` and the ASCII check (`scripts/docs/check-output-ascii.py`)
  are unaffected -- no parser change, and test code is exempt from the ASCII rule.
