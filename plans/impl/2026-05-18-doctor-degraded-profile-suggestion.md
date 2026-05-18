# Plan: route doctor's profile-mismatch suggestion away from balance when pool is degraded

## Context

`braid doctor` runs `check_data_profile_mismatch` and
`check_metadata_profile_mismatch` (`cli/src/doctor.rs:637-651`) and, when
it finds mixed profiles, unconditionally tells the operator to run
`btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft <mount>`
(`cli/src/doctor.rs:580-597`).

On a degraded RAID1 pool (a member is missing) new chunks are
allocated as `single` because RAID1 needs two devices, so
`data_profile_mismatch` will fire essentially any time the pool is
degraded for non-trivial wall time.

Braid already has an invariant for this scenario: repair/replace
first, then run the soft RAID1 balance to drain single-profile chunks
created during degraded operation. This is encoded in:

- `docs/principles.md:21` -- "When clearing the last missing device
  with >=2 devices remaining, both `remove-missing` and `replace`
  (missing path) run a follow-up soft balance to restore RAID1
  profiles for chunks written during degraded operation."
- `tests/repro/degraded-soft-balance.py` -- the existing repro that
  exercises the kill-disk -> degraded writes -> replace -> soft
  balance sequence, proving the soft RAID1 balance is the post-repair
  step, not the during-repair step.
- `docs/btrfs-balance-soft.md` -- the design doc for the soft balance
  flag.

Doctor's profile-mismatch suggestion contradicts that invariant: it
hands the operator the soft balance command without first ensuring the
pool is no longer degraded. `check_pool_missing_devices` already
diagnoses the underlying state in the same `doctor` run (it's the
adjacent check, `cli/src/doctor.rs:602-635`) and emits its own warning
with the correct fix -- but the profile check doesn't consult it, so
its suggestion competes with the right one.

The fix: when the profile-mismatch warn branch fires, probe for missing
devices via `preflight::probe_missing_devids` and replace the balance
recommendation with "pool is degraded -- replace missing device(s)
first, then rebalance".

## Approach

Minimal inline fix in `check_profile_mismatch`. No new caching
abstraction. `preflight::probe_missing_devids` is already a one-liner
returning `Result<Vec<u64>, String>`; the profile checks only need the
boolean "is the pool degraded?", not the parsed device list, so
mirroring the heavyweight `df_snapshot` pattern (`cli/src/doctor.rs:92-110, 508-541`)
would be cargo-culted symmetry, not earned. The extra `btrfs device usage`
calls (at most two, only on the bug-trigger path, zero on the healthy
path) are negligible for a one-shot interactive check.

## Changes

### 1. `cli/src/doctor.rs` -- `check_profile_mismatch` warn branch

Replace the `else` branch at lines 580-597 so the suggestion routes on
missing-device state:

```rust
} else {
    let mut parts: Vec<String> = Vec::new();
    for entry in &entries {
        parts.push(format!(
            "{}: {} used / {} total",
            entry.bg_profile,
            format_bytes(entry.bg_used),
            format_bytes(entry.bg_total),
        ));
    }
    let suggestion = match preflight::probe_missing_devids(ctx.runner, &mount_point) {
        Ok(missing) if !missing.is_empty() => {
            "pool is degraded -- replace missing device(s) first, then rebalance".to_owned()
        }
        _ => format!(
            "run: btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft {mount_point}"
        ),
    };
    CheckResult::warn(
        check_name,
        format!("mixed {type_label} profiles ({}); {suggestion}", parts.join(", ")),
    )
}
```

Notes:

- `preflight` is already imported at `cli/src/doctor.rs:26`.
- `mount_point` is already a local at line 555.
- The probe-error path (`Err(_)`) routes to the original balance
  suggestion -- a conservative fallback. The operator who is already
  staring at a `pool_missing_devices` warn line that says "could not
  probe for missing devices: ..." (the same probe, same error) has the
  context to interpret the balance suggestion correctly.
- Suggestion stays terse on purpose. `pool_missing_devices` prints the
  full `braid replace --old <disk> --new <disk> --missing-id <devid>`
  invocation in the adjacent check; duplicating it here invites drift.
- Uses `--` per `AGENTS.md` CLI Output Style.

### 2. Tests -- add to `cli/src/doctor.rs` tests module

Add three unit tests next to the existing profile-mismatch tests (two
degraded twins after `data_profile_mixed_warns` at line 1908 and
`metadata_profile_mixed_warns` at line 2116, plus one healthy
non-degraded test). Pattern-match on the existing
`pool_missing_devices_warns_with_replace_recommendation` test at
lines 2255-2288.

```rust
// Intent: data_profile_mismatch routes to replace-first language on a degraded pool.
// Why it exists: braid's invariant is replace/repair first, then run the soft
//   RAID1 balance to drain single-profile chunks written during degraded
//   operation (docs/principles.md:21; tests/repro/degraded-soft-balance.py).
//   The mixed-profile warning's balance suggestion contradicts that order on a
//   degraded pool; this test pins the routing that keeps the two messages aligned.
// Scenario: a 2-disk RAID1 lost a disk; new chunks were allocated as `single`
//   while degraded. doctor reports the mixed profile and must tell the operator
//   to replace before balancing.
#[test]
fn data_profile_mismatch_recommends_replace_when_degraded() {
    let (mp_req, mp_out) = mountpoint_ok();
    let (df_req, df_out) = df_json(DF_MIXED);
    let (du_req, du_out) = device_usage_with_missing();
    let runner = MockRunner::default()
        .with_output(mp_req, mp_out)
        .with_output(df_req, df_out)
        .with_output(du_req, du_out);
    let f = write_temp(valid_config_json());
    let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
    let check = find_check(&report, "data_profile_mismatch");
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.message.contains("mixed"), "{}", check.message);
    assert!(
        check.message.contains("degraded"),
        "expected degraded language: {}",
        check.message,
    );
    assert!(
        check.message.contains("replace"),
        "expected replace recommendation: {}",
        check.message,
    );
    assert!(
        !check.message.contains("btrfs balance"),
        "must not recommend balance on degraded pool: {}",
        check.message,
    );
}

// Intent: metadata_profile_mismatch twin of the data test above.
// Why it exists: metadata mismatch on a degraded pool follows the same
//   replace-first invariant; this test pins the parallel routing.
// Scenario: same as data twin, but the mismatch is in metadata block groups.
#[test]
fn metadata_profile_mismatch_recommends_replace_when_degraded() {
    let (mp_req, mp_out) = mountpoint_ok();
    let (df_req, df_out) = df_json(DF_MIXED_METADATA);
    let (du_req, du_out) = device_usage_with_missing();
    let runner = MockRunner::default()
        .with_output(mp_req, mp_out)
        .with_output(df_req, df_out)
        .with_output(du_req, du_out);
    let f = write_temp(valid_config_json());
    let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
    let check = find_check(&report, "metadata_profile_mismatch");
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.message.contains("mixed"), "{}", check.message);
    assert!(
        check.message.contains("degraded"),
        "expected degraded language: {}",
        check.message,
    );
    assert!(
        check.message.contains("replace"),
        "expected replace recommendation: {}",
        check.message,
    );
    assert!(
        !check.message.contains("btrfs balance"),
        "must not recommend balance on degraded pool: {}",
        check.message,
    );
}

// Intent: a mixed profile on a healthy (non-degraded) pool still recommends
//   the soft RAID1 balance.
// Why it exists: pins the Ok(empty) probe branch. Without this, the new
//   routing logic could regress into always emitting the degraded message
//   (e.g. an inverted predicate) and the existing `data_profile_mixed_warns`
//   would not catch it -- that test exercises the Err fallback because it
//   leaves BtrfsDeviceUsageRaw unmocked.
// Scenario: operator interrupted a balance midway; mixed profiles exist but
//   all members are present. doctor should still recommend the balance.
#[test]
fn data_profile_mismatch_recommends_balance_when_healthy() {
    let (mp_req, mp_out) = mountpoint_ok();
    let (df_req, df_out) = df_json(DF_MIXED);
    let (du_req, du_out) = device_usage_healthy();
    let runner = MockRunner::default()
        .with_output(mp_req, mp_out)
        .with_output(df_req, df_out)
        .with_output(du_req, du_out);
    let f = write_temp(valid_config_json());
    let report = run_doctor(f.path(), &runner, &isolated_paths().1, human_options());
    let check = find_check(&report, "data_profile_mismatch");
    assert_eq!(check.status, CheckStatus::Warn);
    assert!(check.message.contains("mixed"), "{}", check.message);
    assert!(
        check.message.contains("-dconvert=raid1,soft"),
        "expected soft balance suggestion on healthy pool: {}",
        check.message,
    );
    assert!(
        !check.message.contains("degraded"),
        "healthy pool must not be labeled degraded: {}",
        check.message,
    );
    assert!(
        !check.message.contains("replace"),
        "healthy pool must not recommend replace: {}",
        check.message,
    );
}
```

The existing tests `data_profile_mixed_warns` (line 1908) and
`metadata_profile_mixed_warns` (line 2116) continue to cover the
probe-error fallback implicitly: they don't mock `BtrfsDeviceUsageRaw`,
so `probe_missing_devids` returns `Err(MissingMock)`, the `_ =>`
fallback fires, and the existing `-dconvert=raid1,soft` assertion
still passes unchanged. Leave them as-is. With the new
`*_recommends_balance_when_healthy` test added above, the `Ok(empty)`
branch is now also pinned, so the three observable branches of the
routing (`Ok(non-empty)` -> degraded, `Ok(empty)` -> balance, `Err` ->
balance) are each anchored by at least one test.

## Critical files

- `cli/src/doctor.rs` -- the one production change is in
  `check_profile_mismatch` (lines 543-600); three new tests in the
  tests module (lines ~1885-2150).
- `cli/src/preflight.rs` -- read-only; `probe_missing_devids` at
  `cli/src/preflight.rs:306-325` is the existing helper to reuse.
- `cli/src/test_fixtures/doctor.rs` -- read-only;
  `device_usage_with_missing()` (lines 191-217),
  `device_usage_healthy()` (lines 169-189), `DF_MIXED` (251-259),
  `DF_MIXED_METADATA` (261-269) are the fixtures the new tests use.
  All four are already pulled into `cli/src/doctor.rs`'s test module
  via the `use crate::test_fixtures::{...}` block at lines 1095-1101,
  so no import changes are required.
- `docs/principles.md:21` -- the replace-first-then-soft-balance
  invariant the new routing aligns with.
- `tests/repro/degraded-soft-balance.py` -- the repro that anchors
  the post-repair soft balance behavior; cited in the test preamble
  for the degraded routing.

## What is intentionally not in scope

- **No caching of the probe result.** Considered mirroring the
  `df_snapshot` pattern by adding a `missing_devices_snapshot` field
  to `DoctorContext`. Rejected: the probe is a one-liner, the third
  "consumer" only needs a boolean, and AGENTS.md guidance
  ("three similar lines beats a premature abstraction") points the
  other way. Revisit if a fourth caller appears that needs the parsed
  device list.
- **No change to `check_pool_missing_devices`.** Its message and logic
  are correct.
- **No change to the `pool_missing_devices` warning's `--old`/`--new`
  placeholder template.** Out of scope for this fix.

## Verification

1. `just test-rust` -- runs `cargo test`. The three new tests pass
   (two degraded twins + one healthy non-degraded); the existing
   `data_profile_mixed_warns` and `metadata_profile_mixed_warns` tests
   still pass (they exercise the probe-error fallback path implicitly).
2. Spot-check the assertion coverage: the three branches of the routing
   (`Ok(non-empty)` -> degraded, `Ok(empty)` -> balance, `Err(_)` ->
   balance) are each anchored by at least one test.
3. (Optional, slow) `just test-vm` to confirm no doctor-related VM
   tests regress. No NixOS VM test currently exercises a degraded pool
   alongside mixed profiles, so this is a smoke check, not direct
   coverage.
