# Plan: fix lsblk capture drift, make parser fail loudly on missing requested columns

## Context

A code-review finding flagged `parse_lsblk_json` as silently tolerant of
unexpected upstream column additions. That framing is wrong — production
calls lsblk with an explicit `--output NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN`
allowlist, so util-linux cannot add new mandatory fields to our JSON;
new columns only appear if we request them.

However, investigating the finding surfaced a real, related bug: the
fixture capture script in `tests/capture-tool-fixtures.py:43` requests
only `NAME,TYPE,SIZE,MODEL,SERIAL,UUID` — it was never updated when
commit `4d139c8` ("tui: add disk bus to disk table") added `ROTA,TRAN`
to the production `CmdRequest::LsblkJson` in `cli/src/cmd.rs:440-448`.
Consequences:

- Both `cli/tests/fixtures/nixos-25.11/lsblk-2disk.json` and
  `cli/tests/fixtures/nixos-unstable/lsblk-2disk.json` lack `rota` and
  `tran` keys, so the golden tests have never exercised those columns.
- `parse_lsblk_json` cannot detect a missing requested column for any
  nullable field. Serde's `Option<T>` deserializes a missing JSON key
  as `None` regardless of whether `#[serde(default)]` is present, so a
  silent rename or removal of any of `SIZE`, `MODEL`, `SERIAL`, `UUID`,
  `ROTA`, `TRAN` in upstream lsblk would parse cleanly and produce
  `None` everywhere.
- The TUI's Data-tab Bus column (consumes `tran` via
  `cli/src/tui/probe.rs:280-293` and renders at
  `cli/src/tui/view/mod.rs:742`) would silently fall back to `"--"`
  with no test signal.

Drift audit (Explore agent) confirmed this is the only
capture-vs-production mismatch across all 21 fixtures in
`tests/capture-tool-fixtures.py` and the 4 fixtures in
`tests/capture-ups-fixtures.py`. Scope stays narrow.

The original finding's proposed fix — a new Cargo test that compares
lsblk's `--output` column list against the parser's serde fields — is
redundant: both are declared in our own Rust source (`cmd.rs` and
`parse/lsblk.rs`), with no external drift surface to monitor. The real
fix is to align capture with production and make the parser require
every always-requested nullable column to be present in the JSON (even
when its value is null).

## The fix

Three coordinated changes, in this order to keep CI green at each step:

### 1. Fix the capture script

**File:** `tests/capture-tool-fixtures.py:42-45`

Change the `--output` list to match production exactly:

```python
# was
f"lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID /dev/vdb /dev/vdc"
# becomes
f"lsblk --json --bytes --output NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN /dev/vdb /dev/vdc"
```

Add a one-line comment cross-referencing `CmdRequest::LsblkJson` in
`cli/src/cmd.rs` so future column changes update both sites.

### 2. Regenerate fixtures on both lanes

```
just capture-all-fixtures
just capture-all-fixtures-unstable
```

The regenerated `lsblk-2disk.json` files will now include `rota` and
`tran` keys on every device (lsblk emits the requested column as a JSON
key for every device entry, with `null` where the device has no value
— e.g. dm-crypt children).

### 3. Require every always-requested nullable column to be present in JSON

**File:** `cli/src/parse/lsblk.rs`

`Option<T>` alone is not strict enough — serde silently maps a missing
key to `None`. To make the parser fail loudly when lsblk drops or
renames a column we requested, use a `required_option` deserializer
helper that errors on missing key but accepts both null and a value:

```rust
// Force the JSON key to be present even when value is null.
// Serde's default for Option<T> silently treats a missing key as None,
// which would let an upstream column rename/removal go undetected in
// the unstable canary lane. Every column requested via
// `CmdRequest::LsblkJson` (cli/src/cmd.rs) must use this helper.
fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Deserialize)]
struct RawLsblkDevice {
    name: String,
    #[serde(rename = "type")]
    device_type: String,
    #[serde(deserialize_with = "required_option")]
    size: Option<u64>,
    #[serde(deserialize_with = "required_option")]
    model: Option<String>,
    #[serde(deserialize_with = "required_option")]
    serial: Option<String>,
    #[serde(deserialize_with = "required_option")]
    uuid: Option<String>,
    #[serde(deserialize_with = "required_option")]
    rota: Option<bool>,
    #[serde(deserialize_with = "required_option")]
    tran: Option<String>,
    #[serde(default)]
    children: Vec<RawLsblkDevice>,
}
```

Notes:

- `name` and `device_type` are non-optional `String`; serde already
  fails parsing if either key is absent.
- `children` keeps `#[serde(default)]` because leaf devices (e.g.
  dm-crypt entries) legitimately omit the field.
- Apply the helper to every column in the `CmdRequest::LsblkJson`
  `--output` list, not just `rota`/`tran`. The same drift risk applies
  to `size`, `model`, `serial`, and `uuid`; treating them uniformly
  avoids re-introducing the gap later.

Order matters: do this *after* step 2 regenerates the fixtures.
Otherwise the existing fixtures (which omit `rota`/`tran`) will fail to
parse and break `golden_lsblk_json` in
`cli/tests/support/golden_common.rs:64-82`.

### 4. Add inline regression tests in `parse/lsblk.rs`

In the existing `#[cfg(test)] mod tests` block, add one test per
required column that confirms an absent JSON key fails parsing. Pattern:

```rust
#[test]
fn lsblk_rejects_missing_required_tran_key() {
    let raw = RawCommandOutput {
        cmd: "lsblk".into(),
        stdout: r#"{"blockdevices":[{
            "name":"vdb","type":"disk","size":1,"model":null,
            "serial":null,"uuid":null,"rota":true
        }]}"#.into(),
        stderr: String::new(),
        exit_status: 0,
    };
    let err = parse_lsblk_json(&raw).unwrap_err();
    assert!(matches!(err, ParseError::InvalidJson { .. }));
}
```

One test per required column (`size`, `model`, `serial`, `uuid`, `rota`,
`tran`) -- each omitting only its own key. These are committed
regressions that prove `required_option` actually fails on a missing
key; if a future refactor swaps the helper for plain `Option`, the
tests fail.

### 5. Add a focused probe test for the `tran` -> `disk_transport` mapping

**File:** `cli/src/tui/probe.rs` (inside the existing `#[cfg(test)] mod
tests` block at line 806)

The Data-tab Bus column path (`probe.rs:280-293` -> `view/mod.rs:742`)
is not exercised by any VM test. `braid-tui-browse` drives the Browse
tab and only the `lsblk -f` path
(`tests/cli/braid-tui-browse.py:79-85`). Add a Rust unit test using the
existing `MockRunner` pattern (see `probe_classifies_unmounted_open_and_closed_mappers`
at `probe.rs:1498` for the established shape):

- Feed `CmdRequest::LsblkJson` a JSON response with a parent device
  whose `tran` is `"sata"` and a `braid-vdb` child.
- Drive whatever entrypoint populates `disk_transport`; assert that the
  resulting map contains `"vdb" -> "sata"`.
- Also feed a variant where `tran` is `null` on the parent and assert
  no transport mapping is created for its child (matches the
  production guard `if let Some(tran) = &dev.tran`).

This is the committed end-to-end guard that the `tran` column actually
reaches the Data-tab Bus value.

## Why this is the right shape

- **Parser fails loudly on real drift.** With `required_option` on every
  always-requested nullable column, a future util-linux that renames
  `ROTA`/`TRAN`/etc. produces JSON without that key, and parsing fails
  with a serde "missing field" error during `just test-rust-unstable`
  — exactly the upstream-drift signal the canary lane exists to
  provide.
- **Tests pin the contract.** The per-column missing-key tests (step
  4) and the probe test (step 5) make this protection an asserted
  property of the codebase, not an implicit one that can be undone by
  a stylistic refactor.
- **Cross-reference comment is defense in depth.** The drift went
  unnoticed for the lifetime of commit `4d139c8` because nothing
  pointed the capture-script editor at `cmd.rs`, or vice versa.

## Verification

1. **Capture regeneration succeeds and produces the expected keys.**
   ```
   just capture-all-fixtures
   just capture-all-fixtures-unstable
   ```
   Inspect `cli/tests/fixtures/nixos-25.11/lsblk-2disk.json` and the
   unstable mirror — every device entry (top-level and crypt child)
   must now contain `"rota"` and `"tran"` keys, alongside `size`,
   `model`, `serial`, `uuid`.
2. **All parser tests pass with the new fixtures and helper.**
   ```
   just test-rust
   just test-rust-unstable
   ```
   This includes the six new per-column missing-key tests and the
   probe test from step 5.
3. **Probe test is the real `tran` end-to-end guard.** The added test
   in `cli/src/tui/probe.rs` is the committed asserted property that
   `tran` flows from lsblk JSON into `disk_transport` keyed by disk
   name -- no VM test covers this path.
4. **CLI-reachable parsers regress nothing.**
   ```
   just test-parsers
   ```
   Covers `cli/src/status.rs` and `cli/src/confirm.rs` lsblk callsites.

## Implementation notes

- Captured both fixture lanes as planned, but kept only the
  `lsblk-2disk.json` column deltas; unrelated generated
  progress/status fixture churn and capture-time UUID churn were
  restored to keep the implementation diff behavioral.

## Out of scope

- Adding a Cargo test that compares `--output` column list against
  serde fields (the finding's original recommendation). The columns and
  the struct are both in our own source with no external drift surface,
  so the comparison adds noise without information.
- Auditing every parser for similar drift. The Explore audit confirmed
  lsblk is the only capture-vs-production mismatch across all 25
  captured fixtures.
- Refactoring the capture script to derive its commands from the Rust
  source (e.g., via a generated commands file). Overkill for a single
  drift instance; a cross-reference comment is sufficient.
