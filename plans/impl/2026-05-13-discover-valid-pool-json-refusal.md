# Plan: positively identify a valid `pool.json` in the discover refusal arm

## Context

`classify_pool_json` (`cli/src/discover.rs:195-227`) folds four unrelated
on-disk conditions into a single `PoolJsonShape::Other` variant:

1. valid UUID-keyed `pool.json` (the legitimate steady state),
2. parseable JSON that lacks a `disks` object or is otherwise off-schema
   (e.g. `{"unexpected":true}`),
3. file present but unreadable (EACCES, EIO),
4. file present but unparseable (truncated by power loss, garbage).

The bare-`braid discover` arm at `cli/src/main.rs:739-744` collapses all
four into the message:

```
pool.json already exists at {} -- use 'braid add' to add disks
```

Only case (1) makes that advice correct. `braid add` calls
`load_membership` (`cli/src/add.rs:1381`); cases (2)/(3)/(4) all fail
that loader with `MembershipError::Corrupt` or `MembershipError::Io`,
so the operator hits the same parse/read failure on the suggested
follow-up command. The remediation that actually works -- documented at
`docs/luks-unlock.md:144-146` and already pinned verbatim in
`MembershipError::Corrupt`'s Display at `cli/src/membership.rs:30-36` --
is `run 'braid discover --write' to rebuild from existing disks`.

The migration test pins the misleading message for both a parseable-but-
unrecognized payload and a non-JSON payload at
`tests/cli/braid-discover-migration.py:135-143`, locking the bug in.

The cleanest correction is to positively identify the *valid* case (load
succeeds) rather than peel out a partial set of failure cases. The
finding that surfaced this proposed splitting `Other` into `Other` /
`Unreadable` / `Unparseable`, but that prescription still leaves case
(2) -- parseable JSON without a `disks` object -- in the same
"use `braid add`" bucket as case (1), where the advice is still wrong.

## Goal

A bare `braid discover` operator sees one of three outcomes depending on
the on-disk shape, each pointing at the right next step:

| Shape                                                       | Behavior                                                            |
| ----------------------------------------------------------- | ------------------------------------------------------------------- |
| missing                                                     | proceed to scan (current behavior, unchanged)                       |
| legacy name-keyed (UUID-identity pre-cutover)               | preview + migration hint (current behavior, unchanged)              |
| valid UUID-keyed (loadable `PoolMembership`)                | refuse with the existing "pool.json already exists -- use 'braid add'" wording |
| corrupt / unparseable / parseable-but-off-schema / unreadable | refuse with the documented `discover --write` rebuild remediation     |

## Out of scope

- **EACCES coverage.** The bare-discover code path runs as `root`
  (NixOS service or sudo-invoked operator command), so neither a Rust
  unit test nor a NixOS VM test can trigger EACCES against a file
  `root` owns. The unreadable-file branch is instead exercised under
  Rust unit tests by inducing EISDIR (directory at `paths.pool_json()`),
  which routes through the same `Err(Io { kind != NotFound })` arm of
  the classifier as EACCES would.
- **Threading the parse `detail` into the read-only refusal message.**
  The bare-discover arm is a one-shot preview; the operator will see
  the full parse detail again as soon as they run any state-loading
  command. Keep the refusal message a single fixed string for that
  branch.
- **Renaming `PoolJsonShape::Other`'s callers in tests.** No unit test
  outside `discover.rs` references the variant name.

## Changes

### 1. Split `PoolJsonShape::Other` into `ValidUuidKeyed` + `Corrupt`

**File:** `cli/src/discover.rs`

- Replace the `PoolJsonShape` definition (lines 195-200) with a
  four-variant enum:
  ```rust
  pub enum PoolJsonShape {
      Missing,
      LegacyNameKeyed,
      ValidUuidKeyed,
      Corrupt,
  }
  ```
- Rewrite the classifier (lines 205-227) to positively identify
  `ValidUuidKeyed` via `crate::membership::load_membership_from`. Only
  fall back to a JSON-shape sniff to peel `LegacyNameKeyed` out of the
  generic load-failure bucket:
  ```rust
  pub fn classify_pool_json(path: &Path) -> PoolJsonShape {
      match crate::membership::load_membership_from(path) {
          Ok(_) => PoolJsonShape::ValidUuidKeyed,
          Err(crate::membership::MembershipError::Io { source, .. })
              if source.kind() == std::io::ErrorKind::NotFound =>
          {
              PoolJsonShape::Missing
          }
          Err(_) => {
              if is_legacy_name_keyed_shape(path) {
                  PoolJsonShape::LegacyNameKeyed
              } else {
                  PoolJsonShape::Corrupt
              }
          }
      }
  }

  /// Last-resort sniff for the pre-LUKS-UUID name-keyed shape, run
  /// only after `load_membership_from` returns a non-NotFound error.
  /// Lives separately from `load_membership_from` because the canonical
  /// loader fails-closed on legacy keys without distinguishing them
  /// from generic corruption -- the read-only refusal arm needs that
  /// positive identification to emit the migration hint instead of
  /// the rebuild remediation.
  fn is_legacy_name_keyed_shape(path: &Path) -> bool {
      let Ok(raw) = std::fs::read_to_string(path) else { return false; };
      let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
          return false;
      };
      let Some(disks) = value.get("disks").and_then(|v| v.as_object()) else {
          return false;
      };
      disks.keys().any(|key| LuksUuid::parse(key).is_err())
  }
  ```
- Update the doc comment at lines 192-204 to describe the new contract
  -- `ValidUuidKeyed` is positively identified by a successful load;
  everything else that is neither missing nor legacy name-keyed lands
  in `Corrupt`.
- The write-path call at line 536 (`== PoolJsonShape::LegacyNameKeyed`)
  is unchanged. The `Other`-named arm goes away and the `Corrupt`/
  `ValidUuidKeyed` variants are only consulted by the read-only path.

### 2. Branch the read-only refusal arm

**File:** `cli/src/main.rs`

- Replace the single `PoolJsonShape::Other` arm at lines 739-745 with
  two arms:
  ```rust
  braid_cli::discover::PoolJsonShape::ValidUuidKeyed => {
      print_cli_error(&format!(
          "pool.json already exists at {} -- use 'braid add' to add disks",
          pool_json.display()
      ));
      std::process::exit(1);
  }
  braid_cli::discover::PoolJsonShape::Corrupt => {
      print_cli_error(&format!(
          "pool.json at {} is corrupt or unreadable -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)",
          pool_json.display()
      ));
      std::process::exit(1);
  }
  ```
- The `Missing | LegacyNameKeyed` arm at lines 727-738 is unchanged.
- The remediation tail (`run 'braid discover --write' to rebuild from
  existing disks (with all intended pool members attached; see
  docs/luks-unlock.md)`) is byte-identical to the canonical phrase at
  `cli/src/membership.rs:34` and `docs/luks-unlock.md:146`. Use that
  exact wording so the operator message family is consistent across
  the three places it lives (`MembershipError::Corrupt` Display,
  `refresh_pool_metadata` warning at `membership.rs:610`, and now the
  read-only refusal).

### 3. Update the migration VM test

**File:** `tests/cli/braid-discover-migration.py`

- Rewrite `assert_existing_pool_json_refuses_preview` (lines 41-50)
  so its docstring and assertion describe the corrupt-refusal path:
  - Rename to `assert_corrupt_pool_json_refuses_preview` (or keep the
    name but rewrite the body -- single-call helper, either is fine).
  - Change the asserted substring from `pool.json already exists at
    /var/lib/braid/pool.json` to `is corrupt or unreadable -- run
    'braid discover --write' to rebuild from existing disks` (the
    stable, operator-facing tail of the new message).
  - Update the docstring to "Assert corrupt/off-schema pool.json gets
    the rebuild remediation".
- Both call sites at lines 135-143 (`{"unexpected":true}` and
  `"not-json-at-all"`) keep their inputs but now exercise the new
  message. Update their `label` strings to say "rebuild remediation"
  instead of "old refusal".
- The post-migration assertion at lines 129-133 stays as-is: it tests
  a valid UUID-keyed `pool.json` and the existing wording (`pool.json
  already exists at /var/lib/braid/pool.json`) remains correct for that
  branch.

### 4. Add a Rust unit test for the four classifier branches

**File:** `cli/src/discover.rs` (test module)

Add four `#[test]` cases with the project's `// Intent / Why / Scenario`
preamble (see `AGENTS.md` Test Conventions), modeled on the existing
`discover_write_refuses_when_pool_json_is_name_keyed` style at lines
1496-1527:

- `classify_pool_json_returns_missing_when_absent` — empty dir.
- `classify_pool_json_returns_legacy_name_keyed_for_name_keyed_shape`
  -- reuse the synthetic stale blob from
  `discover_write_refuses_when_pool_json_is_name_keyed` (line 1507) so
  the two tests share the same legacy fixture by import or duplication.
- `classify_pool_json_returns_valid_uuid_keyed_for_loadable_pool_json`
  -- seed a single-member UUID-keyed `pool.json` (the same
  fixture pattern used in `discover_write_proceeds_when_no_gates_fire`
  at lines 1535-1562).
- `classify_pool_json_returns_corrupt_for_unparseable` -- seed
  `"not-json"`.
- `classify_pool_json_returns_corrupt_for_off_schema` -- seed
  `{"unexpected":true}` so the parseable-but-no-`disks` case is pinned
  separately from the unparseable one.
- `classify_pool_json_returns_corrupt_for_non_not_found_io` -- create
  a *directory* at `paths.pool_json()` so `load_membership_from`'s
  `read_to_string` returns `MembershipError::Io` with a non-NotFound
  kind. Pins that the `Missing` arm matches only on `NotFound`; a
  regression that drops the `if source.kind() == NotFound` guard would
  otherwise misclassify EIO/EISDIR as `Missing`.
- `classify_pool_json_returns_corrupt_for_value_side_conflict` -- seed
  a UUID-keyed `pool.json` whose value-side `name` field repeats
  across two entries (modeled on
  `load_membership_rejects_duplicate_value_side_name` at
  `cli/src/membership.rs:1048-1075`). Pins that the classifier treats
  `MembershipError::Conflict` as `Corrupt`; a regression that pattern-
  matches only on `MembershipError::Corrupt` would otherwise
  misclassify uniqueness-violating files as `ValidUuidKeyed`.

EACCES coverage is intentionally limited to code review: the bare-
discover code path runs as `root` (NixOS service or sudo-invoked
operator command), so neither a Rust unit test nor a NixOS VM test can
trigger EACCES against a file `root` owns. The non-NotFound-I/O test
above exercises the same branch through a different syscall failure.

## Critical files

- `cli/src/discover.rs` — enum + classifier + unit tests (lines
  195-227 and the test module after 1462).
- `cli/src/main.rs` — read-only refusal arm (lines 727-746).
- `tests/cli/braid-discover-migration.py` — helper + call-site labels
  (lines 41-50, 135-143).

## Existing patterns reused

- `crate::membership::load_membership_from` (`cli/src/membership.rs:405`)
  -- canonical "is this a valid pool.json" check. Error contract:
  - `MembershipError::Io { source, .. }` for any `read_to_string`
    failure -- NotFound (file absent), EACCES, EIO, EISDIR (directory
    at path), etc. Only the NotFound kind maps to `Missing`; every
    other I/O error lands in `Corrupt`.
  - `MembershipError::Corrupt { detail, .. }` for parse failures and
    serde schema mismatches (`deny_unknown_fields`, bad UUID keys, the
    legacy name-keyed shape, hybrid UUID/name keys, stale value-side
    `luks_uuid`).
  - `MembershipError::Conflict(String)` for the post-parse value-side
    uniqueness sweep (`cli/src/membership.rs:432, 442, 459`): duplicate
    `name`, `by_id`, or `devid` across UUID-keyed entries. Counts as
    `Corrupt` in the classifier; the catch-all `Err(_)` arm covers it.
  The classifier's `match` must keep that catch-all -- pattern-matching
  on `Err(Corrupt)` alone would misclassify `Conflict` files.
- `MembershipError::Corrupt` Display (`cli/src/membership.rs:30-36`)
  -- canonical operator remediation phrase, byte-identical to the docs
  reference at `docs/luks-unlock.md:146`. The new `Corrupt` message
  quotes the same tail verbatim.
- `LuksUuid::parse` (already imported in `discover.rs`) for the legacy
  name-keyed sniff -- unchanged from the current classifier.
- Test-module fixture patterns from
  `discover_write_refuses_when_pool_json_is_name_keyed` (line 1502) and
  `discover_write_proceeds_when_no_gates_fire` (line 1535) for the new
  classifier unit tests.

## Verification

1. `just test-rust` — exercises the new `classify_pool_json` unit tests
   alongside the existing discover write-path tests.
2. `just test-vm braid-discover-migration` — exercises the bare-discover
   refusal arm end-to-end on a real fixture. The two existing subtests
   (`{"unexpected":true}` and `"not-json-at-all"`) now assert the
   rebuild remediation; the post-migration UUID-keyed subtest at
   lines 129-133 still asserts the existing `pool.json already exists`
   wording.
3. Manual sanity check on a dev VM: write a UUID-keyed `pool.json`,
   run `braid discover`, confirm the message says "use 'braid add'".
   Then `printf garbage > /var/lib/braid/pool.json`, re-run, confirm
   the rebuild remediation appears.
4. Grep cross-check: after the change,
   `git grep "pool.json already exists"` should match exactly three
   sites, all of which exercise the surviving `ValidUuidKeyed` branch:
   - `cli/src/main.rs` -- the surviving refusal-arm format string.
   - `tests/cli/braid-discover-migration.py:131` -- the post-migration
     UUID-keyed subtest.
   - `tests/cli/braid-discover.py:59` -- the recovery-flow assertion
     that bare `braid discover` refuses an already-built valid
     `pool.json`. This case is `ValidUuidKeyed`; the existing wording
     remains correct and the test does not need to change.
   Any match outside those three -- in particular, any surviving hit in
   the rewritten `assert_corrupt_pool_json_refuses_preview` helper --
   is a missed edit.
