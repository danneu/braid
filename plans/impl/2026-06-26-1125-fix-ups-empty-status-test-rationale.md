# Fix the empty-UPS-status sentinel test rationales

## Context

Two tests in `cli/src/ups.rs` pin the empty-status sentinel string
`(unknown -- ups.status missing)` with exact-match assertions:

- `format_status_empty_is_unknown` (`cli/src/ups.rs`, comment block ~437-440)
- `format_human_empty_status_renders_sentinel` (`cli/src/ups.rs`, comment block ~721-725)

Their `// Why it exists` preambles justify the verbatim pin by claiming the
exact text is needed "so the doctor/preflight referral (`Check 'braid ups
status'`) stays actionable." That coupling does not exist in code:

- The preflight referral (`cli/src/preflight.rs#check_ups_not_on_battery`) is a
  fixed string built from its own hardcoded context (`refuse("ups.status is
  empty or missing")`); it never reads, parses, or reproduces the sentinel.
  Changing the sentinel cannot make it unactionable.
- The doctor empty-status arm (`cli/src/doctor.rs#check_ups_daemon_up`) emits
  `"upsc ... responded but ups.status is empty -- driver may still be starting"`
  and does **not** refer operators to `braid ups status` at all -- so the
  "doctor points at `braid ups status`" half is simply false.
- The genuinely-accurate articulation already lives at
  `cli/src/parse/upsc.rs` (the `empty_status_value_produces_no_flags`
  rationale): an empty flag set feeds the `ups_status_empty` JSON warning and
  the `(unknown -- ups.status missing)` human sentinel.

The pin itself is correct and worth keeping -- it is an output-stability
snapshot of an operator-facing string. Only the stated rationale is wrong, and
a wrong rationale actively misleads the next maintainer into thinking the two
strings are linked. The fix is to reword the two preambles to state the real
guarantee.

## Scope decision

**Code comments only.** The same false claim is repeated in dated impl-plan
records (`plans/impl/2026-05-14-ups-empty-status-sentinel.md` x2,
`plans/impl/2026-06-17-unify-ups-severity-classification.md` x1). Those are
deliberately left untouched:

- `plans/` is not in braid's authority chain (`principles.md` -> `decisions/`
  -> `internals/` -> `docs/`); nobody consults it for current truth.
- braid already treats dated records as frozen (Superseded/Deprecated ADRs,
  `## See` sections). A date-stamped plan is a point-in-time snapshot; rewriting
  `2026-05-14-...md` would make it a *less* faithful record of what was
  committed that day. Leaving it, while git history shows the code comment
  corrected later, is the honest end state.
- The flawed reasoning in those plans drove no incorrect code (the 2026-06-17
  conclusion -- leave empty-status untagged because it self-describes -- is
  correct), so there is nothing downstream to fix.

The VM canary `tests/cli/braid-status-ups.py` asserts the sentinel with a
correct rationale ("whole line in human output") and needs no change. Docs and
README contain no occurrence of the false coupling.

## The change

Comment-only edits to `cli/src/ups.rs`. No production code, no assertions, no
test names change. `Intent` and `Scenario` lines stay; only `Why it exists` is
rewritten.

### Test 1 -- `format_status_empty_is_unknown`

Replace the current `Why it exists` (the "must read verbatim so the
doctor/preflight referral ... stays actionable" clause) with a rationale that
names the real source and the sibling surface:

```rust
    // Intent: format_status returns the literal sentinel
    // `(unknown -- ups.status missing)` for an empty flag set.
    // Why it exists: an empty flag set is the parser's empty-status case
    // (see parse/upsc.rs); this is its operator-facing rendering, paired
    // with the machine-facing `ups_status_empty` JSON warning. The
    // exact-match pin guards that rendering against silent degradation --
    // a refactor returning `(unknown)`, `unknown status`, or a blank line
    // would satisfy a substring check yet drop the cause an operator needs.
    // Scenario: dummy-ups fixture with no ups.status line yet.
```

### Test 2 -- `format_human_empty_status_renders_sentinel`

Keep the real UX context (this is the line an operator lands on after
preflight refers them to `braid ups status`) but explicitly correct the
misconception and drop the false doctor reference:

```rust
    // Intent: format_human emits exactly the line
    // `Status: (unknown -- ups.status missing)` when status_flags is empty.
    // Why it exists: this is the line an operator lands on after preflight
    // refuses an empty-ups.status mutation and refers them to `braid ups
    // status` (preflight.rs). The referral itself is a fixed string and
    // does not depend on this wording; what the exact-match pin protects is
    // the landing spot staying self-explaining instead of degrading to a
    // bare `(unknown)`. Snapshots otherwise cover only non-empty flag sets.
    // Scenario: dummy-ups driver published telemetry before populating ups.status.
```

Wording stays ASCII (`--`, straight quotes, `...`) per project style, even
though comments are exempt from `check-output-ascii.py`.

## Verification

This is a comment-only change; behavior cannot change. Confirm nothing else
drifted:

- `just test-rust` -- the full Rust suite still passes. To run only the two
  pinned tests, use two invocations (cargo accepts a single `[TESTNAME]`
  positional; a second one errors with "unexpected argument"):

  ```sh
  cargo test -p braid-cli --lib format_status_empty_is_unknown
  cargo test -p braid-cli --lib format_human_empty_status_renders_sentinel
  ```
- `cargo build -p braid-cli` -- compiles (guards against a malformed comment
  block / stray backtick).
- Manual read-through: each reworded preamble's `Why it exists` no longer
  claims the referral depends on the sentinel text, and no longer asserts that
  doctor refers operators to `braid ups status`.
- `rg "stays actionable|would leave that referral unactionable" cli/src/` --
  returns nothing (both false-claim phrasings removed from live code).

## Out of scope

- `plans/impl/*.md` repetitions (frozen historical records -- see Scope
  decision).
- Adding a doc comment to `format_status` itself, or documenting the sentinel
  in `docs/` -- the finding is about test-rationale accuracy, not new docs.
- Merging or restructuring the two tests -- they pin different layers (the
  helper string vs. the integrated `Status:` line) and both are worth keeping.
