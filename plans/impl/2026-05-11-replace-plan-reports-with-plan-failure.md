# Replace `*PlanReport` Structs with `Result<Plan, PlanFailure<E>>`

## Context

Seven sibling modules each define their own two-field `*PlanReport`
struct (`AddPlanReport`, `RemovePlanReport`, `ReplacePlanReport`,
`UnlockPlanReport`, `EnrollPlanReport`, `RemoveMissingPlanReport`,
`RecoverPlanReport`). All seven encode the same "Shape A
notes-carrying report" contract: the `Err` branch carries
accumulated `PreviewNote`s that the caller must render to stderr
before propagating the error; the `Ok` branch never carries notes
(they live inside the returned `*Plan` instead).

The wart: that "notes-empty-on-Ok" invariant is runtime-only. Each
planner has to remember to write `notes: Vec::new()` on the Ok
return, and each test that exercises an Ok path asserts
`report.notes.is_empty()` to pin the convention. The named struct
plus runtime invariant duplicates information the type system could
just enforce.

Replacing each `*PlanReport` with `Result<Plan,
PlanFailure<E>>` -- where `PlanFailure<E>` is a single generic
struct in `preview.rs` carrying notes alongside the error -- makes
the contract type-level: the Ok branch literally cannot carry
notes, so the runtime invariant disappears along with all the
attendant docstrings and assertions. The seven sibling structs
collapse into one shared type, the `err_empty` closures collapse
into a `PlanFailure::empty` constructor, and the
`std::mem::take(&mut notes)` sites collapse into
`PlanFailure::with_notes(notes, err)`.

This is the project-wide structural answer to a finding that
proposed the same shape change for `remove.rs` alone; doing it for
one module would break uniformity across the seven, so the right
scope is all of them.

Out of scope:

- `cli/src/mount.rs::PlanReport` -- carries `Vec<ProbeEvent>`, not
  notes. Different semantic model; consumed by `unlock` via an
  explicit `ProbeEvent::to_preview_note` conversion. Keep as-is.
- `cli/src/lock.rs` -- has no `*PlanReport`. `plan_lock` already
  returns `Result<LockPlan, LockError>` directly; notes live inside
  `LockPlan` (the planner never accumulates notes before a possible
  Err). Already on the target shape.

## Approach

### 1. Introduce `PlanFailure<E>` in `cli/src/preview.rs`

Add alongside the existing `PreviewNote` / `Preview` types:

```rust
/// Failure-side payload for `plan_xxx` functions. Carries accumulated
/// notes (preflight Info/Warn, busy-op diagnostics) so the caller can
/// render them to stderr before propagating the error. The Ok branch
/// of a planner Result never carries notes -- they live inside the
/// returned plan instead. This makes the "notes-on-Err only" contract
/// a type-level fact and replaces the seven *PlanReport wrappers that
/// previously encoded the same contract at runtime.
#[derive(Debug)]
pub struct PlanFailure<E> {
    pub notes: Vec<PreviewNote>,
    pub error: E,
}

impl<E> PlanFailure<E> {
    /// Pre-preflight failure -- the notes accumulator is still
    /// untouched, so no notes need to survive. Replaces the
    /// `err_empty` closures in the planners that use one.
    pub fn empty(error: E) -> Self {
        Self { notes: Vec::new(), error }
    }

    /// Post-preflight failure -- preflight diagnostics already
    /// accumulated and must reach the caller's stderr render.
    /// Replaces the `RemovePlanReport { notes: std::mem::take(&mut
    /// notes), result: Err(...) }` literal at every post-preflight
    /// return site.
    pub fn with_notes(notes: Vec<PreviewNote>, error: E) -> Self {
        Self { notes, error }
    }
}
```

`#[derive(Debug)]` is required so callers (especially tests) can use
`Result::expect` on the planner output -- `.expect(msg)` needs `E:
Debug`, which for our case is `PlanFailure<E>: Debug`, which requires
both `PlanFailure` and `E` itself to be `Debug`. Every per-command
error enum already derives `Debug` via `#[derive(Debug,
thiserror::Error)]`, so the bound is satisfied.

Do not add `impl<E> From<E> for PlanFailure<E>` -- the implicit
conversion would compete with `?` propagation chains involving
multiple `From` hops, and the explicit constructors read cleanly at
return sites.

### 2. Per-module rewrites (seven modules)

For each of the seven modules, apply the same three-part edit:

**a. Delete the `*PlanReport` struct definition and its doc.**

**b. Change the planner signature.**

```rust
// before
pub fn plan_remove<R, F>(...) -> RemovePlanReport { ... }
// after
pub fn plan_remove<R, F>(...) -> Result<RemovePlan, PlanFailure<RemoveError>> { ... }
```

**c. Rewrite return sites.**

Apply the rewrite rules below to **every** matching return site in
the module -- do not work from a per-module count. The exact tally
shifts as the codebase evolves; instead, the rule is exhaustive by
pattern. After the per-module edit, the file should contain zero
matches for `XxxPlanReport` and zero `err_empty` definitions.

| Old form | New form |
| --- | --- |
| `let err_empty = \|e\| XxxPlanReport { notes: Vec::new(), result: Err(e) };` | (delete the closure) |
| `return err_empty(XxxError::Variant(...));` | `return Err(PlanFailure::empty(XxxError::Variant(...)));` |
| `return err_empty(e.into());` | `return Err(PlanFailure::empty(e.into()));` |
| `Err(e) => return err_empty(e.into()),` | `Err(e) => return Err(PlanFailure::empty(e.into())),` |
| `return XxxPlanReport { notes: std::mem::take(&mut notes), result: Err(e) };` | `return Err(PlanFailure::with_notes(std::mem::take(&mut notes), e));` |
| `return XxxPlanReport { notes, result: Err(e) };` (direct move; `add.rs`-style) | `return Err(PlanFailure::with_notes(notes, e));` |
| `XxxPlanReport { notes: Vec::new(), result: Ok(plan) }` (final Ok) | `Ok(plan)` |

Use `rg "XxxPlanReport\|err_empty"` after the rewrite to confirm zero
remaining matches in the module. The seven planners differ in
boilerplate density (notably `recover.rs` and `add.rs` have many
more return sites than the others), but the rewrite rule is the
same everywhere.

**d. Update the consumer `cmd_xxx`.**

Six of the seven `cmd_xxx` functions today match this shape (using
the shared `preview::emit_notes_to_stderr` helper):

```rust
let report = plan_remove(runner, fs, params);
let plan = match report.result {
    Ok(p) => p,
    Err(e) => {
        preview::emit_notes_to_stderr(&report.notes, RemovePlan::STDERR_STYLE);
        return Err(e);
    }
};
```

Rewrite to:

```rust
let plan = match plan_remove(runner, fs, params) {
    Ok(p) => p,
    Err(PlanFailure { notes, error }) => {
        preview::emit_notes_to_stderr(&notes, RemovePlan::STDERR_STYLE);
        return Err(error);
    }
};
```

`replace.rs` is the **one exception**. `cmd_replace` and
`ReplacePlan::execute` deliberately route stderr through the
module-private wrapper `emit_replace_notes_to_stderr`
(`cli/src/replace.rs:765`), which forwards into the
`replace_stderr_capture` thread-local under `#[cfg(test)]` before
falling back to `eprint!`. The `replace`-only capture seam lets tests
assert byte-exact stderr output on `cmd_replace` failure paths
(`cli/src/replace.rs:4347`). Keep that wrapper intact and have the
new `Err(PlanFailure { notes, error })` arm in `cmd_replace` call
`emit_replace_notes_to_stderr(&notes)` instead of
`preview::emit_notes_to_stderr`. The same wrapper continues to be
used inside `ReplacePlan::execute`.

`remove_missing.rs` and `recover.rs` follow the standard
`preview::emit_notes_to_stderr` pattern (verify during impl).

### 3. Tests (~93 references across 7 modules)

Test code today destructures `report.notes` and `report.result`.
Rewrite the common shapes:

| Old | New |
| --- | --- |
| `let report = plan_remove(...); assert!(report.notes.is_empty()); let plan = report.result.expect("...");` | `let plan = plan_remove(...).expect("...");` (notes-empty-on-Ok is now type-enforced; the assertion goes away) |
| `let plan = report.result.expect("...");` followed by assertions on `plan.notes` | `let plan = plan_remove(...).expect("..."); ... plan.notes ...` |
| `match &report.result { Err(...) => ..., Ok(_) => panic!(...) }; assert_eq!(report.notes.len(), 1);` | `let PlanFailure { notes, error } = match plan_remove(...) { Err(f) => f, Ok(_) => panic!("expected Err, got Ok"), }; /* assert on error */ ; assert_eq!(notes.len(), 1);` |

**Bound caveat for the Err-path rewrite.** `Result::expect_err`
requires the `Ok` type to implement `Debug`. Today, four of the seven
`*Plan` structs do not derive it: `AddPlan` (`cli/src/add.rs:655`),
`RemovePlan` (`cli/src/remove.rs:92`), `ReplacePlan`
(`cli/src/replace.rs:73`), `RemoveMissingPlan`
(`cli/src/remove_missing.rs:79`). The other three (`UnlockPlan`,
`EnrollPlan`, `RecoverPlan`) already do.

Use the explicit `match` form shown above for Err-path tests in
**every** module rather than mixing `match` with `expect_err`. Do
not retrofit `#[derive(Debug)]` onto the four plans -- they contain
heterogeneous fields (e.g., embedded `Config`, `Vec<PoolDevice>`,
ownership snapshots) and the derive would either cascade across
unrelated modules or hit non-`Debug` fields. The explicit `match` is
uniform across modules and keeps the bound footprint contained to
`PlanFailure<E>` plus the already-`Debug` error enums.

No tests hand-construct a `*PlanReport` today, so no fixture changes
are needed beyond receiving sites.

### 4. Doc updates

Three places mention Shape A or `PlanReport` outside the source:

- `plans/impl/2026-04-24-dry-run-preview-refactor.md` -- references
  the `*PlanReport` shape and Shape A contract. Add a note pointing
  to this plan as the follow-up that collapses the wrappers into
  `PlanFailure<E>`.
- `plans/impl/2026-05-07-collapse-shape-a-stderr-emit-helper.md` --
  same: a follow-up note.
- Inline docstrings in the seven modules (struct docs, planner docs,
  and the "Shape A 'notes-carrying report'" comments). Most either go
  away (struct definitions deleted) or rewrite to say "notes survive
  on `PlanFailure::notes`".

No `AGENTS.md` or `docs/principles.md` updates required (neither
mentions Shape A or `*PlanReport`).

## Critical files

- `cli/src/preview.rs` -- add `PlanFailure<E>` (1 struct + 2
  constructors + 1 docstring).
- `cli/src/add.rs` -- delete `AddPlanReport`, rewrite `plan_add` +
  `cmd_add` + tests.
- `cli/src/remove.rs` -- delete `RemovePlanReport`, rewrite
  `plan_remove` + `cmd_remove` + tests.
- `cli/src/replace.rs` -- delete `ReplacePlanReport`, rewrite
  `plan_replace` + `cmd_replace` + tests.
- `cli/src/unlock.rs` -- delete `UnlockPlanReport`, rewrite
  `plan_unlock` + `cmd_unlock` + tests.
- `cli/src/enroll_key_file.rs` -- delete `EnrollPlanReport`, rewrite
  `plan_enroll_*` + `cmd_enroll_*` + tests.
- `cli/src/remove_missing.rs` -- delete `RemoveMissingPlanReport`,
  rewrite planner + cmd + tests.
- `cli/src/recover.rs` -- delete `RecoverPlanReport`, rewrite planner
  + cmd + tests (largest churn -- ~10 return sites).
- `plans/impl/2026-04-24-dry-run-preview-refactor.md` -- add
  follow-up pointer.
- `plans/impl/2026-05-07-collapse-shape-a-stderr-emit-helper.md` --
  add follow-up pointer.

## Reused infrastructure

- `preview::emit_notes_to_stderr` (`cli/src/preview.rs:195`) is
  unchanged. Six of the seven `cmd_xxx` consumers (`cmd_add`,
  `cmd_remove`, `cmd_unlock`, `cmd_enroll_key_file`,
  `cmd_remove_missing`, `cmd_recover`) continue to call it with the
  per-command `STDERR_STYLE` constant. The constants
  (`AddPlan::STDERR_STYLE`, `RemovePlan::STDERR_STYLE`, etc.) are
  unchanged.
- **Exception:** `cmd_replace` keeps using
  `emit_replace_notes_to_stderr` (`cli/src/replace.rs:765`), the
  module-private wrapper around `preview::render_notes_for_stderr_with`
  that feeds the `replace_stderr_capture` thread-local under
  `#[cfg(test)]`. The capture seam is what lets `replace` tests
  assert byte-exact stderr on failure paths; rerouting `cmd_replace`
  through `preview::emit_notes_to_stderr` would silently break those
  tests.
- `PreviewNote`, `PerDiskStyle`, `Preview::render` --
  unchanged.
- Per-module error enums (`RemoveError`, `AddError`, ...) --
  unchanged. `PlanFailure<E>` parameterizes over them. Each already
  derives `Debug` via `thiserror`, satisfying the
  `PlanFailure<E>: Debug` requirement for `.expect()` in tests.

## Verification

1. `just test-rust` -- runs `cargo test --lib --test
   golden_nixos_25_11 --test tty_guard`. The `--lib` build compiles
   the entire `braid-cli` library, including the seven rewritten
   planners and their consumers; every unit test (~220 total, ~93
   touching the report shape directly) executes. The type-level
   invariant means `report.notes.is_empty()` assertions can be
   deleted without weakening coverage.
2. `cargo build --bin braid` -- separate from `just test-rust`. The
   binary `cli/src/main.rs` calls only `cmd_xxx` (whose signatures
   are unchanged), so this is a sanity check, not a primary catch.
   Run it explicitly to confirm the binary still links cleanly.
3. `just test-vm` -- end-to-end coverage that planners + consumers
   still wire together correctly (planner output reaches preview,
   notes reach stderr in the right order on both success and
   failure paths).
4. Behavioral spot-check via `just test-vm braid-remove-softwarn
   braid-recover` -- exercises the two highest-value planners for
   this refactor. `braid-remove-softwarn`
   (`flake.nix:475`) covers the eviction soft-warn path where a
   `PreviewNote::Warn` rides through the Ok branch into
   `plan.notes`; `braid-recover` (`flake.nix:425`) covers the
   planner with the most return sites and the broadest notes-on-Err
   surface. A regression in note-passing surfaces here.
5. `git grep "PlanReport"` after the rewrite -- should match only
   `cli/src/mount.rs::PlanReport` (deliberately out of scope) and
   any historical reference in `plans/impl/`. Zero matches in the
   seven rewritten modules.
6. `git grep "err_empty"` after the rewrite -- should return zero
   matches in `cli/src/`. The closure is removed everywhere it
   appeared.

## Out of scope / non-goals

- No public CLI behavior change -- error texts, stderr render order,
  dry-run stdout layout, and exit codes are unchanged.
- No changes to `*PlanError` enums or to `*Plan` structs themselves.
- No unification of the `*Plan` work-plan structs (those carry
  different per-command state; only the report wrapper is shared).
- `mount.rs::PlanReport` (events-based) and `lock.rs` (no wrapper)
  remain as they are.
