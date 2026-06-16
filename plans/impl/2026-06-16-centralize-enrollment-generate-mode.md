# Plan: centralize the `--generate` -> `EnrollmentPlanMode` mapping

## Context

`EnrollmentPlanMode` (`cli/src/enroll_key_file.rs:55-65`) is a two-variant enum
(`ExistingKeyfile` / `GenerateNew`) that decides whether the per-disk planner
runs the keyfile probe. It is a pure function of the `--generate` bool, but that
mapping -- `if generate { GenerateNew } else { ExistingKeyfile }` -- is open-coded
at **two** production sites that sit on opposite sides of the dry-run / real-run
boundary:

- `EnrollPlan::execute` (`cli/src/enroll_key_file.rs:539-543`) -- real run,
  derives from `self.generate`.
- `plan_enroll` dry-run arm (`cli/src/enroll_key_file.rs:758-762`) -- preview,
  derives from `params.generate`.

Both ultimately map the *same* bool (`EnrollPlan.generate` is copied from
`params.generate` at `:802`), through the *same* expression, in two places. This
module deliberately separates plan-time classification from execute-time
re-planning (`:792-794`: "Steps are a dry-run/preview-only artifact; real
execution re-plans from candidates after passphrase verification"). A drift
between these two copies would be exactly the preview-vs-real classification
divergence the module works to prevent. Centralizing the mapping into one named
constructor removes the possibility of drift and reads more clearly at both call
sites.

This is a pure simplification: the new function is the current expression hoisted
verbatim into one place. No control flow changes; behavior is identical.

## Approach

Add a single named constructor and call it at both derivation sites.

### 1. Add `EnrollmentPlanMode::from_generate`

Immediately after the enum definition (after `cli/src/enroll_key_file.rs:65`), add:

```rust
impl EnrollmentPlanMode {
    /// Single source of truth for the `--generate` -> mode mapping:
    /// `true -> GenerateNew` (keyfile does not exist yet, skip the probe),
    /// `false -> ExistingKeyfile` (probe the on-disk keyfile). Both the
    /// dry-run preview (`plan_enroll`) and the real run (`EnrollPlan::execute`)
    /// route through here so the two paths can never classify a disk
    /// differently.
    fn from_generate(generate: bool) -> Self {
        if generate {
            Self::GenerateNew
        } else {
            Self::ExistingKeyfile
        }
    }
}
```

This matches the established house pattern for boolean/Result -> enum classifiers,
e.g. `ImmutabilityProbe::from_result` (`cli/src/doctor.rs:1512-1516`) and the
broader `from_*` constructor convention (`types.rs` `from_basename`, `lock.rs`
`from_classified`, `secret.rs` `from_zeroizing`, `tui/model.rs` `from_membership`).

**Visibility: private (`fn`), not `pub(crate)`.** Both callers live in
`enroll_key_file.rs` (`EnrollPlan::execute`, `plan_enroll`), and the only
cross-module users of the enum (`add.rs:2099`, `replace.rs:1481`) pass a literal
`ExistingKeyfile` -- they have no `generate` bool, so they never call this
constructor. A private associated function matches the actual caller scope and
adds no unused crate API surface. The enum stays `pub(crate)` (add/replace
reference its variants); only the constructor is private. The `///` is kept for
intent even though the doc-comment rule only mandates one for `pub`/`pub(crate)`
items -- it documents the single-source-of-truth invariant, not a crate boundary.

**Named constructor, not `From<bool>`:** the finding offered `From<bool>` as an
alternative; reject it. `EnrollmentPlanMode::from(true)` is opaque about which arm
`true` selects, whereas `from_generate(self.generate)` is self-documenting and
fits the project's explicit, construct-safe-type direction (recent commits
`aed384ef`, `02d03253`, `d83b2108`).

### 2. Replace both derivation sites

`EnrollPlan::execute` (`:539-543`):

```rust
let mode = EnrollmentPlanMode::from_generate(self.generate);
```

`plan_enroll` dry-run arm (`:758-762`):

```rust
let mode = EnrollmentPlanMode::from_generate(params.generate);
```

## Out of scope (deliberately unchanged)

- **`EnrollPlan.generate: bool` stays a bool.** It independently gates keyfile
  *creation* and partial-recovery error wording (`:556`, `:579`), not just the
  probe-skip, so it is not redundant with `mode` and should not be replaced by a
  stored `EnrollmentPlanMode`. The right altitude is hoisting the *mapping*, not
  changing the stored field.
- **`add.rs:2099` and `replace.rs:1481`** pass `ExistingKeyfile` as a literal --
  those commands have no `--generate` flag, so they are not part of this pattern
  and need no change. (This is why the finding's "three places" is really two.)
- **Tests** construct `EnrollmentPlanMode` variants directly; none derive from a
  bool, so no test changes are needed. The new constructor is additive.

## Verification

- `just test-rust` -- the existing `enroll_key_file` unit tests cover both
  `GenerateNew` and `ExistingKeyfile` planning paths
  (`cli/src/enroll_key_file.rs` tests at `:2673`, `:2734`, `:2807`, `:2856`,
  `:2927`, plus the `ExistingKeyfile` real-run/dry-run cases). Since behavior is
  unchanged, they must stay green with no edits.
- `cargo clippy` (workspace) -- confirm no new warnings; the hoisted `if/else`
  should read clean.
- **Structural verification** (proves what `just test-rust` cannot: that no
  open-coded mapping survives). Per `docs/dev/planning-hygiene.md`, sweep tracked
  files with `git ls-files` + `rg` rather than a bare recursive `grep` -- two
  checks:
  - *Positive -- exactly the two callers exist:*
    `git ls-files 'cli/src/*.rs' | xargs rg -n 'EnrollmentPlanMode::from_generate\('`
    must return exactly 2 hits, both in `cli/src/enroll_key_file.rs` (the
    `EnrollPlan::execute` and `plan_enroll` sites). The `fn from_generate(`
    definition is intentionally excluded: callers qualify with the
    `EnrollmentPlanMode::` prefix, the definition does not.
  - *Negative -- no old open-coded mapping remains:*
    `git ls-files 'cli/src/*.rs' | xargs rg -U -n 'if\s+[\w.]*generate\s*\{\s*EnrollmentPlanMode::GenerateNew'`
    must return 0 hits. Validated to match both current blocks
    (`enroll_key_file.rs:539`, `:758`) before the edit, so a zero result is real
    proof they were removed, not a vacuous pass.

  A bare `grep "GenerateNew\|ExistingKeyfile"` is deliberately *not* used: it also
  matches doc-comment mentions (e.g. `credential_verify.rs`), the enum defs, the
  constructor's `Self::*` arms, test fixtures, and the add/replace literals, so it
  can never reduce to a clean signal.

No new test is added. Existing dry-run and execute tests already exercise both
generate and non-generate modes; an inverted constructor would trip their
missing-mock / request-log assertions. A pure `from_generate(true|false)` mapping
test would only pin implementation shape, so it is deliberately omitted.
