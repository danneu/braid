# Plan: drop pre-ADR-017 "disks" vestiges from config.json test fixtures

## Context

ADR 017 (`docs/decisions/017-runtime-disk-membership.md`) moved disk
membership out of `Config` / `config.json` into a separate runtime
`pool.json`; ADR 024 made that file UUID-keyed. The migration commit
`74feca5` ("move disk membership from nix config to cli-owned runtime
state") removed the `disks` field from `Config`, but several test
fixtures still carry a `"disks":{...}` blob inside `config.json`-shaped
JSON. `RawConfig` does not set `deny_unknown_fields` (only `Ups` does --
see `cli/src/config.rs:32`), so the blob parses and is silently dropped.
The vestige is harmless but misleading: it makes fixtures look like they
test something they do not, and it has already led to a code-review
finding chasing a phantom problem.

The original finding (`cli/src/test_fixtures/doctor.rs:63`) is correct
about the stale field but prescribes an unnecessary fixture extraction
for two tests that do not actually use the shared fixture. This plan
pivots: trim the genuinely stale shapes (the cited fixture plus two
sibling literals that carry the same non-subject blob), drop the
unnecessary extraction, repair two misnamed sibling tests so their
names match their assertions, and bring each touched test up to the
`docs/testing.md` preamble convention while we are in there.

## Scope

In scope (all share root cause: pre-ADR-017 `"disks"` in `config.json`-shaped JSON, or stale post-ADR-017 test naming):

1. The cited fixture in `cli/src/test_fixtures/doctor.rs`.
2. A sibling inline literal in `cli/src/add.rs` that also carries a stale `"disks"` blob inside a `config.json` literal.
3. A non-subject `"disks":{"a":{...}}` blob in `valid_json_bad_schema_empty_mount` -- the test's subject is the empty `mount_point`, the `"disks"` content is the same vestigial noise as #1 and #2.
4. A misnamed `doctor.rs` test whose name claims to test "bad schema" but writes a valid schema -- making it redundant with its sibling.
5. A misnamed `doctor.rs` test whose name says `_disks_warn` but asserts `Skip`.
6. Missing `// Intent / Why it exists / Scenario` preambles above the three touched tests (per `docs/testing.md:11-22`).
7. An em-dash in a `doctor.rs` test comment (project rule: plain ASCII in comments).

Explicitly out of scope (NOT changed):

- `cli/src/doctor.rs:1213-1224` (`valid_json_with_extra_fields_parses_ok`): the inline `"disks":{}` JSON IS the test subject ("extra fields are ignored"). Keep the literal; only fix the em-dash in its in-body comment.
- `cli/src/discover.rs:247,1623,1669`: those reference the *current* `pool.json` recovery format (the legacy name-keyed shape used in adoption tests). Not a vestige.
- `cli/src/journal.rs`, `replace.rs`, `remove_missing.rs`, `status.rs`, `membership.rs`, `add.rs:7716`: all reference the *current* UUID-keyed `pool.json` / `pre_membership` / `target_membership` schema. Not vestiges.

## Changes

### 1. Trim `valid_config_json()` -- the cited fixture

`cli/src/test_fixtures/doctor.rs:62-64`

```rust
pub(crate) fn valid_config_json() -> &'static str {
    r#"{"mount_point":"/mnt/storage"}"#
}
```

Read by ~25 tests in `cli/src/doctor.rs` (grep `valid_config_json` -- all sites write the string to a temp file and run `run_doctor` against it). None of those tests read a `disks` field off the parsed config. Behavior is preserved.

### 2. Trim the stale `"disks"` blob from `cli/src/add.rs:2219`

```rust
r#"{{"mount_point":"/mnt/storage"}}"#
```

Same situation: a `config.json` test literal carrying `"disks":{"d1":{"by_id":"/dev/sda"}}` that `Config` ignores. The test sets up `cmd_add` against a temp config; the `disks` blob has no behavioral role.

### 3. Strip non-subject `"disks"` blob + add preamble to `valid_json_bad_schema_empty_mount` (`cli/src/doctor.rs:1227-1243`)

The test today writes `r#"{"disks":{"a":{"by_id":"/dev/disk/by-id/a"}},"mount_point":""}"#`, but every assertion targets the empty `mount_point` schema failure (`config_file Ok`, `config_schema Fail`, message contains `"mount_point must not be empty"`). The `"disks":{"a":{...}}` blob is not the test subject -- it is the same pre-ADR-017 noise as #1 and #2. Drop it. While editing, add the missing preamble.

```rust
// Intent: empty mount_point fails Config schema validation, with the
//   "must not be empty" message surfaced to the doctor report.
// Why it exists: pins the user-facing failure mode for the most common
//   hand-edit mistake (blanking mount_point) so the doctor report
//   says exactly what is wrong.
// Scenario: an operator hand-edits config.json and leaves mount_point
//   as the empty string; doctor must Fail config_schema and include
//   the schema-builder error message.
#[test]
fn valid_json_bad_schema_empty_mount() {
    let f = write_temp(r#"{"mount_point":""}"#);
    let report = run_doctor(
        f.path(),
        &MockRunner::default(),
        &isolated_paths().1,
        human_options(),
    );
    assert_eq!(find_check(&report, "config_file").status, CheckStatus::Ok);
    let schema = find_check(&report, "config_schema");
    assert_eq!(schema.status, CheckStatus::Fail);
    assert!(
        schema.message.contains("mount_point must not be empty"),
        "unexpected message: {}",
        schema.message
    );
}
```

### 4. Repurpose + rename `declared_disks_skip_when_bad_schema` -> `declared_disks_skips_when_no_membership_even_if_config_schema_fails` + add preamble (`cli/src/doctor.rs:1544-1551`)

As written, this test writes a *valid* schema (`{"disks":{},"mount_point":"/mnt/storage"}` parses fine because `"disks"` is ignored) and asserts `declared_disks` skips. That makes it functionally a duplicate of `declared_disks_skips_when_no_membership` (`cli/src/doctor.rs:1522-1529`), which writes plain `{"mount_point":"/mnt/storage"}` and asserts the same thing.

The actual `check_declared_disks` implementation (`cli/src/doctor.rs:454-468`) never consults `Config`; it calls `membership::load_membership(ctx.paths)` directly and skips only when `pool.json` is `NotFound`, with the message `"skipped (no pool membership file)"`. The pre-existing fn name `_skip_when_bad_schema` falsely suggested a causal link from schema failure to the skip, which the code does not make.

Repurpose to pin the genuinely-missing invariant: `declared_disks` is decoupled from `Config` validity -- it still skips with the same "no pool membership file" message when `config_schema` fails. The renamed test makes that decoupling the explicit subject:

```rust
// Intent: declared_disks skips with the "no pool membership file"
//   message even when Config schema validation fails in the same
//   doctor run.
// Why it exists: pins that declared_disks is decoupled from Config
//   validity. The check reads pool.json directly (ADR 017 / ADR 024),
//   so a Config schema failure does not change its outcome -- it does
//   not turn the check into Fail or Warn, and the skip reason is
//   the absent membership file, not the broken Config.
// Scenario: an operator hand-edits config.json and leaves mount_point
//   empty on a host without pool.json; doctor reports config_schema
//   Fail and declared_disks Skip with "skipped (no pool membership
//   file)" in the same run.
#[test]
fn declared_disks_skips_when_no_membership_even_if_config_schema_fails() {
    let f = write_temp(r#"{"mount_point":""}"#);
    let (_dir, paths) = isolated_paths();
    let report = run_doctor(f.path(), &MockRunner::default(), &paths, human_options());
    assert_eq!(find_check(&report, "config_schema").status, CheckStatus::Fail);
    let check = find_check(&report, "declared_disks");
    assert_eq!(check.status, CheckStatus::Skip);
    assert_eq!(check.message, "skipped (no pool membership file)");
}
```

Empty `mount_point` fails `ConfigBuildError::EmptyMountPoint` (`cli/src/config.rs:46-49`) -- the necessary precondition for the test scenario. The exact-message assertion pins the skip reason (vs `_skips_when_no_membership` which only checks `CheckStatus::Skip`), so a future regression that changed the skip path to "config invalid" would surface here.

### 5. Rename `valid_config_parses_ok_disks_warn` + add preamble (`cli/src/doctor.rs:1113-1129`)

The test asserts `find_check(&report, "declared_disks").status == CheckStatus::Skip` (`cli/src/doctor.rs:1122-1125`), not Warn. Rename and add the required preamble:

```rust
// Intent: a syntactically valid Config parses + schema-validates, and
//   declared_disks skips when no pool.json membership file exists.
// Why it exists: pins the post-ADR-017 contract that declared_disks
//   sources membership from pool.json (not config.json), so a valid
//   config without pool.json yields Skip -- not an error, not Warn.
// Scenario: NixOS-generated config.json reaches a host that has not
//   yet run `braid add`; doctor reports config OK and declared_disks
//   Skip in the same run.
#[test]
fn valid_config_parses_ok_declared_disks_skips() {
```

The "Warn" wording dates back to before ADR 017, when membership was sourced from `Config` and the check could warn directly. Today the check reads `pool.json` and skips when absent. Preserve the existing in-body comment about `beep_path` (lines 1126-1128); only the preamble and the fn name change.

### 6. Replace em-dash in test comment (`cli/src/doctor.rs:1214`)

```rust
// Config no longer has disks -- extra fields are ignored.
```

Project writing-style rule (global `CLAUDE.md`): plain ASCII in code comments unless the surrounding file already uses Unicode. `doctor.rs` uses `--` elsewhere; this is the outlier. The host test `valid_json_with_extra_fields_parses_ok` is not otherwise touched (its `"disks":{}` JSON IS the test subject).

## Critical files

- `cli/src/test_fixtures/doctor.rs` -- the shared fixture (change #1).
- `cli/src/add.rs` -- sibling inline literal (change #2).
- `cli/src/doctor.rs` -- non-subject `"disks"` strip + preamble, repurposed test + preamble, rename + preamble, em-dash (changes #3, #4, #5, #6).

## Verification

- `just test-rust` -- runs all Rust unit tests. Every change either drops noise from a parsed-and-ignored field (no behavioral effect) or strengthens an existing test (#3, #4) in a way that should still pass.
- Spot-check that the repurposed test (#4) actually exercises the intended precondition: `serde_json::from_str::<Config>(r#"{"mount_point":""}"#)` returns `Err("mount_point must not be empty")` per the existing `rejects_empty_mount_point` test (`cli/src/config.rs:140-145`), and `check_declared_disks` (`cli/src/doctor.rs:454-468`) returns `Skip("skipped (no pool membership file)")` when `membership::load_membership` returns `Err(NotFound)`.
- Spot-check that `valid_json_bad_schema_empty_mount` (#3) still passes after the disks-blob strip -- the assertions are unchanged; the only behavioral input is `mount_point: ""`.
- No NixOS VM tests needed -- this change touches only Rust unit-test fixtures, not runtime code paths.

## Non-changes worth recording

The original finding's prescription to "extract a fixture named `config_with_legacy_disks_field` for `valid_json_with_extra_fields_parses_ok` and `valid_json_bad_schema_empty_mount`" is deliberately rejected. Those tests do not use the shared `valid_config_json()` fixture -- they have inline JSON literals. For `valid_json_with_extra_fields_parses_ok` the `"disks":{}` content IS the test subject; for `valid_json_bad_schema_empty_mount` the disks blob is non-subject noise and is being dropped instead (change #3). Extracting a shared fixture would add indirection without making intent clearer.
