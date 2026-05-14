# Fix non-deterministic `status_flags` JSON ordering

## Context

`braid ups status --json` is documented as a "stable shape for scripts"
(`manual/commands/ups-status.md:38-43,76,92`), and the published example
shows `"status_flags": ["OL"]`. The reality today is non-deterministic:
`UpscOutput.status_flags` is a `HashSet<UpsStatusFlag>`
(`cli/src/parse/types.rs:628`), and serde walks `HashSet` iterator order
which is randomized by Rust's `RandomState`. Single-flag states (the
common idle case, just `OL`) look stable by accident, but any
multi-flag state -- `OB LB` during an outage, `OL TESTFAIL`, `OL RB`,
`OL COMMBAD` -- re-emits in a different order on every invocation.
Scripts that diff `--json` output across runs, pin a snapshot, or hash
the document see spurious churn.

Two further inconsistencies make the bug stand out as an oversight
rather than an intentional asymmetry:

- The same `UpscOutput` struct already uses `BTreeMap<String, String>`
  for its catch-all `extra` field (`cli/src/parse/types.rs:640`),
  precisely so JSON output is deterministic.
- The human render already lex-sorts tokens
  (`cli/src/ups.rs:277-279`, `cli/src/tui/view/mod.rs:243-245`), and the
  existing `snapshot_human_lowbattery.snap:6` locks in `Status: LB OB`.
  Only the JSON surface lacks the same discipline.

### Design rationale: why not `BTreeSet`?

A previous draft of this plan proposed swapping `HashSet<UpsStatusFlag>`
for `BTreeSet<UpsStatusFlag>` with a manual `Ord` keyed on `as_token()`.
That shape is incorrect: `UpsStatusFlag::Ol.as_token()` and
`UpsStatusFlag::Unknown("OL".into()).as_token()` both return `"OL"`,
so the proposed `cmp` returns `Ordering::Equal` on values that are not
`Eq` (the derived `Eq` is structural, so `Ol != Unknown("OL".into())`).
This violates Rust's `Ord`/`Eq` consistency law -- `a == b` if and only
if `cmp(a, b) == Equal` -- and causes `BTreeSet::insert` to silently
drop one of two distinct domain values, plus `.contains(&Ol)` to claim
membership for an `Unknown("OL")`-only set. The alternative
`#[derive(Ord)]` would respect `Eq` but order by variant index
(`Ol < Ob < Lb < ...`), diverging from the lex order the human render
and snapshots already use -- the JSON output would be deterministic
but in a third, surprising order.

The fix instead lives at the serialization boundary, which is where
the bug actually surfaces: keep the storage `HashSet` (the parser, the
TUI, and the in-memory invariants are correct as-is) and stabilize the
JSON output via a `#[serde(serialize_with = ...)]` field-level helper
on `UpscOutput.status_flags` that lex-sorts the tokens before writing
the array. The human render's existing sort
(`cli/src/ups.rs:277-279`) stays. The TUI's sort
(`cli/src/tui/view/mod.rs:243-245`) stays. No domain type changes;
no trait laws bent; no collateral churn.

## The change

### 1. Add `serialize_status_flags_sorted` helper and apply to the field

`cli/src/parse/types.rs`

Add a module-private helper next to the existing `Serialize for
UpsStatusFlag` impl (currently `cli/src/parse/types.rs:568-575`):

```rust
/// Serialize `status_flags` as a lex-sorted JSON array. The in-memory
/// HashSet has randomized iteration order; the `--json` contract in
/// `manual/commands/ups-status.md` advertises a stable shape, so the
/// serialization boundary sorts even though the storage does not.
fn serialize_status_flags_sorted<S>(
    flags: &std::collections::HashSet<UpsStatusFlag>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    use serde::ser::SerializeSeq;
    let mut sorted: Vec<&UpsStatusFlag> = flags.iter().collect();
    sorted.sort_by(|a, b| a.as_token().cmp(b.as_token()));
    let mut seq = serializer.serialize_seq(Some(sorted.len()))?;
    for flag in sorted {
        seq.serialize_element(flag)?;
    }
    seq.end()
}
```

Apply it to the field at `cli/src/parse/types.rs:628`:

```rust
#[serde(serialize_with = "serialize_status_flags_sorted")]
pub status_flags: std::collections::HashSet<UpsStatusFlag>,
```

Notes:

- The helper delegates per-flag emission to the existing
  `Serialize for UpsStatusFlag` impl (which writes `as_token()`), so the
  JSON character form of each element is unchanged. Only the array
  order becomes deterministic.
- Per the AGENTS.md doc-comment rule, the helper is a new `fn` at
  module scope and gets a 2-3 line `///` comment justifying the
  boundary (intent: stable JSON order; coupling: the `--json` doc
  contract).
- No type or visibility changes to `HashSet`, `UpsStatusFlag`,
  `UpscOutput`, `UpsSnapshot`, or any of their consumers.

### 2. Lock in JSON determinism with a regression test

Add one test in `cli/src/ups.rs` near the existing `json_*` tests that
constructs a `UpscOutput` populated with **every** known
`UpsStatusFlag` variant (the 16 named variants plus one
`Unknown("ZZZ")`), inserts them in enum-declaration order (which is
emphatically not lex order: the declaration sequence starts `OL, OB,
LB, RB, ...` while the lex sequence starts `BOOST, BYPASS, CAL, ...`),
and asserts the serialized JSON array equals the exact 17-element
lex-sorted token sequence.

A smaller (e.g. two-flag) regression test is not enough: a `HashSet`
with two elements has only two possible iteration orders, so a
two-flag test can land on the lex order roughly half the time even
without the `#[serde(serialize_with = ...)]` hook -- a regression
that deletes the hook silently slips through CI on a lucky hash
seed. With 17 elements, a plain `HashSet` would have to
coincidentally emit the entire lex sequence out of a vastly larger
space of iteration orders for the assertion to pass without the
hook, which is vanishingly unlikely in practice -- regardless of how
`HashSet` chooses among the orderings its hasher makes reachable
(Rust does not promise uniform `N!` permutations, so this is a
qualitative bound, not a probabilistic one). The point is that
removing the hook fails loudly on the first run.

This is the kind of behavioral, structure-insensitive test the
AGENTS.md plan-review rubric calls for: it asserts the serialized
order, which is the contract `manual/commands/ups-status.md`
advertises, not internal representation. It would also fail under
both "rejected" alternatives discussed in Context above
(`BTreeSet` + variant-derived `Ord` would produce variant-index
order, not lex; deleting the hook would produce random order).

Suggested preamble:

```
// Intent: --json status_flags is lex-sorted across every known
// flag, so scripts that diff or hash the document do not see
// spurious churn between runs.
// Why: HashSet iteration is randomized; the manual advertises a
// stable shape; the human render already sorts -- the JSON side
// must match. A 17-element exact-array assertion makes accidental
// passage on a HashSet without the serialize_with hook vanishingly
// unlikely (a two-flag test could plausibly pass on a coin flip).
// Guards against silent removal of the field-level serialize_with
// hook AND a future swap to BTreeSet with a different Ord.
// Scenario: a hypothetical UPS reporting every known flag at once
// plus an unrecognized driver-extension token.
```

Shape of the assertion:

```rust
let mut flags = std::collections::HashSet::new();
// Insert in enum-declaration order -- emphatically not lex order.
for f in [
    UpsStatusFlag::Ol,
    UpsStatusFlag::Ob,
    UpsStatusFlag::Lb,
    UpsStatusFlag::Rb,
    UpsStatusFlag::Hb,
    UpsStatusFlag::Chrg,
    UpsStatusFlag::Dischrg,
    UpsStatusFlag::Cal,
    UpsStatusFlag::Bypass,
    UpsStatusFlag::Off,
    UpsStatusFlag::Over,
    UpsStatusFlag::Trim,
    UpsStatusFlag::Boost,
    UpsStatusFlag::Fsd,
    UpsStatusFlag::TestFail,
    UpsStatusFlag::CommBad,
    UpsStatusFlag::Unknown("ZZZ".into()),
] {
    flags.insert(f);
}
let parsed = UpscOutput {
    status_flags: flags,
    // ... other fields default ...
};
let value: serde_json::Value =
    serde_json::from_str(&serde_json::to_string(&JsonReport::success(&parsed)).unwrap())
        .unwrap();
let actual: Vec<&str> = value["status_flags"]
    .as_array()
    .unwrap()
    .iter()
    .map(|v| v.as_str().unwrap())
    .collect();
assert_eq!(
    actual,
    vec![
        "BOOST", "BYPASS", "CAL", "CHRG", "COMMBAD", "DISCHRG",
        "FSD", "HB", "LB", "OB", "OFF", "OL", "OVER", "RB",
        "TESTFAIL", "TRIM", "ZZZ",
    ],
);
```

## Snapshot/test impact

- No snapshot regenerates. The four `*_human_*.snap` files render
  through `format_human` (already lex-sorted), not through the JSON
  serializer.
- All existing JSON tests assert presence via `.iter().any(...)` and
  do not pin order; they pass unchanged.
- All parser tests (`.contains`, `.is_empty`, `.len`) are
  collection-agnostic; they pass unchanged.
- The `format_status_ob_lb_sorted` unit test
  (`cli/src/ups.rs:308-314`) is unaffected -- it exercises
  `format_status`, which still has its own sort.

## Files modified

| File | Why |
| --- | --- |
| `cli/src/parse/types.rs` | New `serialize_status_flags_sorted` helper; `#[serde(serialize_with = ...)]` on `UpscOutput.status_flags` |
| `cli/src/ups.rs` | New JSON-order regression test |

No changes to: the parser, the TUI model, the TUI probe, the TUI view,
any snapshot or fixture file, the manual docs, or any NixOS module /
VM test.

## Verification

End-to-end checks, in this order:

1. `just test-rust` -- covers the changed serialization path through
   the existing `json_*` tests (which exercise `JsonSuccessReport`'s
   `#[serde(flatten)]` over `UpscOutput`), the four `snapshot_human_*`
   insta snapshots (unchanged), and the new JSON-order regression
   test.
2. `just test-parsers` -- live CLI parser canary in a VM. Includes
   `braid-status-ups`, which boots NUT with a dummy-ups driver and
   invokes `braid ups status --json` against real `upsc` output,
   exercising the full path through `parse_upsc` and serde. Confirms
   the end-to-end shape, not just the unit-test mock.
3. Spot-check: in the dev shell, `cargo run -p braid-cli -- ups
   status --json` against a multi-flag fixture file (or pipe a
   contrived `upsc` capture through `parse_upsc`) and confirm
   `status_flags` is lex-sorted across multiple consecutive runs.

No fixture refresh is required: parser-critical tool versions
(`btrfs-progs`, `cryptsetup`, `util-linux`, `nut`) are unchanged, so
the `just capture-all-fixtures` / `just test-rust-unstable` lanes do
not need to run.

## Out of scope

- The duplication between `format_status` (`cli/src/ups.rs:273`) and
  `format_ups_flags` (`cli/src/tui/view/mod.rs:239`) is genuine -- two
  near-identical "render lex-sorted token list, with a distinct
  empty-set sentinel" helpers. Consolidating them is a separate cleanup
  and is genuinely orthogonal to this fix.
- `manual/commands/ups-status.md` already documents the stable-shape
  contract; no doc edit is needed. The example happens to show a
  single-flag (`["OL"]`) case which is unaffected.
