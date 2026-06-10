# Migrate BtrfsScrubStatusPerDeviceOutput.uuid to the Fsid newtype

## Context

The prior `Fsid`-newtype migration (`plans/impl/2026-06-10-introduce-fsid-newtype.md`)
typed every btrfs filesystem UUID that participates in identity comparison, but
deliberately left one site untouched: `BtrfsScrubStatusPerDeviceOutput.uuid` in
`cli/src/parse/types.rs`. It is the last raw `String` btrfs FSID in the program.

It was out of scope then because it is not part of the plan->recover
identity-comparison surface -- it is parsed from `btrfs scrub status -d` output and
only asserted on in its own parser tests; nothing compares it as identity. But it is
the same kind of value as every FSID now wearing the `Fsid` type, so leaving it raw
is a parity gap: a malformed FSID from a future btrfs version would pass through this
parser unvalidated, and the field's type would still permit raw-string handling at
any consumer wired up later.

Outcome: `BtrfsScrubStatusPerDeviceOutput.uuid` becomes `Fsid`, validated and
canonicalized at the parse boundary exactly like `BtrfsFilesystemShowOutput.uuid`. A
malformed FSID in scrub output becomes a typed `ParseError::InvalidValue` instead of
a silently-accepted bad string. This is the precedent pattern applied one more time;
it closes the migration.

## Scope

In scope: the one field, its single construction site, and the parser's tests.

Out of scope: wiring the scrub-per-device output into domain code (it is parsed but
not yet consumed anywhere outside `cli/src/parse/`); any other parser field.

## Changes

### 1. Type change -- `cli/src/parse/types.rs`

`BtrfsScrubStatusPerDeviceOutput` (the struct near `parse/types.rs#BtrfsScrubStatusPerDeviceOutput`):

```rust
pub struct BtrfsScrubStatusPerDeviceOutput {
    pub uuid: Fsid,            // was: String
    pub devices: Vec<DeviceScrubEntry>,
}
```

`Fsid` is already imported in this file (the prior migration added
`use crate::types::{Fsid, LuksUuid};` for `BtrfsFilesystemShowOutput`). Confirm the
import is present; no new import expected.

### 2. Construction site -- `cli/src/parse/btrfs_scrub_status_per_device.rs`

The parser `parse_btrfs_scrub_status_per_device` extracts the UUID line, currently:

```rust
let uuid = stdout
    .lines()
    .find_map(|l| l.trim().strip_prefix("UUID:").map(|v| v.trim().to_owned()))
    .ok_or_else(|| ParseError::MissingField {
        cmd: CMD.to_owned(),
        field: "UUID".to_owned(),
    })?;
```

Route the extracted raw string through `Fsid::parse`, mapping `FsidParseError` to
`ParseError::InvalidValue` -- mirroring the `btrfs_filesystem_show.rs` precedent
(`parse/btrfs_filesystem_show.rs#parse_btrfs_filesystem_show`). The scrub UUID is
non-optional (a missing line is already `MissingField`), so no `Option`/`.transpose()`
is involved -- it is a straight `?` after the existing `?`:

```rust
let raw_uuid = stdout
    .lines()
    .find_map(|l| l.trim().strip_prefix("UUID:").map(|v| v.trim()))
    .ok_or_else(|| ParseError::MissingField {
        cmd: CMD.to_owned(),
        field: "UUID".to_owned(),
    })?;
let uuid = Fsid::parse(raw_uuid).map_err(|e| ParseError::InvalidValue {
    cmd: raw.cmd.clone(),
    field: "uuid".into(),
    raw: e.raw,
    detail: e.detail,
})?;
```

Use `cmd: raw.cmd.clone()` (the actual `RawCommandOutput.cmd`), not the `CMD`
constant. This matches the `btrfs_filesystem_show` precedent
(`parse/btrfs_filesystem_show.rs#parse_btrfs_filesystem_show`) and this parser's own
`CommandFailed` arm, which both report `raw.cmd.clone()`. (The pre-existing
`MissingField` arm uses `CMD.to_owned()`; it is left as-is to avoid touching an
unrelated assertion, but new error payloads should preserve `raw.cmd`.)

(Drop the `.to_owned()` on the `strip_prefix` arm -- `Fsid::parse` takes `&str` and
owns its result. The `field` value `"uuid"` matches the `btrfs_filesystem_show`
precedent's lowercase field name; the existing `MissingField` uses `"UUID"` and is
left as-is.)

Add `use crate::types::Fsid;` to this file if not already imported (the parser
currently references `uuid::Uuid` only in tests, so an `Fsid` import is likely new).

### 3. Test assertions -- same file, `mod tests`

Three assertion sites read `.uuid` and need `.as_str()` (the other inline fixtures
embed a UUID line but never assert on the parsed field):

- `parses_running_fixture` -- `uuid::Uuid::parse_str(&out.uuid)` -> `parse_str(out.uuid.as_str())`;
  the `out.uuid` used in the failure-message arg works via `Fsid`'s `Display`, or
  switch it to `out.uuid.as_str()` for consistency.
- `parses_finished_fixture` -- same two edits.
- `single_device_finished` -- `assert_eq!(out.uuid, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")`
  -> `assert_eq!(out.uuid.as_str(), "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")`.

The two file-based fixtures (`tests/fixtures/nixos-26.05/btrfs-scrub-per-device-{running,finished}.txt`)
carry a valid hyphenated UUID (`9f6091a4-...`) and the seven inline fixtures use
`aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee`; all are valid UUIDs, so no fixture text
changes are needed.

### 4. New negative test -- malformed FSID rejection

Add a test mirroring `btrfs_filesystem_show.rs`'s
`btrfs_show_returns_invalid_value_for_malformed_fsid`. Build a `RawCommandOutput`
whose `cmd` is a distinct, recognizable string (e.g. `"test-scrub-cmd"`, deliberately
not the `CMD` constant) and whose stdout's UUID line is `UUID: not-a-uuid`, parse it,
and assert the error is `ParseError::InvalidValue` with `field == "uuid"`,
`raw == "not-a-uuid"`, a non-empty `detail`, and `cmd == "test-scrub-cmd"` -- the last
assertion pins that the error preserves `raw.cmd` rather than substituting the `CMD`
constant. Suggested name:
`btrfs_scrub_per_device_returns_invalid_value_for_malformed_fsid`.

This is the behavioral regression guard for the change: it pins that a bad FSID is
rejected at the boundary rather than stored raw. Give it the standard test preamble
(Intent / Why it exists / Scenario) per the repo testing convention.

## Verification

- `cargo build -p braid-cli` -- clean.
- `cargo test -p braid-cli --lib btrfs_scrub_status_per_device` -- all scrub parser
  tests pass, including the new malformed-FSID test.
- `cargo test -p braid-cli --lib` -- full lib suite green (no consumer outside the
  parser module is affected; expect the same pass count as before plus one new test).
- `cargo clippy -p braid-cli --all-targets` -- no new warnings.
- Sweep: `rg 'uuid: String' cli/src/parse/types.rs` returns nothing (this was the
  last one); `rg '\.uuid' cli/src/parse/btrfs_scrub_status_per_device.rs` shows only
  `.as_str()`-qualified or Display uses.

## Notes

- The HeaderBackupPath/PathBuf rust-analyzer diagnostics currently showing in
  `add.rs` and `replace.rs` are unrelated to this change (a separate
  `HeaderBackupPath` typing matter) and are not addressed here.
