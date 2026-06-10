# Introduce the `Fsid` newtype (btrfs filesystem UUID)

## Context

The btrfs pool FSID is a persisted UUID identity compared by **raw string** across
the plan -> recover boundary. It is captured from a mounted pool, serialized as a
bare `String` into `pending-op.json`, and on replay compared with `!=` against a
freshly probed value. Four production sites compare FSIDs by raw string, and
`pending-op.json` is operator-editable -- yet nothing today validates that the
journaled FSID is even a UUID, nor canonicalizes it. Both sides happen to be
lowercase btrfs output, so the mismatch bug is latent; a hand-edited uppercase or
non-canonical FSID would silently miss the cross-check.

This is the exact pattern `LuksUuid` already proved worth typing: a validated,
canonicalized, re-parsing-on-deserialize newtype that is the single source of truth
for an identity. `Fsid` does for the btrfs filesystem UUID what `LuksUuid` does for
the LUKS volume UUID. The FSID is also the one remaining untyped positional `&str`
in the preflight signatures, wedged between an `fs` param and an already-typed
`&MountPoint`.

**Outcome:** every FSID in the program is a canonical, validated `Fsid`; raw-string
identity comparison and positional `&str` swaps become type errors; the
operator-editable journal value is re-validated on load.

## The new type: `cli/src/types.rs`

Add `Fsid` directly beside `LuksUuid`, mirroring it exactly (it is auto-exported via
`pub mod types` in `lib.rs`). Canonical form is **lowercase hyphenated**, identical
to `LuksUuid` -- btrfs already emits this, but we canonicalize so a hand-edited
journal value equates with probed output.

```rust
/// Persistent btrfs filesystem UUID identity. Inner string is canonicalized to
/// lowercase hyphenated form via `Fsid::parse`. The type is the single source of
/// truth for "which btrfs pool" across `pending-op.json`, planner code, and live
/// probes; raw-string FSID comparison across the plan->recover boundary is the
/// mix-up this type makes a compile error.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fsid(String);

#[derive(Debug, Error)]
#[error("invalid btrfs FSID '{raw}': {detail}")]
pub struct FsidParseError {
    pub raw: String,
    pub detail: String,
}

impl Fsid {
    pub fn parse(raw: &str) -> Result<Self, FsidParseError> {
        match uuid::Uuid::parse_str(raw) {
            Ok(u) => Ok(Fsid(u.hyphenated().to_string())),
            Err(e) => Err(FsidParseError { raw: raw.to_owned(), detail: e.to_string() }),
        }
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for Fsid { /* self.0.fmt(f) */ }

impl Serialize for Fsid { /* ser.serialize_str(&self.0) */ }
impl<'de> Deserialize<'de> for Fsid {
    // re-parse through Fsid::parse so a hand-edited pending-op.json FSID is
    // re-validated and re-canonicalized on load -- the operator-editable
    // journal defense, identical to LuksUuid.
}
```

Differences from `LuksUuid`, deliberately: **no `new_v4()`** (braid never mints an
FSID -- btrfs owns it) and **no `format_uuid_list` helper** (FSIDs are not rendered
in membership lists). Reuse the exact `LuksUuid` serde/Display/`as_str` shape so the
two types stay visibly parallel. Mirror `LuksUuid`'s test block: canonicalize
uppercase / simple / URN forms, reject invalid, serde round-trip, and
deserialize-canonicalizes-uppercase.

## Parse boundary: the parser layer (matches the `LuksUuid` precedent)

`Fsid` is constructed at exactly one place: the btrfs-show parser, mirroring how
`CryptsetupLuksUuidOutput.uuid: LuksUuid` is built by `parse_cryptsetup_luks_uuid`
(which already routes raw output through `LuksUuid::parse` and maps failure to
`ParseError`). The FSID field is the un-migrated twin of that pattern.

- `cli/src/parse/types.rs#BtrfsFilesystemShowOutput` -- field `uuid: Option<String>`
  -> `uuid: Option<Fsid>`.
- `cli/src/parse/btrfs_filesystem_show.rs#parse_btrfs_filesystem_show` -- the
  `find_map(... u.to_owned())` that builds `uuid` now parses the found raw value via
  `Fsid::parse`, mapping failure to `ParseError::InvalidValue { field: "uuid", raw,
  detail }` (the variant `parse_cryptsetup_luks_uuid_from_dump` already uses for the
  same situation). Absent uuid line stays `None`; present-but-malformed becomes
  `Err`. Add a parser test pinning the malformed-but-present case to `InvalidValue`,
  parallel to the cryptsetup `luks_uuid_from_dump_returns_invalid_value_when_unparseable`
  test.

Once the parser yields `Option<Fsid>`, **the four FSID-origin consumers inherit
`Fsid` with no per-site parsing**:

- `probe.rs#probe_pool` (`show.uuid.ok_or_else(...)`) -> builds `PoolState.fsid`.
- `probe.rs#probe_fsid` -> return type becomes `Result<Fsid, ProbeError>`.
- `add.rs#classify_braid_disk_fsid` -> `device_fsid != pool_fsid` is `&Fsid != &Fsid`.
- `recover.rs#visible_btrfs_fsid` -> returns `Result<Option<Fsid>, RecoverError>`.

> Note: the `parsed.uuid == *expected` sites in `add.rs#probe_closed_present_luks_target_uuid`
> and the `replace.rs` sibling, plus `probe.rs:177` and `add.rs#target_uuid_map_conflict_to_validation`,
> are **`LuksUuid`** flows (`parse_cryptsetup_luks_uuid`), already typed -- out of scope.

## Compare / guard sites (4)

The four raw-string FSID comparisons the type makes uncomparable-by-accident. The
inequality itself becomes `Option<Fsid> != Option<Fsid>` / `&Fsid != &Fsid` (derived
`PartialEq`, automatic), but two of these guards **also format both sides** with
`fsid.as_deref().unwrap_or("<unknown>")` for the error message -- and `as_deref()`
does **not** compile on `Option<Fsid>` (`Fsid` has no `Deref`, mirroring `LuksUuid`).
Those are real edits, not free flow-through:

| Site | Comparison | Formatting edit |
|---|---|---|
| `add.rs#classify_braid_disk_fsid` | `device_fsid != pool_fsid` | none (no error-side format) |
| `add.rs#validate_execute_pool_identity` | `fresh_pool.fsid != planned_pool.fsid` | `fsid.as_deref().unwrap_or("<unknown>")` x2 -> `fsid.as_ref().map(Fsid::as_str).unwrap_or("<unknown>")` |
| `replace.rs#verify_replace_execute_live_pool_uuid` | `fresh_pool.fsid != planned_pool.fsid` | same `as_deref()` -> `as_ref().map(Fsid::as_str)` rewrite x2 |
| `recover.rs` add cross-check | `&fsid != verified_pool_fsid` | none (formats via `Display` already) |

## Struct fields (5)

| Site | Change |
|---|---|
| `types.rs#PoolState` | `fsid: Option<String>` -> `Option<Fsid>` |
| `status.rs` StatusReport | `fsid: Option<String>` -> `Option<Fsid>` (serializes to the same JSON string) |
| `lock.rs#Snapshot::ProbeFailed` | `fsid: String` -> `Fsid` |
| `journal.rs#AddJournalMode::RecoverableBraidLabeled` | `verified_pool_fsid: String` -> `Fsid` (the headline: the persisted, operator-editable journal value, now re-validated on load) |
| `add.rs#RecoverableBraidTarget` | `verified_pool_fsid: String` -> `Fsid` (the in-memory bridge that carries the verified mounted-pool FSID from probe into the journal field above; typing it keeps the whole chain `Fsid`-shaped with no string detour) |

## Preflight signatures (5): `&str` -> `&Fsid` -- but NOT the low-level sysfs reader

In `cli/src/preflight.rs`, change `fsid: &str` to `fsid: &Fsid` in the **scoped**
path only: the four public guards `require_mutation_preflight`,
`require_lock_preflight`, `require_systemd_stop_lock_preflight`,
`systemd_stop_lock_requires_balance_pause`, plus the private
`check_exclusive_op_with_policy` they all funnel through. In
`require_mutation_preflight` this removes the last untyped positional sitting
between `fs` and `&MountPoint`.

**The private `read_exclop_for_fsid` stays a raw `&str` sysfs-entry reader -- do not
type it.** It has two caller classes with categorically different inputs:

- `check_exclusive_op_with_policy` (scoped) passes a probed `PoolState.fsid` /
  `Snapshot.fsid` -- now a `&Fsid`, so it calls the reader with `fsid.as_str()`.
- `check_any_btrfs_exclusive_op` (the host-wide `braid idle` scan) passes a **raw
  `/sys/fs/btrfs` directory name** straight from `list_dir`, after only the
  `features`/`debug` allowlist. Per its fail-closed contract
  (`BTRFS_SYSFS_NON_FSID_ENTRIES`), every other listed entry must be *read* even if
  it is not a parseable UUID -- a future non-fsid pseudo-dir or a concurrent-unmount
  race must surface as `ExclusiveOpError::Read`, not an FSID parse error. Forcing
  this entry through `Fsid::parse` would either not compile or silently change that
  fail-closed behavior.

So the typed boundary lives at `check_exclusive_op_with_policy` (the scoped wrapper);
the leaf reader stays string-typed because it legitimately serves both a validated
FSID and an unvalidated sysfs entry. Recommended: rename the leaf to
`read_exclop_for_sysfs_entry` so the name stops implying a typed/validated FSID
(optional, but it makes the typed-vs-raw split self-documenting). The path
interpolation `format!("/sys/fs/btrfs/{entry}/exclusive_operation")` is unchanged.

## Mechanical flow-through (~6 call sites)

Where commands extract the FSID to hand to preflight, `Option<String>::as_deref()`
becomes `Option<Fsid>::as_ref()` (yielding `&Fsid`):

- `remove.rs`, `remove_missing.rs`, `replace.rs`, `add.rs` (the
  `pool.fsid.as_deref().expect("mounted pool must have FSID")` sites) -> `.as_ref()`.
- `add.rs` journal-bridge chain (two build sites, ~lines 1186 and 2161):
  `self.pool.fsid.clone().ok_or_else(...)` now yields `Fsid` -> stored in the
  now-`Fsid` `RecoverableBraidTarget.verified_pool_fsid` -> copied into the journal
  by `recoverable_journal_target` (`target.verified_pool_fsid.clone()`, no `.as_str()`
  detour). Every hop stays `Fsid`.
- `status.rs` FSID rendering (`report.fsid.as_deref()` then `format!("FSID: {fsid}")`)
  -> `.as_ref()`; `{fsid}` works via `Display`.
- `probe.rs` diagnostics `result.fsid.as_deref()` -> `.as_ref()`.

## Test fixtures (~38 + journal)

- Inline `fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into())` -> `Some(Fsid::parse("aaaa...").unwrap())`.
  The repetitive `aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee` literal dominates and is a clean find/replace.
- Builders centralize most of it -- update once: `add.rs#pool_mounted_with_fsid(fsid: &str)`
  and `preflight.rs#pool_mounted_for_test` call `Fsid::parse(...).unwrap()` internally;
  callers passing the existing `&str` consts (`POOL_FSID`, `TEST_FSID`,
  `REPLACE_TEST_FSID`, `FSID`, `OTHER_FSID`) stay as-is. Keep the consts as `&str`
  and wrap at the construction site to minimize churn.
- `verified_pool_fsid: ...` fixtures -> `Fsid::parse(...).unwrap()`: `recover.rs` x6
  (`"aaaa...".into()`) and the `add.rs#recoverable_target` builder
  (`verified_pool_fsid: POOL_FSID.to_owned()` -> `Fsid::parse(POOL_FSID).unwrap()`),
  which centralizes the `RecoverableBraidTarget` construction for the add tests.
- **Required fix -- the non-UUID `"fsid-1"` literal:** `journal.rs#roundtrip_add_recoverable_with_enroll_key_file`
  (the `verified_pool_fsid: "fsid-1".into()` construction) and the JSON-body fixture
  in the same module (`"verified_pool_fsid": "fsid-1"`) use a value that is **not a
  UUID**. A re-parsing `Fsid` deserialize rejects it. Replace `"fsid-1"` with a real
  canonical UUID (e.g. reuse the `aaaaaaaa-...` literal). Inspect the JSON fixture
  that also carries a `"luks_uuid"` key under `RecoverableBraidLabeled`: that mode
  has no such field and the enum is `#[serde(deny_unknown_fields)]`, so this is a
  rejection test -- confirm it still rejects for the intended reason after the FSID
  becomes valid (i.e. the unknown-field rejection, not the FSID parse, is what the
  assertion should pin).

## Verification

1. `just test-rust` (or `cargo test -p braid`) -- the migration is type-checked end
   to end; the four compare sites and five scoped preflight signatures become
   `Fsid`-typed at compile time. Confirm the new `Fsid` unit tests and the new parser
   `InvalidValue`-on-malformed-FSID test pass.
2. `cargo build` clean -- no remaining `Option<String>`/`&str` FSID at the migrated
   boundaries (a forgotten `.as_deref()` or raw-string compare is a type error).
3. Completeness sweep over tracked files -- confirm no FSID site was missed:
   `rg 'fsid: Option<String>'`, `rg 'verified_pool_fsid: String'`,
   `rg '\.fsid\.as_deref\(\)'`, and `rg 'fsid != |fsid\.as_str\(\) !='`. Every hit
   should be either migrated or a deliberate out-of-scope `LuksUuid` site. (The leaf
   `read_exclop_for_sysfs_entry` taking `&str` is expected and intended.)
4. `scripts/docs/check-output-ascii.py` if any user-facing string changed (FSID
   rendering is unchanged here).
5. Targeted: round-trip a hand-edited `pending-op.json` with an **uppercase**
   `verified_pool_fsid` through the journal deserialize path and assert it loads as
   the canonical lowercase form and equates with probed output -- the exact defense
   this migration adds, asserted the way `luks_uuid_deserialize_canonicalizes_uppercase`
   asserts it for `LuksUuid`.
6. No `flake.lock` / parser-fixture churn: real btrfs-show fixtures carry valid
   UUIDs, so contract tests keep passing; only synthetic malformed inputs change
   outcome (now `Err`, which is the intended pin). Not a `capture-all-fixtures` event.

## Out of scope

- `LuksUuid` flows that share the `.uuid` field name (cryptsetup parsers) -- already typed.
- Generating or minting FSIDs -- braid never does; btrfs owns FSID creation.

## Implementation notes

- Took the plan's recommended (optional) rename of the leaf sysfs reader:
  `read_exclop_for_fsid` -> `read_exclop_for_sysfs_entry`, with its `fsid: &str`
  param renamed to `entry: &str`. The doc comment now states why the leaf stays
  string-typed (it serves both a validated `fsid.as_str()` and an unvalidated
  `/sys/fs/btrfs` directory name under the fail-closed `check_any_btrfs_exclusive_op`
  scan). Path interpolation is unchanged.
- `preflight.rs` tests: the four scoped guards now take `&Fsid`, but the unit
  tests called them directly with the `&str` `FSID` const. Added a small
  `fn fsid() -> Fsid` test helper (wraps `Fsid::parse(FSID).unwrap()`) used at
  those call sites; the `&str` `FSID`/`OTHER_FSID` consts stay because
  `MockFs::with_sysfs` keys the mock sysfs path by raw string (the leaf reader's
  contract). `OTHER_FSID` is wrapped inline at its single call site.
- The `journal.rs` resurrected-`luks_uuid` JSON rejection test had its
  `"verified_pool_fsid": "fsid-1"` literal changed to a valid UUID (the
  `aaaa...` literal). With a re-parsing `Fsid` deserialize, the non-UUID value
  would have failed FSID parse first and stolen the assertion; the valid UUID
  keeps the rejection pinned on the `deny_unknown_fields` unknown-field reason,
  as the plan required. The companion roundtrip fixture got the same literal.
- The deserialize-canonicalizes-uppercase defense (plan Verification step 5) is
  asserted at the type level in `types.rs#fsid_deserialize_canonicalizes_uppercase`,
  mirroring `luks_uuid_deserialize_canonicalizes_uppercase` exactly; the journal
  `verified_pool_fsid` field routes through this same `Fsid::deserialize`.

## Follow Up

- `cli/src/parse/types.rs#BtrfsScrubStatusPerDeviceOutput` still carries a raw
  `uuid: String` btrfs FSID. It is not part of the plan->recover identity
  comparison surface (only asserted in its own parser tests, never compared as
  identity), so it was correctly out of scope here -- but it is the one remaining
  untyped btrfs FSID and could become `Fsid` for full parity.
