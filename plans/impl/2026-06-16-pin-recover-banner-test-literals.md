# Plan: pin the recover entry-banner test against literals

## Context

`format_recover_entry` (`cli/src/recover.rs#format_recover_entry`) builds the
`braid recover` entry banner with:

```rust
format!(
    "Recovering from interrupted {:?} operation (started {})...",
    journal_op_label(&journal.op),
    journal.started_at
)
```

The quotes around the op label (`"add"`) come *solely* from `{:?}` Debug-formatting
a `&str`. The documented contract (`docs/commands/recover.md`, Basic example) shows
the quoted form `Recovering from interrupted "add" operation (started ...)...`.

The unit test that claims to pin this -- `format_recover_entry_pins_banner_for_each_op_kind`
-- re-derives its expected value with the **same** format expression the impl uses:

```rust
assert_eq!(
    format_recover_entry(&journal),
    format!("Recovering from interrupted {:?} operation (started {})...", label, started_at),
);
```

Both `label` and `started_at` are static test constants, so this asserts agreement
between two copies of one expression rather than against a literal. A "cleanup" that
swaps `{:?}` for `{}` (dropping the quotes) regresses the user-visible banner to
`interrupted add operation`, yet -- because the test reuses the same `{:?}` -- both
sides move together and the test stays green. The test's own `// Intent` preamble
claims it pins the banner literal "byte-for-byte"; today it does not.

**Intended outcome:** the test asserts against fully-expanded literal strings so the
quoting, lowercase label, surrounding wording, and trailing `...` are each pinned
independently -- and the `add` literal is byte-identical to the documented example,
tying the test directly to the contract in `docs/commands/recover.md`.

## Scope

**In scope:** the single unit test `format_recover_entry_pins_banner_for_each_op_kind`
in `cli/src/recover.rs`. The production code (`format_recover_entry`) is correct and
does **not** change -- the banner's `{:?}` quoting is the intended, documented output.

**Explicitly out of scope (verified, not the same defect):** a sweep found four
superficially similar tests that re-derive expected strings via `format!` --
`check_pool_json_for_bare_discover_refuses_valid_uuid_keyed` /
`..._refuses_corrupt` in `cli/src/discover.rs`, and
`generate_rejects_plain_directory_before_luks_discovery` /
`cmd_generate_mountpoint_revoked_between_plan_and_write` in
`cli/src/enroll_key_file.rs`. These are **correct as written** and must not be
touched: each interpolates a genuinely dynamic tempdir path
(`paths.pool_json().display()`, `target.display()`, `tmp.path().display()`) that
cannot be hardcoded, spells out the surrounding wording as its own literal separate
from the impl's `#[error]`/`format!`, and uses plain `{}` Display with no `{:?}`
quoting subtlety. Only the recover banner test combines static-only interpolation
(full literal is writable) with a hidden `{:?}` contract -- that combination is what
makes it the lone genuine instance. "Fixing" the others would add verbosity and
weaken nothing.

## The change

In `cli/src/recover.rs`, function `format_recover_entry_pins_banner_for_each_op_kind`:

Replace the `(op, label)` case table -- where `label` exists only to rebuild the
expected string with the impl's own `{:?}` format -- with an `(op, expected)` table
of fully-expanded literals, and assert directly against the literal:

```rust
let cases = [
    (
        add_op,
        "Recovering from interrupted \"add\" operation (started 2026-03-15T14:30:00Z)...",
    ),
    (
        remove_op,
        "Recovering from interrupted \"remove\" operation (started 2026-03-15T14:30:00Z)...",
    ),
    (
        remove_missing_op,
        "Recovering from interrupted \"remove-missing\" operation (started 2026-03-15T14:30:00Z)...",
    ),
    (
        replace_op,
        "Recovering from interrupted \"replace\" operation (started 2026-03-15T14:30:00Z)...",
    ),
];

for (op, expected) in cases {
    let journal = journal::Journal {
        started_at: started_at.to_owned(),
        op,
        pre_membership: PoolMembership::empty(),
        target_membership: PoolMembership::empty(),
    };
    assert_eq!(format_recover_entry(&journal), expected);
}
```

Notes:
- Keep `let started_at = "2026-03-15T14:30:00Z";` as the journal **input**; the
  literals restate it as **output**, so the timestamp passthrough is also pinned and
  a mismatch fails loudly. (Matching the doc means the literal must carry the
  timestamp; this duplication is the intended contract pin, not drift risk.)
- The `add` literal is byte-identical to `docs/commands/recover.md` line 27.
- This still pins the label mapping from `journal_op_label`
  (`cli/src/recover.rs#journal_op_label`): each literal carries the exact lowercase
  label (`remove-missing`, etc.), so a label regression still fails.
- Optionally add a one-line comment above the table, e.g.
  `// Literal, not format!(...{:?}...) -- the quotes ARE the contract; re-deriving`
  `// them with the impl's own expression would let a {:?}->{} cleanup pass.`
  This defends against a future reviewer "simplifying" the literals back into a
  shared `format!`.
- Update the existing `// Intent / Why it exists / Scenario` preamble only if needed
  so its "compare against the exact stderr line" claim now reads true (it largely
  already does).

## Critical files

- `cli/src/recover.rs` -- the only file modified (the one test function).
- `docs/commands/recover.md` -- source of the canonical `add` literal; read, not
  edited.
- Leave unchanged and correct: the two delegating sibling tests at
  `cli/src/recover.rs` that assert `failure.notes[0] == format_recover_entry(&journal)`
  (placement checks that rightly defer the format contract to this test), and the VM
  test `tests/cli/recover-add-mixed-batch.py` which pins the literal `add` prefix
  against real binary stderr.

## Verification

1. **Green on correct code:** `just test-rust` (or targeted:
   `cargo test --manifest-path cli/Cargo.toml --lib format_recover_entry_pins_banner_for_each_op_kind`)
   -- passes.
2. **Mutation: proves the quoting is now pinned.** Temporarily change the impl's
   `{:?}` to `{}` (`cli/src/recover.rs#format_recover_entry`), re-run the targeted
   test -- it must now **FAIL** (before this change it would have stayed green).
   Revert.
3. **Mutation: proves the label mapping is still pinned.** Temporarily make
   `journal_op_label` return e.g. `"Add"` for the Add arm, re-run -- must **FAIL**.
   Revert.
4. **Lint/format clean:** `cargo clippy` (no new warnings) and `cargo fmt --check`
   for the edited file. The literals are ASCII (`\"`, `...`), so the
   `check-output-ascii.py` gate is unaffected (tests are exempt anyway).
