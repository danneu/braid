# Plan: doctor warns on paused balance

## Context

`braid doctor` does not surface a paused `btrfs balance` today. The check
list in `run_doctor` (`cli/src/doctor.rs:1219-1232`) registers no balance
probe. An operator who paused a balance (manual Ctrl+C, kernel pause
under load, post-`recover` clear leaving an orphan paused state) sees no
flag from doctor even though the pool is mid-conversion.

Sibling surfaces already partially cover this:

- `unlock` calls `status::emit_paused_balance_warning` after mount
  (`cli/src/unlock.rs:171-174`) -- one-shot stderr hint at mount time.
- `recover` auto-resumes paused balances that the journal owns
  (`cli/src/recover.rs:15022-15031`).
- `status` human output already prints
  `Balance: paused, X/Y chunks (Z% complete)` (`cli/src/status.rs:1095-1104`)
  but as a passive status line with no resume hint.

The gap is doctor specifically. The originating finding suggested
`btrfs balance cancel` + `btrfs balance start -dconvert=raid1` as the
remediation; that is wrong -- it throws away the paused balance's
progress. The right verb is `btrfs balance resume`, matching the
guidance already in `emit_paused_balance_warning` and what `recover`
does internally.

Outcome: `braid doctor` registers a `paused_balance` check that warns
with a `btrfs balance resume <mount>` hint when a paused balance is
present, and the resume/cancel advice text becomes a single producer
shared by `emit_paused_balance_warning` and the new doctor check so
both sites can never drift.

## Approach

### 1. Extract shared advice producer in `cli/src/status.rs`

Add a small struct + producer above the existing
`emit_paused_balance_warning` (`status.rs:826-846`). Crate-internal
visibility -- both call sites (`emit_paused_balance_warning` and the
new doctor check) live in this crate, so there is no need to widen the
API. Both items get a short `///` boundary doc per the repo's Doc
Comments rule.

```rust
/// Resume/cancel advice text for a paused balance. Single source of
/// truth shared by `unlock`'s post-mount warning and `doctor`'s
/// `paused_balance` check so the operator-facing wording cannot drift
/// between surfaces.
pub(crate) struct PausedBalanceAdvice {
    pub header: String,     // "paused balance detected -- will not auto-resume"
    pub resume_cmd: String, // "btrfs balance resume /mnt/storage"
    pub cancel_cmd: String, // "btrfs balance cancel /mnt/storage"
}

/// Build the resume/cancel advice for `mount_point`. See
/// `PausedBalanceAdvice`.
pub(crate) fn paused_balance_advice(mount_point: &MountPoint) -> PausedBalanceAdvice {
    PausedBalanceAdvice {
        header: "paused balance detected -- will not auto-resume".to_owned(),
        resume_cmd: format!("btrfs balance resume {mount_point}"),
        cancel_cmd: format!("btrfs balance cancel {mount_point}"),
    }
}
```

Refactor `emit_paused_balance_warning` to consume it -- its 4-line
output shape (blank line, header, indented resume, indented cancel)
stays byte-identical. Existing tests in `status.rs:2619-2666` keep
passing without modification.

### 2. Add `check_paused_balance` in `cli/src/doctor.rs`

Mirror the mounted-only check pattern used by `check_pool_missing_devices`
(`doctor.rs:693-730`) and `check_braid_online_active_when_mounted`
(`doctor.rs:1021`):

```rust
fn check_paused_balance<R: CommandRunner>(ctx: &mut DoctorContext<'_, R>) -> CheckResult {
    if ctx.config.is_none() {
        return CheckResult::skip("paused_balance", "skipped (config not available)");
    }
    if ensure_mountpoint_is_mounted(ctx) != Some(true) {
        return CheckResult::skip("paused_balance", "skipped (pool not mounted)");
    }
    let mount_point = ctx.config.as_ref().unwrap().mount_point().to_owned();

    use crate::status::{BalanceReport, get_balance_report, paused_balance_advice};
    match get_balance_report(ctx.runner, &mount_point) {
        BalanceReport::Paused { .. } => {
            let advice = paused_balance_advice(&mount_point);
            CheckResult::warn(
                "paused_balance",
                format!("{}; run: {}", advice.header, advice.resume_cmd),
            )
        }
        BalanceReport::Idle | BalanceReport::Running { .. } => {
            CheckResult::ok("paused_balance", "no paused balance")
        }
        BalanceReport::Unknown => CheckResult::warn(
            "paused_balance",
            "could not inspect balance status".to_owned(),
        ),
    }
}
```

Notes:

- **No chunk/percent progress in the message.** After a remount with
  `skip_balance`, btrfs prints `0 out of about 0 chunks balanced (0
  considered), nan% left` (fixture
  `cli/tests/fixtures/nixos-25.11/btrfs-balance-status-paused-skip-balance.txt`);
  `parse_chunks_line_nan` (`cli/src/parse/btrfs_balance_status.rs:42-58`)
  collapses that to `pct_left = 0`. Computing `100 - pct_left` would
  print a misleading `100% complete`. The actionable signal is the
  resume command, not the progress; omit chunks and percent entirely.
- **`BalanceReport::Unknown` -> `Warn` on a mounted pool**, matching
  the doctor convention for comparable mounted-pool probe failures:
  `check_data_profile_mismatch` warns on `DfSnapshot::Error`
  (`doctor.rs:640-643`) and `check_pool_missing_devices` warns on a
  probe `Err` (`doctor.rs:725-728`). A `Skip` here would leave overall
  doctor status green even though doctor could not determine whether
  the pool is mid-balance.
- `get_balance_report` is already `pub(crate)` (`status.rs:786`);
  cross-module use from doctor is allowed.
- Single-line message, matching the row format in
  `format_doctor_human_with` (`doctor.rs:1247-1284`).

### 3. Register and label in `doctor.rs`

- Append `check_paused_balance(&mut ctx),` to the `checks` vec in
  `run_doctor` (`doctor.rs:1219-1228`), right after
  `check_metadata_profile_mismatch`. Co-located with the other
  balance/profile checks.
- Add label mapping in `format_doctor_human_with`
  (`doctor.rs:1256-1273`): `"paused_balance" => "paused balance",`.

### 4. Tests in `doctor.rs`

Add unit tests next to the existing `check_data_profile_mismatch`
tests. Reuse the existing fixtures from
`cli/src/test_fixtures/unlock.rs`:

- `unlock_btrfs_balance_status_paused` -- the running paused case with
  real chunk numbers. Sanity-checks the typical paused path.
- `unlock_btrfs_balance_status_idle` -- idle case.

And one additional fixture wired through the same module:

- The `nan%` post-skip-balance case backed by the fixture file at
  `cli/tests/fixtures/nixos-25.11/btrfs-balance-status-paused-skip-balance.txt`.
  Add a small helper next to `unlock_btrfs_balance_status_paused` in
  `cli/src/test_fixtures/unlock.rs` -- e.g.
  `unlock_btrfs_balance_status_paused_skip_balance(mp)` -- that returns
  the same `(CmdRequest, RawCommandOutput)` shape with the fixture
  content inlined. Reuses the existing scaffolding pattern.
- **Add the new helper to the unlock fixture facade.** `mod unlock;`
  at `cli/src/test_fixtures.rs:135` is private; doctor tests import
  unlock fixtures through the `pub(crate) use unlock::{...}` re-export
  block at `cli/src/test_fixtures.rs:229-234`. The new
  `unlock_btrfs_balance_status_paused_skip_balance` symbol must be
  appended to that list alongside `unlock_btrfs_balance_status_idle`
  and `unlock_btrfs_balance_status_paused`, otherwise `doctor.rs` test
  code cannot reach it.

Direct `check_paused_balance` cases:

1. **paused (real chunk numbers) on a mounted pool** ->
   `CheckResult::warn`, `name == "paused_balance"`, message ==
   `"paused balance detected -- will not auto-resume; run: btrfs balance resume /mnt/storage"`,
   and assert the message does NOT contain `"%"` or `"chunks"`.
2. **paused after skip_balance remount (nan%)** -> same exact warn
   message as case 1; asserts the wording is independent of the
   chunk-counter contents and never prints `"100% complete"`.
3. **idle balance on a mounted pool** -> `CheckResult::ok`.
4. **mounted pool but `BtrfsBalanceStatus` errors** (MockRunner returns
   `Err`) -> `CheckResult::warn` with message
   `"could not inspect balance status"`.
5. **pool not mounted** -> `CheckResult::skip("...pool not mounted)")`.
6. **no config** -> `CheckResult::skip("...config not available)")`.

Each test follows the doctor test scaffolding established by
`fn data_profile_mismatch_*` (`doctor.rs:3132+`): a `DoctorContext`
with a `MockRunner::default().with_output(req, out)` and direct
invocation of `check_paused_balance`.

**`run_doctor`-level coverage** (separate from the direct check
tests):

- **Update the existing check-name inventory test** at
  `doctor.rs:1495-1509`. The current sorted vec has 12 names ending at
  `"ups_daemon"`; insert `"paused_balance"` between
  `"metadata_profile_mismatch"` and `"pool_missing_devices"` so the
  list stays sorted. This test catches forgetting to register the new
  check in the vec at `doctor.rs:1219-1228`.
- **Add one `run_doctor`-level paused-balance test** that drives the
  full pipeline against a `MockRunner` returning the paused fixture
  for `BtrfsBalanceStatus`. Asserts the registered check has
  `status == CheckStatus::Warn` and that
  `format_doctor_human(&report)` contains `"paused balance"` (the
  human label from the formatter mapping). This catches forgetting to
  add the `"paused_balance" => "paused balance"` arm at
  `doctor.rs:1256-1273`.

### 5. Update docs/commands/doctor.md

`docs/commands/doctor.md` is the user-facing check inventory; the
JSON-mode contract (`name`, `status`, `message` per check) makes
omitting docs a stale-contract bug. Update:

- **Checks table** (`docs/commands/doctor.md:57-71`): insert a row for
  `paused_balance` between `metadata_profile_mismatch` and
  `smart_self_test` (alphabetical order). Description:
  `Warns if a btrfs balance is paused on the mounted pool (e.g. a
  prior balance interrupted by reboot, manual pause, or kernel pause)
  and suggests resuming with `btrfs balance resume <mount>`.`
- **"What happens under the hood"** (`docs/commands/doctor.md:85-94`):
  extend the mounted-pool step (currently step 3) to mention
  `btrfs balance status` alongside `btrfs filesystem df` and
  missing-device probing, or add it as a separate step. Sequence
  numbering must stay consistent.
- No other docs touched: `principles.md` line 28 ("unlock warns if a
  paused balance is detected") still accurately describes unlock
  behavior; doctor adding a separate surface does not invalidate it.

### 6. No changes to status human renderer

Per design choice: `status` stays passive (existing
`Balance: paused, ...` line is enough). Doctor is the actionable
surface. Avoids growing the status pane and keeps the actionable hint
in one place.

## Files modified

- `cli/src/status.rs` -- add `pub(crate) PausedBalanceAdvice` struct +
  `pub(crate) paused_balance_advice` fn (each with a short `///` doc);
  refactor `emit_paused_balance_warning` to use them.
- `cli/src/doctor.rs` -- add `check_paused_balance`, register in
  `run_doctor`, add label in `format_doctor_human_with`, update the
  check-name inventory test, add direct unit tests, add one
  `run_doctor`-level integration test.
- `cli/src/test_fixtures/unlock.rs` -- add a paused-skip-balance
  fixture helper that inlines the
  `btrfs-balance-status-paused-skip-balance.txt` content.
- `cli/src/test_fixtures.rs` -- extend the existing
  `pub(crate) use unlock::{...}` re-export at lines 229-234 with the
  new fixture name so doctor tests can import it through the facade.
- `docs/commands/doctor.md` -- new row in the checks table and an
  updated mounted-pool probe step.

No changes to: `principles.md`; `status.rs` human renderer; `unlock.rs`
(output is byte-identical after the refactor); `recover.rs`.

## Verification

- `just test-rust` -- new doctor unit tests + the updated inventory
  test + the run_doctor-level integration test all pass; existing
  status tests (`emit_paused_balance_warning_writes_to_buffer`,
  `emit_paused_balance_warning_silent_when_idle` at
  `status.rs:2619-2666`) still pass unchanged (the producer refactor
  preserves the exact 4-line output shape).
- `just test-parsers` -- not strictly needed (no parser changes), but
  cheap to confirm.
- `mdbook build docs` -- ensures the `docs/commands/doctor.md` edits
  do not break cross-links validated by `mdbook-linkcheck`.
- Manual sanity: `cargo run -p braid-cli -- doctor` against a VM where
  a paused balance is induced (e.g., `btrfs balance start ... &`
  followed by `btrfs balance pause`); expect a `WARN paused balance`
  row that does not print a percentage.
- No VM test added: doctor's check-list assertions are unit-tested;
  the underlying paused-balance scenario is already exercised by the
  unlock paused-balance test and the M5/M6 recover matrix tests
  referenced in `recover.rs:15022-15031`.
