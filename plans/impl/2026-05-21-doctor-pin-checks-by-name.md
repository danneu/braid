# Plan: pin doctor checks by name set, not by count

## Context

`cli/src/doctor.rs:1548` asserts `report.checks.len() == 12` inside the
`valid_config_parses_ok_declared_disks_skips` test. The count is brittle:

- Any new check added to `run_doctor` makes every test author bump this
  number, and the failure message ("left: 13, right: 12") names nothing.
- A silent rename or one-for-one swap of a check passes -- the count
  stays the same, but the contract changes.
- The other assertions in the same test already pin meaningful
  invariants by name (`config_file`, `config_schema`, `smart_self_test`,
  `declared_disks`), so the count line is the inconsistent outlier.

The intent of that line is "the vanilla doctor run emits exactly the
expected set of rows." A name-based equality check captures that intent
and produces a useful diff on regression.

Grep confirms this is the only `checks.len()` assertion in the file --
no broader pattern to refactor. The sibling test at
`cli/src/doctor.rs:2110-2125` already runs the same `run_doctor(
valid_config_json(), ...)` setup using only `find_check` calls with no
count, so the project idiom already prefers name-based assertions.

## Change

Replace the single line at `cli/src/doctor.rs:1548`:

```rust
assert_eq!(report.checks.len(), 12);
```

with a sorted-`Vec<&str>` equality assertion over the expected check
names emitted by a vanilla `valid_config_json()` run. A sorted vector
(not a `BTreeSet`) is used so that an accidental duplicate row -- e.g.
`run_doctor` pushing the same `smart_self_test` row twice -- still
fails the assertion:

```rust
let mut actual_names: Vec<&str> =
    report.checks.iter().map(|c| c.name.as_str()).collect();
actual_names.sort();
let expected_names: Vec<&str> = vec![
    "beep_path",
    "braid_online_active",
    "config_file",
    "config_permissions",
    "config_schema",
    "data_profile_mismatch",
    "declared_disks",
    "foreign_luks_uuid",
    "metadata_profile_mismatch",
    "pool_missing_devices",
    "smart_self_test",
    "ups_daemon",
];
assert_eq!(actual_names, expected_names);
```

The 12 expected names come from `run_doctor` at
`cli/src/doctor.rs:1195-1208`:

- 11 fixed-name rows (`config_file`, `config_schema`,
  `config_permissions`, `declared_disks`, `pool_missing_devices`,
  `foreign_luks_uuid`, `data_profile_mismatch`,
  `metadata_profile_mismatch`, `beep_path`, `ups_daemon`,
  `braid_online_active`).
- 1 `smart_self_test` row emitted by `check_smart_selftests` at
  `cli/src/doctor.rs:897-902` -- the membership-load-error branch.
  `isolated_paths()` creates an empty TempDir with no `pool.json`, so
  `membership::load_membership` returns `Err(MembershipError::Io)` and
  `check_smart_selftests` returns a single unscoped `Skip` row with
  message "pool membership not enumerable (...)". (The
  `membership.is_empty()` branch at `cli/src/doctor.rs:905-907` is a
  different code path covered separately by
  `check_smart_selftest_no_members_emits_unscoped_skip` at
  `cli/src/doctor.rs:2075`.)

Leave the existing `find_check` assertions below the count line as-is;
they continue to pin the specific status/subject values that matter.

## Files modified

- `cli/src/doctor.rs` -- replace the one-line count assertion in
  `valid_config_parses_ok_declared_disks_skips`. No new imports
  needed; `Vec` is in the prelude.

## Verification

- `just test-rust` -- the test must still pass. The change is a
  test-only edit; no production code paths move.
- Sanity: temporarily add a 13th check name to `expected_names` and
  confirm the failure message names the missing row (verifies the
  diff actually improves over the count assertion). Revert before
  staging.
