# Fix recover.md basic example + pin banner literal

## Context

The "Basic example" output block in `docs/commands/recover.md:21-29` shows
three strings that `braid recover` never emits, so an operator comparing
real output to the doc would think recovery misbehaved. The drift was
introduced by an incremental shift in `cli/src/recover.rs` that the
existing test net did not catch:

- The entry banner formats the op kind with `{:?}` over a lowercase
  `&'static str` (`cli/src/recover.rs:1185-1191`, `journal_op_label`
  at `cli/src/recover.rs:1861-1868`). Real output is
  `Recovering from interrupted "add" operation (started ...)...` --
  quoted lowercase -- not `Recovering from interrupted Add operation ...`.
- `note: target membership achieved -- the interrupted operation
  completed before the crash.` is fabricated; the real success message
  comes from `recovery_guidance` at `cli/src/recover.rs:3576-3611` and
  emits `note: add completed -- 'wdc' now in the pool.`
- `pool.json written from live pool state.` is the GenericLivePool path
  wording (`cli/src/recover.rs:1152`); the existing-pool add path
  actually emits `pool.json written from completed add membership.`
  (`cli/src/recover.rs:2591` or `:2612`) and then
  `pool.json written from committed add membership.`
  (`cli/src/recover.rs:2271`), in that order, as recovery transitions
  from `PoolMutation` to `PostAddBalanceRaid1`.

The VM test at `tests/cli/braid-recover.py` only substring-asserts the
`"Recovering from interrupted"` prefix (lines 191, 207, 244, 267, 301,
397), and no Rust unit test pins `format_recover_entry`'s exact output.
`recovery_guidance` and the `pool.json written from ...` lines are
pinned (`recovery_guidance` literals at `cli/src/recover.rs:14572-14757`,
write-line literals reachable from VM tests), so the unguarded site is
specifically the banner format.

Scope is isolated: a sweep of `docs/commands/*.md`, `docs/guides/*.md`,
and `README.md` found no sibling fenced output blocks with the same
class of drift.

## Plan

### 1. Update the doc example to byte-accurate strings

File: `docs/commands/recover.md:21-29`

Replace the three drifted lines and add the second pool.json write line
that the real existing-pool add recovery emits. Keep the membership
summary lines unchanged -- they already match `cli/src/recover.rs:1137-1139`.

New block (intended literal):

```
Recovering from interrupted "add" operation (started 2026-03-15T14:30:00Z)...
  pre-operation membership:  {"ironwolf", "toshiba"}
  target membership:         {"ironwolf", "toshiba", "wdc"}
  recovered (live pool):     {"ironwolf", "toshiba", "wdc"}
note: add completed -- 'wdc' now in the pool.
pool.json written from completed add membership.
pool.json written from committed add membership.
pending-op.json cleared. Recovery complete.
```

Changes vs. current:
- L22 `Add operation` -> `"add" operation` (quoted lowercase).
- L26 `note: target membership achieved -- ...` ->
  `note: add completed -- 'wdc' now in the pool.`
- L27 `pool.json written from live pool state.` ->
  two lines: `... completed add membership.` then
  `... committed add membership.`

No other edits to `recover.md` are required.

### 2. Pin the banner literal with a Rust unit test

File: `cli/src/recover.rs` (test module containing the existing
`guidance_*` tests near line 14572).

Add one `#[test]` that constructs a synthetic `Journal` with
`OpKind::Add { ... }` and a fixed `started_at` and asserts:

```rust
// Intent: pin the `braid recover` entry-banner literal so the
//   `{:?}` formatting of the lowercase op label cannot drift
//   silently from what `docs/commands/recover.md` shows.
// Why it exists: docs/commands/recover.md previously claimed
//   `Recovering from interrupted Add operation ...` while the
//   real output was `Recovering from interrupted "add" operation
//   ...` (quoted lowercase). The VM substring assertion at
//   tests/cli/braid-recover.py only checks the `"Recovering from
//   interrupted"` prefix, so the drift went unnoticed until a
//   doc audit caught it.
// Scenario: format a journal for each of the four op kinds and
//   compare against the exact stderr line a real recover run
//   prints to operators.
#[test]
fn format_recover_entry_pins_banner_for_each_op_kind() {
    // assert_eq! on Add, Remove, RemoveMissing, Replace --
    // exact strings including the quoted lowercase op label.
}
```

Use the literal preamble form from `docs/dev/testing.md:11-22`
(`// Intent:`, `// Why it exists:`, `// Scenario:` -- contiguous
`//` line comments directly above the `#[test]`). Pin all four op
kinds in one test rather than four separate `#[test]`s -- the
mapping table is small and the failure message stays clear.

Reuse the existing helpers in the same test module that build journal
fixtures (the `guidance_*` tests already construct `OpKind` values --
copy that pattern). Do not introduce a new helper or builder.

The `note: ...` line is already pinned by
`guidance_add_completed` (`cli/src/recover.rs:14579-14593`); no
additional unit-test guard is needed for that line.

### 3. Pin the add `pool.json` write lines in the existing VM test

File: `tests/cli/recover-add-mixed-batch.py`, inside the
`subtest("Recover mixed-batch add")` block at lines 214-231 where
`/tmp/recover.err` is already captured and asserted on (the existing
block at line 186 builds the journal write-out, not the err
assertions).

Add three assertions next to the existing `soft_replay_wait` /
`soft_replay_ok` ordering check, following the same idiom:

```python
completed_line = "pool.json written from completed add membership.\n"
committed_line = "pool.json written from committed add membership.\n"
assert completed_line in err, (
    "completed-add pool.json line missing, got: " + repr(err)
)
assert committed_line in err, (
    "committed-add pool.json line missing, got: " + repr(err)
)
assert err.find(completed_line) < err.find(committed_line), (
    "completed-add line must precede committed-add line, got: " + repr(err)
)
```

Reuse the assertion style at `tests/cli/recover-add-mixed-batch.py:223-228`
(plain `assert ... in err` plus `err.find(...) < err.find(...)`,
string concatenation rather than f-strings to keep the build-time
f-string-without-placeholders linter happy per
`docs/dev/testing.md:58-62`). No new helpers.

This is the right test layer for these two lines because they are
emitted only during a real recover run (the existing-pool add path is
not reachable from a Rust unit test without standing up LUKS, a btrfs
pool, and a journal on disk). The mixed-batch test already drives
exactly the `Add::PoolMutation` -> `PostAddBalanceRaid1` transition
that emits both lines, so the new assertions ride alongside the
existing soft-balance ordering check at zero extra VM cost.

### 4. Out of scope

- Do not strengthen `tests/cli/braid-recover.py` to assert the full
  banner literal. The Python test runs in a NixOS VM and the
  `started_at` timestamp is dynamic; a substring assertion is the right
  shape there. The Rust unit test in step 2 is sufficient for the
  banner.
- Do not add a doc-snapshot test that re-renders the markdown example
  from code. The example is a teaching aid, not a contract; adding a
  generator is over-engineering for a 7-line doc block.
- Do not touch other command docs -- the audit found no sibling drift.

## Files modified

- `docs/commands/recover.md` -- three line edits inside the fenced
  example block plus one additional line (the second pool.json write).
- `cli/src/recover.rs` -- one new test (with four assertions covering
  every `OpKind`) co-located with the existing `guidance_*` tests,
  including the `// Intent: / // Why it exists: / // Scenario:`
  preamble.
- `tests/cli/recover-add-mixed-batch.py` -- three new assertions
  inside the existing `subtest("Recover mixed-batch add")` block
  pinning both add-recovery `pool.json written from ...` lines and
  their ordering.

## Verification

1. `just test-rust` -- the new banner-literal test must pass.
2. `just test-vm recover-add-mixed-batch` -- the existing VM test
   must still pass with the new assertions on the two add-recovery
   `pool.json written from ...` lines.
3. Visually re-read the new fence block in `docs/commands/recover.md`
   against `cli/src/recover.rs:1185-1191`, `:1137-1139`,
   `:3576-3611`, `:2591`, and `:2271` to confirm each line in the
   example maps to an exact `eprintln!`/`format!` site.
4. `just test-vm braid-recover` -- existing recovery VM test must still
   pass (it asserts on substrings, so doc wording changes do not affect
   it; this is a regression check, not a new assertion).
5. Optional spot check: build the mdbook (`mdbook build docs`) and skim
   the rendered `commands/recover.html` page to confirm the fenced block
   renders cleanly.
