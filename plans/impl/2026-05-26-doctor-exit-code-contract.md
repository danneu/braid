# Give `braid doctor`'s exit-code contract a unit-tested home

## Context

A review finding worried that no test pins "a `Fail`-producing check drives
`report.status` to `Fail` and exit code 1 end-to-end" for the `declared_disks`
UUID-mismatch path. Investigation showed:

- The end-to-end path **is** already covered: `tests/cli/braid-doctor-uuid-swap.py`
  reformats a real LUKS volume and asserts `exit_code != 0` + `report["status"]
  == "fail"` + the `declared_disks` check fails.
- The finding's proposed Rust test (drive `run_doctor` to a `declared_disks`
  UUID mismatch) is **intractable**: `classify_disk_state` (cli/src/doctor.rs:342)
  calls `std::fs::metadata` + `is_block_device()` directly on the real
  filesystem with no mock seam, so a hermetic unit test can never produce
  `LuksUuidMismatch` through `run_doctor`. This split is deliberate and
  documented (doctor.rs:332-337, 3021-3026).

But the investigation surfaced the real architectural gap. `braid doctor`'s most
fundamental behavioral promise -- **worst check wins -> overall status -> process
exit code** -- is pinned in pieces, and one piece has no proper home:

| Contract piece | Where it lives | Where it's tested |
| --- | --- | --- |
| Each check yields the right status | summarizers + classifiers | unit (`summarize_*`) + VM (real-tool) |
| Worst check wins -> overall status | `overall_status` (doctor.rs:1420) | unit (`overall_status_worst_wins`) |
| **Overall status -> exit code** | inline `match` in `cmd_doctor` (doctor.rs:1562-1565) | **only as a side effect of 6 VM tests** |

The status->exit mapping is a pure function of `report.status`, yet it is proven
only by booting VMs and running real `cryptsetup`/`btrfs` to force a `Fail`. The
function that owns the contract (`cmd_doctor`) has no direct test. Separately,
`tests/module/braid-doctor-ups.py:83` captures the process exit code and silently
discards it -- a forgotten assertion in the one `Fail` scenario whose exit code is
not pinned.

**Intended outcome:** the status->exit contract gets an explicit, fast,
unit-tested home; the dead capture is fixed. The finding's per-check concern is
dissolved generically (any `Fail` check -> command failure), not by adding another
per-scenario VM assertion.

## Change 1 (centerpiece): extract + unit-test the status->exit contract

**Extract the mapping onto the report.** Add a method co-located with
`DoctorError` and `cmd_doctor` (cli/src/doctor.rs, right after the `DoctorError`
enum at ~1538):

```rust
impl DoctorReport {
    /// Single source of truth for `braid doctor`'s exit-code contract: a `Fail`
    /// report is a command failure (process exit 1); `Warn`/`Ok`/`Skip` succeed.
    /// Lives on the report -- not inline in `cmd_doctor` -- so the status->exit
    /// mapping is unit-testable without the RealRunner-backed report build.
    pub(crate) fn command_result(&self) -> Result<(), DoctorError> {
        match self.status {
            CheckStatus::Fail => Err(DoctorError::Failed),
            _ => Ok(()),
        }
    }
}
```

**Use it in `cmd_doctor`** (doctor.rs:1562-1565): replace the inline `match
report.status { ... }` tail with `report.command_result()`. Behavior-preserving.

**Add two unit tests** (in the doctor `#[cfg(test)]` module, near
`overall_status_worst_wins` at doctor.rs:2569), each with the project's
Intent/Why/Scenario preamble:

- `doctor_report_command_result_fails_only_on_fail` -- table over all four
  `CheckStatus` variants: `Fail` -> `Err`, `Ok`/`Warn`/`Skip` -> `Ok`. Pins the
  exact policy (e.g. catches a regression that made `Warn` fail the command, or
  `Fail` succeed). Build `DoctorReport { status, checks: vec![] }` directly
  (pub fields, in-module).
- `any_fail_check_escalates_to_command_failure` -- composition test that is the
  faithful generic form of the finding: build a report via
  `overall_status(&[CheckResult::ok("a", ""), CheckResult::fail("b", "boom")])`,
  assert `status == Fail` **and** `command_result().is_err()`. Proves "a
  Fail-producing check -> overall Fail -> command failure" for *any* check at
  once -- dissolving the finding's per-check worry without depending on any
  single check's mockability.

Together with the existing `summarize_declared_disks_promotes_to_fail_on_uuid_mismatch`
(UUID mismatch -> `Fail` check) and `overall_status_worst_wins`, the full chain
is now unit-pinned end to end.

## Change 2: fix the dead capture in `braid-doctor-ups.py`

At tests/module/braid-doctor-ups.py:83, the test captures `exit_code` from a real
`braid_online_active` `Fail` and never asserts it. Assert it (and the overall
status), matching the gold-standard siblings (`braid-doctor-uuid-swap.py:70-73`,
`braid-doctor-foreign-luks-uuid.py:63`):

```python
exit_code, raw = machine.execute("braid doctor --json")
report = json.loads(raw)
assert exit_code != 0, f"doctor must exit non-zero when a check fails: {exit_code}\n{raw}"
assert report["status"] == "fail", f"expected overall fail:\n{raw}"
bo = find_check(report, "braid_online_active")
assert bo["status"] == "fail", f"expected Fail on braid_online_active, got: {bo}"
```

(Asserting the captured variable keeps `raw`/`exit_code` available for the
diagnostic messages and matches the established sibling pattern. A cleaner
alternative is `raw = machine.fail("braid doctor --json")`, which pins non-zero
exit and drops the dead variable in one idiomatic line; either kills the smell.)
No new preamble needed -- this edits an existing test whose comment already states
the check "must fire with Fail severity."

## Explicitly NOT doing

- **No Warn-site consistency pass.** Adding `report["status"]` assertions to the
  other doctor VM sites that assert only a check (braid-doctor.py:153/167/247/318,
  doctor-metadata-mixed.py:109) is *anti-ideal*: once the contract is unit-pinned
  (`overall_status` worst-wins + `command_result`), re-asserting the generic
  invariant in every scenario is redundant. Those sites already pin their exit
  code via `assert exit_code != 0` or `machine.succeed()`; they should assert
  only their scenario-specific check statuses (they do).
- **No churn of the existing 6 VM exit-code assertions.** They are legitimate
  end-to-end smoke for their scenarios; the new unit test is additive.
- **No Rust test driving `run_doctor` to a `declared_disks` mismatch** (the
  finding's literal proposal) -- intractable per the un-mockable FS gate above.

## Critical files

- `cli/src/doctor.rs` -- add `DoctorReport::command_result` (~after line 1538);
  call it in `cmd_doctor` (1562-1565); add two unit tests (~near 2569).
- `tests/module/braid-doctor-ups.py:83` -- assert the captured exit code +
  overall status.

## Verification

- `just test-rust` -- runs the two new unit tests (fast). Confirm
  `doctor_report_command_result_fails_only_on_fail` exercises all four statuses
  and `any_fail_check_escalates_to_command_failure` passes.
- `just test-vm braid-doctor-ups` -- confirm the ups.py change passes (registered
  as `braid-doctor-ups`, flake.nix:312).
- Sanity (optional, cheap insurance that the `cmd_doctor` refactor is
  behavior-preserving): `just test-vm braid-doctor braid-doctor-uuid-swap`.
