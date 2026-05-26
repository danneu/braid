# Fix: stop computing a dead, divergent enroll step list on the real-run path

## Context

`plan_enroll` (`cli/src/enroll_key_file.rs:618-685`) builds an `EnrollPlan`
that serves two phases:

- **dry-run** -- renders `notes` + `steps` via `preview()` (`:442-448`,
  reached only from `cmd_enroll_key_file`'s `if params.dry_run` branch at
  `:713-716`).
- **real-run** -- `execute()` (`:450-501`) reads `notes`, `candidates`, and
  `generate`, then re-plans through `plan_enrollment` + `apply_enrollment`. It
  never reads `self.steps`.

The dry-run branch (`:650-674`) classifies each candidate (passphrase-free) and
compiles steps from the **filtered** `needs_enroll` subset, routing
`AlreadyEnrolled` disks to skip notes. The real-run `else` branch (`:675-677`)
instead compiles steps from the **unfiltered** `candidates`:

```rust
} else {
    compile_enroll_steps(&candidates, key_file_path, generate, paths)
};
```

That result is never read. The work is dead, and `steps` carries two different
meanings depending on construction path -- "filtered preview list" in dry-run,
"unfiltered, never-read list" in real-run -- with no compiler or test guard on
the divergence. The latent footgun: a future reader who wired `self.steps` into
the real-run execute path would silently re-enroll already-enrolled disks
(re-adding slot-1 keyfiles + header backups for `AlreadyEnrolled` members).

**Outcome:** `steps` has one meaning -- "the preview step list" -- and the
unused unfiltered compile is gone. Pure simplification; no behavior change.

### Verified facts (this investigation)

- `execute()` reads `self.notes` (`:458`), `self.candidates` (`:473`), and
  `self.generate` (`:462`, `:479`) -- never `self.steps`. Generate-mode key
  creation is handled directly by `execute()` (`:479-483`), independent of
  steps.
- `preview()` is the only reader of `self.steps` (`:446`); in production it is
  called only on the dry-run branch (`:714`).
- `compile_enroll_steps` is only called from the two `plan_enroll` branches and
  from its own direct unit tests (`:3682`, `:3710`). Empty `steps` already
  renders as `nothing to do.` (`dry_run_all_already_enrolled_emits_zero_steps`,
  `:1824-1865`).
- Existing test `execute_generate_partial_failure_reports_recovery_hint`
  (`:3150-3158`) already constructs `EnrollPlan { steps: vec![], .. }` and runs
  `execute()` successfully -- direct proof real-run execution does not depend on
  `steps`.

## Approach

Set the real-run branch to `Vec::new()` and document `steps` as a preview-only
artifact. Chosen over the alternatives:

- **Rejected -- `steps: Option<Vec<Step>>` (None on real-run):** test
  `plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr` (`:1166`)
  calls `plan.preview()` on a `dry_run=false` plan, so `preview()` would have to
  handle `None` at runtime -- a new panic path for no compile-time gain.
- **Rejected -- split into separate preview/execute types:** all nine sibling
  `*Plan` structs (`add.rs`, `replace.rs`, `recover.rs`, ...) use one type that
  both previews and executes. Splitting only `EnrollPlan` diverges from the
  established convention for no real benefit; the dual `steps`/`candidates`
  design is intentional (real-run must defer classification until after the
  passphrase verify -- see the `:467-470` comment).

## Changes (all in `cli/src/enroll_key_file.rs`)

1. **Real-run branch (`:675-677`)** -- replace the `compile_enroll_steps(&candidates, ..)`
   call with `Vec::new()`, plus a one-line comment stating steps are a
   dry-run/preview-only artifact and the real run re-plans from `candidates`
   after the passphrase verify. This comment is the guard against the
   future-reader footgun (a behavioral test here would have to assert on a
   never-read field, so it would be structure-sensitive -- not added).

2. **`plan_enroll` doc comment (`:614-617`)** -- the sentence "Real-run path
   (`dry_run = false`) leaves every discovered candidate in the step list and
   defers classification to `plan_enrollment` at execute time after the
   passphrase prompt" now describes deleted behavior. Reword to: real-run leaves
   `steps` empty (preview-only) and defers classification to `plan_enrollment`
   at execute time.

3. **`steps` field doc (`:429`)** -- add a one-line `///` recording the invariant
   that is not recoverable from the `Vec<Step>` type: preview-only; empty on
   real-run plans, which re-plan from `candidates`. (The struct-level doc at
   `:422-425` is already accurate and stays as-is.)

## Out of scope / no change

- Test `plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr`
  (`:1166-1203`) asserts `.contains("[skip] disk disk1: not present\n")` on the
  preview and an exact stderr-note string -- neither touches steps, so it keeps
  passing with empty real-run steps. Left unchanged to keep the diff minimal.
- No new test. Existing coverage (the `steps: vec![]` execute test at `:3150`
  and the dry-run filtered-steps tests) already pins the behavior that matters.

## Verification

Pure Rust logic change, no systemd/mount/lock blast radius, so VM tests are not
required.

1. `just test-rust` -- runs `cargo test` for `braid-cli`. Confirm the
   `enroll_key_file` test module passes, especially:
   - `dry_run_all_already_enrolled_emits_zero_steps` (dry-run filtered steps
     unchanged),
   - `dry_run_with_generate_skips_probe` (dry-run generate steps unchanged),
   - `execute_generate_partial_failure_reports_recovery_hint` (real-run execute
     from empty steps),
   - `plan_skip_note_renders_bracketed_in_preview_and_plain_in_stderr`
     (preview note rendering unchanged).
2. Sanity-check that `cargo build` emits no dead-code/unused-import warnings for
   `compile_enroll_steps` (still used by the dry-run branch and its unit tests).
