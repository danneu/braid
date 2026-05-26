# Fix: replace unreachable `journal_targets.is_empty()` no-op with a fail-closed invariant

## Context

`AddPlan::execute()` has a post-Pass-1 branch (`cli/src/add.rs:1146-1150`) that
disarms the LUKS cleanup guard, prints `format_add_noop`, and returns `Ok(())`
when `journal_targets` is empty. This branch is **unreachable**, and worse, it
encodes the opposite of what the directly-coupled `.expect()` 300 lines down
already asserts.

Proof of unreachability:
- `execute()` returns at `add.rs:970` when `work_plan.is_noop()`, which is
  `self.targets.is_empty()` (`add.rs:567-569`). Past that point `targets` is
  non-empty.
- `journal_targets` is cloned from `initial_journal_targets` (`add.rs:1047`) and
  only ever grows -- the sole `insert` is at `add.rs:1129`; it is never cleared.
- `initial_journal_targets` gets one entry per `Fresh` and per `OpenRecoverable`
  target at planning time (`add.rs:2002`, `add.rs:2067`).
- Every `ClosedPresentLuks` target either returns `Err`
  (`NoBtrfs`/`ForeignPool` via `identity_to_error`, `add.rs:1105`) or reaches
  `SamePool` and inserts (`add.rs:1129`). `AddLuksBtrfsProbe` has exactly those
  three variants (`add.rs:110-117`).
- Therefore, whenever `targets` is non-empty, `journal_targets` is non-empty at
  `add.rs:1146`. The branch cannot fire on any real input.

Two further problems make this worth fixing rather than leaving:
1. **Contradictory models.** The `.expect()` at `add.rs:1447-1451` justifies
   itself with "*journal_targets.is_empty() short-circuits earlier*" and treats
   the empty case as impossible. Line 1146 instead treats it as a friendly
   no-op. The codebase holds two opposite mental models of one state.
2. **Mis-classified cleanup.** The branch calls `luks_guard.disarm()` -- the
   *success-path* action per the guard's contract (`add.rs:363-365`) -- for a
   condition that, if it ever occurred, is an internal accounting bug.
3. **Redundant message.** `format_add_noop` already owns the legitimate no-op
   wording via the planning-time `PreviewNote::Info` at `add.rs:1778` (rendered
   by `emit_notes_to_stderr` at `add.rs:964`). Line 1148 is a second call site
   future readers must reconcile.

Goal: make the impossibility explicit and fail-closed before the journal write,
delete the redundant no-op exit, and bring line 1146 into agreement with the
line-1447 invariant.

## The change (`cli/src/add.rs`)

### 1. Replace the unreachable branch

Replace the branch at `add.rs:1146-1150`:

```rust
if journal_targets.is_empty() {
    luks_guard.disarm();
    eprintln!("{}", format_add_noop(&self.names));
    return Ok(());
}
```

with a fail-closed invariant:

```rust
// A non-empty work plan must yield >=1 journal target: is_noop()
// (targets.is_empty()) already returned at the top of execute(), and every
// surviving target either inserts into journal_targets (Fresh/OpenRecoverable
// at planning, ClosedPresentLuks SamePool above) or returns Err. Empty here is
// an internal accounting bug -- fail closed before the journal write instead
// of falling through. The downstream pool_after .expect() relies on this.
if journal_targets.is_empty() {
    return Err(AddError::Validation(
        "add work plan has targets but produced no journal targets after \
         identity verification"
            .into(),
    ));
}
```

Specifics:
- **Drop `luks_guard.disarm()`.** This is now an error path; leaving the guard
  armed lets its `Drop` close any Pass-1-opened mappers best-effort, per the
  guard contract (`add.rs:363-365`). In practice nothing is tracked when this is
  reached, so `Drop` is a no-op -- but the code now expresses "error path", not
  "success no-op".
- **Drop the `eprintln!(format_add_noop(...))` call.** Do **not** remove the
  `format_add_noop` function -- it stays used at `add.rs:1778`.
- **Message style:** bare factual contradiction naming the stage, matching the
  existing internal-invariant returns at `add.rs:1111`, `:1228`, `:2041`. There
  is no `"internal error:"`/`"BUG:"` prefix convention in this codebase; do not
  introduce one.

### 2. Update two now-stale comments (same file, same commit)

Deleting the real-run `eprintln!(format_add_noop(...))` invalidates two comments
that describe a real-run `eprintln!` path. After the change, `format_add_noop`
has a *single* call site (`add.rs:1778`) that builds the planning-time
`PreviewNote::Info`; dry-run renders that note via `Preview::render` and real-run
emits the *same* note via `emit_notes_to_stderr` (`add.rs:964`). Reword both
comments to describe that single-source flow:

- **`format_add_noop` doc comment (`add.rs:890-892`).** Currently: "Shared by the
  dry-run `PreviewNote::Info` and the real-run stderr `eprintln!` so both paths
  see byte-identical wording." Reword to: builds the planning-time
  `PreviewNote::Info`; dry-run renders it via `Preview::render` and real-run emits
  it via `emit_notes_to_stderr`, so both channels see byte-identical wording from
  one source.
- **Planning no-op comment (`add.rs:1771-1776`).** Currently ends "...matching
  real-run's `eprintln!("Nothing to do -- ...")` wording via the shared
  `format_add_noop` helper." Reword the trailing clause to: real-run emits this
  same Info note via `emit_notes_to_stderr`, so dry-run and real-run share one
  `format_add_noop` source (no separate real-run `eprintln!`).

## Why `AddError::Validation` (return), not `unreachable!`/`expect` (panic)

This area is *not* a uniform "always return" idiom -- it mixes both styles.
Nearby invariants panic (`unreachable!` at `add.rs:1142` and `:2077`;
`pool_after.expect(...)` at `add.rs:1447`), while others return
(`AddError::Validation` at `add.rs:1110-1113`, `:1228`, `:2041-2045`). So the
choice is decided on this site's merits, not a blanket convention:

- **Post-mutation cleanup (the deciding reason):** this check sits *after* the
  Pass-1 LUKS opens and *before* the journal write and the sleep-inhibitor
  acquisition (`add.rs:1174`). Returning `Err` lets `LuksCleanupGuard::Drop`
  close any opened mappers best-effort (guard contract, `add.rs:363-365`) and
  routes through `cmd_add`'s normal `AddError` rendering. A panic is the wrong
  tool at a command boundary that already models its internal failures as
  `AddError`.
- **Carrier:** `AddError::Validation(String)` is the variant the other
  *returned* internal invariants here already use (`add.rs:1110`, `:1228`,
  `:2041`); enum at `add.rs:42-100`.
- **Rule note:** the residual-invariant rule (AGENTS.md) forbids only
  `debug_assert!`; both `Err` and `expect`/`unreachable!` are hard errors in all
  builds, so the rule does not pick between them -- the cleanup reason above
  does. (The original finding rejected `expect` by citing this rule, which is
  inaccurate; the conclusion -- prefer the returned error -- still holds, for the
  cleanup reason.)

## Tests

**No new test for the invariant firing.** The condition is unreachable by any
real input; triggering it would require hand-constructing an inconsistent
`AddWorkPlan` (non-empty `targets`, empty `initial_journal_targets`, zero
`ClosedPresentLuks`) and calling `execute()` -- a white-box, structure-sensitive
test of an impossible state, which the project's test rubric says not to demand.

**Existing tests are the regression guard; they must pass unchanged:**
- `no_journal_on_noop_add` (`add.rs:5810`) -- pins the observable no-op contract:
  a real-run no-op add succeeds with no journal written and the sleep inhibitor
  never acquired (inhibitor acquired at `add.rs:1174`, journal written after).
  Note this does *not* by itself prove the return happened at `add.rs:970` rather
  than the old line-1146 branch -- both precede the inhibitor and journal write --
  so treat it as the end-to-end no-op contract guard, not proof of the
  short-circuit point.
- Planning-time short-circuit is covered by the work-plan no-op tests:
  `plan_add_already_in_pool_is_note_only_success` (`add.rs:8309`) pins zero steps
  + the "Nothing to do -- ..." Info note, and
  `add_open_present_luks_same_uuid_same_backing_drift_noops` (`add.rs:9054`)
  asserts `work_plan.is_noop()` and empty `initial_journal_targets`
  (`add.rs:9087-9091`). These prove an already-in-pool scenario yields
  `is_noop()`, which is what routes `execute()` to the line-970 return.
- `format_add_messages_pin_disk_name_list_and_grammar` (`add.rs:8279`) -- pins
  `format_add_noop` output; stays valid because the function remains used at
  `add.rs:1778`.

## Files

- `cli/src/add.rs` -- three edits, all in this file: (1) replace the branch at
  `add.rs:1146-1150`; (2) reword the `format_add_noop` doc comment
  (`add.rs:890-892`); (3) reword the planning no-op comment (`add.rs:1771-1776`).
  No other files.

## Verification

- `just test-rust` -- runs the CLI unit tests, including the three above. Expect
  all green; no behavioral change on any real input.
- `cargo build` (or `just`-equivalent) clean -- confirm no dead-code/unused
  warnings. Spot-checks: `format_add_noop` still used at `add.rs:1778`;
  `luks_guard.disarm()` still called on the success path at `add.rs:1364`;
  `self.names` is a struct field used elsewhere (`format_add_done` path).
- No VM test required: pure-Rust, no real-input behavior change (the branch is
  unreachable and the other two edits are comment rewords), localized to one
  file. Per AGENTS.md, scope tests to the touched path for small localized
  changes.
