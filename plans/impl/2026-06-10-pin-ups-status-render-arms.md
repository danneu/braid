# Plan: pin format_human's recessive render arms (UPS status)

## Context

`braid ups status` renders its human summary through `format_human` in
`cli/src/ups.rs`. Each per-field line branches on `Option`s, but all four
captured `upsc` fixtures (`cli/tests/fixtures/nixos-26.05/upsc/upsc-*.txt`)
are homogeneous -- every one publishes both `input.transfer.{low,high}`, both
`device.{mfr,model}`, `battery.mfr.date`, and `ups.test.result`. So the four
`snapshot_human_*` tests pin only the dominant arm of each branch, and four
recessive render arms have zero assertions:

1. **Input transfer context** (`ups.rs#format_human`, the `_ => String::new()`
   arm) -- voltage present but only one transfer bound. A refactor that emitted
   a half-bound (`Input: 120.0 V (transfer -142 V)`) would ship misleading
   output uncaught. *(The original finding.)*
2. **Device line** (`ups.rs#format_device_line`) -- the model-only, mfr-only,
   and neither arms. Swapping the single-field arms, or emitting a stray blank
   `Device: ` line, would pass every test. *(Structural twin of #1.)*
3. **`Battery manufactured:`** omitted when `battery.mfr_date` is None.
4. **`Last test:`** omitted when `test_result` is None.

All four share one root cause -- homogeneous fixtures -- and the same fix shape
the three existing `format_human` unit tests already cite ("captured fixtures
populate these fields, so snapshots only pin the Some arm; this catches drift").
This is pure test coverage: **no production behavior changes, so no docs / ADR /
fixture updates.**

## Approach

Single file: `cli/src/ups.rs`, the `#[cfg(test)] mod tests` module. No
production code is touched. Every new / retrofitted test keeps the project's
`// Intent: / Why it exists: / Scenario:` preamble naming the mutation it guards.

### 1. Add a `base_output()` test builder

`UpscOutput` has no `Default` (its `status_flags` Vec order is a contract, not a
default), so every unit test hand-spells all 8 fields. Add one private helper
returning a baseline (status `OL`, every optional field absent), then drive each
unit test off it via struct-update so the test shows only the field it varies:

```rust
/// Baseline `UpscOutput` for render-branch tests: status OL, every optional
/// field absent. Tests override just the field under test via
/// `UpscOutput { field: .., ..base_output() }`. `UpscOutput` has no `Default`
/// (status order is a contract), so this is the single home for that fact.
fn base_output() -> UpscOutput {
    UpscOutput {
        status_flags: vec![UpsStatusFlag::Ol],
        battery: BatteryFields::default(),
        load_pct: None,
        realpower_nominal_watts: None,
        input: InputFields::default(),
        test_result: None,
        device: DeviceFields::default(),
        extra: std::collections::BTreeMap::new(),
    }
}
```

Retrofit the three existing unit tests to use it (assertions unchanged):
- `..._renders_dash_for_missing_optional_fields` -> `let parsed = base_output();`
- `..._load_omits_estimated...` -> `UpscOutput { load_pct: Some(50), ..base_output() }`
- `..._empty_status_renders_sentinel` -> `UpscOutput { status_flags: Vec::new(), ..base_output() }`

### 2. New test -- Input transfer half-bounds (the finding)

Cover both partial orderings; assert the line is exactly bare, no `transfer`
substring. This directly guards the finding's `(transfer -142 V)` worry:

```rust
#[test]
fn format_human_omits_transfer_context_when_bounds_incomplete() {
    for input in [
        InputFields { voltage: Some("120.0".into()), transfer_low:  Some("88".into()),  ..Default::default() },
        InputFields { voltage: Some("120.0".into()), transfer_high: Some("142".into()), ..Default::default() },
    ] {
        let rendered = format_human("ups", &UpscOutput { input, ..base_output() });
        assert!(rendered.lines().any(|l| l == "Input: 120.0 V"), "got: {rendered}");
        assert!(!rendered.contains("transfer"), "got: {rendered}");
    }
}
```

### 3. New test -- Device line collapse

model-only -> `Device: <model>`; mfr-only -> `Device: <mfr>`; neither -> no
`Device:` line:

```rust
#[test]
fn format_human_device_line_collapses_to_present_field() {
    let model_only = UpscOutput { device: DeviceFields { model: Some("Back-UPS ES 550G".into()), ..Default::default() }, ..base_output() };
    assert!(format_human("ups", &model_only).lines().any(|l| l == "Device: Back-UPS ES 550G"));

    let mfr_only = UpscOutput { device: DeviceFields { mfr: Some("APC".into()), ..Default::default() }, ..base_output() };
    assert!(format_human("ups", &mfr_only).lines().any(|l| l == "Device: APC"));

    assert!(!format_human("ups", &base_output()).contains("Device:")); // neither -> omitted
}
```

### 4. Fold the two omission arms into the dash test

The dash test already builds the all-None struct and already executes both
omission arms -- it just never asserts on them. Add two negative assertions and
broaden the Intent (rename to `format_human_renders_sentinels_and_omits_absent_provenance`),
since omitted-line behavior is distinct from a `--` sentinel:

```rust
assert!(!rendered.contains("Battery manufactured"), "got: {rendered}");
assert!(!rendered.contains("Last test"), "got: {rendered}");
```

All four asserted substrings (`transfer`, `Device:`, `Battery manufactured`,
`Last test`) are unique to their lines, so the negative assertions can't be
fooled by another line.

## Verification

1. `just test-rust` -- all unit tests green (the 2 new pass against current
   code; the 3 retrofitted are assertion-identical).
2. **Mutation check (confirms each test bites -- braid's "fail for the right
   reason"):** temporarily
   - add a `(None, Some(high)) => format!(" (transfer -{high} V)")` arm
     -> test #2 must fail;
   - swap `format_device_line`'s single-field arms, or return
     `Some(String::new())` for `(None, None)` -> test #3 must fail;
   - add `else { writeln!(out, "Last test: never") }` -> the dash test must fail.

   Revert each mutation.
3. `cargo fmt` clean; `cargo insta test` shows **no** snapshot changes (the four
   `snapshot_human_*` tests are untouched).

No docs / ADR / fixture changes -- behavior is unchanged; this closes a test
coverage gap only.
