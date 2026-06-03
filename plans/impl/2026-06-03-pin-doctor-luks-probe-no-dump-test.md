# Plan: pin doctor's declared-disk probe to `isLuks` + `luksUUID` (never `luksDump`)

## Context

`braid doctor`'s declared-disk probe (`classify_luks_identity`, `cli/src/doctor.rs#classify_luks_identity`)
issues exactly two cryptsetup commands per disk: `CryptsetupIsLuks` (via
`luks::probe_luks_header`) then `CryptsetupLuksUuid`. It deliberately never runs
`luksDump`. That "never dump" property is a real safety contract, not a style
preference: `probe_luks_header`'s own doc (`cli/src/luks.rs#probe_luks_header`,
the note at the `LuksHeaderState` enum) records that `isLuks`/`crypt_load` can
*write* -- auto-recover a one-good-copy LUKS2 header from the good copy under
metadata locking. A redundant second `crypt_load` probe (`luksDump`) would
multiply that auto-recovery write surface on what is supposed to be a read-only
diagnostic, and `isLuks` already gates on the same `crypt_load`, so a second
probe is pure redundancy.

This contract is currently **unpinned**, and the code has already drifted on it
once. Commit `3ff2ec15` ("fix(doctor): fail on declared disk luks uuid swaps")
introduced `classify_luks_identity` and carried a stale `luksDump` assumption:
the `DiskState::LuksHeaderOk` doc claimed the probe ran `isLuks` + `luksDump` +
`luksUUID`. The *doc* was later corrected (it now reads `isLuks` + `luksUUID`),
but the *tests* were never cleaned up. The two tests that drive
`classify_luks_identity` -- `classify_luks_identity_returns_luks_uuid_mismatch_when_observed_diverges`
and `classify_luks_identity_returns_luks_header_ok_when_uuid_matches`
(`cli/src/doctor.rs` test module) -- each still register a
`luks_dump_text_ok(device)` mock that the production path never consumes.
`MockRunner::with_output` (`cli/src/cmd.rs#MockRunner`) only inserts into a map
and never asserts a mock was used, so those tests pass whether or not a dump call
exists. Nothing fails if a future change re-adds a `luksDump`/`luksDumpText` call
to `classify_luks_identity`.

**Intended outcome:** a regression test that locks the wiring contract the doc
claims, plus removal of the dead mock that currently masks the regression.

## Scope note (why this is bounded)

The shared helper `probe_luks_header` is *already* guarded: `probe_luks_header_ok`
(`cli/src/luks.rs` test module) deliberately omits the dump mock, so a re-added
dump there hits `MissingMock -> ProbeFailed` and fails. The only unguarded piece
is the doctor-specific `isLuks` + `luksUUID` *composition* in
`classify_luks_identity`. No broader probe-audit refactor is warranted.

## Changes

All changes are test-only, confined to the `#[cfg(test)] mod tests` block of
`cli/src/doctor.rs`. No production code changes.

### 1. Add a dedicated wiring-contract test

Add a new test next to the existing `classify_luks_identity_*` tests
(`cli/src/doctor.rs`, ~line 3596) that drives the happy path and asserts the
**exact** recorded request log. Mirror the positive-form idiom already used at
`cli/src/luks.rs#tests` (`assert_eq!(runner.requests(), vec![...])`, around
line 2056); doctor.rs currently only has the negative `!...any(...)` form
(around line 6190), so this introduces the stronger exact-vector assertion.

Concrete shape:

```rust
// Intent: classify_luks_identity probes a declared disk with exactly
//   `cryptsetup isLuks` then `cryptsetup luksUUID`, and never `luksDump`.
// Why it exists: doctor is a read-only diagnostic, but isLuks/crypt_load can
//   auto-recover (write) a one-good-copy LUKS2 header under metadata locking;
//   a redundant second crypt_load probe (luksDump) would multiply that write
//   surface for no gain. The DiskState doc already drifted once (commit
//   3ff2ec15) claiming luksDump was part of the probe -- pin the wiring so a
//   re-added dump call fails loudly instead of passing silently against a
//   leftover optional mock.
// Scenario: a healthy declared member at its by-id path whose live UUID matches
//   pool.json; the probe must touch the device exactly twice, in order.
#[test]
fn classify_luks_identity_issues_isluks_then_luksuuid_only() {
    let device = "/dev/disk/by-id/wwn-0x1";
    let expected = test_uuid(1);
    let (is_luks_req, is_luks_out) = is_luks_ok(device);
    let (uuid_req, uuid_out) = luks_uuid_ok(device, expected.as_str());
    let runner = MockRunner::default()
        .with_output(is_luks_req, is_luks_out)
        .with_output(uuid_req, uuid_out);

    let state = classify_luks_identity(&runner, device, &expected);

    // Sanity: the path actually completed (not short-circuited to ProbeFailed).
    assert!(matches!(state, DiskState::LuksHeaderOk));
    // Load-bearing: exact request set pins presence (isLuks + luksUUID),
    // order, count, and absence of any dump variant or other probe.
    assert_eq!(
        runner.requests(),
        vec![
            CmdRequest::CryptsetupIsLuks {
                device: device.to_owned(),
            },
            CmdRequest::CryptsetupLuksUuid {
                device: device.to_owned(),
            },
        ],
    );
}
```

Notes:
- No dump mock is registered -- so the assertion is the guard, and the exact
  vector also rejects a dump whose result is *ignored* (a case a
  `MissingMock`-only guard would miss).
- `test_uuid` (`cli/src/test_fixtures/shared.rs#test_uuid`), `is_luks_ok`,
  `luks_uuid_ok` (`cli/src/test_fixtures/mount.rs`), `MockRunner`,
  `CmdRequest` are already imported in the doctor test module. Reusing
  `test_uuid(1)` is the established convention (seeds 1/2 are reused freely
  across doctor tests).
- `#[cfg(test)]` items are exempt from the project `///`-doc-comment rule; the
  3-section preamble is the required documentation per Test Conventions.

### 2. Remove the dead `luks_dump_text_ok` mocks (cleanup that also hardens the two existing tests)

In both `classify_luks_identity_returns_luks_uuid_mismatch_when_observed_diverges`
and `classify_luks_identity_returns_luks_header_ok_when_uuid_matches`
(`cli/src/doctor.rs`, the `let (dump_req, dump_out) = luks_dump_text_ok(device);`
lines and the corresponding `.with_output(dump_req, dump_out)` chain entries):

- Delete the `luks_dump_text_ok(device)` binding and its `.with_output(...)` call.
  Each runner then mocks only `is_luks_ok` + `luks_uuid_ok` -- the exact set the
  path consumes. This removes misleading dead code (it implied `luksDump` is part
  of the path) and makes both tests fail-closed via `MissingMock -> ProbeFailed`
  if a dump call with a consumed result is re-added.
- Drop `luks_dump_text_ok` from the `use crate::test_fixtures::{...}` import
  block (around `cli/src/doctor.rs:1814`). These two callsites are its only uses
  in doctor.rs; leaving the import would be an unused-import warning. The helper
  itself stays in `cli/src/test_fixtures/mount.rs` (still used by `mount.rs`
  tests) -- only the doctor import line changes.

## Files

- `cli/src/doctor.rs` (test module only): add one test; remove two dead mock
  registrations and one import symbol.

No production files, no Nix, no fixtures, no docs change. (The `DiskState`
doc comments already correctly describe `isLuks` + `luksUUID`.)

## Verification

1. `just test-rust` -- runs `cargo test` for the `braid-cli` crate. The new test
   and the two edited tests must pass; nothing else should change.
2. Confirm the new test *and* the two edited tests actually guard (manual,
   temporary). At the top of `classify_luks_identity`, insert a throwaway dump
   probe in the result-*consuming* form the function already uses for `luksUUID`
   (doctor.rs#classify_luks_identity), so the unmocked dump's error propagates
   instead of being silently dropped:

   ```rust
   match runner.run(&CmdRequest::CryptsetupLuksDumpText { device: device.to_owned() }) {
       Ok(_) => {}
       Err(e) => return DiskState::ProbeFailed(e.to_string()),
   }
   ```

   Re-run `just test-rust` and verify all three go red:
   - `classify_luks_identity_issues_isluks_then_luksuuid_only` -- the unmocked
     dump returns `MissingMock -> ProbeFailed`, so `state` is no longer
     `LuksHeaderOk` and the test's `assert!(matches!(state, ...LuksHeaderOk))`
     sanity check panics (the exact-vector assert, never reached, would also
     fail).
   - both edited tests -- `MissingMock -> ProbeFailed` makes their
     `match state { ... }` hit the `other => panic!` arm.

   Revert the throwaway. Do not commit it. (This consuming shape is the
   realistic regression -- a genuine re-added `luksDump` consumes its output to
   use it. The discarding `let _ = runner.run(...)` shape, by contrast, drops
   the `MissingMock` error, falls through to the mocked isLuks + luksUUID, and
   leaves the two edited tests green -- it is caught *only* by the new
   exact-vector test. That asymmetry is exactly why both guards exist; see the
   Change 1 note on a dump "whose result is ignored.")

No VM tests or fixture refresh: this is a pure Rust unit-test change with no
parser-critical tool-version change and no behavioral/module change.
