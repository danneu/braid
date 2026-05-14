# Plan: assert format_human's empty-status sentinel verbatim

## Context

`braid ups status` is the operator surface preflight and doctor refer
operators to when `upsc` returns an empty `ups.status` flag set:

- `cli/src/preflight.rs:475-477` -- fail-closed: "cannot verify UPS is
  on utility power (ups.status is empty or missing) -- refusing to
  start {op}. Check 'braid ups status', restore utility power, then
  retry."
- `cli/src/doctor.rs:711-717` -- warn: "upsc {name} responded but
  ups.status is empty -- driver may still be starting".

The line operators are sent to look at is emitted at
`cli/src/ups.rs:190` and renders to `Status: (unknown -- ups.status
missing)`. Today nothing asserts that wording end-to-end:

- `format_status_empty_is_unknown` (`cli/src/ups.rs:321-325`) only
  checks `format_status(...).contains("unknown")`. A refactor that
  returned bare `"unknown"`, `"unknown status"`, or any other
  `*unknown*` string would still pass -- the parenthetical disappears
  silently.
- The four `snapshot_human_*` tests (`cli/src/ups.rs:707, 750, 781,
  810`) all cover non-empty flag sets (online / onbattery / lowbattery
  / replace-battery). `tests/capture-ups-fixtures.{nix,py}` does not
  capture an empty-status state, so no fixture-backed snapshot exists.
- `tests/cli/braid-status-ups.py:53-68` exercises the empty-status
  branch only under `--json` (asserts `warning == "ups_status_empty"`
  and structural fields, never the human render).

A refactor of `format_human` that skips the `Status:` line when flags
are empty, drops the parenthetical, or replaces the sentinel with
operator-unfriendly wording would ship silently and leave operators
with a doctor/preflight referral they cannot act on.

## Change

Three small edits in two files. No new fixtures, no new snapshot
files, no changes to the capture scripts.

### 1. New Rust unit test in `cli/src/ups.rs::tests`

Mirror the direct-construction pattern at
`cli/src/ups.rs:425` (`format_human_renders_dash_for_missing_optional_fields`)
and `cli/src/ups.rs:453`
(`format_human_load_omits_estimated_when_nominal_watts_missing`):
build a `UpscOutput` literal with `status_flags: HashSet::new()`, call
`format_human("ups", &parsed)`, and assert that the rendered output
contains the line `Status: (unknown -- ups.status missing)` as a
**whole, exact line** (not a substring). Place it adjacent to the
other `format_human_*` direct-construction tests (after the load-omits
test at line 472, before the JSON sentinel tests).

Use the same `rendered.lines().collect::<Vec<_>>().contains(&"...")`
idiom already used at `cli/src/ups.rs:442` so a regression like
`UPS Status: ...`, `Status: (unknown -- ups.status missing) [stale]`,
or any other suffix/prefix drift fails the assertion. Preamble follows
the canonical `// Intent: / // Why it exists: / // Scenario:` form
required by `docs/testing.md:10`.

```rust
// Intent: format_human emits exactly the line `Status: (unknown -- ups.status missing)`
// when status_flags is empty.
// Why it exists: preflight (preflight.rs:475-477) and doctor
// (doctor.rs:711-717) both point operators at `braid ups status` when
// ups.status is empty. A refactor that drops the parenthetical, adds
// a prefix/suffix, or changes the sentinel would leave that referral
// unactionable; snapshots only cover non-empty flag sets.
// Scenario: dummy-ups driver published telemetry before populating ups.status.
#[test]
fn format_human_empty_status_renders_sentinel() {
    let parsed = UpscOutput {
        status_flags: std::collections::HashSet::new(),
        battery: BatteryFields::default(),
        load_pct: None,
        realpower_nominal_watts: None,
        input: InputFields::default(),
        test_result: None,
        device: DeviceFields::default(),
        extra: std::collections::BTreeMap::new(),
    };
    let rendered = format_human("ups", &parsed);
    let lines: Vec<&str> = rendered.lines().collect();
    assert!(
        lines.contains(&"Status: (unknown -- ups.status missing)"),
        "got: {rendered}"
    );
}
```

### 2. Tighten existing `format_status_empty_is_unknown`

`cli/src/ups.rs:321-325`: replace `assert!(rendered.contains("unknown"))`
with `assert_eq!(rendered, "(unknown -- ups.status missing)")`. The
substring check is too loose -- it passes against any string containing
`"unknown"`. The new assertion locks the inner-helper wording at the
`format_status` boundary, complementing the `format_human` integration
assertion in (1).

While editing the test, also rewrite the preamble to the canonical
`// Intent: / // Why it exists: / // Scenario:` form
(`docs/testing.md:10`). The current preamble at lines 316-319 uses
`// Why:`, which is nonconforming. Target wording, e.g.:

```rust
// Intent: format_status returns the literal sentinel `(unknown -- ups.status missing)`
// for an empty flag set.
// Why it exists: preflight fails closed on an empty set; the rendered
// sentinel must read verbatim so the doctor/preflight referral
// (`Check 'braid ups status'`) stays actionable. A substring check
// would let `(unknown)` or `unknown status` ride through.
// Scenario: dummy-ups fixture with no ups.status line yet.
```

### 3. Extend VM test `tests/cli/braid-status-ups.py`

After the existing empty-status `--json` assertions (after
`assert "error" not in parsed_empty, parsed_empty` on line 68), insert
a no-`--json` check against the same staged config. Assert the
sentinel as a **whole, exact line** via `splitlines()`, not as a
substring -- the contract is the line at `cli/src/ups.rs:190`, so
`UPS Status: ...` or `Status: (unknown -- ups.status missing) [stale]`
must fail.

```python
human_empty = machine.succeed("braid --config /tmp/empty-ups.json ups status")
assert "Status: (unknown -- ups.status missing)" in human_empty.splitlines(), (
    "expected empty-status sentinel as a whole line in human output, got:\n"
    + human_empty
)
```

This catches wrapper-level wiring regressions (PATH plumbing,
`--config` propagation, etc.) that the Rust unit test cannot.

## Files modified

- `cli/src/ups.rs` -- add `format_human_empty_status_renders_sentinel`;
  tighten `format_status_empty_is_unknown` and refresh its preamble.
- `tests/cli/braid-status-ups.py` -- one human-render assertion appended
  to the existing empty-status block.

No edits to `cli/src/preflight.rs`, `cli/src/doctor.rs`, capture
scripts, snapshot files, or fixtures. No new module-level constants
(extracting `EMPTY_STATUS_SENTINEL` as a `const` would make the tests
ride along with sentinel changes instead of catching them, defeating
the purpose).

## Verification

End-to-end:

1. `just test-rust` -- the new test and the tightened existing test both
   run under `cargo test`. Expected: pass.
2. `just test-vm braid-status-ups` -- the parser-canary VM test now
   asserts the human render too. Expected: pass.

Bite check (temporary, to confirm the assertions actually catch
regressions -- do not commit):

- Edit `cli/src/ups.rs:275` to drop the ` -- ups.status missing`
  qualifier (e.g. return `"(unknown)"`).
- Rerun `just test-rust`: both `format_human_empty_status_renders_sentinel`
  and `format_status_empty_is_unknown` should fail.
- Rerun `just test-vm braid-status-ups`: the new human-render assertion
  should fail.
- Revert the edit before committing.

## Out of scope

- New captured empty-status fixture under `cli/tests/fixtures/.../upsc/`
  and a fifth `snapshot_human_*` snap. The sentinel is a braid-internal
  string with no dependency on `upsc` tool version; a snapshot here
  would add capture-script and fixture-refresh burden across the
  stable + unstable lanes for marginal additional coverage.
- Sharing a single sentinel constant across `format_status`,
  `preflight`, and `doctor`. Each of those callsites uses
  operator-context-specific wording (preflight refuses, doctor warns,
  status surfaces a sentinel); unifying them would be a regression in
  message specificity.
