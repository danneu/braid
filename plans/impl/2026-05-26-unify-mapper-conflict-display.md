# Unify Mapper Conflict LUKS UUID Rendering

## Summary

Unify the duplicate `Option<LuksUuid>` display helpers used by LUKS and probe
mapper-conflict errors. This is a refactor only: public `MapperConflict`
messages must remain byte-for-byte equivalent for both `Some(uuid)` and
`None`.

## Key Changes

- Keep a single shared helper in `cli/src/luks.rs`, renamed to a clearer
  `pub(crate)` name such as `mapper_conflict_found_display`.
- Add a `///` doc comment explaining that this helper centralizes
  mapper-conflict rendering so probe and LUKS errors do not drift.
- Update `LuksError::MapperConflict`, internal `OwnershipError::Conflict`, and
  `ProbeError::MapperConflict` to call the shared helper.
- Delete the duplicate private `found_display` helper from `cli/src/probe.rs`.
- Do not change any error variant fields, conversion logic, or remediation
  wording.

## Test Plan

- Add or extend focused Rust unit coverage that renders the four public
  `MapperConflict` Display cases:
  - `LuksError::MapperConflict { found: Some(uuid), .. }`
  - `LuksError::MapperConflict { found: None, .. }`
  - `ProbeError::MapperConflict { found: Some(uuid), .. }`
  - `ProbeError::MapperConflict { found: None, .. }`
- Use exact `assert_eq!(err.to_string(), expected)` assertions, not substring
  checks, so remediation wording and punctuation stay locked. With
  `name = "disk1"`, expected UUID
  `11111111-1111-1111-1111-111111111111`, and found UUID
  `99999999-9999-9999-9999-999999999999`, both `LuksError` and `ProbeError`
  should render the `Some` case as:

  ```text
  disk 'disk1' mapper '/dev/mapper/braid-disk1' is open but not backed by the configured disk. Expected LUKS UUID 11111111-1111-1111-1111-111111111111, found 99999999-9999-9999-9999-999999999999. Close the conflicting mapper with 'sudo cryptsetup close braid-disk1' and re-run.
  ```

  Both should render the `None` case as:

  ```text
  disk 'disk1' mapper '/dev/mapper/braid-disk1' is open but not backed by the configured disk. Expected LUKS UUID 11111111-1111-1111-1111-111111111111, found no backing (stale mapper). Close the conflicting mapper with 'sudo cryptsetup close braid-disk1' and re-run.
  ```
- Run `just test-rust`.

## Assumptions

- The helper belongs in `luks.rs` because it is LUKS-specific rendering and
  both `ProbeError` and `OwnershipError` already depend on the LUKS module.
- No VM tests are needed because this is pure Rust error-rendering cleanup with
  no storage, systemd, mount, or parser behavior change.
