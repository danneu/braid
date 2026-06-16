# Plan: migrate disk identity from disk name to LUKS UUID

## Goal

Make `LuksUuid` the primary persistent identity for braid pool membership.
Disk names, mapper names, and LUKS labels stay as human-facing presentation and
runtime handles, but they must no longer decide membership, choose a mutation
target, or correlate live btrfs state to `pool.json`.

This is a total cutover. braid is unreleased, so there is no legacy migration
path for old `pool.json` or `pending-op.json` shapes. Old state should fail
closed with clear remediation text.

## Refinement Workflow

This file is expected to go through multiple review and revision cycles before
implementation. While it remains in `plans/todo/`, agents should refine this
plan only; do not implement code from it in the same pass.

Refinement rules:

- Ground every technical change in current source or docs before editing a
  claim. Prefer file and symbol names over brittle line numbers.
- Keep discovered facts separate from preferences:
  - source truth goes into `Current Source Audit`, `In-Flight Work`, or the
    relevant technical section;
  - unresolved choices go into `Open Decisions`;
  - missing plan work goes into `Discovered TODO Log`.
- Do not leave important implementation intent only in the TODO log. If a TODO
  changes how the migration should work, fold it into the main plan section and
  close the TODO.
- Every behavioral invariant should have a behavioral, structure-insensitive
  test listed in `Test Plan`.
- Every risk should have a mitigation, a test, or an explicit accepted-risk
  note.
- Keep the plan ASCII-only.

## Plan Polish Checklist

These checkboxes track plan readiness, not implementation progress. Leave them
unchecked until a reviewer or revising agent has explicitly verified the item
against current code.

- [x] Current source audit matches HEAD and known in-flight local changes
  (see `Current Source Audit` and `In-Flight Work`; both anchored to symbol
  names plus recent commit `905e9ca`).
- [x] Identity invariants are decision-complete and have no conflicting
  exceptions (see `Identity Rules`: the only carve-outs are discover
  bootstrapping, the add adoption gate, the lock observed-mapper close, and
  the Remove-recovery never-enriched fallback, each scoped in its own
  section).
- [x] Data model and on-disk JSON shapes are explicit (see `New Data
  Model`, `Membership Shape`, and `Journal Schema` for explicit Rust and
  JSON shapes including `deny_unknown_fields` coverage).
- [x] Command migration covers add, remove, remove-missing, replace, recover,
  lock, unlock, mount, status, TUI, and discover (see `Command Migration`).
- [x] Test plan maps every invariant and risky premise to behavioral
  coverage (see `Test Plan` for the unit, recovery, lock, VM, and manual
  matrices; structural directives that cannot be behaviorally pinned --
  Pattern #3 comment placement, Remove-recovery carve-out scope, doc-comment
  compliance -- are explicitly called out as reviewer-audit contracts).
- [x] Docs, manual, scripts, and module audit work are listed (see
  `Documentation`, including the verbatim before/after for principles 2 and
  5, decision 024, and the `docs/luks-unlock.md` reconciliation section).
- [x] Risk register has a mitigation, test, or accepted-risk note for every
  risk (see `Risk Register`).
- [x] Definition of done is binary and reviewable (see `Definition of
  Done`).
- [x] `Open Decisions` has no blocking unresolved items.
- [x] `Discovered TODO Log` has no open Blocking items.

## Current Source Audit

As of this update, the current code still uses disk names as the load-bearing
identity:

- `cli/src/membership.rs`: `PoolMembership.disks` is
  `BTreeMap<String, DiskMember>`. `DiskMember` stores `by_id`,
  optional `luks_uuid`, optional `devid`, and `added_at`. `enrich_from_pool_state`
  still parses the name from `PoolDevice.mapper` with `name_from_mapper`.
- `cli/src/types.rs`: `LuksUuid` and `ByIdPath` are transparent public `String`
  newtypes. `LuksUuid` stores whatever text the caller provides, so uppercase,
  simple-form, and canonical UUIDs compare as different values.
- `cli/src/types.rs`: `ConfigDisk.name` is still a raw `String`, so probe
  outputs can be mixed into membership lookups without carrying the new
  `DiskName` constructor invariant.
- `cli/src/journal.rs`:
  - `OpKind::Add.targets` is `BTreeMap<String, AddJournalTarget>` keyed by
    disk name. `AddJournalTarget` carries `by_id`, `mapper_name`
    (redundant), and a nested `AddJournalMode`. The only place a
    LUKS UUID appears today is inside
    `AddJournalMode::RecoverableBraidLabeled { luks_uuid }`;
    `AddJournalMode::FreshLuks` carries no UUID. The migration moves
    the UUID into the outer map key for both modes (so fresh-LUKS
    gains an op-time-generated UUID at the key) and deletes
    `mapper_name`.
  - `OpKind::Remove` is `OpKind::Remove { name: String }` -- a single
    disk-name field, no UUID anywhere. The migration adds `luks_uuid:
    LuksUuid` as the authoritative identity field and keeps `name`
    for logging only.
  - `OpKind::RemoveMissing` is `OpKind::RemoveMissing { phase,
    devid, restore_raid1_after_commit }` -- already devid-only, no
    name or UUID. The shape is intentionally unchanged by the
    migration: btrfs exposes no LUKS UUID for a truly missing
    device, so the journal continues to carry `devid` and recovery
    resolves it through persisted membership.
  - `OpKind::Replace` carries no op-level UUID at all today: the
    fields are `phase`, `old_name`, `new_name`, `new_target`,
    `source`, `restore_raid1_after_commit`. The source side has no
    UUID anywhere, and the new-target UUID exists only nested inside
    `ReplaceJournalMode::ExistingLuks { luks_uuid }` (fresh-LUKS new
    targets have no UUID until format). The migration introduces
    `old_uuid` and `new_uuid` at the op level (so identity is a
    flat field, never derived from a name lookup) and removes the
    nested `luks_uuid` from `ReplaceJournalMode::ExistingLuks`.
  - The audit phrasing of "duplicate UUIDs in value fields" therefore
    overstates the dedup angle: most of the migration's
    journal-schema delta is adding op-level UUID fields where there
    are none today, not collapsing duplicates.
- `cli/src/add.rs` and `cli/src/replace.rs`: fresh LUKS targets inject
  `--label braid-<name>` through raw `luks_format_extra_opts`. Fresh
  targets do not pre-generate a LUKS UUID.
- `cli/src/cmd.rs` and `cli/src/luks.rs`: `CryptsetupLuksFormat` carries only
  `device` and raw `extra_opts: Vec<String>`.
- `cli/src/discover.rs`: discovery makes one `CryptsetupLuksDumpText`
  call and runs `parse_cryptsetup_luks_version` and
  `parse_cryptsetup_luks_label` over the same raw output. There is no
  consolidated `parse_cryptsetup_luks_dump_text`, and no UUID extraction yet.
  `DiscoverOutcome.members` is still `BTreeMap<String, ByIdPath>`.
- `cli/src/lock.rs`: lock plans `open_mappers` by reconstructing
  `mapper_name(name)` from membership keys, and scans orphans by parsing mapper
  names. A drifted but member-owned mapper can be excluded from orphans without
  being closed.
- Tests and fixtures directly construct `LuksUuid("...".into())` and write
  `membership.disks.insert("name", ...)` in many modules, especially
  `recover.rs`.

## In-Flight Work

There is no uncommitted work in the local tree relevant to this
migration.

Recent commit `905e9ca fix(discover): serialize discovery with pool lock`
adds `discover` to the wrapper's `flock /run/braid-pool.lock` fail-fast
case in `modules/braid/braid-wrapper.sh`. The serialization is enforced
at the wrapper layer (see Principle 12), not in Rust -- the discover
Rust code takes no in-process lock. The migration must therefore (a)
not alter the wrapper's case statement or remove `discover` from the
flock set, and (b) not introduce Rust-side ordering assumptions that
would only hold under the wrapper's serialization. The
discover-vs-mutating-command race test in the Test Plan exercises this
end-to-end and is the verification obligation here.

## Identity Rules

Codify these in `docs/principles.md` and a new decision record:

- `LuksUuid` is the primary persistent identity for code.
- `DiskName` is presentation. The on-disk LUKS label `braid-<DiskName>` is
  offline UX. The mapper name `braid-<DiskName>` is a runtime handle.
- `ByIdPath` is hardware addressing: it tells braid which physical device to
  open or format. It is not membership identity.
- When a live LUKS UUID is observable, code must correlate by `LuksUuid`.
- The persisted `DiskMember.devid` is the only fallback used to resolve a live
  devid back to a UUID when the live LUKS UUID is unobservable:
  - `PoolState.null_underlying`: a mapper is open but `cryptsetup status`
    reports `device: (null)`.
  - `PoolState.missing_devids`: btrfs reports a missing device by devid only.
- No code path may parse a disk name out of a mapper path or LUKS label to
  decide membership, choose a mutation target, or correlate live pool state.
- Narrow exceptions:
  - `discover` may read `Label: braid-<name>` from cold LUKS headers to
    bootstrap a UUID-keyed membership. Identity is still the UUID read
    from the same dump.
  - `add` may use a matching `braid-<name>` label as an adoption gate for a
    returning `PresentLuks` disk, but membership decisions still use UUID,
    devid, and FSID checks.
  - display-only fallbacks may parse mapper names for messages. Any remaining
    `name_from_mapper` display fallback needs a comment saying it is not an
    identity boundary.

## New Data Model

### Value Types

Move validation into the value types, not into scattered call sites.

- Add `DiskName` in `cli/src/types.rs`.
- Lock down `LuksUuid`, `DiskName`, `ByIdPath`, and `LuksFormatExtraOpts`:
  - private inner fields;
  - `Display` and `as_str()` for observation;
  - validating constructors;
  - custom `Deserialize` through those constructors;
  - `Serialize` emits the canonical representation;
  - `Ord` and `PartialOrd` where needed for map keys and reverse lookups.
- Keep validation helpers in `types.rs` or a small validation module used by
  both `types.rs` and `membership.rs`. Do not make `types.rs` call back into
  `membership.rs`.

**Trait derivations (pinned).** Each value type derives the following set,
no more and no less. Adding traits later is cheap; over-deriving up front
forces every call site to live with surface area it does not use.

- `LuksUuid`: `Clone`, `Debug`, `PartialEq`, `Eq`, `Hash`, `PartialOrd`,
  `Ord`. `Hash` is required because `LuksUuid` is used as a `HashMap` key
  in lock-side close-set classification (the `member_owned`/orphan probe
  pass). `Display` and `Serialize` are explicit `impl` blocks (canonical
  form) rather than derives. `Deserialize` is an explicit `impl` routing
  through `LuksUuid::parse`. No `Default` (there is no canonical
  zero-UUID and tests use `test_uuid(seed)`).
- `DiskName`: same set as `LuksUuid` -- `Clone`, `Debug`, `PartialEq`,
  `Eq`, `Hash`, `PartialOrd`, `Ord`. `Hash` is required for the TUI
  display caches and lock-side `disks_by_name` lookups. No `Default`.
- `ByIdPath`: `Clone`, `Debug`, `PartialEq`, `Eq`, `Hash`, `PartialOrd`,
  `Ord`. `Hash` for symmetry with the other path-identifying newtypes
  (no current `HashMap<ByIdPath, _>` exists, but adding it as a value or
  key later should not require a re-derive churn). No `Default`.
- `LuksFormatExtraOpts`: `Clone`, `Debug`, `PartialEq`, `Eq`, `Default`.
  `Default` is required because `Default` is the legal empty case (the
  user supplied no extras) and several call sites use
  `LuksFormatExtraOpts::default()` rather than threading an empty parse
  result. No `Hash` (the type is not used as a map key anywhere).
  `Serialize`/`Deserialize` are derives through the inner `Vec<String>`,
  not custom impls -- the validation gate is `parse()`, which is the
  only construction path for production code (deserialize uses the same
  parse path via `try_from` on the deserialized vector). No `PartialOrd`
  or `Ord` (extras are an unordered argv slice for equality purposes).

`LuksUuid`:

- `LuksUuid::parse(&str)` accepts every UUID form accepted by `uuid::Uuid`, then
  canonicalizes to lowercase hyphenated text with `uuid.hyphenated().to_string()`.
- `LuksUuid::new_v4()` generates fresh identity for new LUKS formats.
- Ensure the `v4` feature is enabled on the `uuid` dependency in
  `cli/Cargo.toml`. The current entry is
  `uuid = { version = "1", features = ["serde", "v7"] }`; add `"v4"` to
  the feature list (do not replace the existing features). LUKS UUIDs are
  format-agnostic so v4 is the conventional choice; an audit during this
  edit confirmed no in-tree caller of `Uuid::new_v7` (no production code
  uses the `v7` feature today), but the feature is left in place because
  removing it is out of scope for this plan and the v4/v7 swap would be
  a footgun for a literal-following implementer.
- Remove raw tuple construction from production and tests. Tests use helpers
  like `test_uuid(seed)` or `LuksUuid::parse(...)`.

`DiskName`:

- Uses the existing disk-name contract: starts with an ASCII letter, contains
  ASCII letters, digits, hyphens, or underscores, and is at most 32 chars.
- `membership::parse_disk_spec` returns `(DiskName, ByIdPath)`.

`ByIdPath`:

- Validates the `/dev/disk/by-id/` prefix at all production and deserialize
  boundaries.
- `membership::validate_by_id` already exists (today's loose helper).
  Replace it and `is_valid_disk_name` with constructor-backed validation:
  `DiskName::parse` owns the disk-name contract and `ByIdPath::parse`
  owns the by-id prefix contract. Route `parse_disk_spec`, discover,
  and every production call site through those constructors, then delete
  the free-standing helpers or make any remaining helper private to
  `types.rs`. Do not leave a public `validate_by_id` or
  `is_valid_disk_name` surface after the cutover. (Note for the
  implementer: there are no scattered open-coded
  `starts_with("/dev/disk/by-id/")` checks to delete -- the only such
  check in production source is inside `validate_by_id` itself, and it
  is consumed by routing callers through `ByIdPath::parse`.)
- Concrete `is_valid_disk_name` call sites that MUST move to
  `DiskName::parse` in the same cutover:
  - `cli/src/membership.rs::parse_disk_spec` (today's primary caller).
  - `cli/src/discover.rs:220`: `if !crate::membership::is_valid_disk_name(disk_name) { ... DiscoverWarning::InvalidDiskName ... continue; }`.
    The `DiscoverWarning::InvalidDiskName` arm stays; the predicate becomes
    `DiskName::parse(disk_name).is_err()` (or the warning surface is
    derived from `DiskName::parse`'s `Err` directly). Leaving the
    free-standing helper alive "for compatibility" while migrating other
    callers would let discover and the rest of the codebase diverge on
    what a valid disk name is.
  - Both `is_valid_disk_name` and `validate_disk_name` (the wrapper at
    `membership.rs:142-159`) are deleted in the same change. The
    cutover ordering is: introduce `DiskName::parse`, route every
    call site through it, then remove the free-standing helpers.

`LuksFormatExtraOpts`:

- Private `Vec<String>` wrapper for user-supplied `cryptsetup luksFormat`
  extras.
- Constructor signature is pinned:

  ```rust
  pub fn parse(extras: &[String]) -> Result<Self, LuksFormatExtraOptsError>;
  ```

  The slice form avoids forcing every caller to clone a `Vec`. The
  `Result<Self, _>` return keeps the error raisable before the input
  is moved, so the caller can render the offending token in the
  error and still own the original input. Do not add a
  `TryFrom<Vec<String>>` impl in the same change; routing every call
  site through one named entry point keeps the validation boundary
  obvious. Empty input MUST succeed and produce an empty
  `LuksFormatExtraOpts`.

  **Error type placement.** `LuksFormatExtraOptsError` is a dedicated
  error type defined alongside `LuksFormatExtraOpts` in `types.rs`,
  NOT a new variant on `parse::ParseError`. `parse::ParseError` is for
  tool-output parsing (`btrfs`, `cryptsetup`, `lsblk`, `smartctl`, `nut`)
  -- argv validation is a different domain and overloading the
  tool-output enum would force every `parse::*` consumer to either
  ignore a never-fired variant or pattern-match on a token-naming
  shape that has no analogue in tool output. Surface as:

  ```rust
  #[derive(Debug, Error)]
  pub enum LuksFormatExtraOptsError {
      #[error("--luks-format-arg '{token}' targets a braid-managed cryptsetup option (--uuid, --label); braid sets these itself and rejects user-supplied overrides")]
      ManagedFormatFlag { token: String },
  }
  ```

  Call-site wrapping: `AddError::ManagedFormatFlag(LuksFormatExtraOptsError)`
  and `ReplaceError::ManagedFormatFlag(LuksFormatExtraOptsError)` are
  added as `#[from]`-style variants on the existing error enums so
  `main.rs` matches the error at the same boundary where it matches
  every other `AddError`/`ReplaceError` today. The CLI entrypoints
  do NOT match `LuksFormatExtraOptsError` directly; they match the
  outer `AddError`/`ReplaceError` variant and let the `Display` chain
  render the inner message. This keeps the CLI's command-error
  taxonomy stable (one error type per command) while keeping the
  validation primitive reusable for any future call site that builds
  `LuksFormatExtraOpts` outside add/replace.
- Constructor rejects every option braid manages internally:
  - `--uuid`, `--uuid=<value>` (managed: `OPT_UUID` at
    `reference/cryptsetup/src/cryptsetup_arg_list.h:217`).
  - `--label`, `--label=<value>` (managed: `OPT_LABEL` at
    `reference/cryptsetup/src/cryptsetup_arg_list.h:109`).
- Pinned cryptsetup audit (resolved at planning time): on cryptsetup
  `2.8.4` (the pinned upstream in `reference/cryptsetup/configure.ac`,
  `AC_INIT([cryptsetup],[2.8.4])`), `luksFormat` exposes NO short alias
  for the managed flags. The authoritative evidence is the `ARG(...)`
  macro invocations in `reference/cryptsetup/src/cryptsetup_arg_list.h`:
  - `OPT_LABEL` (line 109): `ARG(OPT_LABEL, '\0', POPT_ARG_STRING, ...)`
  - `OPT_UUID` (line 217): `ARG(OPT_UUID, '\0', POPT_ARG_STRING, ...)`

  The second positional argument to `ARG(...)` is the popt short name;
  `'\0'` means "no short alias". The `luksFormat`-options summary in
  `reference/cryptsetup/man/cryptsetup-luksFormat.8.adoc` lists `--uuid`
  and `--label` exclusively in long form, with no short equivalent
  named anywhere in the action's option list. The audit is therefore
  closed: the reject list is locked to the long-form tokens
  enumerated in the section above (`--uuid`, `--uuid=<value>`,
  `--label`, `--label=<value>`). The required tests deliberately do not
  include speculative short-alias cases because none exist on the pinned
  version. If a future
  `nixpkgs` bump introduces a short alias for any managed flag, the
  fixture-refresh event (per AGENTS.md's parser-critical tool-version
  policy) MUST extend the reject list and add a matching test in the
  same change.
- braid's CLI uses `require_equals = true` on `--luks-format-arg` so a bare
  `--uuid <value>` token pair cannot reach the option parser, but the reject
  list still covers the bare-token form defensively in case that clap
  configuration is changed.
- No mutable accessor. Executor code gets `as_slice()`.

Rejections surface as `LuksFormatExtraOptsError::ManagedFormatFlag`
(defined above). The variant carries the offending token verbatim, and
`Display` text MUST be:

```text
--luks-format-arg '{token}' targets a braid-managed cryptsetup option (--uuid, --label); braid sets these itself and rejects user-supplied overrides
```

`{token}` is the exact slice the user passed (with or without `=`, with or
without a trailing value). Tests pin the substring
`--luks-format-arg '{token}' targets a braid-managed cryptsetup option`.

### Membership Shape

Before:

```rust
pub struct PoolMembership {
    pub disks: BTreeMap<String, DiskMember>,
}

pub struct DiskMember {
    pub by_id: ByIdPath,
    pub luks_uuid: Option<LuksUuid>,
    pub devid: Option<u64>,
    pub added_at: Option<String>,
}
```

After:

```rust
pub struct PoolMembership {
    disks: LuksUuidMap<DiskMember>,
}

pub struct DiskMember {
    pub name: DiskName,
    pub by_id: ByIdPath,
    pub devid: Option<u64>,
    pub added_at: Option<String>,
}
```

The UUID lives only as the map key. Do not duplicate it in `DiskMember`.

On-disk `pool.json`:

```json
{
  "disks": {
    "8c78a966-ef17-4610-b835-5b376ef10b4e": {
      "name": "toshiba1",
      "by_id": "/dev/disk/by-id/ata-TOSHIBA_...",
      "devid": 1,
      "added_at": "2026-03-27T12:00:00Z"
    }
  }
}
```

Add `#[serde(deny_unknown_fields)]` at every layer of the persisted
shape, not only on `DiskMember`:

- `PoolMembership` (the outer struct holding `disks:
  LuksUuidMap<DiskMember>`): the attribute rejects unknown top-level
  keys in `pool.json` such as a hand-edited `schema_version` or
  `stale_field`. Without it, the outer container is transparent over
  `disks` and accepts arbitrary siblings silently. This guards against
  the "future schema-bump conversation tempts an implementer to add
  `version: 1` without the strictness gate" footgun.
- `DiskMember`: the attribute rejects stale value-side fields
  (`luks_uuid` is the concrete case the migration drops).
- `Journal` (in `journal.rs`, which carries `pre_membership` and
  `target_membership`): same reasoning. An unknown top-level key in
  `pending-op.json` must fail closed.
- Any other concrete persisted struct in the module.

The test plan pins this with four distinct cases: (a) an entry whose
value carries a stale `luks_uuid` field under an otherwise-valid UUID
key, (b) a top-level entry whose key is a disk name rather than a
UUID, (c) an unknown top-level key in `pool.json` alongside a
valid `disks` field, (d) an unknown top-level key in
`pending-op.json` alongside the valid journal fields.

### `LuksUuidMap`

Use a wrapper for every UUID-keyed map loaded from JSON:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct LuksUuidMap<V>(BTreeMap<LuksUuid, V>);
```

Its custom `Deserialize` must read raw string keys, parse/canonicalize each
key, and reject duplicate canonical keys before insertion. This prevents the
default `BTreeMap` last-wins behavior from dropping a member when JSON contains
both uppercase and lowercase spellings of the same UUID.

Duplicate canonical keys must fail with a serde error detail containing:

```text
duplicate LUKS UUID key after canonicalization: <canonical-uuid>
```

When this happens under `load_membership`, the outer
`MembershipError::Corrupt` display wraps that detail with the corrupt-file
remediation text specified below. Tests assert against the duplicate-key
substring so they catch a regression that accidentally stops canonicalizing
while still rejecting invalid UUID syntax.

Expose this public surface:

```rust
impl<V> LuksUuidMap<V> {
    pub fn new() -> Self;
    pub fn get(&self, uuid: &LuksUuid) -> Option<&V>;
    pub fn get_mut(&mut self, uuid: &LuksUuid) -> Option<&mut V>;
    pub fn contains_key(&self, uuid: &LuksUuid) -> bool;
    /// Inserts a new entry. Errors closed if `uuid` already maps to a value;
    /// callers must `remove` explicitly before re-inserting under the same UUID.
    pub fn insert(&mut self, uuid: LuksUuid, value: V) -> Result<(), LuksUuidMapConflict>;
    pub fn remove(&mut self, uuid: &LuksUuid) -> Option<V>;
    pub fn iter(&self) -> impl Iterator<Item = (&LuksUuid, &V)>;
    pub fn keys(&self) -> impl Iterator<Item = &LuksUuid>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}

impl<V> Default for LuksUuidMap<V> {
    fn default() -> Self { Self::new() }
}

impl<'a, V> IntoIterator for &'a LuksUuidMap<V> {
    type Item = (&'a LuksUuid, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, LuksUuid, V>;
}

#[derive(Debug, Error)]
#[error("duplicate LUKS UUID: {uuid} already in LuksUuidMap")]
pub struct LuksUuidMapConflict {
    pub uuid: LuksUuid,
}
```

`LuksUuidMapConflict` is a struct (not enum) because it has exactly one
failure mode. It implements `std::error::Error` via `thiserror::Error`
with `source() = None` (no wrapped cause -- the only field is the
colliding UUID). Callers wrap it:

- `PoolMembership::insert` translates `LuksUuidMapConflict { uuid }`
  into `MembershipError::Conflict(format!("uuid '{uuid}' already in
  use under UUID {uuid}"))`, matching the pinned `uuid '...' already
  in use ...` pattern in `Membership API`. The wrapped variant is
  flattened into the existing `MembershipError::Conflict(String)`
  surface rather than added as a new structured variant, because
  `PoolMembership::insert` already returns `MembershipError::Conflict`
  for the three other uniqueness axes (name, by-id, devid) and a
  separate variant for UUID would force every caller to pattern-match
  two near-identical error shapes.
- The `OpKind::Add` planning path that inserts into
  `LuksUuidMap<AddJournalTarget>` either (a) raises
  `AddError::DuplicateUuid` first (the cloned-disk operator-friendly
  shape pinned under `add.rs`) or (b) propagates the raw
  `LuksUuidMapConflict` as the defense-in-depth backstop. Both paths
  fail closed before any journal write.

`LuksUuidMap`'s deserialize duplicate-key error does NOT use
`LuksUuidMapConflict` -- serde's invariant is that custom deserialize
errors are `D::Error`. The duplicate-key contract is pinned at the
serde-error-string level (the substring
`duplicate LUKS UUID key after canonicalization: <canonical-uuid>`).

**No `FromIterator` impl (pinned).** `LuksUuidMap` intentionally omits
`FromIterator<(LuksUuid, V)>`. A `FromIterator` impl has to choose
between (a) silently overwriting on duplicate keys (the `BTreeMap`/
`HashMap` default), which is exactly the failure mode the fail-closed
`insert` is designed to prevent, or (b) panicking on duplicates, which
is unrepresentable in production code that must fail closed with a
`Result`. Construction paths instead build the map explicitly:

```rust
let mut map = LuksUuidMap::new();
for (uuid, value) in entries {
    map.insert(uuid, value)?;
}
```

The ordering of inserts is therefore the call-site's responsibility,
not the collection's. In `add.rs` planning (the only production site
that builds a `LuksUuidMap<AddJournalTarget>` from a list of disk
specs), iterate over `params.specs` in their parse-order and `insert?`
each one in sequence; the first collision wins -- which matches the
operator-visible error ordering pinned in `AddError::DuplicateUuid`
(sort by `(name, by_id)` lex on `by_id`). In `discover.rs` (the only
production site that builds a `LuksUuidMap<DiskMember>` from probe
output), iterate over the alias-deduped result in `by_id` lex order
and `insert?` each one in sequence. Tests use the same explicit
pattern through helper closures over fixture vecs.

The fail-closed `insert` signature is load-bearing for every UUID-keyed
persisted map, not only for membership. `OpKind::Add.targets` is a raw
`LuksUuidMap<AddJournalTarget>` -- it is **not** layered through
`PoolMembership::insert`'s additional uniqueness checks. With overwrite
semantics, two `PresentLuks` adoption targets pointing at dd-cloned
disks (distinct by-ids, distinct names, identical LUKS UUID) would land
under the same canonical key during Add planning: the second `insert`
silently drops the first from the journal, planning continues to
execute both, and recovery replays only the surviving entry while the
unrecorded target has already been opened and added to the pool. This
is the in-process analogue of the Deserialize duplicate-key contract
the wrapper already enforces, so the two paths now agree.

`PoolMembership::insert` continues to layer name/by-id/devid uniqueness
on top of UUID uniqueness; with `LuksUuidMap::insert` fail-closed,
membership inherits UUID-uniqueness without an extra check site.

Use `LuksUuidMap` for:

- `PoolMembership.disks`;
- `OpKind::Add.targets`;
- any future UUID-keyed persisted map.

### Membership API

Make `PoolMembership.disks` private. Provide helpers:

```rust
impl PoolMembership {
    pub fn empty() -> Self;
    pub fn by_uuid(&self, uuid: &LuksUuid) -> Option<&DiskMember>;
    pub fn by_uuid_mut(&mut self, uuid: &LuksUuid) -> Option<&mut DiskMember>;
    pub fn by_name(&self, name: &DiskName) -> Option<(&LuksUuid, &DiskMember)>;
    pub fn by_by_id(&self, by_id: &ByIdPath) -> Option<(&LuksUuid, &DiskMember)>;
    pub fn by_devid(&self, devid: u64) -> Result<Option<(&LuksUuid, &DiskMember)>, MembershipError>;
    pub fn iter(&self) -> impl Iterator<Item = (&LuksUuid, &DiskMember)>;
    pub fn names(&self) -> impl Iterator<Item = &DiskName>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn insert(&mut self, uuid: LuksUuid, member: DiskMember) -> Result<(), MembershipError>;
    pub fn remove_by_uuid(&mut self, uuid: &LuksUuid) -> Option<DiskMember>;
}
```

`insert()` and `load_membership()` enforce:

- UUID uniqueness;
- disk-name uniqueness;
- by-id uniqueness;
- non-`None` devid uniqueness.

**Check order (pinned).** When two or more axes would fire on the same
insertion, the check order is: (1) UUID, (2) disk-name, (3) by-id,
(4) devid. The first failing check is the one returned; subsequent
checks do not run. Rationale: order matches the on-the-wire schema --
UUID is the map key (the persistent identity), and name/by-id/devid
are value fields in their `DiskMember` declaration order. Operator
diagnostics consistently lead with the UUID, and tests that assert a
single-axis message pin each axis with a fixture engineered to
trigger exactly that axis. A regression that flipped the order would
lock in whatever ordering the first test happened to assert; pinning
the order explicitly here makes the contract testable independent of
test-suite drift.

`PoolMembership::insert` raises `MembershipError::Conflict(String)` for every
uniqueness violation. The inner string follows these patterns:

```text
uuid '<uuid>' already in use under UUID <uuid>
name '<name>' already in use under UUID <other-uuid> while inserting UUID <new-uuid>
by_id '<by-id>' already in use under UUID <other-uuid> while inserting UUID <new-uuid>
devid '<devid>' already in use under UUID <other-uuid> while inserting UUID <new-uuid>
```

The secondary uniqueness pass in `load_membership` uses the same insert path,
so duplicate value-side name/by-id/devid state fails with
`MembershipError::Conflict` naming the offending field, value, and colliding
UUIDs. The structured `DuplicateDevid` variant is reserved for `by_devid`
lookups against an already-constructed membership.

`by_devid()` fails closed with `DuplicateDevid` if corrupt membership contains
the same devid twice. The variant carries every colliding UUID so operator
diagnostics name all of them (a `[LuksUuid; 2]` shape would only name two
even when three or more entries share a devid, forcing the operator into
whack-a-mole):

```rust
MembershipError::DuplicateDevid {
    devid: u64,
    members: Vec<LuksUuid>,
}
```

`Display` and the persisted-ordering rule MUST emit `members` sorted by
canonical UUID lexicographic order (lowest UUID first). This is enough
for stable error reproducibility in tests and keeps the message stable
under map-iteration reordering.

`Display` text MUST be:

```text
duplicate devid {devid} in pool membership across UUIDs {uuid1}, {uuid2}[, {uuid3}, ...]
```

with every colliding UUID enumerated in canonical lexicographic order
and joined by `, ` (no Oxford comma, no truncation). The
`RecoverError::DuplicateDevidDuringReplay` message reuses this body
verbatim by including the same `duplicate devid <devid>` substring; tests
assert the substring rather than the full sentence.

Cardinality of a real braid pool is small (~12 members on a NAS), so
`by_devid` and `by_by_id` are O(n) scans over the map. The simplicity
outweighs the cost of maintaining a secondary index.

**`MembershipError` variant inventory (pinned).** The migrated enum has
exactly these variants, no more, no less:

```rust
#[derive(Debug, Error)]
pub enum MembershipError {
    #[error("pool membership file corrupt at {path}: {detail} -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)")]
    Corrupt { path: PathBuf, detail: String },

    #[error("{0}")]
    Conflict(String),

    #[error("duplicate devid {devid} in pool membership across UUIDs {}", format_uuid_list(.members))]
    DuplicateDevid { devid: u64, members: Vec<LuksUuid> },

    #[error("failed to read pool membership file at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to write pool membership file at {path}: {source}")]
    Save { path: PathBuf, #[source] source: std::io::Error },
}
```

Pre-migration `MembershipError` carries IO/Parse/Save-shaped variants
on master. The migration:

- Folds the existing `Parse` variant into `Corrupt` (the new variant
  carries the serde detail as `detail: String`; old `Parse` is gone).
- Retains `Io` and `Save` with their existing role: read-side IO
  failure that is NOT a parse error (file missing where required,
  EACCES, EIO) and write-side IO failure on `save_membership`. The
  `{path}` field is the same `pool.json` path the rest of the enum
  carries; the `{source}` chain preserves the underlying error for
  `anyhow` context.
- Adds `Conflict(String)` and `DuplicateDevid` (specified above).

No catch-all `Other(anyhow::Error)` and no `From<serde_json::Error>`
generic conversion: every error path that bubbles up to `Display`
maps to exactly one of the five variants above so operator-facing
wording is enumerable. Tests pin one example per variant.

`MembershipError::Corrupt` shape is pinned:

```rust
#[error("pool membership file corrupt at {path}: {detail} -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)")]
Corrupt {
    path: PathBuf,
    detail: String,
}
```

- `path` is the on-disk `pool.json` path captured at the failing
  `load_membership` site.
- `detail` is the underlying serde error rendered through `{e}` (the
  default `Display`), not `{e:#}` (which is the `anyhow` chain form):
  the default form is one line and round-trips cleanly into the
  pinned operator-facing message above. Tests assert on the
  `{path}` and the leading text, not on the serde-detail body.
- The variant does NOT carry a wrapped `serde_json::Error`. Therefore
  the `Error::source()` chain returns `None` for `Corrupt`. Operator
  output (which renders via `Display`) sees the formatted message
  exactly as pinned, and the `anyhow` chain does not append a
  duplicate "caused by: ..." line.

This shape rules out the three implementer choices the reviewer
called out: `{path, detail}` over single `String`; `source() = None`
over wrapping the serde error; `{e}` over `{e:#}` for the embedded
detail. The display text is operator-facing contract:

```text
pool membership file corrupt at {path}: {detail} -- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)
```

The Single-User Cutover guidance and the old-shape `pool.json` test must
reference this exact remediation string rather than restating their own
wording. The "with all intended pool members attached" clause is
load-bearing: an operator who runs `discover --write` while a member
is detached lands a `pool.json` short that member, and after the next
`unlock` "fix forward" is the only recovery path. The clause is
duplicated inline in the error rather than delegated to
`docs/luks-unlock.md` because the message is the first thing the
operator sees and many will run the suggested command without
reading the doc.

## LUKS Format Boundary

Fresh LUKS formats must have identity before `cryptsetup luksFormat` runs.

Change `CmdRequest::CryptsetupLuksFormat` from raw extras only to structured
managed fields plus validated extras:

```rust
CryptsetupLuksFormat {
    device: String,
    uuid: LuksUuid,
    label: String,
    extra_opts: LuksFormatExtraOpts,
}
```

`label` is typed `String` (not a new `LuksLabel` newtype). The value
is always derived at the call site as `format!("braid-{}", name)`
where `name: &DiskName`. `DiskName` already validates the
`braid-`-eligible character set, so the constructed label inherits
the validation; a separate `LuksLabel` newtype would not add an
invariant that `DiskName` does not already enforce. The `String`
typing matches today's `cmd.rs` shape for tool-argv strings and
keeps the migration mechanical at this surface. If a future surface
needs to round-trip a label observed from cold disks (where the
`braid-` prefix is the only contract), introduce `LuksLabel` then;
do not pre-introduce it here.

`cmd.rs` renders:

```text
cryptsetup luksFormat --type luks2 --batch-mode --key-file=- \
  --uuid <uuid> --label <label> \
  <validated extra opts...> <device>
```

Because `extra_opts` rejects managed fields, argv ordering cannot let user
input override the journaled identity or the braid label.

Pre-generating the UUID before `luksFormat` runs lets `OpKind::Add` record
the authoritative UUID from t=0 as a single map key, eliminating two-stage
journal entries. It also makes a re-run of `luksFormat` after a mid-format
crash produce the same UUID, which makes recovery's
`BtrfsDeviceScanForget` semantics cleaner: the device-scan-forget set is
stable across retries.

Call-site changes:

- `main.rs`: parse `LuksFormatArgs.luks_format_extra_opts` into
  `LuksFormatExtraOpts` before constructing `AddParams` or `ReplaceParams`.
  `AddParams` and `ReplaceParams` should accept `&LuksFormatExtraOpts`, not
  `&[String]`.
- `add.rs`: generate `LuksUuid::new_v4()` for each fresh target while planning,
  before journal write. Store `label = braid-<name>` as a structured
  `CryptsetupLuksFormat` request field, derived from the target name at the
  call site. Delete the current `--label` injection into a raw `Vec<String>`
  at `add.rs:1533-1536` (inside the `ConfigDiskState::PresentNotLuks` arm of
  fresh-LUKS planning, where the injection lives inline rather than in a
  helper). Do not store a separate label string in the journal.
- `replace.rs`: same for a fresh replacement target. Delete
  `effective_luks_format_opts` (`replace.rs:1052-1057`) and route the
  structured `label` field through `CryptsetupLuksFormat` directly. Both
  `build_replace_work_plan` (`replace.rs:1104-1107`) and
  `build_replace_journal_target` (`replace.rs:1164`) call sites move with
  the helper; the previously injected `--label braid-<name>` becomes a
  structured field derived from `new_name` at the same call site that
  constructs the `CryptsetupLuksFormat` request.
- `luks.rs`: `luks_format()` accepts `uuid`, `label`, and
  `&LuksFormatExtraOpts`, and passes them through the request type.
- Journal replay uses the same structured request fields. It must never
  reconstruct managed fields from raw `extra_opts`.

### `CmdRequest::CryptsetupLuksUuid` request shape (pinned)

The double-drift close probe (Replace/Remove/recover) and the
ExistingLuks new-target re-probe (Replace open-boundary) both need to
ask `cryptsetup luksUUID <device>` against either a mapper path or a
by-id path. The migration introduces one unified `CmdRequest` variant
with a single `device: String` field, NOT a two-variant split or a
typed `MapperName`/`ByIdPath` enum:

```rust
CryptsetupLuksUuid { device: String }
```

The field is `String`, not `MapperName` or `ByIdPath`, because the
cryptsetup CLI accepts either a `/dev/mapper/<name>` path or a
`/dev/disk/by-id/<id>` path through the same positional argument --
the runner does not (and should not) introspect the form. Call sites
render their input through `MapperName::Display`/`ByIdPath::as_str()`
at the request-construction boundary and pass the resulting `String`:

- Close double-drift probe call sites
  (`replace.rs:707`, `recover.rs:2935`, `remove.rs:180-183`):
  `CryptsetupLuksUuid { device: format!("/dev/mapper/{}", mapper.as_str()) }`
  where `mapper: &MapperName` is the observed (journaled or probed)
  mapper for that call site.
- ExistingLuks new-target open-boundary probe in `replace.rs::execute`
  (the two sites at `replace.rs:534` and `replace.rs:592`):
  `CryptsetupLuksUuid { device: new_target.by_id.as_str().to_owned() }`
  where `new_target.by_id: &ByIdPath`.
- Stranded-mapper resolution inside the lock close-set builder (the
  per-stranded-mapper `cryptsetup luksUUID` issued during the
  member-owned/orphan classification pass): same as the close probe
  shape -- mapper-form `device`.

The plan's earlier shorthand `CryptsetupLuksUuid { mapper: ... }`
versus `CryptsetupLuksUuid { device: ... }` was inconsistent --
read every prior reference as `device: <rendered-string>`. Unit tests
that pin per-site probe behavior assert the literal `device` string
the runner observes, so a regression that flipped to a typed variant
would either fail to compile (call sites use `device:`) or re-render
the form differently and fail the literal-equality test.

### Recording/dry-run runner trait surface (pinned)

The recording `CommandRunner` test sink already pattern-matches each
`CmdRequest` variant by name; the new and migrated variants add the
following obligations to the test surface:

- `CryptsetupLuksFormat { device, uuid, label, extra_opts }`: the
  recording sink stores the full structured shape. Test assertions
  on this variant inspect `uuid: LuksUuid`, `label: String`, and
  `extra_opts: LuksFormatExtraOpts` directly via field access, NOT
  by reparsing a rendered argv. The positive-extras forwarding
  regression test (under Test Plan) asserts the structured
  `extra_opts` contains the user-supplied non-managed tokens
  unchanged.
- `CryptsetupLuksUuid { device }`: the recording sink stores the
  single `device: String` field. Tests that wire a pretend probe
  return value (the double-drift and open-boundary regression
  tests) key their lookup by the `device` string they expect.
  Recording-runner contract: by default each `CryptsetupLuksUuid`
  request returns a recorded canned UUID indexed by the `device`
  field (the test wires the mapping); a missing entry returns a
  recorded canned error indexed the same way. This mirrors the
  existing `CryptsetupLuksDumpText` recording shape (the test
  case for `cryptsetup luksDump` already keys by device path).
- Dry-run preview: `CryptsetupLuksUuid` requests are PROBE-shape
  (they read state, they do not mutate), so they MUST NOT appear
  in any dry-run preview output. The existing dry-run rendering
  policy is to render only mutating `CmdRequest` variants; the
  new `CryptsetupLuksUuid` variant follows that policy and the
  per-site call sequence is hidden from preview output. Snapshot
  files therefore see no shape change from this variant.

## Journal Schema

No version bump and no compatibility shim. Old journals fail to parse with an
actionable remediation.

Apply `#[serde(deny_unknown_fields)]` at two places, not "variant by
variant":

1. On the `OpKind` enum container itself. For a `#[serde(tag = "op")]`
   internally-tagged enum, this container attribute applies to the
   flattened variant object, so unknown top-level keys alongside the
   discriminator (`op`) fail closed.
2. On every concrete struct embedded inside an `OpKind` variant -- in
   particular `AddJournalTarget` and `ReplaceJournalTarget`. These
   structs are independent containers and need their own attribute.

`#[serde(deny_unknown_fields)]` as a variant attribute is not legal
serde syntax and must not appear in the implementation.

A hand-edited `pending-op.json` whose Add target resurrects the
removed `luks_uuid`, `mapper_name`, `label`, or `luks_label` value field MUST fail
`load_journal` with `JournalError::Parse` that names the unknown
field, not be silently ignored. The test plan pins this behavior so a
future "ergonomic" removal of the attribute is visible.

**`Option<PathBuf>` serde policy (pinned).** Both `AddJournalTarget`
and `ReplaceJournalTarget` carry `enroll_key_file: Option<PathBuf>`
inside their `mode` variants. `None` MUST serialize as
`"enroll_key_file": null` (the default serde behavior); do NOT add
`#[serde(skip_serializing_if = "Option::is_none")]`. Two reasons:

- With `deny_unknown_fields` on the containing structs, both forms
  deserialize identically -- omitted and `null` both round-trip to
  `None`. The choice is whether the on-disk schema is explicit
  about absence. Explicit `null` is the cheaper form to reason
  about (every committed VM-test golden JSON shows every field,
  no implicit-by-absence semantics).
- A forgotten `skip_serializing_if` is the failure mode the
  reviewer flagged: cross-version diffs in committed VM-test golden
  output that do not flip behavior but do flip the bytes. The
  default policy (always emit, even `null`) is a stable wire format
  by construction; the implementer cannot accidentally toggle it
  by adding or removing the attribute. The "removed value-side
  fields fail to deserialize" tests (the
  `deny_unknown_fields` regression cases for resurrected
  `luks_uuid`/`mapper_name`/`label`/`luks_label`) are about the
  schema shape -- they are unaffected by this choice because the
  test inputs include the resurrected fields explicitly.

If a future serde version makes `skip_serializing_if` the project
default at the workspace level, this rule MUST be honored as a
local override on every `Option<PathBuf>` journal field, with a
comment pointing to this section. Symmetric policy applies to any
new `Option<_>` value-side field added to journal variants by
future changes.

Schema rules:

- Add:
  - `OpKind::Add { targets: LuksUuidMap<AddJournalTarget>, phase }`.
  - `AddJournalTarget` contains `name: DiskName`, `by_id: ByIdPath`, and
    `mode: AddJournalMode`. All mode-specific data lives inside the variant;
    the target struct itself never carries `luks_uuid`, `mapper_name`,
    `label`, `luks_label`, or `extra_opts`.
  - `AddJournalMode::FreshLuks { extra_opts: LuksFormatExtraOpts,
    enroll_key_file: Option<PathBuf> }`: the structured `extra_opts` lives
    inside the variant where it is used, mirroring the existing
    nested-mode shape (`luks_format_extra_opts` already sat inside the
    fresh-luks variant on master). The migration replaces the raw
    `Vec<String>` with `LuksFormatExtraOpts` and drops the now-redundant
    `luks_label` field.
  - `AddJournalMode::RecoverableBraidLabeled { verified_pool_fsid: String,
    enroll_key_file: Option<PathBuf> }`: the migration drops only the
    nested `luks_uuid` field (identity moves to the `targets` map key).
    `verified_pool_fsid` is retained: it backstops the FSID cross-check
    in the Add-recovery `RecoverableBraidLabeled` arm at
    `recover.rs:2304-2311` (`visible_btrfs_fsid(...) != verified_pool_fsid
    => RecoverError::Failed`), which the UUID gate does not subsume.
    Adoption variants do not carry `extra_opts` because there is no
    format step to configure.
  - Replay derives the label as `format!("braid-{}", target.name)` at the
    same call site that constructs the `CryptsetupLuksFormat` request. The
    structured `extra_opts` inside `FreshLuks` is the only journaled
    cryptsetup configuration. Replay also derives
    `mapper_name(&target.name)` for the expected steady-state mapper.
- Remove:
  - `OpKind::Remove { luks_uuid: LuksUuid, name: DiskName }`.
  - `name` is for logging only.
- Remove missing:
  - Keep `devid` because btrfs exposes no UUID for a truly missing device.
  - Recovery and execution resolve `devid -> LuksUuid` through persisted
    membership before removing the member.
- Replace:
  - `OpKind::Replace { old_uuid, old_name, new_uuid, new_name, new_target,
    source, restore_raid1_after_commit, phase }`.
  - `ReplaceJournalTarget` contains `by_id: ByIdPath` and `mode:
    ReplaceJournalMode`. The target struct itself carries no
    `mapper_name`, no value-side UUID, no stored label, and no
    `extra_opts`.
  - `ReplaceJournalMode::FreshLuks { extra_opts: LuksFormatExtraOpts,
    enroll_key_file: Option<PathBuf> }`: structured `extra_opts` lives
    inside the variant, mirroring the Add path and the existing
    nested-mode shape. The `luks_label` field on master is dropped;
    replay derives the label as `format!("braid-{}", new_name)` when
    constructing `CryptsetupLuksFormat`.
  - `ReplaceJournalMode::ExistingLuks { enroll_key_file: Option<PathBuf> }`:
    the migration drops only the nested `luks_uuid` field (identity
    moves to the op-level `new_uuid`). `enroll_key_file` is retained
    with its current semantics. Adoption variants do not carry
    `extra_opts` (no format step to configure).
  - Existing-LUKS new targets get `new_uuid` from the preflight probe.
    Fresh-LUKS new targets get `new_uuid` from `LuksUuid::new_v4()`.

The existing phase enums (`AddPhase`, `ReplacePhase`, `RemoveMissingPhase`)
and the `ReplaceJournalSource` enum are unchanged in shape and semantics --
the migration is identity-key-only. Replay state machines do not move.

`ReplaceJournalSource::Live.old_mapper` is retained deliberately. It
is not consulted for identity decisions (those move to `old_uuid` at
the op level), and it is not consulted for `btrfs replace start`
(which uses the observed live `devid`). It IS consulted by
`replace.rs`'s post-commit `close_mapper_best_effort` call -- this is
a Pattern #1 use: a kernel device path for a close operation. The
field is the **observed** mapper at replace-planning time, journaled
so the post-commit close survives a recovery replay. This mirrors
`lock.rs`'s "close observed, not reconstructed" doctrine for the
same drift-safety reason: if the operator drifted the mapper between
plan and post-commit close, journaling the observed mapper means the
close still targets the right dm slot. Add a doc comment on the
field stating the Pattern #1 role and the parallel to lock.rs.

**Double-drift defense-in-depth UUID probe.** Journaling the observed
mapper closes the single-drift gap (operator renames the mapper
between plan and post-commit close). It does not close the
double-drift gap: between plan and post-commit close (or any recovery
replay of the close, which has an even wider window) the operator
could close `old_mapper` and re-open a different physical disk under
the same mapper name. The `CryptsetupClose { mapper: old_mapper }`
call would then target the operator's foreign disk.

Before issuing `CryptsetupClose` at `replace.rs:707`, the
symmetric recovery-replay site at `recover.rs:2935`, AND the analogous
post-commit close in `remove.rs:180-183`, the executor MUST probe
`cryptsetup luksUUID /dev/mapper/<mapper>` and require the result to
equal the journaled op-level UUID (`old_uuid` for Replace, `luks_uuid`
for Remove -- both are sourced from the same identity invariant). On
mismatch (or on probe failure -- the mapper was already closed, or the
kernel returned an unexpected status), the executor MUST log a warning
naming the mapper, the journaled UUID, and the observed UUID (or probe
error), then SKIP the `CryptsetupClose` request. Rationale: if the dm
slot no longer holds the journaled identity it is the operator's to
manage, and braid blindly closing it would be the data-loss
escalation. This is defense-in-depth, not a primary safety mechanism
-- the primary mechanism remains the journaled observed mapper.

The probe applies to the post-commit close path only. For Replace the
`expected_present_identities`-equivalent validation that fires before
`btrfs device remove` already enforces UUID at the mutating step, and
for Remove the explicit `validate_pool_topology` gate (sourced from
`work_plan.expected_present_identities` at `remove.rs:322-328`) does
the same; this carve-out is scoped to the cleanup close that runs
after the btrfs commit. The Remove close runs in-process immediately
after `btrfs device remove`, so its drift window is narrower than
Replace's recovery-replay window -- but the in-process Replace close
at `replace.rs:707` has the same shape as the Remove in-process close,
and applying the probe to the former while omitting it from the
latter would be a silent asymmetry. Apply the probe at all three
sites. Mirror the same defense-in-depth probe at the
recover.rs:2935 site so a long-delayed recovery replay inherits it.
Remove has no recovery-replay path that re-issues its close
(`OpKind::Remove` is intentionally skipped by `replay_post_mutation`
at `recover.rs:1771-1783` and the operator's recovery path is to
re-run `braid remove`), so no recovery-side mirror is needed for
remove.

**Accepted risk: probe-to-close TOCTOU window.** The double-drift
defense-in-depth probe issues `cryptsetup luksUUID /dev/mapper/<mapper>`
and then `CryptsetupClose` against the same mapper as two separate
ioctls. Between the two calls an operator could `cryptsetup close`
the journaled identity and `cryptsetup open` a foreign disk under
the same mapper name, causing the close to target the foreign dm
slot. The window is microseconds in the in-process case
(`replace.rs:707`, `remove.rs:180-183`) and bounded by the recovery
replay's own timing in the long-delayed case (`recover.rs:2935`).
The probe closes the single-drift gap (operator drifted the mapper
between plan and post-commit close) and the broad recovery-replay
gap; it does NOT claim to close every conceivable concurrent
operator action. This is left uncovered for the same reasons the
in-process lock close double-drift is left uncovered (see the
"Accepted risk: in-process member-owned close double-drift" paragraph
under `lock.rs`): the hazard surface is "operator closes a foreign
mapper", `cryptsetup close` tears down only the dm slot without
modifying the foreign disk's bytes, and the trigger requires
concurrent operator action against the same mapper during a running
braid command. Document the gap explicitly so a future reader does
not treat the probe as airtight and remove an adjacent safety net
on the "we already probe before close" assumption.

The `Journal` top-level struct embeds two `PoolMembership` snapshots,
`pre_membership` and `target_membership`. Both rekey transitively when
`PoolMembership` rekeys; explicit round-trip coverage in the test plan must
exercise a journal carrying non-empty snapshots in both fields, otherwise the
canonicalizing `Deserialize` could silently regress only inside the journal.

`JournalError::Parse` display text must include:

```text
failed to parse pending-op.json: <serde-error>. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.
```

Add the matching docs section in `docs/luks-unlock.md`.

**`JournalError` variant inventory (pinned).** The migrated enum has
exactly these variants:

```rust
#[derive(Debug, Error)]
pub enum JournalError {
    #[error("failed to parse pending-op.json: {detail}. Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.")]
    Parse { detail: String },

    #[error("failed to read pending-op.json at {path}: {source}")]
    Io { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to write pending-op.json at {path}: {source}")]
    Save { path: PathBuf, #[source] source: std::io::Error },
}
```

`Parse` is the only variant whose `Display` text the migration pins
verbatim; `Io` and `Save` preserve the existing role they have on
master (read/write IO failure outside the parse path). No catch-all
`Other` variant. Tests pin one example per variant.

**`OpKind::Remove.name` log surfaces (pinned).** `name` is "for
logging only" in the journal-schema sense (identity decisions read
`luks_uuid`), but the field must be explicitly surfaced at a
defined set of sites so tests pinning log substrings are
non-ambiguous. The `name` field is consumed by:

- `remove.rs` `info!`/`println!` progress messages at the start
  of the operation (`"removing disk <name>"`) and after the
  post-commit close (`"removed disk <name> (uuid <uuid>)"`).
- `recover.rs` Remove-recovery diagnostic surfaces -- though
  `OpKind::Remove` is intentionally skipped by
  `replay_post_mutation` (`recover.rs:1771-1783`), the journal
  body is still rendered in `braid recover` planning output and
  in `RecoverError` bodies that quote the operation under
  consideration; both must read `name` from the op, not
  reconstruct it from membership.
- `journal.rs::Display`-shaped helpers and any future
  `pending-op.json` inspect commands.

The field is NOT consumed by:

- Any planning, gating, or identity decision in `remove.rs`
  (those read `luks_uuid` exclusively).
- Any live-pool correlation (those read
  `PoolDevice.luks_uuid`/`devid`).

A grep contract for the reviewer audit: after the migration,
`OpKind::Remove`'s `name` field has exactly the call sites above
(progress messages, recovery-diagnostic rendering, future inspect
commands). A regression that uses `name` for a planning decision
is a contract violation; the test for Remove identity
(`remove resolves name to UUID at the boundary and removes by UUID`)
plus the drifted-mapper test exercise the negative case.

**Accepted risk: journal-as-identity trust surface.** Post-migration,
the journaled `pre_membership`/`target_membership` snapshots are
load-bearing for identity decisions in two specific ways: (a)
`RemoveMissing` recovery resolves `devid -> LuksUuid` through
`journal.pre_membership.by_devid(devid)` (and the analogous
`target_membership` lookup in the post-mutation phase); (b) the
recovery rebuild path admits live UUIDs into the rebuilt membership
only when the UUID appears in the journal's "expected for the
current recovery phase" set. The trust boundary therefore moved
from `pool.json` (which `discover --write` can rebuild from live
disks) to `pending-op.json` (which has no rebuild story besides
"delete and re-run after manual reconciliation").

The plan accepts this trust expansion rather than adding a
live-corroboration cross-check against journal-snapshot UUIDs. Two
reasons:

- Legitimate post-commit recovery flows have `pre_membership`
  entries that intentionally have no live observation -- a
  `Remove` whose commit landed but whose journal has not yet been
  cleared has the removed UUID in `pre_membership` and nowhere in
  `pool.devices`, `pool.missing_devids`, or `pool.null_underlying`.
  A broad "every snapshot UUID must have live corroboration" check
  would deadlock that flow at the topology-mismatch arm.
- The structural defenses for accidental corruption are already in
  place: `deny_unknown_fields` on `Journal` rejects unknown keys;
  the structured `RecoverError::DuplicateDevidDuringReplay` and
  `RecoverError::NoMemberForJournaledDevid` variants surface
  duplicate or missing devid resolutions instead of silently
  removing the wrong member; and `live_pool_matches_membership`
  routes corruption into structured errors rather than the generic
  topology-mismatch wording. These cover the "coherent but malicious
  value substitution" failure modes the `deny_unknown_fields`
  attribute cannot catch on its own.

The residual risk is a coherent hand-edit of `pending-op.json` that
either (i) inserts a foreign UUID into `target_membership` and
attaches a physical foreign disk with that exact UUID before the
recovery rebuild runs, or (ii) changes a `DiskMember.devid` inside
`pre_membership` so the `devid -> LuksUuid` resolution returns a
different existing member's UUID. Both require operator action
against `pending-op.json` directly (a file the operator-facing
remediation documentation in `docs/luks-unlock.md` explicitly tells
the operator NOT to edit), AND a second concurrent condition
(foreign physical disk; existing member sharing the substituted
devid). Mitigation is the operator-facing contract in
`docs/luks-unlock.md`: do not hand-edit `pending-op.json`; remove
it after manual reconciliation only. Surface this risk in the
Risk Register and in `docs/luks-unlock.md`.

## Command Migration

### Shared Patterns

- Boundary commands that accept a user disk name resolve it once with
  `membership.by_name(&DiskName)`, clone the UUID, and pass UUID downstream.
- Code that constructs a mapper path uses `mapper_name(&member.name)` only
  when it is addressing braid's expected mapper.
- Code that consumes observed live pool state uses:
  - `PoolDevice.luks_uuid -> membership.by_uuid(...)` when present;
  - `NullUnderlyingDevice.devid -> membership.by_devid(...)` when the backing
    device is `(null)`;
  - `missing_devids -> membership.by_devid(...)` for missing remove/replace.
- Code that rebuilds membership from live state inserts entries under the live
  `PoolDevice.luks_uuid`; it finds display name and prior `added_at` from the
  expected membership or journal by UUID, not by mapper name. "Rebuilds" here
  means the recovery paths that authoritatively reconstruct the membership
  set from live btrfs topology (the `recover.rs` rebuild described below);
  it does NOT mean `enrich_from_pool_state`, which is the in-process update
  path with a stricter foreign-UUID policy specified in the `membership.rs`
  section.
- Remaining `name_from_mapper` uses are display-only fallbacks or discover/add
  adoption gates. Add comments at those sites.

Every name-keyed call site in the current codebase falls into one of five
patterns. Each has a single mechanical translation; use this taxonomy as
the migration cheat sheet during the per-file sweep:

1. **`mapper_name(name)` for kernel device path** -- no semantic change.
   The call site continues to construct `braid-<name>` because `name` now
   comes from the in-memory `DiskMember.name` field rather than being
   parsed out of a path. Callers fetch `member.name`, then call
   `mapper_name(&member.name)`. UX/kernel-debug only.
2. **`name_from_mapper(path)` for correlation** -- replace with
   `membership.by_uuid(&dev.luks_uuid)` for present devices, or
   `membership.by_devid(dev.devid)` for `NullUnderlyingDevice` and
   `missing_devids`. Never parse a name back out of a path to decide
   membership.
3. **`name_from_mapper(path)` for display only** -- keep, but add a
   doc comment at each surviving site noting it must not be used for
   identity decisions. Lock-side status output and error messages live
   here.
4. **`d.mapper == mapper_name(name)` find-by-mapper** -- replace with
   `d.luks_uuid == target_uuid`. The target UUID is resolved once from
   `membership.by_name(&user_input_name)` at command entry and threaded
   through.
   - Concrete site: `resolve_replace_source` (`replace.rs:1226-1235`)
     today finds the live source with `pool.devices.iter().find(|d|
     d.mapper == *old_mn)` where `old_mn = mapper_name(&old_name)`, and
     then stores `ReplaceSource::Live { mapper: old_mn.clone(), devid
     }`. Two distinct changes are required here:
     1. The find predicate flips to `d.luks_uuid == old_uuid`, where
        `old_uuid` is the UUID resolved once from
        `membership.by_name(&old_name)` at command entry (Pattern 4 as
        stated).
     2. The `mapper:` field assigned to `ReplaceSource::Live` (and
        propagated into `ReplaceJournalSource::Live.old_mapper` by
        `build_replace_journal_target`) MUST be cloned from the
        matched device's observed `PoolDevice.mapper`, **not** from
        the reconstructed `old_mn`. This is the "close observed, not
        reconstructed" doctrine from lock.rs applied at the planning
        boundary: if the operator has drifted the mapper between plan
        and post-commit close, journaling the reconstructed name
        leaves the post-commit `close_mapper_best_effort` call at
        `replace.rs:707` targeting the wrong dm slot. Mapper drift
        between plan and post-commit close then reopens the same
        leak the lock.rs migration closes.
   - Concrete site: `execute_replace_post_maintenance_recovery`
     (`recover.rs:2992`) today does `pool.devices.iter().find(|d|
     d.mapper == new_mn)` where `new_mn = config::mapper_name(new_name)`
     to locate the new disk's `devid` for the post-commit
     `pool_resize_device` call. The find predicate flips to
     `d.luks_uuid == new_uuid`, where `new_uuid` is the op-level field
     added by this migration (`OpKind::Replace.new_uuid`). Unlike the
     `:1226-1235` site, only the find predicate changes here -- no
     `mapper:` field is journaled at this point because the post-commit
     resize does not address a mapper string. A regression that left
     this site as a mapper-name find would silently fail to locate
     the new disk on benign mapper drift, returning the
     "could not find new disk ..." error path; the regression test
     pinned in the Test Plan exercises exactly this case.
5. **Literal `"braid-"` prefix strip** -- all display fallbacks. Keep
   as-is; never an identity decision.

### `membership.rs`

- Replace the struct shape and helper API.
- Rewrite `enrich_from_pool_state` to correlate by UUID.
  - Policy (load-bearing for operator trust): `enrich_from_pool_state`
    MUST NOT insert a new UUID-keyed entry under any circumstance. It
    only updates `devid` and (where applicable) `added_at` for entries
    whose UUID is already present in membership.
  - Live `PoolDevice.luks_uuid` values not present in membership are
    "foreign" and MUST be surfaced as a logged warning naming the
    foreign UUID and the observed mapper; they are NOT admitted into
    membership and the existing entries in membership are NOT silently
    removed for missing them. The function is "best-effort" only in the
    sense that it tolerates partial live state; it is not best-effort
    on the foreign-admission axis.
  - The eprintln warning is necessary but not sufficient. After
    `enrich_from_pool_state` returns, the next mutating command runs
    against the original membership and the warning rolls past in
    stderr; an operator who does not happen to be watching never sees
    that a foreign UUID was tolerated, and the next `lock` will close
    the foreign mapper as orphan with no persistent surface in between.
    `braid doctor` MUST therefore expose a structured non-zero check
    that fires whenever the most recent `probe_pool` observation
    contains a `PoolDevice.luks_uuid` not present in membership. The
    check name is `foreign-luks-uuid`; its `CheckResult` body names
    every foreign UUID and its observed mapper.
  - **Foreign-UUID plumbing (pinned: returned alongside).** The set
    of foreign UUIDs observed during enrichment is returned alongside
    the membership update, NOT routed through a thread-local. The
    return shape is:

    ```rust
    pub struct EnrichmentReport {
        pub foreign: BTreeMap<LuksUuid, MapperName>,
    }

    pub fn enrich_from_pool_state(
        membership: &mut PoolMembership,
        pool: &PoolState,
    ) -> Result<EnrichmentReport, MembershipError>;
    ```

    Rationale: a thread-local is invisible to callers, untestable
    without unsafe global state manipulation, and breaks any future
    parallel-command code path. Returning the report alongside the
    update is one extra field on the return type that every caller
    can ignore (the `Ok(_)` arm) or forward (the doctor/status
    caller). `refresh_pool_metadata` similarly returns an
    `EnrichmentReport` (or a wrapper containing it plus its other
    return data); the doctor render layer reads
    `report.foreign` and renders the `foreign-luks-uuid` check from
    it. Pin with a unit test that builds a `PoolState` with one
    foreign-UUID device and asserts the returned
    `EnrichmentReport.foreign` contains exactly the foreign UUID
    mapped to its observed mapper. A second test asserts the
    `foreign-luks-uuid` doctor check renders non-zero with the
    foreign UUID and observed mapper in its body when given that
    `EnrichmentReport`.

  - **Foreign-UUID warning wording (pinned).** The `eprintln!`
    emitted by `enrich_from_pool_state` for each foreign UUID is
    exactly:

    ```text
    Warning: live LUKS UUID {uuid} observed at mapper {mapper} is not in pool membership; not admitting (run 'braid doctor' for the structured report)
    ```

    `{uuid}` is the canonical lowercase hyphenated form;
    `{mapper}` is the observed `PoolDevice.mapper` rendered through
    `MapperName::Display`. The warning is emitted at the
    `enrich_from_pool_state` site, not at every caller's render
    site, so the operator-visible string is identical regardless of
    entry point. Tests pin the substring
    `live LUKS UUID <uuid> observed at mapper <mapper> is not in pool membership`.
  - Pin this with two unit tests:
    - **Foreign live UUID does not admit**: a live `PoolDevice` with a
      UUID `U_F` not in membership; assert (a) `enrich_from_pool_state`
      returns success with membership byte-for-byte unchanged, (b) the
      function logs a warning naming `U_F`, (c) no entry under `U_F`
      appears in the resulting membership.
    - **Known UUID with new devid updates in place**: a live
      `PoolDevice` whose UUID `U_K` is in membership but with a new
      `devid` value; assert the entry under `U_K` now carries the new
      devid and no other field changed. (Mirrors today's enrichment
      contract on the legitimate path.)
  - The recovery-side rebuild in `recover.rs` (which DOES insert
    entries under live UUIDs as part of authoritative membership
    reconstruction during replay) is a separate code path with its
    own UUID-admission rules described in the `recover.rs` section.
    The two paths are intentionally asymmetric: `enrich_from_pool_state`
    runs in normal-path commands and must not be a backdoor for
    foreign-UUID admission, while recovery rebuilds are scoped by
    journal phase and operator-attested pre/target membership.
- Add a helper for null-underlying display enrichment that uses `by_devid`.
- `refresh_pool_metadata` remains best-effort, but corruption from invalid
  persisted keys/values must be surfaced as a warning and must not rewrite a
  partially loaded membership.

  **Warning sink (pinned).** The corruption warning goes through the same
  `eprintln!("Warning: ...")` sink today's `refresh_pool_metadata` already
  uses at `cli/src/membership.rs:211, 217` for load and save failures.
  Concretely: when `load_membership(paths)` returns
  `Err(MembershipError::Corrupt { path, detail })`, the function emits
  `eprintln!("Warning: pool membership file corrupt at {path}: {detail}
   -- run 'braid discover --write' to rebuild from existing disks (with
  all intended pool members attached; see docs/luks-unlock.md)")` (i.e.
  the `Corrupt` `Display` rendered with a `Warning: ` prefix to match the
  existing two warning lines in the function), then returns without
  calling `enrich_from_pool_state` or `save_membership`. Do NOT introduce
  a new `DiscoverWarning`, `MembershipWarning`, or doctor-diagnostic
  stream variant: `refresh_pool_metadata` is called from many entry points
  (status, doctor, mount, unlock, the recovery paths), and centralizing
  the message at the `eprintln!` site keeps the operator-visible string
  identical at every entry point. The full `MembershipError::Corrupt`
  remediation phrase reaches the operator -- a softer or truncated
  message would diverge from the hard-error path the operator sees if
  they then run a command that calls `load_membership` directly.

  **Sidecar snapshot on first corruption discovery (pinned).**
  BEFORE returning from the warning path, `refresh_pool_metadata`
  MUST attempt to copy the on-disk `pool.json` to a timestamped
  sidecar `pool.json.corrupt-<RFC3339-UTC>` adjacent to the
  original. The timestamp is rendered as RFC3339 UTC with
  second precision and a literal `Z` suffix:
  `chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()`
  (or the `time` crate equivalent). No fractional seconds, no
  offset other than `Z`. Example filename:
  `pool.json.corrupt-2026-05-12T17:42:09Z`. The clock source is
  the system wall clock at the call site (no monotonic-clock
  trick -- the timestamp is for operator-facing filenames, not
  ordering). Two corruption events inside the same second land
  the second event on the numeric-suffix path
  (`pool.json.corrupt-<RFC3339>.1`, then `.2`, ...) defined
  below. The copy preserves a stable forensic reference for the
  operator: a subsequent command that calls `load_membership`
  directly hard-errors, and `discover --write` (when allowed by
  its pre-save gates) rewrites the file -- both paths destroy the
  exact bytes that triggered the warning. The sidecar is a
  no-mutation guarantee on the live state (the original
  `pool.json` is unchanged) and gives a future `braid doctor` or
  manual reconciliation a real artifact to compare against. Sidecar
  write failures (permission denied, disk full, IO error) are
  themselves logged as a secondary `Warning: ...` line but MUST
  NOT mask or modify the primary corruption warning; the primary
  warning is the load-bearing operator signal. If a sidecar with
  the same timestamp already exists (sub-second collision under
  the same command), append a numeric suffix
  (`pool.json.corrupt-<RFC3339-UTC>.<N>`) starting at `1`; do not
  overwrite a prior sidecar. Pin with a unit test: build a
  filesystem fake with a corrupt `pool.json`, call
  `refresh_pool_metadata`, and assert (a) the primary corruption
  warning is emitted; (b) a sidecar matching
  `pool.json.corrupt-*` exists with byte-for-byte the original
  corrupt content; (c) the original `pool.json` is unchanged.

  **Precedence vs `load_membership` (pinned).** The same corrupt
  `pool.json` will surface as:
  - a hard `MembershipError::Corrupt` from any command path that calls
    `load_membership` directly and propagates the result (boundary
    commands like `add`, `remove`, `replace`, `lock`, `unlock`,
    `discover` -- and `status`/`doctor` when they read membership for
    the primary output, not the post-mount enrichment hop);
  - a `Warning: ...` line from any path that calls `refresh_pool_metadata`
    (the post-mount enrichment hop in `unlock`, the post-commit
    enrichment in `add`/`replace`, the periodic enrichment from
    `status`/`doctor`).

  This asymmetry is intentional: `refresh_pool_metadata` is best-effort
  enrichment that runs after a successful primary operation, and
  aborting the operation on a `pool.json` corruption discovered at the
  enrichment hop would block legitimate work (the primary operation has
  already succeeded against live btrfs state). Commands that consult
  `pool.json` for their primary decision (load membership before
  planning) MUST fail closed; commands that consult it only for
  enrichment (refresh metadata after a successful mutation) MUST warn
  and continue. The plan does NOT add a precedence reconciliation hack
  -- both paths read the same file independently, and a hand-corrupted
  `pool.json` racing between the two paths shows up the same way each
  time it is read.
- Delete the existing free-standing `validate_no_conflicts`. It enforces only
  two of the four uniqueness invariants; `PoolMembership::insert` enforces
  all four (UUID, name, by-id, devid) in one place, so the helper becomes
  dead code on cutover. Removing it in the same change prevents future
  callers from bypassing the full invariant set by reaching for the older
  helper. Ordering: route the two production callers --
  `plan_add` (`add.rs:1235`) and `plan_replace` (`replace.rs:1478`) --
  through `PoolMembership::insert` first, then delete the helper. Removing
  the helper before the callers move produces a compile-fail cliff; moving
  the callers first keeps the tree green for the rest of the cutover.

**`save_membership` writer signature (pinned).** The function signature
on master is `save_membership(m: &PoolMembership, paths: &StatePaths)
-> Result<(), MembershipError>`. The signature does NOT change under
the migration -- `PoolMembership` is still passed by reference, the
return type is unchanged. The internal serialization changes (the
outer struct now serializes as a `LuksUuidMap`-keyed object), but the
external contract is identical. Every existing writer call site
therefore compiles unchanged at the writer boundary; the changes are
purely in how each call site builds the `PoolMembership` it passes
(UUID-keyed insertion instead of name-keyed `disks.insert`).

**Writer call site inventory (pinned).** The migration touches every
site that constructs or mutates a `PoolMembership` before passing it
to `save_membership` (the writer signature is unchanged, but the
construction code is). The full inventory:

- `cli/src/add.rs:1065` -- post-commit save of `final_membership`
  after `btrfs device add`.
- `cli/src/add.rs:1112` -- post-finalize save after the second
  per-target step.
- `cli/src/replace.rs:676` -- post-commit save of
  `target_membership` after `btrfs replace start`.
- `cli/src/remove.rs:356` -- post-commit save of
  `target_membership` after `btrfs device remove`.
- `cli/src/recover.rs:1052` -- Remove-recovery save of
  `recovered` (the carve-out + rebuild path).
- `cli/src/recover.rs:2147` -- Add-recovery save of `recovered`
  after pool-mutation replay.
- `cli/src/recover.rs:2416, 2437` -- RemoveMissing-recovery saves.
- `cli/src/recover.rs:2493` -- RemoveMissing pre-commit save of
  `journal.pre_membership` (the unchanged-snapshot path).
- `cli/src/recover.rs:2578` -- post-recovery save of `recovered`.
- `cli/src/recover.rs:2767, 2779, 2820` -- Replace-recovery saves
  of `journal.pre_membership` in the various carve-out arms.
- `cli/src/recover.rs:2971` -- post-Replace-recovery save of
  `recovered`.
- `cli/src/main.rs:743` -- the `Commands::Discover --write` arm
  saving the discover outcome.
- `cli/src/membership.rs:216` -- the in-process save inside
  `refresh_pool_metadata` (called from `enrich_from_pool_state`
  success path).

The migration audit MUST visit every site above. Test fixtures and
helpers (`add.rs:3188, 4323`, `membership.rs:235, 255`,
`remove.rs:955`) move mechanically once the `PoolMembership`
constructor signatures change. A grep contract for the reviewer
audit: `git grep -n 'save_membership' cli/src` after the cutover
must show only the writer-callers above (plus test fixtures) and
zero leftover name-keyed `disks.insert` calls feeding into any of
them.

### `add.rs`

- Iteration order over `OpKind::Add.targets` (now `LuksUuidMap`-keyed):
  every operator-visible iteration MUST sort by `DiskName` before
  rendering. Today's `BTreeMap<String, _>` produced alphabetical-by-name
  ordering implicitly; the new UUID-keyed map produces UUID-lex order,
  which is effectively random per disk and reorders on every fresh
  `braid add` (because each new disk gets a new v4 UUID). The
  operator-visible surfaces in add execution:
  - planning-time and execution-time progress messages
    (`info!`/`println!` lines that name the target disks);
  - dry-run preview rendering of the Add work plan;
  - any operator-facing error body that enumerates targets.

  Build a `Vec<(&DiskName, &AddJournalTarget)>` from
  `targets.iter().map(|(_, t)| (&t.name, t))`, sort by `DiskName`,
  then iterate that vec for any operator-visible loop. Internal
  iteration that does not produce operator output (e.g. dependency
  ordering inside the executor, hashmap rebuilds) may iterate the
  raw `LuksUuidMap` directly.

  **Concrete operator-visible iteration sites in `add.rs` (pinned).**
  The implementer migrates every `for (_, target) in &targets` loop
  in `add.rs` to one of the two patterns above. The set of
  operator-visible loops (the ones that must sort by `DiskName`) is:

  - `plan_add`: the planning-time progress messages that report each
    target's name to the operator (the `info!`/`println!` block that
    today renders one line per spec in input order; under the
    migration it renders one line per target in `DiskName` order).
  - `build_add_work_plan`: the dry-run preview rendering of the Add
    work plan -- one entry per target, ordered for operator reading.
  - `cmd_add` execution-time progress messages issued between
    `cryptsetup luksFormat` and `btrfs device add` for each target.
  - `AddError::*` bodies that enumerate targets (currently
    `AddError::DuplicateUuid` and any error path that quotes "for
    targets X, Y, Z").

  Loops that may iterate the raw `LuksUuidMap` directly:

  - The executor's per-target `CmdRequest` issue loop (the actual
    `cryptsetup luksFormat` / `btrfs device add` issuance) -- the
    order is internally consistent (the recording sink observes the
    same order as the dry-run preview because both build off the same
    sorted vec in `build_add_work_plan`).
  - Any hashmap rebuild used to seed downstream lookups.

  **Recover.rs Add replay operator-visible loops (pinned).** Apply the
  same rule to `recover.rs` Add replay. The four concrete loops are:

  - `discover_add_targets_before_mount` (`recover.rs:~1942`): the
    `for (uuid, target) in targets` loop that emits per-target
    discovery-phase progress lines. Build the sorted vec once at the
    top of the function and iterate it.
  - `verify_recover_passphrase_for_add_replay` (`recover.rs:~2020`):
    the loop that emits per-target passphrase-verification progress
    lines. Same pattern.
  - `execute_add_pool_mutation_recovery` first-pass open loop
    (`recover.rs:~2211`): the loop that emits per-target
    open-and-scan progress lines. Same pattern.
  - `execute_add_pool_mutation_recovery` final irreversible-adoption
    loop (`recover.rs:~2270`): the loop that emits per-target
    irreversible-adoption progress lines. Same pattern.

  Replay determinism does not require sorting -- but operator-visible
  progress lines emitted from inside those loops MUST sort by
  `DiskName` for the same UX reason. The simplest cutover is to build
  the sorted vec once at the top of each function and reuse it across
  the loop body. A grep contract for the reviewer audit: after the
  migration, every `for (_, target) in targets` loop in `add.rs` and
  `recover.rs` either iterates a pre-sorted vec or is annotated as
  internal-only with a `// Internal iteration: operator-visible
  output uses the sorted vec at <line>.` comment.

  Pin with a unit test for `add.rs` planning output: build an
  `AddParams` with three targets whose `DiskName` order is the
  reverse of their UUID-lex order (e.g. `a-disk` at `test_uuid(99)`,
  `m-disk` at `test_uuid(50)`, `z-disk` at `test_uuid(1)`); capture
  the progress-line iteration order through the recording test sink
  and assert it is `["a-disk", "m-disk", "z-disk"]`. The TUI/status/
  doctor/preflight test fixtures already pin this ordering for other
  surfaces; this is the symmetric pin for the add path.

- Parse disk specs to `(DiskName, ByIdPath)`.
- Check duplicate input names by `DiskName`.
- For fresh targets, generate UUID during planning and include it in:
  - work plan;
  - dry-run `CryptsetupLuksFormat` request;
  - journal Add target map key;
  - `target_membership`.
- For `PresentLuks` / returning-disk adoption:
  - use the probed UUID as the Add target map key;
  - keep label matching only as an adoption safety gate;
  - use FSID and live UUID checks for identity.
- Pre-journal-write per-target UUID uniqueness assert. After
  generating (FreshLuks) or probing (PresentLuks /
  RecoverableBraidLabeled) each target's UUID and BEFORE the
  journal write AND BEFORE any `cryptsetup luksFormat` step,
  assert that the target UUID is absent from BOTH (a) the
  in-memory `PoolMembership` keys AND (b) the live `pool.devices`
  UUID set observed at planning time. On collision, abort
  planning with a structured `AddError::DuplicateUuid` (the same
  variant already pinned for the cloned-disk planning path; reuse
  the existing variant rather than introducing a second).

  **Gate raise site (pinned).** The uniqueness assert lives inside
  `plan_add` in a new helper `assert_target_uuid_unique(uuid,
  &membership, &live_pool, &targets) -> Result<(), AddError>`.
  The helper is called once per target inside the `for spec in
  &params.specs` planning loop, immediately after the target's
  UUID is generated (FreshLuks) or probed (PresentLuks /
  RecoverableBraidLabeled) and immediately before
  `targets.insert(uuid, target)`. Placing the helper inline in
  `plan_add` (rather than at the call-site of `plan_add`) keeps
  the gate, the journal-write, and the `cryptsetup luksFormat`
  step in the same function so the "before X, before Y" ordering
  is locally verifiable.

  **Gate ordering vs in-flight `targets` map (pinned).**
  > **Superseded** by `plans/impl/2026-06-16-slim-add-uuid-assert-freshluks-guard.md`:
  > `assert_target_uuid_unique` no longer checks the live `pool.devices` scope.
  > That live-pool axis now lives in per-caller guards
  > (`classify_live_pool_match` for `PresentLuks`,
  > `assert_fresh_uuid_absent_from_live_pool` for `FreshLuks`); the assert is
  > identity-only (in-flight + membership). The two-scope description below
  > records the original design.

  The pre-write
  uniqueness assert checks two scopes: (1) membership keys, (2) live
  `pool.devices` UUID set. It does NOT check the in-flight `targets`
  map -- that scope is the cloned-disk-across-targets case, and it
  is handled by `AddError::DuplicateUuid` raised explicitly BEFORE
  the membership/live-pool assert. Concrete order within `plan_add`
  for each target:

  1. Generate or probe the target UUID.
  2. If the UUID is already in the in-flight `targets` map keyed by
     a different `by_id`, raise `AddError::DuplicateUuid` naming
     both `(name, by_id)` pairs (the cloned-disk-across-targets
     case; this is the exact arm that "discover already closes with
     `DiscoverError::DuplicateUuid`" in the section below).
  3. Otherwise, call `assert_target_uuid_unique` to check
     membership keys and live `pool.devices` UUIDs. On collision,
     raise `AddError::DuplicateUuid` naming the in-flight target
     plus a synthesized `(name, by_id)` for the colliding existing
     member (membership case) or the colliding live device
     (live-pool case). Concretely, in the membership-collision
     case the second `(name, by_id)` pair is the existing member's
     `name` and `by_id`; in the live-pool case it is the
     observed `PoolDevice.mapper` rendered through
     `MapperName::Display` (as the "name" surface) and the empty
     string (as the by-id surface, because live pool devices do
     not carry a by-id observation -- the test plan accepts an
     empty by-id placeholder in this arm).
  4. `targets.insert(uuid, target)?` (the `LuksUuidMap::insert`
     fail-closed call). This is the defense-in-depth backstop;
     under correct gate behavior it never fires.
  5. Continue to the next spec.

  Rationale and pin: identical to the symmetric `replace.rs`
  pre-journal-write uniqueness assert -- a stale journaled
  `new_uuid` from a different host (cloned image) or a foreign
  braid-format disk attached between planning and execution
  corrupts the UUID-uniqueness invariant if the planning path
  does not refuse it. The `LuksUuidMap::insert` fail-closed and
  `PoolMembership::insert` uniqueness checks are the
  defense-in-depth backstops; this gate is the pre-write refusal.
- Insert membership with `PoolMembership::insert(uuid, DiskMember { name, by_id,
  devid: None, added_at: None })`.
- After the btrfs add commits, enrich from live pool state before saving
  committed `pool.json`.

#### `AddError::DuplicateUuid` (cloned-disk planning path)

Two `PresentLuks` adoption targets in a single `braid add` invocation
that probe to the same LUKS UUID under distinct by-id paths and
distinct disk names (the dd-cloned-disk case) MUST surface as a
structured, operator-friendly error before any journal write, before
any `CryptsetupLuksFormat`, and before any `PoolMembership::insert`.
Falling through to `MembershipError::Conflict` from
`PoolMembership::insert` (or to `LuksUuidMapConflict` from
`LuksUuidMap::insert` on the journal `targets` map) would technically
fail closed, but the operator message would not name both by-id paths
and would not flag the cloned-disk diagnosis -- exactly the gap
discover already closes with `DiscoverError::DuplicateUuid`.

Add an `AddError::DuplicateUuid` variant mirroring discover's:

```rust
AddError::DuplicateUuid {
    uuid: LuksUuid,
    name1: DiskName,
    by_id1: ByIdPath,
    name2: DiskName,
    by_id2: ByIdPath,
}
```

`Display` text MUST be:

```text
duplicate LUKS UUID across add targets: braid-{name1} ({by_id1}) and braid-{name2} ({by_id2}) share UUID {uuid} -- relabel or detach one before retrying (this typically indicates a dd-cloned disk)
```

Sort `(name1, by_id1)` and `(name2, by_id2)` lexicographically by
`by_id` for determinism, matching discover's `label_collision` helper
ordering (`discover.rs:104-113`). Raise from `add.rs` planning **before**
delegating to `LuksUuidMap::insert` on the journal `targets` map and
**before** `PoolMembership::insert`; those two paths remain the
defense-in-depth backstop for non-`add.rs` insertion paths. The shape
mirrors `DiscoverError::DuplicateUuid` so the two operator workflows
read consistently.

### `remove.rs`

- Resolve `params.name` to `(target_uuid, member)` at command entry.
- Store `OpKind::Remove { luks_uuid: target_uuid, name: member.name }`.
- All live target checks compare `PoolDevice.luks_uuid == target_uuid`.
- Remove the target with `remove_by_uuid`.
- Continue to source `RemoveWorkPlan::target_mapper` from the matched
  `PoolDevice.mapper` (today at `remove.rs:144`), never from
  `mapper_name(&member.name)`. The post-commit `CryptsetupClose` at
  `remove.rs:180-183` consumes that field directly, so reconstructing
  the mapper from the member name here re-opens the same drift hazard
  the Replace migration closes via Pattern 4 (`replace.rs:1226-1235`).
  The UUID identity decision happens at the find step
  (`PoolDevice.luks_uuid == target_uuid`); the close still needs the
  observed mapper string, not a reconstructed one.
- Before the post-commit `CryptsetupClose` at `remove.rs:180-183`,
  issue a `CryptsetupLuksUuid { mapper: target_mapper }` probe and
  require the result to equal `target_uuid`. Mismatch (or probe
  failure) demotes the close to a logged-warning skip naming the
  mapper, the expected `target_uuid`, and the observed UUID (or probe
  error). This is the same defense-in-depth contract specified for
  Replace's post-commit close; see the "Double-drift defense-in-depth
  UUID probe" section under Journal Schema for the full rationale and
  the three sites the probe covers (`replace.rs:707`,
  `recover.rs:2935`, `remove.rs:180-183`). No recovery-side mirror for
  Remove because `OpKind::Remove` is intentionally skipped by
  `replay_post_mutation` (`recover.rs:1771-1783`).

### `remove_missing.rs`

- Resolve the requested missing devid through `membership.by_devid`.
- Fail if:
  - the devid is not in `pool.missing_devids`;
  - membership has no member with that prior devid;
  - membership has duplicate prior devids.
- Remove the member by UUID, not by name.
- Do not run `cryptsetup luksUUID` for the missing target; there is no backing
  device.

### `replace.rs`

- Resolve `--old <name>` once to `old_uuid`.
- Live source:
  - identify the source by `PoolDevice.luks_uuid == old_uuid`;
  - use the observed live `devid` for `btrfs replace start`.
- Missing source:
  - if `--missing-id` is supplied, require it to match the old member's
    persisted devid and to appear in `pool.missing_devids`;
  - if `--missing-id` is omitted and exactly one btrfs missing devid exists,
    require that devid to match the old member;
  - fail if the old member has no persisted devid.
- New target:
  - fresh new disk gets `new_uuid = LuksUuid::new_v4()`;
  - existing LUKS new disk gets `new_uuid` from the probe;
  - journal stores `old_uuid` and `new_uuid` at op level.
- Pre-journal-write `new_uuid` uniqueness assert. After
  generating or probing `new_uuid` and BEFORE the journal write
  AND BEFORE any `cryptsetup luksFormat` step, assert that
  `new_uuid` is absent from BOTH (a) the in-memory
  `PoolMembership` keys (excluding `old_uuid`, which is being
  replaced) AND (b) the live `pool.devices` UUID set observed at
  planning time. On collision, abort planning with a structured
  `ReplaceError::DuplicateUuid { uuid: LuksUuid, scope: DuplicateUuidScope }`
  BEFORE any journal write. `where` is a Rust keyword so the
  field name MUST be `scope` (not `where`); the scope enum is:

  ```rust
  pub enum DuplicateUuidScope {
      Membership,
      LivePool,
  }
  ```

  `Display` for `DuplicateUuidScope` renders `"membership"` and
  `"live_pool"` respectively so the operator-facing wording
  matches the prior text contract. Rationale: a fresh v4
  collision is astronomically rare, but the realistic case is an
  operator who attaches a foreign braid-format disk between
  planning and execution whose UUID happens to match a stale
  journaled `new_uuid` from a different host (cloned image), or
  an `ExistingLuks` new target whose probed UUID has crept into
  membership through a separate unobserved path. Either case
  corrupts the UUID-uniqueness invariant the migration is built
  around; refusing before journal-write keeps the residual blast
  radius bounded. Pin with a unit test that injects a colliding
  UUID into membership and asserts the structured error fires
  before any journal write or `CryptsetupLuksFormat` request, and
  a second test that injects the collision into `pool.devices`
  with the same assertion.
- Committed membership removes `old_uuid` and inserts `new_uuid`.

#### ExistingLuks new-target UUID re-verification at the open boundary

The post-commit close defense-in-depth probe specified under
`Journal Schema > Double-drift defense-in-depth UUID probe` runs on
the mapper AFTER `btrfs replace start` has already written pool data
to the new target. That probe does not cover the symmetric hazard at
the open boundary: between Replace planning and execution, the
operator could swap the physical disk at `new_target.by_id` (USB
shuffle, hot-plug into the wrong slot, replug of a different braid
disk by accident). The planning-time probe is no longer the
identity of the disk currently sitting at `new_target.by_id`, so
`ensure_luks_open(new_by_id, passphrase)` plus the subsequent
`btrfs replace start` would write pool data into a foreign LUKS
volume with no btrfs-level identity check to stop it.

For `ReplaceJournalMode::ExistingLuks` new targets, the executor MUST
re-probe identity before opening the new disk:

- Issue a `CryptsetupLuksUuid { device: new_target.by_id }` probe
  (the by-id-form probe, not the mapper-form used by the post-commit
  close double-drift probe) before either of the
  `ensure_luks_open(runner, &new_name, &new_by_id, &passphrase)`
  call sites in `replace.rs::execute` (`replace.rs:534` and
  `replace.rs:592`) when the prep mode is `ExistingLuks`.
- Require equality with op-level `new_uuid`. Mismatch aborts the
  replace with a structured error naming `new_target.by_id`,
  `new_uuid`, and the observed UUID; probe failure aborts with the
  same shape (an unreadable LUKS header at the by-id boundary is
  itself a fail-closed condition because identity cannot be
  confirmed before the open). Wording mirrors the
  `finish_uncommitted_replace_recovery` `:2697` arm so operator
  remediation reads consistently across the planning, execution, and
  recovery boundaries.
- Apply the same re-probe at every recovery-replay site that
  re-opens or `btrfs replace start`s against the new target inside
  `execute_replace_pool_mutation_recovery` (the same arms covered by
  the "Replace-recovery ExistingLuks re-source UUID assert" section
  above). Recovery replay shares the same drift window as planning-
  to-execution and benefits from the same gate.
- Fresh-LUKS new targets do not need this re-probe at the open
  boundary because the gate at the `cryptsetup luksFormat` step
  (the structured `uuid` field in `CmdRequest::CryptsetupLuksFormat`)
  writes the journaled UUID into the disk's header before any open;
  any swap-in-place reformat is caught by the FreshLuks adoption
  gates at finish-time and at recovery replay.

This re-probe is the planning/execution-time analogue of the
post-commit close defense-in-depth probe. The two probes together
close the open-boundary swap and the post-commit-close re-open swap;
neither subsumes the other.

### `recover.rs`

This is the largest mechanical rewrite.

- Replace membership iteration over `(name, member)` with
  `(uuid, member)`.
- Replace live-state correlation by mapper-name parsing with UUID/devid
  correlation.
- Recovery replay reads authoritative identity from:
  - Add: each `targets` map key;
  - Remove: `OpKind::Remove.luks_uuid`;
  - Replace: `OpKind::Replace.old_uuid` and `new_uuid`;
  - RemoveMissing: `devid -> membership.by_devid`.
- When rebuilding `pool.json` from live btrfs topology:
  - only expected UUIDs for the current recovery phase are admitted;
  - `by_id` is still live-resolved through `/dev/disk/by-id/`;
  - display `name` and historical `added_at` are copied by UUID from current
    membership first, then journal snapshots, then freshly stamped if absent.
- Unknown live UUIDs fail closed or are ignored only where the current code
  already has an explicit "foreign" policy; do not admit them by mapper name.
- For `NullUnderlyingDevice` encountered during replay, the resolution chain
  is journaled `luks_uuid` -> persisted `DiskMember.devid` -> live
  `null_underlying.devid`. Order matters: a journal entry written before
  enrichment populated `devid` will not resolve in the middle hop. Replay
  must surface the gap as a recoverable error, not as silent admission of a
  mapper whose backing identity is unobservable. Add a structured variant
  for this case:

  ```rust
  RecoverError::JournalUuidDevidGap { luks_uuid: LuksUuid }
  ```

  `Display` text MUST contain the literal substring
  `journaled LUKS UUID <uuid> has no persisted devid; cannot resolve null-underlying mapper`
  so tests pin the wording without restating the surrounding sentence.
- The `devid -> LuksUuid` resolution MUST run against the journal's
  sealed membership snapshot, never against live `pool.json`. The
  snapshot follows the existing phase-keyed selection at
  `recover.rs:3381-3388`:
  - `RemoveMissingPhase::PoolMutation` -> `journal.pre_membership.by_devid(devid)`.
  - `RemoveMissingPhase::PostRemoveMissingMaintenance` ->
    `journal.target_membership.by_devid(devid)`.

  This is the existing source-of-truth split, retained on purpose:
  `pre_membership` and `target_membership` are sealed at mutate-time
  precisely so the `devid -> LuksUuid` lookup cannot drift if an
  unrelated `braid discover --write` or any operator `pool.json` edit
  happens between mutate and recover. Routing the lookup through live
  `pool.json` would silently invalidate the resolution.

- `by_devid(devid)` against the journaled snapshot has three relevant
  outcomes during RemoveMissing replay, and each MUST be handled
  explicitly:
  - `Ok(Some((uuid, member)))` -- normal path: proceed to remove the
    resolved UUID.
  - `Err(MembershipError::DuplicateDevid { devid, members })` --
    recovery surfaces a recoverable error, names the duplicate devid
    and every colliding UUID, aborts the replay, and directs the
    operator to the recovery-scenarios guide. Surface this as:

    ```rust
    RecoverError::DuplicateDevidDuringReplay {
        devid: u64,
        members: Vec<LuksUuid>,
    }
    ```

    `Display` text MUST contain the literal substring
    `duplicate devid <devid> in journaled membership across UUIDs <uuid1>, <uuid2>[, ...]`
    with UUIDs in canonical lexicographic order (mirrors the
    `MembershipError::DuplicateDevid` ordering rule). Do not silently skip
    the target and do not continue replaying against an ambiguous prior
    binding.
  - `Ok(None)` -- the journaled devid does not match any persisted
    devid in the journal's `pre_membership`/`target_membership`
    snapshot. Because the snapshot is sealed at mutate-time, this
    outcome cannot be caused by an operator `pool.json` edit between
    mutate and recover; it fires only when the journal entry itself
    was written against a never-enriched member (every member in that
    snapshot has `devid == None` for the relevant entry, so no member
    can match the live btrfs missing devid the journal recorded).
    The prior binding to live btrfs is unrecoverable from the journal
    alone. Recovery MUST abort with a structured error:

    ```rust
    RecoverError::NoMemberForJournaledDevid { devid: u64 }
    ```

    `Display` text MUST contain the literal substring
    `no member in journaled membership has devid <devid>; the journal entry was written against a never-enriched member -- see docs/luks-unlock.md and manual/guides/recovery-scenarios.md before removing /var/lib/braid/pending-op.json`.
    The remediation deliberately does NOT instruct the operator to
    "repair `pool.json`": the live file is irrelevant because the
    resolution did not consult it. The journal MUST remain in place
    so the next recovery attempt sees the same state; recovery MUST
    NOT silently clear the journal or fall back to a name-based
    lookup.

#### Add-recovery FreshLuks adoption gate

The pre-flight Add-replay loop today gates adoption of a returning
fresh-format target by label only (`recover.rs:1958-1960` as of commit
`b189bb6`: `if label.as_deref() != Some(luks_label.as_str()) { continue; }`;
line 1957 is the enclosing `journal::AddJournalMode::FreshLuks { luks_label,
.. } =>` match arm). Under the
new model the `targets` map key IS the pre-generated UUID, and the expected
label is derived from `target.name` rather than stored in the journal -- so the
authoritative identity is reachable from inside the adoption arm. The migration
MUST tighten the `FreshLuks` arm to additionally assert that the probed live
UUID matches the map-key UUID:

There are FOUR FreshLuks adoption sites in the Add-recovery flow, and
ALL FOUR must move in lockstep. Today every one of them is a label-only
gate; a half-migration that tightens any subset leaves the data-loss
path open in the remaining sites:

- `discover_add_targets_before_mount`'s `FreshLuks` arm
  (`recover.rs:1957` -- enclosing match arm; `:1958-1960` -- label
  guard).
- `verify_recover_passphrase_for_add_replay`'s `FreshLuks` arm
  (`recover.rs:2029`).
- `execute_add_pool_mutation_recovery`'s first-pass open loop
  (`recover.rs:2226` -- the `FreshLuks` arm inside the
  `if !add_targets_all_live` loop that runs before the passphrase
  prompt, ending in `ensure_luks_open` + `scan_mapper_if_btrfs_visible`
  at the existing `target.mapper_name` path).
- `execute_add_pool_mutation_recovery`'s final irreversible-adoption
  arm (`recover.rs:2350` -- the `ConfigDiskState::PresentLuks` arm
  inside the second `for (name, target) in targets` loop, ending in
  `ensure_luks_open` + `crate::pool::pool_add_device`). This is
  exactly the "open + btrfs device add of a foreign disk" data-loss
  path: a label-only match here lets a post-crash out-of-band reformat
  ride into the btrfs pool via `pool_add_device`.

Each of the four arms MUST assert `&probed_uuid ==
target_uuid_from_map_key` in addition to the existing
derived-label match (`Label: braid-<target.name>`). On mismatch, all
four arms abort with `RecoverError::Failed(message)` where `message`
names `target.by_id`, the journaled UUID, and the observed UUID --
NOT a new dedicated variant. Rationale: the existing 2350 arm already
returns `RecoverError::Failed("...unexpected LUKS label")`, the
discover/verify arms (1957/2029) sit inside functions that already
plumb `RecoverError::Failed` for every other operator-facing abort,
and the new error path subsumes the old "...unexpected LUKS label"
wording inside `Failed` rather than splitting it across two enum
variants. The four sites use the same message template so operator
remediation reads consistently:

```text
add recovery aborted: target {by_id} LUKS UUID mismatch -- journaled {expected_uuid}, observed {actual_uuid} ({target_state}); the disk at this by-id was reformatted out-of-band between crash and recovery (see manual/guides/recovery-scenarios.md)
```

`{target_state}` is `"fresh-luks"` or `"recoverable-braid-labeled"`
depending on the arm; everything else is identical across the four
sites. Tests pin the substring
`add recovery aborted: target <by_id> LUKS UUID mismatch` plus both
UUIDs.

The plan deliberately does NOT introduce a dedicated
`RecoverError::AdoptionUuidMismatch` variant. The four call sites
already use `Failed(String)` for adjacent error conditions (label
mismatch, missing mapper, foreign FSID); a structured variant
would force every downstream test to match either the structured
variant or `Failed`, doubling assertion surface for the same
operator-visible message. The structured variants this migration
DOES introduce (`JournalUuidDevidGap`, `DuplicateDevidDuringReplay`,
`NoMemberForJournaledDevid`) are reserved for cases where the
caller's remediation differs (the resolution did NOT consult the
live file; the operator action is different from "fix the live
pool topology"). The FreshLuks UUID-mismatch path's remediation
is the standard "investigate the foreign reformat and run recovery
again" workflow, which fits `Failed(String)`.

**Variant placement (pinned).** The three new structured variants
live as top-level variants on `RecoverError` (`recover.rs:32-55`)
alongside `Probe`, `Cmd`, `Parse`, `Journal`, `Membership`, `Mount`,
`Luks`, `Failed`, and `AckCleanupFailed`. Do not introduce a sub-enum
`RecoverError::JournaledSnapshot(JournaledSnapshotError)`: the
existing enum is flat and adding a single sub-enum layer would force
every caller that matches on `RecoverError` to either pattern-match
two levels or rely on `Display` chaining. The three variants are:

```rust
#[derive(Debug, Error)]
pub enum RecoverError {
    // ... existing variants ...

    #[error("journaled LUKS UUID {luks_uuid} has no persisted devid; cannot resolve null-underlying mapper")]
    JournalUuidDevidGap { luks_uuid: LuksUuid },

    #[error("duplicate devid {devid} in journaled membership across UUIDs {}", format_uuid_list(.members))]
    DuplicateDevidDuringReplay { devid: u64, members: Vec<LuksUuid> },

    #[error("no member in journaled membership has devid {devid}; the journal entry was written against a never-enriched member -- see docs/luks-unlock.md and manual/guides/recovery-scenarios.md before removing /var/lib/braid/pending-op.json")]
    NoMemberForJournaledDevid { devid: u64 },
}
```

The `format_uuid_list` helper renders the `Vec<LuksUuid>` joined by
`, ` in canonical lexicographic order (same contract as
`MembershipError::DuplicateDevid`). The helper is a free function
defined in `cli/src/types.rs` (where `LuksUuid` itself lives), NOT
in `recover.rs` and NOT as an associated function on `LuksUuid`.
Rationale: `format_uuid_list` is consumed from both `recover.rs`
(`RecoverError::DuplicateDevidDuringReplay`) and `membership.rs`
(`MembershipError::DuplicateDevid`), so co-locating with one of the
consumers forces the other to reach across modules and silently
ties the helper to that module's visibility. Placing it next to the
type whose Vec it formats is the orphan-rules-safe analogue of an
inherent method without binding it to `Vec<LuksUuid>` syntactically.
Signature:

```rust
pub(crate) fn format_uuid_list(uuids: &[LuksUuid]) -> String {
    let mut sorted: Vec<&LuksUuid> = uuids.iter().collect();
    sorted.sort();  // canonical lexicographic order via Ord on LuksUuid
    sorted
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
```

Source of `probed_uuid` at each site. `ConfigDiskState::PresentLuks`
already carries `uuid: LuksUuid` as a field (see `cli/src/types.rs:137`
-- the field is named `uuid`, not `luks_uuid`, and is populated by the
same `probe_config_disk` call that produces `label`). No new field is
added to `ConfigDiskState::PresentLuks`, and no separate
`CryptsetupLuksUuid` probe is inlined at any of the four sites; the
implementer destructures `uuid` from the same `probed.state` already
yielding `label`. Concretely:

- Site 1 (`recover.rs:1957` arm; destructure at `:1942-1946`): the
  `let ConfigDiskState::PresentLuks { uuid, label, mapper_open } =
  probed.state else { ... };` line already binds `uuid`. The
  `FreshLuks` arm at `:1958-1960` adds the UUID assert next to the
  existing label compare.
- Site 2 (`recover.rs:2029` arm inside
  `verify_recover_passphrase_for_add_replay`): destructure `uuid`
  alongside whatever the verify path already pulls from
  `probed.state`; assert against the map-key UUID before the existing
  label check.
- Site 3 (`recover.rs:2226` `FreshLuks` arm inside the first-pass open
  loop; destructure at `:2211-2218`): the existing `let
  ConfigDiskState::PresentLuks { uuid, label, mapper_open } =
  probed.state else { continue; };` already binds `uuid`. The
  `FreshLuks` arm at `:2226-2230` adds the UUID assert next to the
  existing `label.as_deref() != Some(luks_label.as_str())` continue.
- Site 4 (`recover.rs:2350` arm inside `execute_add_pool_mutation_recovery`'s
  final loop): the current destructure is
  `ConfigDiskState::PresentLuks { label, .. }`; tighten the pattern to
  `ConfigDiskState::PresentLuks { uuid, label, .. }` and add the UUID
  assert next to the existing label compare. This is the
  irreversible-adoption arm; the destructure tightening is the
  smallest change that closes the data-loss path the plan calls out.

The same wiring applies to the Replace-recovery FreshLuks gate at
`recover.rs:2783` (specified in the next subsection): tighten the
current `ConfigDiskState::PresentLuks { label, .. }` destructure to
`ConfigDiskState::PresentLuks { uuid, label, .. }` and assert
`uuid == new_uuid` next to the label compare.

Why this matters: label-only matching was a silent data-loss path. If a
host crashed mid-format and an out-of-band actor (test fixture, old
image, manual `cryptsetup luksFormat ... --label braid-<name>`) then
reformatted the disk at the same `target.by_id` before recovery ran, the
old code would accept the foreign disk as the format target by label
alone, then `cryptsetup open` it and `btrfs device add` it. Btrfs would
admit a foreign LUKS volume into the pool with no checksum violation
(checksums on fresh members are mod-time clean). Asserting
`probed_uuid == target_uuid` closes this path because the pre-generated
UUID was written into the original disk's header by braid's own
`luksFormat` and an out-of-band reformat will not reproduce it.

#### Add-recovery RecoverableBraidLabeled re-source UUID assert

The four FreshLuks adoption sites enumerated above each have a sibling
`RecoverableBraidLabeled` arm in the same match. Today every one of those
sibling arms already runs a UUID guard (`if &uuid != luks_uuid { continue; }`
at `recover.rs:1952, 2021, 2221, 2275`) sourced from the nested
`AddJournalMode::RecoverableBraidLabeled.luks_uuid` field. Because this
migration drops that nested field (identity moves to the `targets` map
key), every one of those four sibling arms MUST re-source the UUID
comparison from the map-key UUID in the same change that drops the
nested field. A half-migration that updates only the FreshLuks arms
would leave the RecoverableBraidLabeled arms referencing a deleted
field (compile error at best) or, if the implementer keeps the field
"for the comparison only", silently leaves the comparison sourced from
the wrong place and re-opens the same out-of-band-reformat data-loss
path the FreshLuks tightening closes -- a returning braid-labeled disk
whose header was reformatted between crash and recovery would pass a
stale or absent UUID check.

The four `RecoverableBraidLabeled` sibling sites that MUST re-source
the UUID comparison from the `targets` map-key UUID in lockstep with
the FreshLuks tightening:

- `recover.rs:1952` (`discover_add_targets_before_mount`).
- `recover.rs:2021` (`verify_recover_passphrase_for_add_replay`).
- `recover.rs:2221` (`execute_add_pool_mutation_recovery`'s
  first-pass open loop).
- `recover.rs:2275` (`execute_add_pool_mutation_recovery`'s final
  irreversible-adoption arm -- the same loop that holds the
  FreshLuks `:2350` site).

At each site, tighten the destructure to drop the `luks_uuid` binding
(the field is gone) and reuse the same `uuid` already destructured
from `ConfigDiskState::PresentLuks` (Sites 1, 3, and 4 already bind
`uuid` for the sibling FreshLuks arm; Site 2 destructures `uuid`
alongside whatever the verify path pulls today). On mismatch, the
`:1952` and `:2021` arms continue with the same skip/error shape they
use today; the `:2221` and `:2275` arms return `RecoverError::Failed`
naming `target.by_id`, the map-key UUID, and the observed UUID, with
the same naming pattern the FreshLuks arms use.

#### Replace-recovery ExistingLuks re-source UUID assert

`finish_uncommitted_replace_recovery`'s `ExistingLuks` arm at
`recover.rs:2697` today already runs a UUID guard
(`match &probed.state { ConfigDiskState::PresentLuks { uuid, .. } if uuid == luks_uuid => {} ... }`)
sourced from the nested `ReplaceJournalMode::ExistingLuks.luks_uuid`
field. Because this migration drops that nested field (identity moves
to the op-level `new_uuid`), the same arm MUST re-source the
comparison from the op-level `new_uuid` in the same change. Apply the
same re-sourcing to any analogous Replace-replay adoption arm inside
`execute_replace_pool_mutation_recovery` that re-opens or
`btrfs replace start`s against the new target -- replay must read
identity from `OpKind::Replace.new_uuid`, never from the deleted
nested field. Existing mismatch wording at `:2697`
("recover replace target '{}' LUKS UUID mismatch: expected ..., found
...") stays as-is; only the source of `expected` flips from
`luks_uuid` (mode-nested) to `new_uuid` (op-level).

#### Replace-recovery FreshLuks adoption gate

The symmetric data-loss path exists for Replace. Today
`finish_uncommitted_replace_recovery`'s `FreshLuks` arm at
`recover.rs:2783` (the `ConfigDiskState::PresentLuks` arm inside the
`journal::ReplaceJournalMode::FreshLuks { luks_label, .. } =>` match)
gates adoption of a half-prepared replacement disk by label only:
`if label.as_deref() != Some(luks_label.as_str()) { return ...; }`.
On match, it proceeds to verify the passphrase against the present
LUKS header, optionally enroll the keyfile, back up the LUKS header,
save `pre_membership`, and clear the journal. None of those steps
reaches `btrfs device add` (that has not happened yet at finish-time),
but the act of saving `pre_membership` and clearing the journal
against a foreign disk strands the operator: the journal is gone, the
header backup names a disk that is not the one braid prepared, and a
later retry of `braid replace` against the same by-id has no record
of the prior preparation.

Under the new model, `Replace.new_uuid` is pre-generated at planning
time and journaled at the op level, so the recovery gate can and
should assert UUID match too. The migration MUST tighten this arm:

- `finish_uncommitted_replace_recovery`'s `FreshLuks` arm
  (`recover.rs:2783`): require `probed.uuid == new_uuid` (the op-level
  field) in addition to the existing derived-label match. On
  mismatch, return `RecoverError::Failed` naming `new_target.by_id`,
  the journaled `new_uuid`, and the observed UUID.
- The analogous Replace-replay adoption arms inside
  `execute_replace_pool_mutation_recovery` that re-open or
  `btrfs replace start` against the new target must consult
  `new_uuid` at the op level before any `cryptsetup open` or
  `btrfs replace start`. Replay must never derive identity from
  `ReplaceJournalMode::FreshLuks`'s `luks_label` field alone.

Why this matters: identical reasoning to the Add-recovery gate. If a
crash between fresh-format and finish leaves the disk reformatted
out-of-band under the same `braid-<new_name>` label but a different
UUID, label-only finish-time admission would commit braid to a
foreign header. Pre-generating `new_uuid` and asserting it at the
gate closes the path.

#### Remove-recovery `null_underlying` guard: never-enriched carve-out

Today's `recover.rs:1015-1029` Remove-recovery guard restores any
pre_membership disk btrfs still owns (in `null_underlying` or
`missing_devids`) using mapper-name reconstruction:
`pool.null_underlying.iter().any(|n| n.mapper == config::mapper_name(name))`.

Pattern 2 of the cheat sheet maps `name_from_mapper(path)` correlation
to `membership.by_devid(dev.devid)`. For `NullUnderlyingDevice`, the
device side carries no UUID by construction (`types.rs:109-116`), so
this is the right target. But `DiskMember.devid` is `Option<u64>`,
populated only after `enrich_from_pool_state`. A disk added but never
re-probed (host crashed between `add` commit and the next read-side
command) has `devid == None`, and a pure devid lookup misses the live
`null_underlying[i].devid` entirely. The member then silently
disappears from `recovered` -- the migration regresses today's
drift-blind-but-thorough behavior.

Carve-out: keep an expected-mapper-name fallback INSIDE the Remove
recovery guard only, scoped narrowly to the `devid == None` case.

- When the candidate `member.devid` is `Some(d)`, use `by_devid` (and
  the corresponding `missing_devids.contains(&d)` check) as the
  pattern-2 rewrite specifies.
- When `member.devid` is `None`, additionally accept
  `pool.null_underlying.iter().any(|n| n.mapper == config::mapper_name(&member.name))`
  as a positive match. (`NullUnderlyingDevice.mapper` is `MapperName` and
  `config::mapper_name` returns `MapperName`; both sides compare as the
  newtype directly, matching the existing same-shape check at
  `recover.rs:1020-1021`.)
- On positive match, ALSO stamp the matched
  `NullUnderlyingDevice.devid` onto the restored `DiskMember.devid`
  before placing the entry into `recovered`. Concretely: find the
  `NullUnderlyingDevice` whose `mapper == config::mapper_name(&member.name)`,
  copy its `devid` value, and set `restored_member.devid = Some(observed_devid)`.
  Without this stamping, the restored entry stays at `devid: None`
  forever -- `enrich_from_pool_state` refuses to touch it (no live
  UUID is observable for a null-underlying mapper, and the policy
  pinned under `enrich_from_pool_state` requires UUID correlation),
  and the entry cannot be removed by any first-class command:
  `braid remove` requires a present `PoolDevice.luks_uuid` match
  (the disk is null-underlying); `braid remove-missing` requires
  `member.devid` to be `Some` and present in `pool.missing_devids`.
  An operator who lands in this state has no remediation short of
  hand-editing `pool.json`, which is exactly the operation
  `deny_unknown_fields` and the corrupt-file fail-closed posture are
  designed to discourage. Stamping the devid at restoration time is
  bounded by the same trust assumption the carve-out already makes
  (the operator-attested `pre_membership` entry IS the identity
  decision); attributing the observed devid to that entry adds no
  new trust surface.

The carve-out is bounded: it only restores entries already in
operator-attested `pre_membership`, never admits unknown live devices,
and only applies inside the Remove-recovery guard (not in
`enrich_from_pool_state`, not in the general live-pool rebuild, not in
the Add adoption arm). It does not erode the "no identity decisions
from mapper names" invariant because the operator-attested
pre_membership entry IS the identity decision; the mapper-name fallback
just confirms btrfs still owns the slot. Document this carve-out
explicitly at the call site with a comment pointing to this section,
in the same way the discover/add adoption gates are carved out.

**Carve-out call-site comment text (pinned).** The literal comment
at the carve-out site (`recover.rs:~1020`, the
`pool.null_underlying.iter().any(|n| ...)` block) MUST be:

```rust
// Pattern 2 carve-out (Remove-recovery never-enriched fallback):
// when member.devid is None, additionally accept a positive match on
// pool.null_underlying.mapper == config::mapper_name(&member.name).
// This is the only place in the codebase where a mapper-name compare
// participates in an identity decision; scope is bounded to
// Remove-recovery's null_underlying restoration of an operator-attested
// pre_membership entry. See plans/impl/<plan-file>.md, section
// "Remove-recovery null_underlying guard: never-enriched carve-out".
```

The plan-file reference is updated to the promoted impl path when
`/promote-plan` runs; the placeholder `<plan-file>` is the only piece
the implementer adjusts. The comment exists so a future "no
name_from_mapper anywhere" lint sweep does not silently drop the
carve-out without re-reading the rationale.

**Accepted risk: out-of-band `cryptsetup open` collision.** The
carve-out trusts operator-attested `pre_membership` for identity. If
an operator manually `cryptsetup open`s a foreign disk under the
expected mapper name `braid-<member.name>` AND that mapper
subsequently appears in `pool.null_underlying` (the
backing-device-`(null)` case), the carve-out treats the slot as
"btrfs still owns this member" and restores the membership entry.
Restoration itself is safe -- the restored `DiskMember.by_id` still
points at the original physical disk, so downstream `unlock` reopens
the correct disk -- but any in-recovery step that addresses
`/dev/mapper/braid-<member.name>` between restoration and the next
probe (a future Remove cleanup the plan adds, or an interim operation
the recovery flow performs against the expected mapper path) would
operate on the operator's foreign dm slot until the next observation
disambiguates. The contract for callers of the carve-out is therefore:
do not queue mutating mapper-addressed operations against a
carved-out entry inside the recovery plan. The next read-side command
re-enriches the member and dispels the ambiguity.

#### `live_member_names` and the journal-clearing gate

`live_member_names` (defined at `recover.rs:1804`) and
`live_pool_matches_membership` (`recover.rs:1504`) today produce a
`BTreeSet<String>` of disk names parsed out of `dev.mapper.0` via
`name_from_mapper`. The migration MUST move ALL of these to UUID; a
half-migration that leaves the names-based helper alive while the rest
of the file is UUID-correlated would silently corrupt the journal-
clearing decisions made by `execute_remove_missing_pool_mutation_recovery`
(at `recover.rs:2486-2507` and similar sites).

Concrete failure mode: mapper drift causes `live` to omit a member
(because the helper parses a name that doesn't exist in membership);
`expected_live` derived from `member.name` happens to match by
coincidence; `live_pool_matches_membership` returns `true` for a gate
that should have returned `false`; the journal gets cleared at the
wrong phase and persists the wrong membership snapshot.

Required call-site rewrite list (every site MUST move; `name_from_mapper`
MUST NOT survive in any of these helpers):

- `live_member_names` (`recover.rs:1804`): returns
  `BTreeSet<LuksUuid>` derived from `dev.luks_uuid`, not
  `dev.mapper.0`.
- `validate_live_members_allowed` (`recover.rs:1811`): consumes the
  `BTreeSet<LuksUuid>` produced above.
- `add_targets_all_live` (`recover.rs:1832`): the `targets` map key
  IS the UUID under the new schema, so the "is this Add target
  already live?" check becomes set-containment on
  `BTreeSet<LuksUuid>`.
- `live_pool_matches_membership` (`recover.rs:1504-1529`): compares
  UUIDs in the live set to UUIDs in the membership map keys. The
  helper's return type changes from `bool` to
  `Result<bool, JournaledSnapshotError>` so journaled-snapshot
  corruption surfaces as a structured error at the call site (see
  "Post-migration semantics" below for the exact contract and the
  `JournaledSnapshotError` definition). Existing `bool` callers
  rewrap with `?` and the journal-clearing-gate arms (the
  `recover.rs:2487, 2502, 2565, 2861, 2864` sites) route
  `JournaledSnapshotError::DuplicateDevid` and
  `JournaledSnapshotError::NoMemberForDevid` into the
  structured `RecoverError` variants below instead of folding
  them into the generic topology-mismatch text.
- All caller sites at `recover.rs:1360, 1505, 1836, 2011, 2135,
  2207, 2270, 2858` move in lockstep. None of them may pass through
  `name_from_mapper`.

Post-migration semantics of `live_pool_matches_membership` (full spec).
This helper is the journal-clearing gate, so its precise shape matters
for every site that returns a hard error or clears the journal --
`execute_remove_missing_pool_mutation_recovery` (`recover.rs:2487, 2502,
2565`) and `finish_uncommitted_replace_recovery` (`recover.rs:2861,
2864`). A `true` where today returns `false` clears the journal
prematurely; a `false` where today returns `true` deadlocks recovery.

Compute three UUID sets and the predicate:

```
live_uuids    = { dev.luks_uuid for dev in pool.devices }
missing_uuids = {
    membership.by_devid(d)?.uuid
    for d in pool.missing_devids
}
expected      = membership.keys()  // every UUID in the map
```

Predicate: `live_uuids ∪ missing_uuids == expected`, AND
`live_uuids ∩ missing_uuids == ∅`. Both clauses are required: the
disjoint clause catches the rescue-mid-recovery case where a
previously-missing UUID has been re-attached and so appears in both
`live_uuids` and `missing_uuids` (a union-only check would silently
clear the journal in this case; see the live/missing intersection
test below).

The helper's return type is `Result<bool, JournaledSnapshotError>`
(not `bool`) so journaled-snapshot corruption is routed to structured
errors at the call site rather than folded into a generic
topology-mismatch message. `JournaledSnapshotError` is a small
recover-side error type defined alongside `live_pool_matches_membership`
in `recover.rs`:

```rust
enum JournaledSnapshotError {
    DuplicateDevid { devid: u64, members: Vec<LuksUuid> },
    NoMemberForDevid { devid: u64 },
}
```

It exists specifically to bridge `membership.by_devid` outcomes into
the existing `RecoverError::DuplicateDevidDuringReplay` /
`RecoverError::NoMemberForJournaledDevid` variants without overloading
`MembershipError` (which today covers parse/IO/load shapes, not
snapshot-walk shapes). Outcomes for each devid in
`pool.missing_devids`:

- `Ok(Some((uuid, member)))` from `membership.by_devid(d)` -- normal
  resolution; the UUID feeds into `missing_uuids` and the predicate
  evaluates as specified.
- `Err(MembershipError::DuplicateDevid { devid, members })` --
  short-circuit the helper and return
  `Err(JournaledSnapshotError::DuplicateDevid { devid, members })`.
  Callers that gate journal clearing (`recover.rs:2487, 2502, 2565,
  2861, 2864`) MUST translate this into
  `RecoverError::DuplicateDevidDuringReplay { devid, members }` with
  the same `Display` contract as the direct
  `journal.pre_membership.by_devid(devid)` corruption path specified
  under recover.rs RemoveMissing above. The plain topology-mismatch
  error string is reserved for the genuine "live pool topology does
  not match" case where the predicate evaluates to `Ok(false)` --
  corruption MUST NOT silently downgrade into that wording, because
  the operator's remediation differs (the pending-op.json was
  hand-corrupted; the live pool is irrelevant).
- `Ok(None)` from `membership.by_devid(d)` for a devid in
  `pool.missing_devids` -- the journaled snapshot has no member with
  that prior devid. Short-circuit the helper and return
  `Err(JournaledSnapshotError::NoMemberForDevid { devid: d })`.
  Callers translate to
  `RecoverError::NoMemberForJournaledDevid { devid: d }` with the
  `Display` contract specified under recover.rs RemoveMissing above.
  As with the duplicate case, the topology-mismatch wording MUST NOT
  be reused for this corruption.

The bool/Result distinction matters because the callers at
`recover.rs:2487, 2502, 2565, 2861, 2864` today all funnel a `false`
return into the same topology-mismatch error string. With the new
signature each caller pattern-matches on the result: `Ok(true)` clears
the journal, `Ok(false)` returns the topology-mismatch error (genuine
mismatch), and `Err(JournaledSnapshotError::*)` returns the matching
structured `RecoverError` variant.

**Caller rewrap pattern (pinned).** Every call site uses an explicit
`match` on the helper result, NOT the `?` early-return shorthand,
because each arm routes to a distinct outcome (the `Err` arms map
to a structured `RecoverError` variant that is NOT
`JournaledSnapshotError` -- they cannot be auto-bubbled through
`?` without a wrapper trait that does not exist). The canonical
shape at each call site is:

```rust
match live_pool_matches_membership(&pool, snapshot) {
    Ok(true) => { /* journal-clear path */ }
    Ok(false) => return Err(RecoverError::Failed(/* topology-mismatch wording */)),
    Err(JournaledSnapshotError::DuplicateDevid { devid, members }) => {
        return Err(RecoverError::DuplicateDevidDuringReplay { devid, members });
    }
    Err(JournaledSnapshotError::NoMemberForDevid { devid }) => {
        return Err(RecoverError::NoMemberForJournaledDevid { devid });
    }
}
```

`snapshot` is `&journal.pre_membership` or `&journal.target_membership`
per the per-site selection pinned in the next subsection. The eight
call sites are: `recover.rs:1360, 1505, 1836, 2011, 2135, 2207, 2270,
2858`. Use the literal `match` form -- not a helper that hides the
Err-arm translation, and not `?`. Rationale: each site picks its
own topology-mismatch wording (some quote "expected pool to match
pre-membership", some "expected post-commit membership"), so a
shared `?`-able conversion would force the wording to live on
`JournaledSnapshotError::Display` and lose the per-site
specificity. Tests pin one example per `match` arm at one of the
eight sites; a regression that collapses arms via `?` would either
fail to compile (mismatched error types) or flatten the per-site
wording (would fail a `Display` substring assertion).

Concrete case enumeration (each MUST have a unit test):

- **Mapper drift, UUID match** (the trivial case the prior text named):
  `dev.mapper = "braid-WRONG"` and `dev.luks_uuid = U_M` with `U_M` in
  membership. Returns `true`. Today's code returns `false`.
- **Never-enriched present member**: a member whose persisted `devid` is
  `None` but whose UUID is in `pool.devices`. The UUID is in
  `live_uuids` and in `expected`; `missing_uuids` does not include it.
  Returns `true`.
- **Missing on both sides**: a member whose persisted `devid = Some(d)`
  and `d` appears in `pool.missing_devids`. The UUID is in
  `missing_uuids` and `expected`, not in `live_uuids`. Returns `true`.
- **Gone-without-trace**: a member whose UUID is not in `pool.devices`
  and whose `devid` is either `None` or `Some(d)` where `d` is not in
  `pool.missing_devids`. The UUID is in `expected` but not in
  `live_uuids ∪ missing_uuids`. Returns `false`. This must stay `false`
  (today's code also returns `false`); a regression to `true` would
  clear the journal while a member is silently absent.
- **Foreign live UUID**: `dev.luks_uuid = U_F` with `U_F` not in
  membership. The UUID is in `live_uuids` but not in `expected`.
  Returns `false`. This pins the foreign-pool refusal under UUID
  identity.
- **Duplicate devid in missing**: `pool.missing_devids = [d]` and
  `membership.by_devid(d)` returns `Err(DuplicateDevid)`. The helper
  returns `Err(JournaledSnapshotError::DuplicateDevid { devid: d,
  members: [...] })` (not `Ok(false)`). The caller translates this
  into `RecoverError::DuplicateDevidDuringReplay`. A test pinning
  `Ok(false)` here would lock in the silent-downgrade-to-topology-
  mismatch regression the corrected signature is meant to prevent.
- **Unknown devid in missing**: `pool.missing_devids = [d]` and
  `membership.by_devid(d)` returns `Ok(None)`. The helper returns
  `Err(JournaledSnapshotError::NoMemberForDevid { devid: d })`. The
  caller translates this into `RecoverError::NoMemberForJournaledDevid`.
  Same anti-regression reasoning as the duplicate-devid case.

Snapshot selection at each caller (pinned per-site). The helper takes
the snapshot as a parameter -- it does not consult `ReplacePhase` or
any other phase enum internally. Each caller passes the snapshot that
encodes the "what should the pool look like right now?" question for
its branch. Today's call sites already encode this correctly; the
migration preserves the per-site selection verbatim:

- `execute_remove_missing_pool_mutation_recovery` (`recover.rs:2487, 2502, 2565`):
  - `:2487` -- precondition `pool.missing_devids.contains(&devid)`
    (the operation has not yet committed). Pass
    `&journal.pre_membership`.
  - `:2502` -- precondition `!pool.missing_devids.contains(&devid)`
    (the operation committed). Pass `&journal.target_membership`.
  - `:2565` -- same shape: post-membership branch passes
    `&journal.target_membership`.
  This matches the phase-keyed mapping pinned for the structured
  `RecoverError` translation (`PoolMutation -> pre_membership`,
  `PostRemoveMissingMaintenance -> target_membership`); the call-site
  selection is the source of truth, not `RemoveMissingPhase`.
- `finish_uncommitted_replace_recovery` (`recover.rs:2861, 2864`):
  - `:2861` -- builds `pre_topology` for the
    `!committed && pre_topology` branch (the operation has not yet
    committed). Pass `&journal.pre_membership`.
  - `:2864` -- inside the `committed` branch (`btrfs replace` has
    finished and the new disk is live). Pass
    `&journal.target_membership`.
  The selection key here is `committed` (a local boolean derived from
  live pool topology: `live.contains(new_uuid) && !live.contains(old_uuid)`
  under the migration; today's code derives it from
  `live_member_names`). It is NOT `ReplacePhase`. Pin this in code:
  the implementer MUST preserve today's call-site selection literally
  -- do not derive a `ReplacePhase`-to-snapshot table and route both
  call sites through it, because `ReplacePhase` does not encode the
  pre/post distinction the way `RemoveMissingPhase` does (replace's
  phase enum tracks finer-grained replay state).

A regression that passed `&journal.target_membership` at the `:2861`
branch or `&journal.pre_membership` at the `:2864` branch would clear
the journal against the wrong snapshot and persist the wrong
membership at the wrong phase. Pin with a test that builds a replace
recovery scenario where `pre_membership` and `target_membership` differ
in exactly one UUID and asserts each branch consults the snapshot named
above (e.g. by asserting which UUID survives in the post-recovery
`pool.json`).

Out of scope for this helper: `pool.null_underlying[i].devid`.
`null_underlying` is the "mapper is open but `cryptsetup status`
reports `device: (null)`" case; the mapper is part of the live set
in btrfs's eyes but its backing UUID is unobservable. Treating those
devids as "live" here would require a third resolution hop that
matters only for display and Remove-recovery's never-enriched
carve-out (handled separately in this plan). The journal-clearing
gate does not consult `null_underlying`; today's helper does not
either.

- Add fixture helpers early:

```rust
fn test_uuid(seed: u64) -> LuksUuid {
    // Hand-pad a canonical hyphenated UUID. The first 20 hex digits are
    // zero so `seed` is the only varying bits, which makes UUID-lex order
    // identical to seed order. The reverse-of-UUID-lex fixtures in the
    // TUI/status/doctor/preflight tests rely on this property: pick a
    // high seed for the name that should sort lex-last by UUID
    // (`a-disk` at seed=99) and a low seed for the name that should
    // sort lex-first (`z-disk` at seed=1) to get reverse ordering.
    LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", seed))
        .expect("hand-padded UUID is canonical")
}

fn disk_member(seed: u64, name: &str, by_id: &str) -> (LuksUuid, DiskMember) {
    (
        test_uuid(seed),
        DiskMember {
            name: DiskName::parse(name).expect("valid disk name in fixture"),
            by_id: ByIdPath::parse(by_id).expect("valid by-id path in fixture"),
            devid: None,
            added_at: None,
        },
    )
}

fn disk_member_with(
    seed: u64,
    name: &str,
    by_id: &str,
    devid: Option<u64>,
    added_at: Option<&str>,
) -> (LuksUuid, DiskMember) {
    let (uuid, mut m) = disk_member(seed, name, by_id);
    m.devid = devid;
    m.added_at = added_at.map(|s| s.to_owned());
    (uuid, m)
}
```

`disk_member` defaults `devid` and `added_at` to `None`; tests that need
a populated devid or `added_at` call `disk_member_with`. The two-helper
shape keeps the common case one-liner while letting devid-sensitive
tests (enrichment, RemoveMissing replay, Remove-recovery never-enriched
carve-out) state intent at the call site.

**Seed allocation convention.** Each test module owns a disjoint seed
range; cross-module fixture borrowing is forbidden. The
reverse-of-UUID-lex fixtures pinned for `tui/`, `status.rs`,
`doctor.rs`, `preflight.rs`, and `add.rs` planning use `1` (z-disk),
`50` (m-disk), `99` (a-disk) -- treat these three values as reserved
for the reverse-ordering pin and do not reuse them elsewhere. All
other modules allocate from their own block:

- `cli/src/membership.rs` tests: 100-199.
- `cli/src/journal.rs` tests: 200-299.
- `cli/src/add.rs` tests: 300-399.
- `cli/src/remove.rs` tests: 400-499.
- `cli/src/replace.rs` tests: 500-599.
- `cli/src/recover.rs` tests: 600-799 (largest module; double block).
- `cli/src/lock.rs` tests: 800-899.
- `cli/src/discover.rs` tests: 900-999.
- `cli/src/tui/*` tests: 1000-1099.
- `cli/src/status.rs` tests: 1100-1199.
- `cli/src/doctor.rs` tests: 1200-1299.
- `cli/src/preflight.rs` tests: 1300-1399.
- All other modules: 2000+ on first use, claimed in a comment at the
  top of the test module.

Within a module, allocate ascending from the block start. If a fixture
shares state with another fixture in the same module, reuse the seed
explicitly (the seed IS the fixture identity for cross-fixture
matching). The convention is enforced by reviewer audit, not a
compile-time gate; a regression that picks an out-of-range seed is
caught only if the resulting fixture happens to collide with another
module's test that runs in the same process -- which is enough of a
tripwire for module-local discipline.

The algorithm is pinned (`format!("00000000-0000-0000-0000-{:012x}", seed)`)
rather than `Uuid::new_v5` or `Uuid::from_u128` so that:

- The mapping `seed -> LuksUuid` is trivially readable from a test diff
  -- no namespace constant, no `as u128` cast to keep in your head.
- Canonical lowercase hyphenated form falls out of the format string
  unchanged, so `LuksUuid::parse` round-trips deterministically.
- UUID-lex order equals seed order, which is the single property the
  reverse-of-UUID-lex fixtures (`a-disk` at high seed, `z-disk` at low
  seed) require. A v5/v4/`from_u128` mapping would make the
  seed-to-lex-order relationship opaque and force every fixture author
  to recompute it.

`LuksUuid::parse` MUST accept this form (it canonicalizes via
`uuid::Uuid::parse_str`, which accepts hyphenated lowercase). Tests that
construct fixtures call `test_uuid(seed)`; production code never calls
it.

### `discover.rs`

Discovery is the only cold-disk label consumer.

The existing parsers `parse_cryptsetup_luks_version` and
`parse_cryptsetup_luks_label` already share a single
`CryptsetupLuksDumpText` raw output in discover. Add a third parser
that consumes the same raw output:

- Introduce `parse_cryptsetup_luks_uuid_from_dump` in
  `cli/src/parse/cryptsetup_luks_uuid.rs` (the SAME file that holds
  the existing `parse_cryptsetup_luks_uuid` parser for the
  `cryptsetup luksUUID` command). Both parsers extract the same
  `LuksUuid` value type and share the same canonicalization
  contract, so co-locating them keeps the value-type-to-source
  relationship one-hop. Do NOT create a new sibling module.
  The new function scans the `luksDump` text body for the `UUID:`
  line and routes the value through `LuksUuid::parse`. Reuse
  `RawCommandOutput`; do not introduce a new `CmdRequest`.
- The existing `parse_cryptsetup_luks_uuid` (which parses the
  `cryptsetup luksUUID` command stdout) continues to handle that
  command but MUST be updated to route its trimmed stdout through
  `LuksUuid::parse` instead of constructing `LuksUuid(trimmed.to_owned())`
  directly (`cli/src/parse/cryptsetup_luks_uuid.rs:35` today). The
  raw-tuple construction is incompatible with the value-type
  constructor lockdown and bypasses canonicalization. Every
  `LuksUuid` value that flows into `PoolDevice.luks_uuid` through
  `probe_pool` (`cli/src/probe.rs:304`) or
  `classify_mapper_ownership` (`cli/src/luks.rs:771`) must therefore
  be canonical, so that downstream `membership.by_uuid(&dev.luks_uuid)`
  lookups against a canonical-key map cannot silently miss when an
  upstream `cryptsetup` release emits an uppercase or URN form. The
  existing `uuid::Uuid::parse_str` sanity check stays but becomes
  redundant once `LuksUuid::parse` owns the validation.
  (Note: `parse_cryptsetup_status` is unrelated -- it extracts only
  the `device:` line and does not produce a `LuksUuid`; no edit needed
  there.)
- Missing `UUID:` and invalid `UUID:` text in the dump must surface
  as field-specific parse errors. Discovery maps those parse errors to
  explicit warnings and skips the disk:
  - `DiscoverWarning::MissingLuksUuid { path: String }` displays:
    `skipping {path}: luksDump output missing UUID`
  - `DiscoverWarning::InvalidLuksUuid { path: String, raw: String, detail: String }` displays:
    `skipping {path}: invalid LUKS UUID "{raw}" -- {detail}`

  `path` is `String`, matching every existing `DiscoverWarning::*`
  variant on master (`LuksDumpFailed`, `LuksDumpUnparseable`,
  `UnsupportedLuksVersion`, `CannotCanonicalize`, `InvalidDiskName`
  all type `path: String`). Do NOT introduce a `PathBuf` here -- the
  inconsistency would force every `DiscoverWarning::Display`
  consumer to handle two render paths, and the discover warnings
  are operator-facing one-liners where the lossy-Unicode `Display`
  behavior of `String` is exactly what we want (a non-UTF-8 by-id
  symlink should surface its observed bytes, not panic). These are
  separate from the existing label handling: a missing label still
  silently skips as it does today, while a malformed `braid-*` label
  remains `DiscoverWarning::InvalidDiskName`.

Then update discovery:

- `DiscoverOutcome.members` becomes `PoolMembership`. Downstream consumers
  already treat discover output as authoritative membership, so a
  `PoolMembership`-shaped field lets `discover --write` save its output
  through the same `save_membership` path as every other writer and avoids
  a second collection type that the implementer would have to define and
  convert. The cloned-disk `DuplicateUuid` check fires as an explicit
  pre-insert pass in `discover.rs` (already specified above as "Raise it
  from the discover code path before delegating to
  `PoolMembership::insert`").
- Keep the current alias dedup and friendly label-collision ordering:
  - canonicalize by-id symlink;
  - group by `DiskName`;
  - if the same label appears on two distinct physical canonical paths, return
    the existing `LabelCollision` error;
  - if aliases point to the same physical disk, choose by
    `(by_id_priority, filename)`.
- After alias dedup, insert exactly one UUID-keyed `DiskMember` per physical
  disk.
- Duplicate UUIDs after dedup MUST surface as a structured discover-side
  error analogous to `LabelCollision`, not as a generic
  `MembershipError::Conflict` from `PoolMembership::insert`. The
  cloned-disk case (dd-imaged drive, physically distinct disks sharing
  one LUKS UUID) is operator-friendly to diagnose only when both
  by-id paths are named in the error. Discover already has both
  canonical paths in scope at insertion time.

  Add `DiscoverError::DuplicateUuid { uuid: LuksUuid, name1: DiskName,
  path1: String, name2: DiskName, path2: String }` mirroring the
  existing `DiscoverError::LabelCollision { name, path1, path2 }`
  variant (`discover.rs:18-22`). Raise it from the discover code path
  before delegating to `PoolMembership::insert`. `Display` text MUST
  be:

  ```text
  duplicate LUKS UUID: braid-{name1} ({path1}) and braid-{name2} ({path2}) share UUID {uuid} -- relabel or detach one before retrying (this typically indicates a dd-cloned disk)
  ```

  The shape mirrors the existing `LabelCollision` wording ("X and Y --
  relabel or detach one before retrying") so the two operator workflows
  read consistently. Sort `(name1, path1)` and `(name2, path2)`
  lexicographically by `path` for determinism, matching the
  `label_collision` helper at `discover.rs:104-113`.
  `PoolMembership::insert`'s `Conflict` error remains the defense-in-depth
  backstop for non-discover insertion paths.

**Precedence (pinned): `LabelCollision` fires before `DuplicateUuid`.**
A cloned-disk scenario with two physical drives carrying the same
LUKS UUID AND the same `braid-<name>` label would, in principle,
trip both `LabelCollision` and `DuplicateUuid`. The check order is
fixed: label collision is checked first (matching the existing
`discover.rs` flow that asserts label uniqueness during the
alias-dedup pass at `:104-113`), so the operator sees
`LabelCollision { name, path1, path2 }`. The `DuplicateUuid` check
runs only over the label-deduped set (one entry per physical disk
keyed by `(canonical_by_id, label)`), so it fires only when labels
are distinct but the UUID is shared. Rationale: a cloned disk under
the same name is a stronger operator signal -- the operator named
both disks identically, indicating they intended the same role, and
the label-collision remediation (relabel one) is the same first
step as the UUID-collision remediation. Tests for this precedence
must build a fixture where both axes would fire and assert the
returned error is `LabelCollision`, not `DuplicateUuid`.

#### `discover --write` pre-save fail-closed gates

`discover --write` is the operator's primary repair tool when
`pool.json` is corrupt or absent: it computes membership from live
disks and calls `save_membership` directly, with no
`load_membership` step. Two failure modes follow from that shape and
MUST be closed by explicit pre-save gates inside the discover
command path (NOT inside `save_membership`, which is also called by
the normal mutating commands after their primary operation
succeeds):

1. **Refuse when `/var/lib/braid/pending-op.json` exists.** The
   cutover runbook lists "No `/var/lib/braid/pending-op.json`" as a
   precondition but the plan adds no enforcement. A corrupt
   `pool.json` is exactly the state where a `pending-op.json` is
   most likely to coexist (mid-recovery), and overwriting
   `pool.json` in that window invalidates
   `journal.pre_membership`/`target_membership` against the new
   live state. The next `braid recover` then routes through
   `live_pool_matches_membership` against an inconsistent pair,
   which can clear the journal at the wrong phase (the false
   `Ok(true) -> premature clear` failure mode the gate-spec
   already calls out) or deadlock recovery into the topology-
   mismatch path even though the live pool is valid.

   Required behavior: BEFORE computing membership and BEFORE any
   `save_membership` call, `discover --write` MUST check whether
   `paths.journal_path()` (i.e. `/var/lib/braid/pending-op.json`)
   exists. If it does, the command aborts with a structured error
   and `save_membership` is NOT called.

   Error wording (pinned for test):
   `discover refusing to write pool.json: pending-op.json exists at {path} -- run 'braid recover' first (see docs/luks-unlock.md)`.

   No `--force-overwrite-journal` escape hatch in this migration.
   The cutover runbook is the primary caller and already lists "No
   pending-op.json" as a precondition; the fail-closed gate enforces
   that precondition rather than hoping the operator reads it.

2. **Refuse when on-disk `pool.json` is in old name-keyed shape.**
   `discover --write` does not call `load_membership`, so
   `MembershipError::Corrupt` never fires for it. The cutover
   runbook tells the operator to move the old `pool.json` aside
   (step 4 of the runbook); if they forget, the old name-keyed
   `pool.json` is silently overwritten -- and the backup the
   operator was supposed to make in step 2 becomes the only
   recovery path. Worse, after a botched cutover the operator may
   notice `discover --write` produced fewer members than expected
   (e.g. a disk was momentarily detached) with no in-place reference
   to compare against.

   Required behavior: BEFORE any `save_membership` call,
   `discover --write` MUST sniff the on-disk `pool.json` schema.
   If the file exists, read it as raw JSON (NOT through
   `load_membership`'s typed `serde_json::from_str::<PoolMembership>`),
   inspect the `disks` field's keys, and require every key to be a
   canonical hyphenated UUID. The sniff regex mirrors the one
   already pinned in `scripts/braid-destroy.sh` (anchored
   hyphenated UUID match). If any key is not a canonical UUID, the
   command aborts with a structured error and `save_membership` is
   NOT called.

   Error wording (pinned for test):
   `discover refusing to write pool.json: existing file at {path} is not in UUID-keyed format -- back it up and move it aside before retrying (see docs/luks-unlock.md)`.

   If the existing `pool.json` is absent, malformed-but-empty, or
   not parseable as JSON at all, the sniff is treated as "no
   conflict" and the command proceeds; the goal is to refuse a
   silent overwrite of a recognizable old-shape file, not to
   double up the `load_membership` corruption check (which fires
   from every other command path).

Both gates apply ONLY to the discover command path
(`Commands::Discover` in `main.rs:720`'s arm with `args.write`).
They do NOT apply to `save_membership` callers from
`add`/`remove`/`replace`/`recover`, all of which have already
loaded membership through the corrupt-fail-closed path or
constructed it programmatically. The gates live in the discover
arm or in a thin discover-side helper, not inside `save_membership`.

Test pins (Test Plan additions):

- **`discover --write` refuses when `pending-op.json` exists**:
  build a `Paths` whose `journal_path()` points at a tempfile
  containing a valid `Journal` JSON; run the discover-write code
  path against a live-disk fixture that would otherwise produce
  two members. Assert (a) the command returns the structured
  error whose `Display` contains the literal substring
  `discover refusing to write pool.json: pending-op.json exists at`;
  (b) the recording layer observed zero `save_membership` writes;
  (c) the on-disk `pool.json` (if it existed pre-call) is
  byte-for-byte unchanged.
- **`discover --write` refuses when existing `pool.json` is
  name-keyed**: pre-populate the `Paths.pool_json_path()` location
  with a synthetic old-shape `pool.json` (top-level disk-name
  keys); run the discover-write code path against a live-disk
  fixture that would otherwise produce two UUID-keyed members.
  Assert (a) the command returns the structured error whose
  `Display` contains the literal substring
  `is not in UUID-keyed format -- back it up and move it aside`;
  (b) the recording layer observed zero `save_membership` writes;
  (c) the synthetic old-shape `pool.json` is byte-for-byte
  unchanged on disk.
- **`discover --write` proceeds when neither gate fires**: with no
  `pending-op.json` and either no `pool.json` or a valid UUID-keyed
  `pool.json` on disk, the command writes the new membership
  through `save_membership` as today. Pins that the gates are
  fail-closed-on-condition, not fail-closed-by-default.

### `lock.rs`

Lock must close observed live member mappers, not reconstructed names.

**Today's baseline (verify before refactoring).** Today's `plan_lock`
calls only `probe_fsid` for preflight; it makes no `probe_pool` call,
so per-device live UUIDs are not available to it. Today's close set
is `open_mappers`, constructed by reconstructing
`mapper_name(&name)` from membership keys and filtering by
`fs.exists(...)`. Today's orphan handling is `scan_orphan_mappers`
over `/dev/mapper`. There is no drift detection because the lock
plan never reads per-device live state.

This migration **adds** a `probe_pool` call so per-device UUIDs are
visible at planning time, then routes the close-set decision through
those observed UUIDs. The legacy name-derived close set survives only
in the new `FsidOnly` fallback, where it stays drift-blind because
the data needed for drift detection is exactly what `probe_pool`
provides and `probe_fsid` does not.

Why the close-set is one helper, not two. The naive design splits
classification into "member-owned mappers, derived from membership" and
"orphan mappers, derived by scanning `/dev/mapper`". This split has no
return path for the case that matters most under the migration: a
stranded `braid-*` mapper whose underlying LUKS UUID matches a member.
The stranded mapper is not in the membership-derived set (its observed
name does not match `mapper_name(&member.name)`), and the orphan scanner
was about to classify it as orphan -- but it is member-owned. A helper
that takes `&member_owned` and returns only `orphan_mappers` cannot
reclassify; the reviewer or future refactor must build both sides in one
pass to make reclassification representable in the API, not only in
prose. The unified helper below is the fix; do not re-split it.

Replace the split `open_mappers` plus `scan_orphan_mappers` model with a single
close-set builder. The migration adds a `MemberOwnedClose` for member-owned
closes, classified at scan time so `execute` never has to redo the identity
decision:

```rust
struct MemberOwnedClose {
    mapper: MapperName,    // observed mapper, e.g. MapperName("braid-wrong".into())
    display_name: DiskName,
}

struct LockCloseSets {
    member_owned: Vec<MemberOwnedClose>,
    orphan_mappers: Vec<OrphanMapper>,
}
```

`MemberOwnedClose.mapper` is `MapperName`, not raw `String`. The
mapper-as-`MapperName` typing matches every other observed-mapper
field the migration introduces (`PoolDevice.mapper: MapperName`,
`NullUnderlyingDevice.mapper: MapperName`,
`ReplaceJournalSource::Live.old_mapper: MapperName`). Stringly-typed
mapper paths are exactly the foot-gun the rest of the migration is
closing; do not reintroduce one here. The existing `OrphanMapper`
struct on master still types `mapper: String` for legacy reasons --
update its declaration in the same change so the close-set fields
are consistently `MapperName`.

Concrete `OrphanMapper.mapper` consumer sweep (the implementer MUST
visit every call site in the same change that retypes the field;
leaving any one site reading a raw `String` is what causes the
silent-Display-divergence the reviewer flagged):

- `OrphanMapper::mapper(&self) -> &str` accessor at
  `cli/src/lock.rs:46-48`: change return type to `&MapperName` (or
  delete the accessor and have callers read `.mapper` directly --
  it is `pub(super)` in scope already). Every caller of `.mapper()`
  below moves with the change.
- `close_set_paths` at `cli/src/lock.rs:114-121`: today builds
  `/dev/mapper/{m}` from `&str` mappers. Under the migration the
  helper takes `&[MemberOwnedClose]` and `&[OrphanMapper]` directly
  (no separate `open_mappers: &[String]` argument), and renders
  each mapper through `MapperName::Display` -- which for a newtype
  over `String` is byte-identical to today's output. The "every
  observed mapper is a `MapperName` instance" rule is what makes
  this safe; render through `Display` to keep the wire format
  stable.
- `compile_lock_steps` at `cli/src/lock.rs:197-247`: same pattern;
  the per-orphan `description: format!("close LUKS mapper {} (orphan)",
  orphan.mapper())` and the `CryptsetupClose { mapper: orphan.mapper().to_owned() }`
  lines render through `MapperName::Display` (or `.0.clone()` if a
  raw `String` is needed by the request type at the time of this
  edit -- the migration touches `CryptsetupClose` only if its
  `mapper` field already moved, see below).
- `LockPlan::execute` at `cli/src/lock.rs:299, 348, 433-437`: the
  `orphan_mappers` field iteration moves with the type change.
- Dry-run preview at `cli/src/lock.rs:434, 461, 501`: the
  `PreviewNote::Warn(orphan_mapper_warn_body(om.mapper()))` line
  passes `&MapperName` (or `MapperName::as_str()`) into the
  warn-body builder; the builder either accepts `&MapperName`
  directly or reads `.as_str()` to keep the rendered text
  byte-identical to today's.

Snapshot review obligation. Snapshot files that bake
`OrphanMapper`-rendered mapper strings live under:

- `cli/src/snapshots/` (dry-run preview snapshots for `lock`,
  `add`, `replace`).

The implementer MUST grep these for `braid-` mapper text after the
typing change, regenerate with `INSTA_UPDATE=always cargo test`, and
diff the result. Any non-empty diff is a `MapperName::Display`
divergence -- investigate and align before accepting. Acceptance
criterion: snapshot bytes are unchanged because `MapperName`'s
`Display` is byte-identical to today's `String` rendering.

**`CryptsetupClose { mapper: String }` stays `String` in this
migration (pinned).** The field is NOT retyped to `MapperName` as
part of this migration. Rationale: the close-call sites already
render through `MapperName::Display` at the request-construction
boundary (the call sites flipped to `MapperName` in the
consumer-sweep above pass the `Display`-rendered string into
`CryptsetupClose`), so the wire-format bytes are already
`MapperName`-derived. Retyping the request field would force a
snapshot regeneration for every dry-run preview that mentions a
`CryptsetupClose` step (the rendering path is `Display`-clean today
but a typed field on the request can render differently if the
emitter is touched), inflating the migration's snapshot blast
radius for a purely-internal ergonomic gain. Defer the retype to a
post-migration cleanup. Leave a `// TODO(post-migration): retype
mapper: String to MapperName once this migration lands.` comment
on the `CryptsetupClose` variant.

Downstream call sites
(`compile_lock_steps`, `close_set_paths`, the `forget_devs` builder)
operate on `MapperName` and `MapperName::Display` for argv rendering.

**Keep two structs in this migration (pinned).** This migration keeps
two structs (`MemberOwnedClose` and `OrphanMapper`). Unifying them
into one `LockMapperClose { mapper: MapperName, display_name, kind }`
is explicitly DEFERRED to a follow-up refactor. Rationale: this
migration already touches every consumer of the close-set;
re-shaping the close-set struct AND re-rendering every dry-run
preview snapshot in the same change inflates blast radius for a
purely-internal ergonomic gain. The two-struct shape preserves
byte-identical snapshot output (only the `OrphanMapper.mapper`
field-type flips from `String` to `MapperName`, which `Display`-
renders identically); the unification would force a snapshot-shape
diff this migration's reviewer cannot easily distinguish from a
real behavior diff. Concrete consequences:

- `compile_lock_steps` and `close_set_paths` take
  `(member_owned: &[MemberOwnedClose], orphan_mappers:
  &[OrphanMapper])` -- two parallel slices, same order today, same
  order tomorrow.
- The two loops in `LockPlan::execute` remain two loops, even if
  they end up structurally identical apart from the warn body.
  Justify the duplication with a comment naming the deferred
  unification.
- Leave a `// TODO(post-migration): consider unifying
  MemberOwnedClose and OrphanMapper into LockMapperClose { kind }
  once this migration lands -- see plans/impl/<plan-file>.md.`
  comment at the struct declarations so a future refactor reviewer
  sees the deferred choice and does not have to re-derive it.

The choice does NOT affect the `compile_lock_steps` /
`close_set_paths` signatures' caller side: both callers already pass
the close set as two slices today. The dry-run preview snapshot
files are therefore unchanged.

Planner behavior:

- When the pool is mounted, try `probe_pool` first.
- On full probe success:
  - classify `pool.devices` by UUID;
  - classify `pool.null_underlying` by persisted devid;
  - scan `/dev/mapper` for remaining `braid-*` mappers and resolve each by
    `cryptsetup status` plus `cryptsetup luksUUID`;
  - if the UUID matches membership, put the observed mapper into
    `member_owned`;
  - otherwise put it in `orphan_mappers` with the existing warning.
- If `/dev/mapper` cannot be read, warn and return the pass-1 `member_owned`
  entries with no orphans. This preserves current best-effort semantics.
- The per-stranded-mapper `cryptsetup status` plus `cryptsetup luksUUID`
  resolution can itself fail on a single mapper. Treat that as a
  per-mapper degrade: log a warning, demote that mapper to `orphan_mappers`,
  continue scanning. Do not let one cryptsetup hiccup tank the whole lock.

  **Per-stranded-mapper execution shape (pinned).** The resolution is
  serial (one mapper at a time), in-process, inside a helper
  `classify_stranded_mapper(runner, &mapper) -> Result<StrandedClass,
  CmdError>` called from `plan_lock` AFTER the `pool.devices` and
  `pool.null_underlying` classification passes complete. The helper
  issues exactly two `CmdRequest` calls per mapper, in this order:

  1. `CryptsetupStatus { mapper: mapper.clone() }` -- to confirm
     the mapper is a `cryptsetup`-managed dm slot, not a foreign
     dm device that happens to be `braid-*`-named, and to extract
     the backing device path. A non-zero exit or unparseable
     output ends the helper with `Err(CmdError::...)`.
  2. `CryptsetupLuksUuid { device: format!("/dev/mapper/{}",
     mapper.as_str()) }` -- the unified `CryptsetupLuksUuid`
     variant pinned under `LUKS Format Boundary`. The parsed UUID
     is matched against membership keys; a match yields
     `Ok(StrandedClass::MemberOwned { display_name })`, a non-match
     yields `Ok(StrandedClass::Orphan)`.

  ```rust
  enum StrandedClass {
      MemberOwned { display_name: DiskName },
      Orphan,
  }
  ```

  Failures are captured per-mapper: `plan_lock` collects
  `Result<StrandedClass, CmdError>` for each stranded mapper, then
  walks the result vector and folds it into the close-set:

  ```rust
  for (mapper, result) in stranded_classifications {
      match result {
          Ok(StrandedClass::MemberOwned { display_name }) => {
              member_owned.push(MemberOwnedClose { mapper, display_name });
          }
          Ok(StrandedClass::Orphan) => {
              orphan_mappers.push(OrphanMapper { mapper, disk_name: /* see F28 */ });
          }
          Err(cmd_err) => {
              eprintln!("Warning: failed to classify stranded mapper {mapper}: {cmd_err}; treating as orphan");
              orphan_mappers.push(OrphanMapper { mapper, disk_name: /* see F28 */ });
          }
      }
  }
  ```

  Not parallel (the `cryptsetup` ioctls are not contention-sensitive
  at the scale of a NAS pool, and serial execution makes the
  recording-runner test contract trivial -- each
  `classify_stranded_mapper` call observes exactly one
  `CryptsetupStatus` and at most one `CryptsetupLuksUuid` in the
  request log per mapper, in deterministic order). Not batched
  (cryptsetup has no batch query primitive). The per-mapper failure
  capture is the only divergence from the happy-path code; the
  warning message above is the pinned text, and the per-mapper
  degrade test asserts the literal substring `failed to classify
  stranded mapper`.
- If the new `probe_pool` call fails for per-device reasons,
  fall back to `probe_fsid` so `require_lock_preflight` still runs.
  This `probe_pool`-then-`probe_fsid` chain is new in this migration:
  today's `plan_lock` only calls `probe_fsid`, so there is no
  preexisting fallback to "preserve" here -- the chain is being
  introduced as part of the UUID-identity work. Model the snapshot
  as an enum:
  ```rust
  enum LockSnapshot {
      Full(PoolState),
      FsidOnly { fsid: String, probe_error: ProbeError },
  }
  ```
  The `Full` arm drives the UUID-classified close set above; the
  `FsidOnly` arm runs `require_lock_preflight` against the FSID and uses
  the legacy name-derived close set plus the legacy orphan scan
  (which is the only thing `plan_lock` does today).

  **Ordering parity (pinned).** Today's `close_set_paths` helper at
  `cli/src/lock.rs:114-121` already iterates `open_mappers` first and
  then `orphan_mappers` (verified at HEAD: `open_mappers.iter().chain(orphan_mappers.iter())`).
  The `LockSnapshot::FsidOnly` arm MUST preserve this ordering -- do
  not refactor it to interleave, sort, or reverse the two sets in
  the fallback. The `Full` arm's pinned `member_owned` first, then
  `orphan_mappers` rule (above) collapses onto the same ordering, so
  the two arms produce identical close orders on identical inputs;
  this is verified, not coincidental. Pin with a unit test that
  builds a `LockPlan` via the FsidOnly path with both a member-owned
  mapper and an orphan and asserts the close order matches the
  Full-path test on the same inputs.

  Emit a warning to stderr whose body MUST be exactly:

  ```text
  warning: per-device probe failed ({probe_error}); falling back to FSID-only lock preflight. Mapper drift detection is disabled for this run. In this mode, mappers under the names braid-<member-name> are closed without verifying their LUKS UUID; an unrelated disk opened under that name will be torn down.
  ```

  `{probe_error}` is the `Display` of the `ProbeError` captured in the
  `FsidOnly` variant. Tests pin two substrings independently:
  `Mapper drift detection is disabled for this run.` AND
  `an unrelated disk opened under that name will be torn down.` so a
  future ergonomic edit that drops either operator-relevant signal
  surfaces in CI. The second sentence is load-bearing: the FsidOnly
  fallback runs `scan_orphan_mappers_by_name` against the legacy
  name-derived `open_mappers` set, which classifies any mapper under
  `braid-<member.name>` as member-owned by name alone. A foreign disk
  (or a stuck dm slot from a prior crash) opened under that name is
  then closed without a UUID check. This is not a write-corruption
  hazard -- `cryptsetup close` tears down only the dm slot, leaving
  the foreign disk's bytes intact -- but it is an operator surprise
  the bare `Mapper drift detection is disabled` wording does not
  foreshadow.
- Rename today's `scan_orphan_mappers` to `scan_orphan_mappers_by_name`
  and keep it for the `FsidOnly` branch. Its internal membership lookup
  switches from `disks.contains_key(disk_name)` to
  `membership.by_name(...).is_some()` so it compiles against the new map
  shape; it stays drift-blind in this fallback (drift detection requires
  per-device probe data, which only `LockSnapshot::Full` provides), but
  stranded `braid-*` mappers are still found and closed.

  **`DiskName::parse` boundary at the rewritten lookup.** The post-rewrite
  call site is roughly:
  ```rust
  let Some(disk_name_raw) = name_from_mapper(&entry) else { continue; };
  let disk_name = match DiskName::parse(disk_name_raw) {
      Ok(n) => n,
      Err(_) => {
          // Malformed mapper: braid-<malformed-name>. Treat as orphan.
          orphans.push(OrphanMapper { mapper: entry.into(), disk_name: disk_name_raw.to_owned() });
          continue;
      }
  };
  if membership.by_name(&disk_name).is_some() {
      continue;
  }
  orphans.push(OrphanMapper { mapper: entry.into(), disk_name: disk_name.to_string() });
  ```
  On `DiskName::parse` failure (the mapper is `braid-<not-a-valid-disk-name>`,
  e.g. `braid-..foo`), the mapper falls through to the orphan classification
  rather than skipping silently. Rationale: a malformed `braid-`-prefixed
  mapper is by definition not a current member (no member can have a name
  that `DiskName::parse` rejects, because every membership insertion goes
  through `DiskName::parse`), so it cannot be member-owned -- orphan is
  the correct classification. Skipping it silently would leak a `braid-*`
  mapper through `braid lock` and surface as an orphan-mapper warning
  only on the next lock, which is the same outcome with worse latency.
  The `OrphanMapper.disk_name` field retains the raw text for the
  per-orphan warning body. Do NOT log+drop the malformed mapper: that
  is the silent-skip regression the lock orphan policy is designed to
  prevent.

  **`OrphanMapper.disk_name` field type post-migration (pinned).**
  Stays `String`. Do NOT retype it to `DiskName` or
  `Option<DiskName>` -- the field's purpose is to carry the parsed
  basename of the mapper into the per-orphan warning body
  (`name_from_mapper(path)`), which by construction admits text
  that `DiskName::parse` rejects (malformed-mapper case above) and
  also admits text the parser accepts (legitimate orphan with a
  valid-looking basename). A `DiskName` typing would force the
  malformed-mapper case to either (a) use a sentinel `DiskName`
  (no construction path exists, by design) or (b) lose the raw
  text, blanking the warning body. An `Option<DiskName>` typing
  would force every consumer to handle both arms when the
  practical answer is identical (render whatever raw text was
  captured). Field declaration:

  ```rust
  pub struct OrphanMapper {
      pub mapper: MapperName,
      pub disk_name: String,
  }
  ```

  The `mapper` field IS retyped to `MapperName` per the
  consumer-sweep above; the divergence between the two fields is
  intentional. Tests for the malformed-mapper case construct
  `OrphanMapper { mapper: MapperName::new_unchecked(...),
  disk_name: "..foo".to_owned() }` directly and assert the
  rendered warning includes the raw `..foo` basename.
- The match on `probe_pool`'s `Err(ProbeError)` MUST enumerate every variant
  explicitly -- no catch-all `_ => fallback`. The audit at HEAD (see
  `cli/src/probe.rs:58-88`) lists `Cmd`, `Parse`, `PoolDevice`, `NotBtrfs`,
  `UnsupportedLuksVersion`, `MapperConflict`, and `MountInfo`. Policy:
  - `NotBtrfs` aborts (preserves current behavior).
  - `Cmd`, `Parse`, `PoolDevice`, `UnsupportedLuksVersion`, `MapperConflict`,
    `MountInfo` fall back to `probe_fsid` and emit the
    `LockSnapshot::FsidOnly` warning. These are the per-device probe
    failures the FSID-only path is designed to absorb.
  - Add a comment above the match forcing any new `ProbeError` variant
    to opt in explicitly, so a future variant cannot inherit the
    fallback by default and silently mask a real configuration error.

Execution behavior:

- `LockPlan` stores `member_owned`, not just name-derived `open_mappers`.
- `execute` iterates `member_owned` first, then `orphan_mappers`, preserving
  today's `close_set_paths` ordering of member-owned closes before orphan
  closes.
- `execute` calls `CryptsetupClose` with the observed mapper string for every
  entry in that order.
- The `btrfs device scan --forget` close set is built from the same iteration
  order and the same observed mapper strings as execution.

**Accepted risk: in-process member-owned close double-drift.** The
"Double-drift defense-in-depth UUID probe" specified in `Journal Schema`
covers `replace.rs:707`, `recover.rs:2935`, and `remove.rs:180-183`
but NOT `LockPlan::execute`'s `CryptsetupClose` calls against the
`member_owned` set. In principle, between lock-planning (when UUID
classification ran) and lock-execution (when `CryptsetupClose` is
issued), an operator could manually `cryptsetup close` a member
mapper and `cryptsetup open` a foreign disk under the same name,
causing `LockPlan::execute` to `cryptsetup close` the foreign mapper.
This is left uncovered for three reasons:

- The window is the in-process span between classification and the
  per-mapper close, normally measured in milliseconds to seconds; the
  three covered sites have either a recovery-replay window (much
  wider) or run after a long-running btrfs operation.
- The hazard surface is "operator closes a foreign mapper" -- the
  foreign disk's contents are not modified by `cryptsetup close`; only
  the dm slot is torn down. None of the covered sites' hazards are
  symmetric here (they all involve writes to the foreign disk via
  btrfs after the swap).
- The trigger requires concurrent operator action against the same
  mapper name during a running `braid lock`, which is itself misuse.

A future extension that adds the probe to `LockPlan::execute` would
be acceptable but is not required by this migration. Leave a
`// Accepted risk: ...` comment at the `CryptsetupClose` site
referencing this paragraph so a future reviewer does not silently
re-introduce the gap when extending the section.

Planner-side close-set wiring (explicit). The `compile_lock_steps`
helper (`lock.rs:197`) and `close_set_paths` helper (`lock.rs:114`)
today both consume `open_mappers: &[String]` derived from reconstructed
`mapper_name(&name).0`. For the `LockSnapshot::Full` arm, the
migration MUST route the planner-side close set through the SAME
observed mapper strings used by execution -- not just for the actual
`CryptsetupClose` step but for the `BtrfsDeviceScanForget` step and
the dry-run preview as well. Concretely:

- `compile_lock_steps`, the close-set passed to it, and
  `LockPlan.member_owned` (the successor to today's
  `LockPlan.open_mappers`) ALL derive from
  `pool.devices[i].mapper.0` for the `LockSnapshot::Full` arm.
- The `forget_devs` set passed to `BtrfsDeviceScanForget` (both in
  `compile_lock_steps` at `lock.rs:213` and the
  `LockPlan::execute` site at `lock.rs:348`) is built from the same
  observed mapper strings.
- The `LockSnapshot::FsidOnly` arm continues to use reconstructed
  names via `scan_orphan_mappers_by_name` (no observed-mapper data
  available without a per-device probe).

Why this matters: if execution closes the observed `braid-WRONG`
mapper but the forget set was built from the reconstructed
`braid-<name>` string, the existing `forget_devs.retain(|p|
fs.exists(p))` filter at `lock.rs:349` drops the reconstructed path
(it does not exist on disk) and skips `btrfs device scan --forget` for
the actually-held slot. The physical close still happens, but the
btrfs scan registry retains a stale dm-uuid entry -- exactly the race
`BtrfsDeviceScanForget` was added to close.

Test pin (Test Plan addition): under `LockSnapshot::Full`, a drifted
member-owned mapper (`pool.devices[i].mapper = "braid-WRONG"`,
`pool.devices[i].luks_uuid` matches a member) appears in `forget_devs`
with the observed name `braid-WRONG` and NOT the reconstructed name
`braid-<member.name>`. This pins both the planner-side wiring and the
forget-set source-of-truth.

### Other Rust Touch Points

- `mount.rs`: iteration and expected mapper construction use `member.name`;
  stored UUID verification reads the map key.
- `unlock.rs`: post-mount enrichment no longer depends on mapper name parsing.
- `status.rs`: display names come from membership by UUID/devid. Status output
  must not surface pool-json-sourced devids as authoritative live data.
- `cli/src/types.rs`: migrate `ConfigDisk.name` from raw `String` to
  `DiskName`. Probe outputs already originate from validated config/member
  names, so the constructor invariant should hold at the boundary rather than
  being re-checked by downstream command code.
- `tui/probe.rs`:
  - `devid_to_name`: chain `membership.by_uuid(&dev.luks_uuid)` for present
    devices in `domain.devices`, falling back to `membership.by_devid(d.devid)`
    for `domain.null_underlying`. Read the canonical name from
    `DiskMember.name` once the member is resolved.
  - `name_to_luks_uuid`: resolve through membership rather than scanning
    `domain.devices`. Every member's UUID IS the `LuksUuidMap` key
    regardless of present/null-underlying state, so the post-migration
    body is `membership.by_name(&name).map(|(uuid, _)| uuid.clone())`
    with no fallback. (Pre-migration prose referenced
    `DiskMember.luks_uuid`, but that value-side field is removed by
    this migration -- the map key is the canonical identity for
    members in every state.)
  - `disk_transport` lsblk-correlation loop (today's `if let Some(name)
    = crate::config::name_from_mapper(&child.name)` site that builds
    the `HashMap<String, String>` keyed by disk name): this is
    Pattern #3 -- the downstream consumer in
    `cli/src/tui/view/mod.rs` (`disk_table`) iterates
    `model.disk_names` and looks up transport by name, so the map
    stays name-keyed for display. Keep the `name_from_mapper` call
    here with a `// Pattern #3: display-only -- do not use for
    identity decisions.` comment.
  - Regression pin: after the refactor, no `name_from_mapper` call in
    this file makes an identity decision (the only surviving call is
    the Pattern #3 `disk_transport` site above). A grep for
    `name_from_mapper` will still match that single site; tag it
    with the Pattern #3 comment as the contract.
- `tui/mod.rs` (`run`, lines around 33-38): today's code does
  `membership.disks.keys().cloned().collect()` to build a
  `disk_names: Vec<String>` and
  `.iter().map(|(k, m)| (k.clone(), m.by_id.to_string())).collect()`
  to build a `disk_by_id: HashMap<String, String>`. Both are
  display-only caches consumed by `tui/view/mod.rs::disk_table`,
  which iterates `model.disk_names` and looks up by display name.

  Today's outer map is `BTreeMap<String, _>` keyed by name, so
  `disk_names` is implicitly alphabetical (A-Z) -- operator-friendly
  for the disk table. Post-migration the outer map becomes
  `LuksUuidMap<DiskMember>` keyed by `LuksUuid`, so a naive
  `membership.iter().map(|(_, m)| m.name.clone())` produces names in
  UUID-lexicographic order -- effectively random per disk. The TUI
  disk table would then show disks in a different (and shuffling-on-
  reshuffle) order on every fresh `braid add`.

  Required post-migration behavior: build `disk_names: Vec<DiskName>`
  by iterating `membership.iter()` and **then sorting by `DiskName`**
  (the existing `Ord` on the value type) before handing it to
  `Model::new`. Build `disk_by_id` from the same sorted name list so
  the two caches stay parallel.

  The same rule applies at every other surface that iterates
  `LuksUuidMap` into operator-visible ordered output. The audit was
  performed during planning; the following sites MUST sort by
  `DiskName` (no further audit is required, and `browse/*` is
  explicitly out of scope -- it does not consume `PoolMembership`):

  - `cli/src/status.rs:242` (`for name in membership.disks.keys()` --
    the "Unpooled membership disks" loop in `build_compact_drives`):
    iterate the membership entries, then push `CompactDrive` entries
    in `DiskName`-sorted order so the compact drives list is stable
    and operator-friendly.
  - `cli/src/status.rs:379-383` (`membership.disks.iter().map(|(name,
    member)| probe_config_disk(...))`): collect the `(DiskName,
    &DiskMember)` pairs, sort by `DiskName`, then call
    `probe_config_disk` over the sorted iterator. The resulting
    `verbose_ctx.disks` (and therefore the human-readable status disk
    table) is then `DiskName`-sorted.
  - `cli/src/doctor.rs:407-415` (`pool_membership.disks.iter()...
    .collect()` building the `classifications` vec for
    `summarize_declared_disks`): same pattern -- collect, sort by
    `DiskName`, then map to `classifications`. The `declared_disks`
    doctor check output is then stable across pools regardless of
    UUID key order.
  - `cli/src/preflight.rs:28` (`let names: Vec<&str> =
    membership.disks.keys().map(String::as_str).collect()` -- the
    `check_pool_unlocked_if_membership_exists` comma-separated
    operator message): build the name list from sorted iteration so
    the error message lists members in `DiskName` order. A
    random-looking ordering inside a `braid add` error is exactly
    the kind of regression the snapshot heuristic ("display-label
    movements") could misclassify.

  Pin each surface with a regression test on a fixture whose
  `DiskName` order is the reverse of UUID-lexicographic key order
  (e.g. `name="a-disk"` at the lexicographically-last UUID,
  `name="m-disk"` at the middle UUID, `name="z-disk"` at the first
  UUID):

  - `build_compact_drives` test: assert the returned `Vec<CompactDrive>`
    (or at least its name slice) is `["a-disk", "m-disk", "z-disk"]`
    for the unpooled-membership entries.
  - `compute_status_report` (or whichever helper produces
    `verbose_ctx.disks`): assert `verbose_ctx.disks` is
    `DiskName`-sorted.
  - `declared_disks` doctor-check test: assert the resulting
    classification list (or the rendered `CheckResult` body) is in
    `DiskName` order.
  - `check_pool_unlocked_if_membership_exists` test: assert the
    error body contains `"a-disk, m-disk, z-disk"` rather than the
    UUID-key iteration order.

  A regression that pinned any of these orders to raw
  `membership.iter()` (or to `membership.names()` raw iteration) would
  silently produce reversed output and must fail the corresponding
  test. These tests are the structural floor for operator output
  stability and are the only behavioral defense the plan accepts
  against the snapshot-acceptance heuristic ("snapshot changes are
  display-label movements or expected shape changes") swallowing a
  silent reorder.

  Pin this with an explicit assertion in the TUI hot-unplug test
  (NOT a snapshot regression -- a snapshot would flag every unrelated
  TUI change, and the rekey risk is narrow enough that a structure-
  insensitive assertion is the better blast radius). The assertion:
  build a `PoolMembership` whose three members have names sorted
  differently from their UUID-key order (e.g.
  `name="a-disk"` at the lexicographically-last UUID,
  `name="m-disk"` at the lexicographically-middle UUID,
  `name="z-disk"` at the lexicographically-first UUID); after
  building the model, assert `model.disk_names == ["a-disk", "m-disk", "z-disk"]`
  (alphabetical-by-name, NOT membership-iteration-order); and for
  each name `n` confirm
  `model.disk_by_id[n] == membership.by_name(&DiskName::parse(n)?)
  .unwrap().1.by_id.to_string()`. A regression that pinned the order
  to `membership.names()` raw iteration would silently produce
  `["z-disk", "m-disk", "a-disk"]` and pass the old assertion.
- `doctor.rs`, `enroll_key_file.rs`, `preflight.rs`, `main.rs`,
  `pool.rs`, remaining `tui/*`, `browse/*`: replace direct `.disks`
  access with helper methods. Name completion uses
  `membership.names()`.
- `config.rs`: `name_from_mapper` stays. Update its doc comment to flag it as
  display-only after migration. Identity decisions never call it.
- `cli/src/test_fixtures/*` (especially `discover.rs`): rebuild the
  shared test fixture helpers on top of the UUID-keyed model and the
  `disk_member(seed, name, by_id)` helper rather than open-coding
  fixture shapes inline in each test module.
- Every new `pub`/`pub(crate)` item the migration introduces (`DiskName`,
  `LuksFormatExtraOpts`, `LuksUuidMap`, new `PoolMembership` helpers,
  expanded `MembershipError` variants, `MemberOwnedClose`, etc.) needs a
  `///` doc comment per `AGENTS.md`'s "Doc Comments" rule. Doc-comment
  compliance is part of the cutover, not a follow-up.
- `scripts/braid-destroy.sh`: jq must treat `.disks` keys as UUIDs and display
  names from `.value.name`. The script MUST sniff the schema at start
  and fail closed if `.disks` keys do not parse as UUIDs -- otherwise
  the script silently no-ops against an old-shape `pool.json` (e.g.,
  the cutover backup), reporting "destroy complete" while the disks
  remain intact, which is a confidentiality hazard if the disks are
  then sold or returned. Required sniff (one jq expression evaluated
  before any destructive iteration):
  ```sh
  if ! jq -e '.disks | keys | all(test("^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"))' "$POOL_JSON" >/dev/null; then
      echo "error: $POOL_JSON is not in UUID-keyed format (expected canonical UUID keys under .disks); refusing to destroy" >&2
      exit 1
  fi
  ```
  Anchored hyphenated UUID match so a top-level field accidentally
  named "disks" with name-keyed entries fails the sniff. The script
  also asserts non-null `.value.name` and non-null `.value.by_id` for
  every entry it iterates so a partially-shaped input does not pass
  literal `null` to `cryptsetup` or `wipefs`.
- `modules/braid/*`: audit only; no functional change expected.

## Documentation

Update docs in the same implementation commit as the code.

- `docs/principles.md`:
  - Principle 5 ("Stable identifiers") -- verbatim before/after; the
    implementer pastes the new text without composing wording.
    `docs/principles.md` uses em-dashes (`—`) throughout, so the
    Before string below is the literal current file content (em-dash
    intact for `old_string` to match). The After string keeps em-dashes
    where the surrounding doc already does, per the project's "match
    surrounding file" exception to the ASCII-default style rule:

    **Before:**

    > All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are `braid-<disk-name>` (e.g., `braid-toshiba`) — deterministic, human-friendly, debuggable in `lsblk`, systemd logs, and error messages.

    **After:**

    > All persistent storage config uses `/dev/disk/by-id/` paths. Never `/dev/sdX`. Mapper names are `braid-<disk-name>` (e.g., `braid-toshiba`) — deterministic, human-friendly, debuggable in `lsblk`, systemd logs, and error messages. **`LuksUuid` is the primary persistent identity for code; the disk name and the LUKS label are presentation; `by_id` is for hardware addressing. When the live LUKS UUID is unobservable for a device the kernel/btrfs still reports (`null_underlying` mapper, btrfs `missing_devids`), btrfs `devid` is the only authorized live-fallback identity. No code path may decide membership, target a device, or correlate live pool state by parsing a name out of a mapper path or LUKS label, except in two narrow cases: `discover` bootstrapping a UUID-keyed membership from cold disks, and returning-disk adoption safety in `add` (the `PresentLuks` path may gate adoption on label match, but identity correlation still uses `LuksUuid`/`devid`/FSID).** [Why ->](decisions/024-luks-uuid-identity.md)

  - Principle 2 ("CLI-owned membership") -- verbatim before/after for the
    pool.json best-effort clause. Same em-dash note applies; the Before
    quotes the current file's em-dash so `old_string` matches:

    **Before:**

    > `pool.json` is a best-effort operational snapshot — it tells braid which drives to attempt unlocking, not what the pool actually looks like. Any state that can be read from live btrfs (devids, device counts, FSID) must come from btrfs, not pool.json. Commands like `status` must never surface pool.json-sourced devids; devids are authoritative only when read from a mounted filesystem via `btrfs device usage` or equivalent.

    **After:**

    > `pool.json` is a best-effort operational snapshot — it tells braid which drives to attempt unlocking, not what the pool actually looks like. Any state that can be read from live btrfs (devids, device counts, FSID) must come from btrfs, not pool.json. Commands like `status` must never surface pool.json-sourced devids; **for display authority**, devids are authoritative only when read from a mounted filesystem via `btrfs device usage` or equivalent. **Persisted `DiskMember.devid` carries prior-binding authority only**: when live btrfs reports a device by `devid` alone (the `null_underlying` mapper case and the btrfs `missing_devids` case), the persisted `devid` is the authorized fallback identity for re-binding that live device to its membership entry. This is not a display-side use of pool.json devid; status output continues to draw devids from live btrfs. [Why ->](decisions/024-luks-uuid-identity.md)
- `docs/decisions/017-runtime-disk-membership.md`:
  - update `pool.json` example to UUID-keyed shape;
  - keep status `Active`;
  - add a see-also link to the new decision.
- New `docs/decisions/024-luks-uuid-identity.md`:
  - status `Active -- Refines 017-runtime-disk-membership.md`;
  - context: name-keyed membership and label/mapper drift risk;
  - decision: UUID identity, name/label presentation, by-id addressing, devid
    restricted fallback;
  - **Runtime Handles And Labels** subsection: a 7-bullet enumeration of
    the non-negotiables that future changes must respect:
    1. Mapper names remain `braid-<DiskName>`.
    2. LUKS labels remain `braid-<DiskName>`.
    3. Both mapper names and labels are presentation/runtime handles, not
       identity.
    4. `LuksUuid` is the only persistent identity for membership
       decisions.
    5. Code MAY construct `mapper_name(&member.name)` when opening or
       addressing braid's expected mapper.
    6. Code MUST NOT parse mapper names or LUKS labels to decide
       membership, target a member, or correlate live pool state. Two
       narrow exceptions: (a) `discover` bootstrapping from cold disks;
       (b) returning-disk adoption safety in `add` (`PresentLuks` path)
       may gate adoption on label match, but identity correlation still
       uses `LuksUuid`/`devid`/FSID.
    7. `lock` is the special cleanup case: classify live mappers by
       UUID/devid first, then close the **observed** mapper name (not a
       reconstructed `mapper_name(&member.name)`), so drifted-but-member-
       owned mappers are closed correctly.
  - consequences: pool/journal schema rewrite, pre-generated LUKS UUIDs,
    managed `luksFormat` options rejected from user extras;
  - alternatives rejected (kept inside the decision doc itself, not
    duplicated here).
- `docs/index.md`: register decision 024.
- `docs/luks-unlock.md`: add a new section titled "Unparseable
  state-file reconciliation" that both `MembershipError::Corrupt` and
  `JournalError::Parse` remediation phrases point to. Section content
  (required pieces):
  - The verbatim `MembershipError::Corrupt` remediation phrase:
    `run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)`.
  - The membership subsection says old-shape or corrupt `pool.json` should be
    moved aside only after confirming existing disks are the intended pool, then
    rebuilt with `braid discover --write`.
  - The verbatim error phrase:
    `Remove /var/lib/braid/pending-op.json after manual reconciliation (see docs/luks-unlock.md) and re-run.`
  - **When it is safe to remove `pending-op.json`**: (a) the operation
    has not yet committed any disk-level mutation (no LUKS format
    applied, no btrfs device add, no cryptsetup open of a fresh-format
    target); OR (b) the user has confirmed via `braid status` that the
    live pool reflects the intended state and the journal entry is
    stale.
  - **When it is NOT safe**: any case where a partially-completed
    mutation is in flight -- e.g. `mkfs.btrfs` run but `btrfs device
    add` did not, or a `replace` paused mid-rebuild. Follow the
    recovery-scenarios guide instead.
  - Pointer to `manual/guides/recovery-scenarios.md`.
- `docs/tool-behavior/device-disappearance.md`: explain UUID identity and devid
  fallback for null-underlying and missing devices.
- Sweep `docs/decisions/{007,009,011,012,022}.md` for stale name-as-identity
  wording. Decision 022 (dry-run preview model) is the most likely to
  need a real edit because `LockPlan` gains structured close-set fields;
  the others get a grep sweep only and most likely require no change.
- Update `README.md` and every manual page that describes `pool.json`,
  `pending-op.json`, `discover`, `lock`, `unlock`, `remove-missing`, `replace`,
  or recovery. The enumerated set at planning time (output of
  `grep -l 'pool.json\|pending-op.json\|luks_uuid' manual/`) is:
  - `manual/commands/add.md`
  - `manual/commands/discover.md`
  - `manual/commands/doctor.md`
  - `manual/commands/lock.md`
  - `manual/commands/recover.md`
  - `manual/commands/remove.md`
  - `manual/commands/remove-missing.md`
  - `manual/commands/replace.md`
  - `manual/commands/unlock.md`
  - `manual/guides/getting-started.md`
  - `manual/guides/recovery-scenarios.md`
  - `manual/guides/troubleshooting.md`

  Edits required per file:
  - Any inline `pool.json` example MUST be rewritten to the UUID-keyed
    shape pinned in `New Data Model > Membership Shape`.
  - Any reference to `disks: BTreeMap<String, ...>` or
    "keyed by disk name" MUST be replaced with "keyed by LUKS UUID";
    keep "the disk name is stored as a value field" for clarity.
  - `manual/guides/recovery-scenarios.md` additionally needs these
    new scenario sections (see below).
  - The other files keep their narrative structure; only on-disk
    schema examples and identity-language wording change.

  `manual/guides/recovery-scenarios.md` updates (pinned):
  - Add a section "Pending-op file corruption" that includes the
    verbatim `JournalError::Parse` remediation phrase and the
    "when it is safe / not safe to remove `pending-op.json`" decision
    table specified in the `docs/luks-unlock.md` updates.
  - Add a section "Out-of-band reformat during recovery" that
    covers the Add FreshLuks / RecoverableBraidLabeled and Replace
    FreshLuks / ExistingLuks UUID-mismatch refusal paths (the
    operator-facing remediation is "investigate the foreign
    reformat and rerun recovery"). Quote the pinned error wording
    (`add recovery aborted: target ... LUKS UUID mismatch` and
    `recover replace target '...' LUKS UUID mismatch: expected ...,
    found ...`) so the operator searching for the message in the
    guide lands here.
  - Add a section "Never-enriched member with null-underlying
    mapper" that explains the Remove-recovery carve-out (the
    operator-facing remediation is "let `braid recover` complete;
    the next read-side command observes the devid and `braid
    remove-missing` is available again").
  - Add a section "Duplicate or missing devid in journal snapshot"
    that explains the `RecoverError::DuplicateDevidDuringReplay` and
    `RecoverError::NoMemberForJournaledDevid` paths (the
    operator-facing remediation is "do not edit `pool.json`; the
    resolution did not consult it -- re-run recovery after manual
    reconciliation of `pending-op.json`").

  A grep contract for the reviewer audit: after the migration,
  `grep -l 'BTreeMap<String' manual/` returns no results (no
  manual page describes the on-disk shape as name-keyed), and every
  pool.json example block round-trips through the
  `load_membership` test path against the new schema.
- Do not edit generated `manual/book/*` directly unless the project workflow
  requires committing generated manual output.

## Execution Checklist

Land as one coherent change. The map key change ripples through most command
code, and partial states will not be meaningful.

Start from committed master HEAD. There is no in-flight staged work to
preserve (see "In-Flight Work").

- [ ] Add and lock down value types plus `LuksUuidMap`.
- [ ] Write `test_uuid(seed)` and `disk_member(seed, name, by_id)` test
  helpers in `cli/src/test_fixtures/` before any production rekey. The
  recover.rs fixture rekey then stays purely mechanical, bounding its
  blast radius.
- [ ] Rewrite `PoolMembership` and membership helpers.
- [ ] Update journal schema and parse remediation.
- [ ] Update `CmdRequest::CryptsetupLuksFormat`, `luks_format`, and
  `LuksFormatExtraOpts`.
- [ ] Update parser/discover to read UUID from `cryptsetup luksDump` text.
- [ ] Migrate add, remove, remove-missing, and replace.
- [ ] Migrate recover.
- [ ] Migrate lock close-set design.
- [ ] Migrate mount, unlock, status, TUI, doctor, enroll, preflight, main, and
  browse.
- [ ] Update scripts and docs.
- [ ] Migrate fixtures and snapshots.
  - **Fixture rekey site inventory (pinned).** Every file below
    holds `disks.insert(...)` test fixtures keyed on disk name and
    MUST be rekeyed to UUID-keyed insertion through the new
    `disk_member`/`disk_member_with` helpers from
    `cli/src/test_fixtures/`. The grep at planning time (output of
    `grep -rln 'disks\.insert' cli/src/`) is:
    - `cli/src/add.rs`
    - `cli/src/journal.rs`
    - `cli/src/membership.rs`
    - `cli/src/preflight.rs`
    - `cli/src/recover.rs`
    - `cli/src/remove_missing.rs`
    - `cli/src/remove.rs`
    - `cli/src/replace.rs`
    - `cli/src/status.rs`
    - `cli/src/test_fixtures/add.rs`
    - `cli/src/test_fixtures/lock.rs`
    - `cli/src/test_fixtures/mount.rs`
    - `cli/src/test_fixtures/remove_missing.rs`
    - `cli/src/test_fixtures/remove.rs`
    - `cli/src/test_fixtures/replace.rs`
    - `cli/src/test_fixtures/shared.rs`
    - `cli/src/test_fixtures/status.rs`
    - `cli/src/tui/probe.rs`

    Production sites (the non-`test_fixtures` files above) move as
    part of their per-command migration; the `test_fixtures/`
    files move as a single batch after the production rekey is
    complete. A grep contract for the reviewer audit: after the
    cutover, `grep -rn 'disks\.insert(' cli/src/` returns zero
    hits (no remaining name-keyed insertion path; every fixture
    constructs through `disk_member`/`disk_member_with` plus the
    new `LuksUuidMap::insert` / `PoolMembership::insert` helpers).
- [ ] Run the verification matrix.

The tree will not compile through the early type/model migration because the
map-key type is mid-change; treat this as expected and use the compiler as the
migration checklist. Once the command surfaces through browse are migrated, run
`cargo check --workspace` continuously through the scripts, docs, fixture, and
snapshot work. The verification matrix is the first point where all tests are
expected green.

## Single-User Cutover

No compatibility migration is implemented. A new binary refuses old name-keyed
`pool.json` with the exact `MembershipError::Corrupt` remediation specified in
`Membership API`; the operator cutover is to move old state aside and rebuild
UUID-keyed membership with `discover --write`.

Preconditions before installing/running the new binary:

- No `/var/lib/braid/pending-op.json`.
- The pool is healthy on the old binary.
- All intended member disks are attached and readable.
- Every intended member is LUKS2 with `Label: braid-<name>`.
- No unrelated `braid-*`-labeled disks are attached.

Cutover:

1. Record the expected member count from the existing
   `/var/lib/braid/pool.json`: `EXPECTED=$(jq '.disks | length' /var/lib/braid/pool.json)`.
2. Back up `/var/lib/braid/pool.json`.
3. Install the new braid binary.
4. Move old `/var/lib/braid/pool.json` aside.
5. Run `sudo braid discover --write --expect-count="$EXPECTED"` (see
   `discover --write --expect-count` below). This fails closed when
   discovery produces fewer members than the moved-aside `pool.json`
   listed -- catches the partial-attach hazard where a momentarily
   detached disk (loose cable, USB power glitch, udev race) silently
   produces a smaller membership. The two pre-save gates pinned
   under `discover.rs > discover --write pre-save fail-closed gates`
   additionally enforce (a) absence of `pending-op.json` and (b)
   absence of a name-keyed `pool.json` at the target path; if the
   operator forgets step 4, the schema sniff aborts before any write,
   so the backup from step 2 is not needed for recovery.
6. Inspect generated UUID-keyed `pool.json`; it must contain only the intended
   members, with the expected names.
7. Run `sudo braid unlock`, `sudo braid status`, and `sudo braid doctor`.

Rollback is only supported before the first new-code `unlock`: restore the old
binary and old `pool.json`. After the first new-code `unlock`, fix forward.

### `discover --write --expect-count`

Add an optional `--expect-count <N>` flag to `braid discover --write`.
When set, after the existing discovery + alias-dedup + duplicate-UUID
checks complete and BEFORE writing the new `pool.json`, the command
MUST compare the number of UUID-keyed members it is about to write
against `<N>`. If the produced count is less than `<N>`, the command
MUST abort with a structured error naming the expected and actual
counts and MUST NOT write `pool.json`. A produced count equal to `<N>`
proceeds normally; a produced count greater than `<N>` also proceeds
(this is the "operator attached a new disk after recording the
expectation" case and is not a safety hazard -- the inspect step is
the operator's gate for unexpected extras). The flag is optional on
`discover --write` so existing recovery flows that lack a prior count
remain unchanged; the cutover runbook is the primary caller.

Error wording (pinned for test): `discover refusing to write pool.json: expected at least {expected} members, found {actual} -- check that all intended pool members are attached and readable, then retry`.

A unit test pins the refusal: build a discover scenario that produces
two valid members and call the command path with
`--expect-count=3`; assert the command returns the structured error,
the error display contains the literal substring above, and no
`save_membership` write was issued.

## Open Decisions

Use this section only for high-impact unresolved choices that materially change
implementation. When resolved, fold the decision into the relevant plan section
and keep the closed item here for auditability.

No open decisions are currently known.

Template for future entries:

```md
- [ ] DEC-001: <question>
  - Default: <recommended answer>
  - Impact: <what changes if chosen differently>
  - Resolve by: <source read, user decision, or prototype>
  - Status: Open
```

## Test Plan

### Rust Unit Tests

Membership and value types:

- `LuksUuid::parse` canonicalizes uppercase, simple, and URN forms to lowercase
  hyphenated text.
- Canonicalization integration tests (pin where canonicalization actually
  matters, not just at the parser):
  - An uppercase-hyphenated UUID key in `pool.json` deserializes equal
    to the lowercase form. An `insert` of the lowercase form into a
    membership that already contains the uppercase form is rejected as
    a duplicate.
  - A simple-form UUID in a journal value field (`OpKind::Remove.luks_uuid`,
    `OpKind::Replace.old_uuid`, `OpKind::Replace.new_uuid`) deserializes
    equal to the hyphenated form and equates on lookup against
    `PoolDevice.luks_uuid` parsed from cryptsetup output.
  - Round-trip: an uppercase or simple-form UUID loaded from `pool.json`
    re-serializes in canonical lowercase hyphenated form on next save.
- Invalid UUIDs fail at deserialize for pool keys and journal fields.
- `LuksUuidMap` rejects duplicate canonical keys for pool and Add journal maps,
  and the deserialize error contains
  `duplicate LUKS UUID key after canonicalization: <canonical-uuid>`.
- **`LuksUuidMap::insert` fail-closed regression**: starting from an
  empty `LuksUuidMap<AddJournalTarget>`, insert a target under
  `U_A`; then attempt a second `insert(U_A, ...)`. Assert the second
  call returns `Err(LuksUuidMapConflict { uuid: U_A })` and the
  original value is unchanged in the map. Pins that the in-process
  insert path agrees with the Deserialize duplicate-key contract.
- **Add cloned-disk duplicate-UUID refusal (planning path)**: build
  Add planning input where two `PresentLuks` adoption targets point
  at distinct by-ids and distinct disk names but the same probed
  LUKS UUID (the dd-cloned-disk case). Assert planning fails with
  `AddError::DuplicateUuid { uuid, name1, by_id1, name2, by_id2 }`
  whose `Display` contains the literal substring
  `duplicate LUKS UUID across add targets: braid-{name1} ({by_id1}) and braid-{name2} ({by_id2}) share UUID {uuid}`
  with `(name1, by_id1)` and `(name2, by_id2)` lexicographically
  ordered by `by_id`. Assert the error fires before any journal write
  and before any `LuksUuidMap::insert`/`PoolMembership::insert`, and
  that the recording `CommandRunner` observes no
  `CryptsetupLuksFormat` and no `BtrfsDeviceAdd` for either target.
  Tests the discover-symmetric `AddError::DuplicateUuid` shape; a
  regression that falls through to the generic
  `MembershipError::Conflict` or `LuksUuidMapConflict` would not name
  both by-id paths and must fail this test.
- `LuksUuidMap` serializes as a flat JSON object, not `{"0": ...}`.
- `LuksUuidMap` shape regression: serialize a `LuksUuidMap<V>` with one
  entry and assert the output JSON is `{"<canonical-uuid>": {<value>}}`,
  not `{"0": {<value>}}`. Re-deserialize the produced JSON and assert
  equality with the original. Catches accidental removal of
  `#[serde(transparent)]` on the wrapper, which would silently break
  every `pool.json` and `pending-op.json` fixture.
- `DiskName` deserialize rejects invalid names.
- `ByIdPath` deserialize rejects non-by-id paths.
- `PoolMembership::insert` rejects duplicate UUID, name, by-id, and non-`None`
  devid with `MembershipError::Conflict` messages following the exact
  field/value/colliding-UUID patterns specified in `Membership API`.
- `PoolMembership::insert` errors (does not silently overwrite) when called
  with a UUID already in the map; replacement requires explicit
  `remove_by_uuid` first.
- `by_devid` returns `Err(DuplicateDevid)` on corrupt duplicate devids.
- Old-format `pool.json` (name-keyed top-level entries) fails
  `load_membership` with `MembershipError::Corrupt`. Assert the displayed
  error contains the exact remediation suffix
  `-- run 'braid discover --write' to rebuild from existing disks (with all intended pool members attached; see docs/luks-unlock.md)`.
  The failure must come from the UUID-key parse step on the outer map.
- Stale value-side `luks_uuid` in an otherwise-valid `pool.json` fails
  `load_membership` with `MembershipError::Corrupt`, and the error
  message names the unknown field. Concretely: build a JSON document
  whose key is a valid canonical UUID and whose value is
  `{ "name": "toshiba1", "by_id": "...", "luks_uuid": "<some-uuid>",
  "devid": 1, "added_at": "..." }`, and assert the error is the
  `deny_unknown_fields` failure on `DiskMember`, not the outer-key
  failure. This pins value-side strictness independently of the
  outer-key failure path.
- Outer-container strictness: `pool.json` with a valid `disks` field
  alongside an unknown top-level key (e.g.
  `{ "disks": {...}, "schema_version": 1 }`) fails `load_membership`
  with `MembershipError::Corrupt` from the `deny_unknown_fields`
  attribute on `PoolMembership`. Symmetric test on `pending-op.json`
  pins the same attribute on `Journal`.
- Hybrid `pool.json` rejection: a `pool.json` with two entries, one
  UUID-keyed and one disk-name-keyed, fails `load_membership` (does
  not silently load the UUID-keyed half). Pins that `LuksUuidMap`'s
  deserialize fails on the first non-UUID key rather than tolerating
  it; guards against a future "tolerate non-UUID keys for ergonomics"
  drift that would partially load a corrupt file and re-serialize it
  as authoritative.
- `load_membership` on a `pool.json` whose two UUID entries carry the same
  non-`None` `devid` returns `MembershipError::Conflict` naming the duplicate
  devid and both colliding UUIDs. This pins load-time enforcement separately
  from the `by_devid` lookup guard.
- **Load-time duplicate `name` rejection**: `load_membership` on a
  `pool.json` whose two distinct UUID-keyed entries carry the same
  `name` value returns `MembershipError::Conflict` naming the
  duplicated `name`, the two colliding UUIDs, and following the same
  field/value/colliding-UUID pattern as the duplicate-devid case.
  A hand-edited or corrupted `pool.json` must not load with two
  members sharing a name; this pins load-time enforcement separately
  from the `PoolMembership::insert` guard so a regression that only
  enforces uniqueness on the insert path (and not on load) fails this
  test.
- **Load-time duplicate `by_id` rejection**: `load_membership` on a
  `pool.json` whose two distinct UUID-keyed entries carry the same
  `by_id` value returns `MembershipError::Conflict` naming the
  duplicated `by_id`, the two colliding UUIDs, and following the
  same field/value/colliding-UUID pattern as the duplicate-devid
  case. Mirrors the duplicate-`name` load test on the by-id axis;
  together with the duplicate-devid load test, these three pin
  load-time enforcement of every non-key uniqueness invariant.
- `MembershipError` variants surface via `Display` with messages that name the
  offending field and value (UUID, name, by-id, devid). Pin the `Conflict`,
  `Corrupt`, and `DuplicateDevid` wording so recovery docs can quote it.
- `MembershipError::DuplicateDevid` Display includes every colliding
  UUID (not only two) when three or more members share a devid, and
  the UUIDs appear in canonical lexicographic order.
- Multi-disk `pool.json` (>=3 members) round-trips through serialize +
  `atomic_write` + `load_membership` with stable, UUID-sorted key order;
  iteration order is deterministic regardless of insertion order.

LUKS format boundary:

- `LuksFormatExtraOpts::parse` rejection coverage (each must surface
  `LuksFormatExtraOptsError::ManagedFormatFlag` naming the offending token;
  empty input succeeds; mixed valid/invalid input fails and names the
  invalid one):
  - `--uuid=<x>`
  - `--uuid` as a bare token (defensive even though `require_equals = true`
    on `--luks-format-arg` makes this unreachable from the CLI)
  - `--label=foo`
  - `--label foo`
  Per the pinned-cryptsetup audit in `New Data Model > LuksFormatExtraOpts`
  (cryptsetup 2.8.4, `OPT_UUID` and `OPT_LABEL` both have short name
  `'\0'` in `cryptsetup_arg_list.h`), the reject list is long-form only
  and the test matrix above is exhaustive for the pinned version. No
  speculative `-U`/`-L` case is required; add one only if a future
  fixture-refresh event introduces a short alias upstream.
  Replace shares the same validation; cover one symmetric case for
  `replace` to prove the wiring.
- Add and replace reject managed `--luks-format-arg` values before any journal
  write and before any `CryptsetupLuksFormat` request.
- Fresh add and fresh replace recording-runner tests assert
  `CryptsetupLuksFormat` carries structured `uuid`, `label`, and
  validated extras.
- **Positive-extras forwarding regression**: one recording-runner test
  for `add` and one for `replace`, each invoked with a single valid
  `--luks-format-arg=--use-random` (a non-managed, non-empty extra).
  Assert the captured `CmdRequest::CryptsetupLuksFormat` carries that
  exact token inside its `LuksFormatExtraOpts` (`extras`) field, in
  argv order, and that no other token leaks into the structured
  `uuid`/`label` fields. Pins the user-facing contract that valid
  user-supplied extras reach the cryptsetup invocation; a regression
  that silently drops accepted extras (passes parse, drops at execute)
  passes the rejection suite and the empty-extras suite yet fails this
  test.
- Synthetic mid-format recovery uses the same pre-generated UUID from the
  journal.

Journal:

- New Add/Remove/RemoveMissing/Replace shapes round-trip.
- A `Journal` carrying non-empty `pre_membership` and `target_membership`
  snapshots round-trips through serialize + `load_journal`, including
  canonicalization of UUID keys inside both snapshots. Pins the transitive
  rekey path that a top-level journal-only test would miss.
- Each new `OpKind` shape (Add with multi-target `LuksUuidMap`,
  Remove, RemoveMissing, Replace -- both `FreshLuks` and `ExistingLuks`
  mode arms) round-trips through serialize + `atomic_write` +
  `load_journal` against a real tempfile path, not just an in-memory
  string. Assert deterministic, UUID-sorted key order inside
  `OpKind::Add.targets` and inside both `pre_membership` and
  `target_membership` snapshots regardless of insertion order.
  Mirrors the pool.json `atomic_write` + `load_membership` bullet so
  a regression in journal serialization (torn writes, non-deterministic
  key order, partial flush) is caught by the same shape of test the
  pool.json side uses, not only by in-memory JSON equality.
- Old name-keyed journal shape returns `JournalError::Parse` with the exact
  remediation phrase (verbatim string match, including trailing period).
- Add and Replace JSON containing removed value-side UUID, `mapper_name`, or
  stored label fields fails under `deny_unknown_fields` where that attribute is
  used.
- **Variant-level `deny_unknown_fields` symmetry pin for Remove and
  RemoveMissing**: the migration does not remove any field from
  `OpKind::Remove` or `OpKind::RemoveMissing` (Remove gains
  `luks_uuid` and keeps `name`; RemoveMissing's shape is unchanged),
  so there are no removed-field cases to mirror. The risk the
  Add/Replace tests pin -- that the container-level
  `#[serde(deny_unknown_fields)]` on `OpKind` is actually effective
  on each variant -- still applies to Remove and RemoveMissing. Add
  one rejection test per variant that hand-builds a
  `pending-op.json` whose `OpKind::Remove` (resp. `OpKind::RemoveMissing`)
  carries an unknown extra field alongside the legitimate keys and
  asserts `load_journal` returns `JournalError::Parse` naming the
  unknown field. Pins that the container attribute reaches these
  variants too; a regression that drops the attribute (or moves it
  to per-variant struct attributes and forgets Remove/RemoveMissing)
  passes the Add/Replace tests yet fails this one.
- Invalid UUIDs in Add map key, Remove UUID, Replace old UUID, and Replace new
  UUID all fail at `load_journal`.
- Phase enums (`AddPhase`, `ReplacePhase`, `RemoveMissingPhase`) and
  `ReplaceJournalSource` round-trip every variant after the schema rewrite,
  proving the migration did not regress replay state shape.

Parser and discover:

- The new `parse_cryptsetup_luks_uuid_from_dump` parser extracts a
  canonical `LuksUuid` from the `UUID:` line of a `luksDump` text body,
  using the existing stable fixture.
- **`parse_cryptsetup_luks_uuid` canonicalization regression**: feed a
  synthetic `cryptsetup luksUUID` stdout containing an uppercase
  hyphenated UUID (e.g. `8C78A966-EF17-4610-B835-5B376EF10B4E\n`)
  and assert the produced `LuksUuid` equates (via `==`) with the
  lowercase form loaded from a `pool.json` UUID-keyed entry under
  the canonical lowercase form. The test must drive
  `parse_cryptsetup_luks_uuid` directly (not the dump parser), since
  this parser is the one consumed by `probe_pool`,
  `classify_mapper_ownership`, and every path that populates
  `PoolDevice.luks_uuid`. Symmetric URN-form
  (`urn:uuid:8c78a966-...`) and simple-form (32 hex digits, no
  hyphens) coverage is provided by the `LuksUuid::parse` unit tests
  above; this test specifically pins the parser-to-membership flow.
- Missing `UUID:` and invalid `UUID:` fail with field-specific parse errors.
- Discover maps a missing `UUID:` line to
  `DiscoverWarning::MissingLuksUuid { path }`, skips that disk, and the
  warning display is exactly
  `skipping {path}: luksDump output missing UUID`.
- Discover maps an invalid `UUID:` value to
  `DiscoverWarning::InvalidLuksUuid { path, raw, detail }`, skips that disk,
  and the warning display starts with
  `skipping {path}: invalid LUKS UUID "{raw}" --`.
- Discovery runs version, label, and UUID parsers over a single shared
  `CryptsetupLuksDumpText` raw output (one `luksDump` invocation per
  by-id).
- Discover alias dedup with two by-id aliases for one physical disk yields one
  UUID-keyed member using the preferred alias.
- Discover label collision on two physical disks with the same `braid-<name>`
  still returns the friendly `LabelCollision` error before any generic
  membership conflict.
- **Discover cloned-disk DuplicateUuid friendly error**: two physical
  disks with distinct labels (`braid-disk1`, `braid-disk2`) but the
  same LUKS UUID (cloned/dd-imaged drive) surface as
  `DiscoverError::DuplicateUuid { uuid, name1, path1, name2, path2 }`,
  not as a generic `MembershipError`. Both by-id paths and both disk
  names appear in the error. Pin the message wording so operator
  remediation does not rot.

Command behavior:

- `enrich_from_pool_state` updates members by live UUID even when the mapper
  name is wrong.
- Null-underlying display correlation uses persisted devid.
- `remove` resolves name to UUID at the boundary and removes by UUID.
- **Drifted-member `remove` closes observed mapper**: plan a remove
  where the matching `PoolDevice.mapper = "braid-WRONG"` (drifted)
  and the matching `PoolDevice.luks_uuid == target_uuid`. Membership
  records the same UUID under `name = "right"`. Assert:
  (a) `RemoveWorkPlan::target_mapper == "braid-WRONG"` (cloned from
  the observed `PoolDevice.mapper`, not reconstructed from
  `mapper_name(&right)`); (b) the post-commit `CryptsetupClose` at
  `remove.rs:180-183` in the recording `CommandRunner` issues
  `CryptsetupClose { mapper: "braid-WRONG" }`, not
  `CryptsetupClose { mapper: "braid-right" }`. Mirrors the Replace
  observed-mapper-journaling regression on the Remove path.
- **Remove post-commit close UUID-probe defense-in-depth (double drift)**:
  plan a remove whose work plan records `target_uuid = U_OLD` and
  `target_mapper = "braid-WRONG"`. Between `btrfs device remove` and
  the post-commit close, simulate operator double-drift: the mapper
  `braid-WRONG` now holds a foreign disk whose `cryptsetup luksUUID`
  reports `U_FOREIGN != U_OLD`. Wire the recording `CommandRunner` so
  a `CryptsetupLuksUuid { mapper: "braid-WRONG" }` probe returns
  `U_FOREIGN`. Assert: (a) execution issues exactly one
  `CryptsetupLuksUuid` probe against `braid-WRONG` before any close;
  (b) zero `CryptsetupClose` requests hit `braid-WRONG` because the
  probe mismatch demoted the close to a logged-warning skip; (c) the
  warning text names `braid-WRONG`, `U_OLD`, and `U_FOREIGN`. A
  control arm with `cryptsetup luksUUID` returning `U_OLD` MUST still
  issue the close (the probe is fail-safe-skip on mismatch only, not
  fail-skip on any condition). Mirrors the Replace post-commit close
  UUID-probe test at `replace.rs:707` on the Remove path. No
  recovery-side mirror because `OpKind::Remove` is intentionally
  skipped by `replay_post_mutation`.
- `remove-missing` removes the UUID whose persisted devid matches the btrfs
  missing devid and makes zero `cryptsetup luksUUID` requests for the missing
  target.
- **Missing-`remove-missing` decoy regression**: membership has two
  UUID-keyed entries:
  - `U_R -> { name: "misleading-label", by_id: "/dev/disk/by-id/right", devid: Some(2) }`
  - `U_D -> { name: "decoy", by_id: "/dev/disk/by-id/decoy", devid: Some(99) }`
  `remove-missing` takes a btrfs devid, not a disk name, so the test must not
  mention an operator-typed name. `pool.missing_devids = [2]`. After
  `cmd_remove_missing(2)`, assert: (a) the entry under `U_R` is gone, (b) the
  entry under `U_D` is unchanged, (c) the recording `CommandRunner` observed
  zero `CryptsetupLuksUuid` requests for the missing target. The different
  persisted names and by-id paths are decoys; only the persisted devid selects
  `U_R`.
- **Forward `remove-missing` never-enriched refusal**: membership has
  two UUID-keyed entries with `devid: None` (enrichment never ran for
  any member). `pool.missing_devids = [2]`. Run `cmd_remove_missing(2)`.
  Assert: (a) the call returns a structured error
  (`RemoveMissingError::NoMemberForDevid { devid: 2 }` or the migration's
  equivalent named variant) whose `Display` contains the literal substring
  `no member in membership has devid 2`; (b) membership is byte-for-byte
  unchanged on disk after the call returns; (c) the recording
  `CommandRunner` observed zero mutating requests of any shape
  (`BtrfsDeviceRemove`, `CryptsetupClose`, `BtrfsDeviceScanForget`,
  `save_membership`). Mirrors the recovery-replay `RemoveMissing replay
  no-member-for-devid refusal` test on the forward path; a regression
  that falls through to remove an arbitrary entry (or panics on the
  never-enriched case) passes the existing positive-path test and the
  decoy regression yet fails this one.
- Missing-path `replace` cross-checks `--old` name, old UUID, and missing devid;
  it refuses mismatches and missing persisted devids.
- **Missing-path `replace` decoy regression**: missing-path replace
  receives `--old <name>` from the operator. The operator types the actual
  persisted name for the target member: `--old misleading-label`.
  Membership has two UUID-keyed entries:
  - `U_R -> { name: "misleading-label", by_id: "/dev/disk/by-id/right", devid: Some(2) }`
  - `U_D -> { name: "decoy", by_id: "/dev/disk/by-id/misleading-label", devid: Some(99) }`
  `pool.missing_devids = [2]`. Run
  `cmd_replace --old misleading-label --new replacement=/dev/disk/by-id/new`.
  Assert:
  (a) name-to-UUID resolution selects `U_R`, (b) the persisted-devid
  cross-check confirms `U_R.devid == 2` and `2 in pool.missing_devids`,
  (c) the entry under `U_R` is gone after commit, (d) a new entry
  exists under a fresh `LuksUuid` (`new_uuid != U_R`, `new_uuid != U_D`),
  (e) the journal records `OpKind::Replace { old_uuid: U_R, new_uuid:
  <fresh>, ... }` with `old_uuid` matching the old member's key, (f)
  the entry under `U_D` is unchanged, (g) the recording
  `CommandRunner` observed zero `CryptsetupLuksUuid` requests for the
  missing target. `U_D`'s by-id basename intentionally matches the typed
  old name, so a buggy by-id-keyed lookup would choose `U_D`; the UUID-keyed
  model must choose `U_R` via `name -> UUID` and then confirm the persisted
  devid.
- Live-path `replace` targets the old source by UUID, not mapper name.
- **Replace observed-mapper journaling regression**: plan a live
  replace where the matching `PoolDevice.mapper = "braid-WRONG"`
  (drifted) and the matching `PoolDevice.luks_uuid == old_uuid`.
  Membership records the same UUID under `name = "right"`. Assert:
  (a) `resolve_replace_source` returns
  `ReplaceSource::Live { mapper: "braid-WRONG", devid }`, not the
  reconstructed `mapper_name(&right)`;
  (b) the journaled `ReplaceJournalSource::Live.old_mapper == "braid-WRONG"`;
  (c) the post-commit `close_mapper_best_effort` call (`replace.rs:707`)
  in the recording `CommandRunner` issues
  `CryptsetupClose { mapper: "braid-WRONG" }`, not
  `CryptsetupClose { mapper: "braid-right" }`. Pins Pattern 4's
  observed-mapper-clone requirement at the planning site.
- **Replace post-commit close UUID-probe defense-in-depth (double drift)**:
  plan a live replace whose journal records `old_uuid = U_OLD` and
  `ReplaceJournalSource::Live.old_mapper = "braid-WRONG"`. Between
  plan and post-commit close, simulate operator double-drift: the
  mapper `braid-WRONG` now holds a foreign disk whose
  `cryptsetup luksUUID` reports `U_FOREIGN != U_OLD`. Wire the
  recording `CommandRunner` so a `CryptsetupLuksUuid { mapper:
  "braid-WRONG" }` probe returns `U_FOREIGN`. Assert:
  (a) execution issues exactly one `CryptsetupLuksUuid` probe against
  `braid-WRONG` before any close; (b) zero `CryptsetupClose` requests
  hit `braid-WRONG` because the probe mismatch demoted the close to
  a logged-warning skip; (c) the warning text names `braid-WRONG`,
  `U_OLD`, and `U_FOREIGN`. Symmetric pin at `recover.rs:2935`:
  exercise the same drift through the recovery replay path and
  assert the recovery-side close is skipped identically. A control
  arm with `cryptsetup luksUUID` returning `U_OLD` MUST still issue
  the close (the probe is fail-safe-skip on mismatch only, not
  fail-skip on any condition).
- **Replace ExistingLuks new-target open-boundary UUID re-probe**:
  plan a live replace where the prep mode is
  `ReplaceJournalMode::ExistingLuks` and op-level `new_uuid = U_NEW`
  with `new_target.by_id = /dev/disk/by-id/Y`. Between planning and
  execution, simulate operator disk swap: the disk now at
  `/dev/disk/by-id/Y` reports `cryptsetup luksUUID = U_FOREIGN != U_NEW`.
  Wire the recording `CommandRunner` so a `CryptsetupLuksUuid { device:
  "/dev/disk/by-id/Y" }` probe returns `U_FOREIGN`. Assert:
  (a) execution issues exactly one by-id-form `CryptsetupLuksUuid`
  probe against `/dev/disk/by-id/Y` before any
  `CryptsetupLuksOpen`/`ensure_luks_open` call; (b) zero
  `CryptsetupLuksOpen` requests hit the new disk; (c) zero
  `BtrfsReplaceStart` requests issue; (d) the returned error names
  `new_target.by_id`, `U_NEW`, and `U_FOREIGN`. Symmetric recovery
  pin: drive `execute_replace_pool_mutation_recovery` through the
  same disk-swap scenario and assert the recovery side aborts
  identically. A control arm with the probe returning `U_NEW` MUST
  continue to the open and to `btrfs replace start` (the gate is
  fail-safe-skip on mismatch only). Pins the open-boundary re-probe
  required by the ExistingLuks new-target spec; the post-commit
  close double-drift probe does not exercise this hazard.
- **Replace post-maintenance recovery new-disk find by UUID
  (`recover.rs:2992` Pattern 4)**: build a recovery scenario where
  `execute_replace_post_maintenance_recovery` is reached with a journal
  whose `OpKind::Replace.new_uuid = U_NEW` and `new_name = "right"`,
  and a live `pool` whose new-disk device record has
  `pool.devices[i].mapper = "braid-WRONG"` (drifted) but
  `pool.devices[i].luks_uuid == U_NEW`. Assert: (a) the function
  locates the device by UUID rather than by reconstructed
  `mapper_name(&new_name)`; (b) the recording `CommandRunner` observes
  a `BtrfsDeviceResize`/`pool_resize_device` call targeting that
  device's `devid`; (c) the function does NOT return the
  `could not find new disk '...' in the live pool` error path. A
  regression that left this site as the original
  `pool.devices.iter().find(|d| d.mapper == new_mn)` mapper-name find
  would silently fail to locate the disk on benign mapper drift and
  surface that error string -- this test pins the Pattern 4 rewrite
  at the site. The site is distinct from the post-commit close
  defense-in-depth probe at `recover.rs:2935` (which covers the
  `close_old_mapper_best_effort` path) and from the
  `finish_uncommitted_replace_recovery` UUID-mismatch test at
  `:2783` (which covers the FreshLuks adoption gate); none of those
  exercise the `:2992` find-by-mapper site.
- Recovery rebuilds membership by UUID and preserves `added_at` by UUID.
- TUI hot-unplug test shows names for both present and null-underlying devices
  without mapper-name parsing.

Recovery (added by this migration):

Add-recovery preflight fixture (shared by the two preflight tests
below): an Add journal entry under pre-generated UUID `U_J` records
target `name = "disk1"` and `target.by_id = /dev/disk/by-id/X`.
Replay derives the expected label as `braid-disk1`. The disk at
`/dev/disk/by-id/X` has been out-of-band reformatted with a different
LUKS UUID `U_F` under the same label `braid-disk1`.

- **Add FreshLuks UUID-mismatch refusal at
  `discover_add_targets_before_mount` (`recover.rs:1957-1960` arm)**:
  drive Add-recovery through `discover_add_targets_before_mount` with
  the fixture above. Assert the function returns
  `RecoverError::Failed(msg)` where `msg` contains the literal substring
  `add recovery aborted: target /dev/disk/by-id/X LUKS UUID mismatch`
  and names both `U_J` and `U_F`; that the recording `CommandRunner`
  observes zero `CryptsetupLuksOpen` for `target.by_id`; and that no
  `btrfs device add` is issued for the foreign disk. Pins the
  discover-side arm independently of the passphrase-verification arm
  below.
- **Add FreshLuks UUID-mismatch refusal at
  `verify_recover_passphrase_for_add_replay` (`recover.rs:2029` arm)**:
  drive Add-recovery through `verify_recover_passphrase_for_add_replay`
  with the same fixture above. Assert the function returns
  `RecoverError::Failed(msg)` with the same substring contract
  (`add recovery aborted: target /dev/disk/by-id/X LUKS UUID mismatch`
  plus both UUIDs); that the recording `CommandRunner` observes zero
  `CryptsetupLuksOpen`/passphrase-verification calls against
  `target.by_id`; and that no `btrfs device add` is issued. Pins the
  passphrase-verification arm independently of
  `discover_add_targets_before_mount`, mirroring the per-site treatment
  of `recover.rs:2226` and `recover.rs:2350` below. A regression that
  re-tightened only one of the two arms passes the other test and fails
  this one.
- **Add FreshLuks UUID-mismatch refusal at `recover.rs:2226` (first-pass
  open loop)**: same fixture as above, but driven through
  `execute_add_pool_mutation_recovery`'s first `if !add_targets_all_live`
  open loop. Assert the function returns `RecoverError::Failed(msg)`
  with the same `add recovery aborted: target ... LUKS UUID mismatch`
  substring and both UUIDs; that the recording `CommandRunner`
  observes zero `CryptsetupLuksOpen` for `target.by_id`, zero
  `BtrfsDeviceScan` against `/dev/mapper/<target.mapper_name>` (the
  `scan_mapper_if_btrfs_visible` path). Pins refusal before any side
  effect at this specific arm.
- **Add FreshLuks UUID-mismatch refusal at `recover.rs:2350` (final
  irreversible-adoption arm)**: same fixture, but driven through the
  final `for (name, target) in targets` loop in
  `execute_add_pool_mutation_recovery`, the
  `ConfigDiskState::PresentLuks` arm. Assert the function returns
  `RecoverError::Failed(msg)` with the same substring contract; that
  the recording `CommandRunner` observes zero `CryptsetupLuksOpen` for
  `target.by_id` and zero `BtrfsDeviceAdd` against
  `/dev/mapper/<target.mapper_name>`. This is the data-loss arm: a
  passing test here directly pins that an out-of-band reformat between
  crash and recovery cannot ride into the btrfs pool via
  `pool_add_device`.
- **Replace FreshLuks UUID-mismatch refusal at
  `finish_uncommitted_replace_recovery` (`recover.rs:2783`)**: build a
  Replace journal with op-level `new_uuid = U_J`, fresh-LUKS new
  target name `disk2`, `new_target.by_id = /dev/disk/by-id/Y`. The
  disk at `/dev/disk/by-id/Y` has been out-of-band reformatted with a
  different UUID `U_F` under the same label `braid-disk2`. Recovery
  MUST return `RecoverError::Failed` naming `new_target.by_id`, the
  journaled `U_J`, and the observed `U_F`. Assert the recording
  `CommandRunner` observes zero
  `CryptsetupLuksOpen`/`CryptsetupTryPassphrase`-style requests for
  the foreign disk, no header backup writes, no `save_membership`,
  and the journal is still present on disk after the call returns.
  Mirrors the Add `recover.rs:2350` test on the Replace side.
- **Replace ExistingLuks UUID-mismatch refusal at
  `finish_uncommitted_replace_recovery` (`recover.rs:2697`)**: build a
  Replace journal with op-level `new_uuid = U_J`, prep mode
  `ReplaceJournalMode::ExistingLuks`, and `new_target.by_id =
  /dev/disk/by-id/Y`. The disk at `/dev/disk/by-id/Y` reports
  `cryptsetup luksUUID = U_F != U_J` (operator swapped or reformatted
  the disk between Replace commit and finish-time recovery). Recovery
  MUST return `RecoverError::Failed` whose `Display` contains the
  pinned `:2697` wording (`recover replace target '{}' LUKS UUID
  mismatch: expected ..., found ...`) with `expected = U_J` sourced
  from the op-level `new_uuid` field and `found = U_F`. Assert the
  recording `CommandRunner` observes zero header backup writes,
  zero `save_membership`, zero journal-clearing writes, and the
  journal is still present on disk after the call returns. Pins that
  the comparison is sourced from `new_uuid` (op-level) after the
  mode-nested `luks_uuid` is deleted; a regression that re-sources
  from a stale or absent field would either fail to compile or admit
  the foreign disk at finish time. Mirrors the FreshLuks `:2783` test
  on the ExistingLuks finish-time branch.
- **Add RecoverableBraidLabeled UUID-mismatch refusal at
  `discover_add_targets_before_mount` (`recover.rs:1952` arm)**: same
  preflight fixture as the FreshLuks Site-1 test except the journal
  entry uses `AddJournalMode::RecoverableBraidLabeled` (the disk at
  `target.by_id` already carries a `braid-<name>`-labelled LUKS2
  header that the operator wants the recovery to re-adopt). The
  on-disk header has been out-of-band reformatted with `U_F` and
  re-labelled `braid-disk1` between crash and recovery; the
  `targets` map-key UUID remains `U_J`. The discover-side
  RecoverableBraidLabeled arm MUST observe the
  `U_J != U_F` mismatch (re-sourced from the map-key UUID, not from
  the deleted mode-nested `luks_uuid` field) and skip/error as the
  arm does today; assert the recording `CommandRunner` observes
  zero `CryptsetupLuksOpen` for `target.by_id`. Pins that the
  re-source flipped from the deleted nested field to the map-key
  UUID at this site.
- **Add RecoverableBraidLabeled UUID-mismatch refusal at
  `verify_recover_passphrase_for_add_replay` (`recover.rs:2021` arm)**:
  same fixture, but driven through
  `verify_recover_passphrase_for_add_replay`'s RecoverableBraidLabeled
  arm. Assert the recording `CommandRunner` observes zero
  `CryptsetupLuksOpen`/passphrase-verification calls against
  `target.by_id`. Pins the re-source at this site independently of
  `:1952`.
- **Add RecoverableBraidLabeled UUID-mismatch refusal at
  `recover.rs:2221` (first-pass open loop)**: same fixture, driven
  through `execute_add_pool_mutation_recovery`'s first
  `if !add_targets_all_live` open loop, RecoverableBraidLabeled arm.
  Assert the function returns `RecoverError::Failed` naming
  `target.by_id`, `U_J`, and `U_F`; that the recording `CommandRunner`
  observes zero `CryptsetupLuksOpen` for `target.by_id` and zero
  `BtrfsDeviceScan` against `/dev/mapper/<target.mapper_name>`.
- **Add RecoverableBraidLabeled UUID-mismatch refusal at
  `recover.rs:2275` (final irreversible-adoption arm)**: same
  fixture, but driven through the final `for (name, target) in
  targets` loop in `execute_add_pool_mutation_recovery`, the
  RecoverableBraidLabeled adoption arm (the data-loss twin of the
  FreshLuks `:2350` site). Assert the function returns
  `RecoverError::Failed` naming `target.by_id`, `U_J`, and `U_F`;
  that the recording `CommandRunner` observes zero
  `CryptsetupLuksOpen` for `target.by_id` and zero `BtrfsDeviceAdd`
  against `/dev/mapper/<target.mapper_name>`. This is the
  RecoverableBraidLabeled-side data-loss arm: a passing test here
  pins that a returning braid-labelled disk reformatted out-of-band
  between crash and recovery cannot ride into the btrfs pool via
  `pool_add_device` once the mode-nested `luks_uuid` is deleted.
  A regression that re-sourced from a stale or absent field would
  silently re-open the same data-loss path the FreshLuks tightening
  closes; this test catches it.
- **RemoveMissing replay no-member-for-devid refusal**: build a
  journal whose `pre_membership` snapshot contains no member with
  `devid == Some(2)` (every member has `devid: None`, modelling the
  never-enriched cause). Live `pool.json` is irrelevant to this test
  because the resolution runs against `journal.pre_membership`, not
  the live file. Journal `OpKind::RemoveMissing { phase:
  RemoveMissingPhase::PoolMutation, devid: 2, ... }`. Recovery MUST
  return `RecoverError::NoMemberForJournaledDevid { devid: 2 }` whose
  `Display` contains the literal substring `no member in journaled
  membership has devid 2`, leaves the journal in place, and emits
  zero `BtrfsDeviceRemove`/membership-mutation side effects.
  Symmetric coverage: a second test with `phase:
  PostRemoveMissingMaintenance` and the same gap in
  `target_membership` produces the identical error, pinning that the
  resolution honours the phase-keyed snapshot selection at
  `recover.rs:3381-3388`.
- **Recovery null-underlying middle-hop gap refusal**: build a journal whose
  `luks_uuid` resolves to a membership entry with `devid == None` (the
  enrichment never ran). The live `pool.null_underlying` contains a
  mapper whose backing identity is unobservable. Recovery MUST return
  `RecoverError::JournalUuidDevidGap { luks_uuid }` whose `Display`
  contains the literal substring `journaled LUKS UUID <uuid> has no
  persisted devid`, leave the journal in place, and emit zero
  `CryptsetupClose`/`BtrfsDeviceScanForget` side effects against the
  unresolved mapper. Pins the middle-hop gap as a structured refusal
  rather than silent admission.
- **Remove-recovery never-enriched null_underlying restoration**:
  `pre_membership` contains a UUID `U_N -> DiskMember { name:
  "flapper", by_id: "...", devid: None, added_at: None }` (never
  enriched -- host crashed between commit and the next read-side
  command). `pool.null_underlying` contains one entry with
  `mapper = "braid-flapper"` and a `devid`. The Remove-recovery
  guard MUST restore `U_N` into `recovered` because the expected
  mapper name matches the `null_underlying` entry, even though the
  devid-side lookup would have returned `None`. The carve-out is
  scoped to `member.devid == None` only -- a member with a
  populated devid that mismatches `null_underlying.devid` still
  fails to restore.
- **Remove-recovery carve-out queues no mutating mapper-addressed
  step**: same fixture as the previous test (carve-out fires for
  `U_N` with `devid: None` and the matching `null_underlying`
  entry). After recovery returns, assert that the recording
  `CommandRunner` observed zero mutating mapper-addressed requests
  against `/dev/mapper/braid-flapper` from the recovery plan
  itself: zero `CryptsetupClose { mapper: "braid-flapper" }`, zero
  `BtrfsDeviceScanForget` against that path, and zero
  `BtrfsDeviceRemove`/`BtrfsDeviceAdd` referencing it. Pins the
  accepted-risk contract from the carve-out spec: restoration is
  the only effect; any mutating mapper-addressed work must wait
  for the next probe to disambiguate the slot. Read-side restoration
  side effects (writing the restored membership into `recovered`)
  remain allowed.
- **Remove-recovery carve-out stamps devid and re-enables
  `remove-missing`**: same fixture as the carve-out restoration
  test, but the `pool.null_underlying` entry carries an observed
  `devid = 7`. After recovery returns, assert the restored
  membership entry under `U_N` now has `devid == Some(7)` (the
  observed devid was stamped onto the restored member). Then,
  with `pool.missing_devids = [7]` modelling the next read-side
  observation, run `cmd_remove_missing(7)` against the restored
  membership and assert: (a) the call succeeds; (b) the entry
  under `U_N` is gone from the resulting membership; (c) the
  recording `CommandRunner` observed the expected
  `BtrfsDeviceRemove` against `devid = 7`. Pins the carve-out's
  devid-stamping contract: without the stamp, `cmd_remove_missing`
  refuses (no member has `devid == Some(7)`) and the entry is
  trapped in `pool.json` forever. A regression that restores the
  entry with `devid: None` passes the restoration test and the
  no-mutating-step test yet fails this one.
- **RemoveMissing replay duplicate-devid refusal**: during
  RemoveMissing replay, build a journal whose `pre_membership`
  snapshot contains two entries with `devid == Some(2)` so that
  `journal.pre_membership.by_devid(2)` returns
  `Err(MembershipError::DuplicateDevid { devid: 2, members: [U_A, U_B, ...] })`.
  Recovery must return `RecoverError::DuplicateDevidDuringReplay {
  devid: 2, members: [U_A, U_B, ...] }` whose `Display` contains the
  literal substring `duplicate devid 2 in journaled membership across UUIDs`
  followed by every colliding UUID in canonical lexicographic order.
  The error MUST abort the replay and leave the journal in place;
  remediation is the recovery-scenarios guide, not a `pool.json` edit
  (the resolution did not touch the live file).
- **`live_pool_matches_membership` drift regression**: construct a
  `PoolState` whose `devices[0].mapper = "braid-WRONG"` but
  `devices[0].luks_uuid = U_M`, and a `PoolMembership` containing
  exactly that UUID (under any disk name). Call
  `live_pool_matches_membership(&pool, &membership)` and assert it
  returns `true` (UUID match, mapper drift does not matter). Today's
  helper returns `false` here because it parses the mapper name and
  compares names; the migration's helper compares UUIDs. The opposite
  case -- `devices[0].mapper = mapper_name(&member.name).0` but
  `devices[0].luks_uuid` is foreign -- must return `false` because
  the UUID does not appear in the membership map.
- **`live_pool_matches_membership` full case enumeration**: cover each
  case listed in the `live_member_names and the journal-clearing gate`
  spec section with a dedicated unit test. The helper returns
  `Result<bool, MembershipError>`; tests pin the exact return shape:
  - never-enriched present member (UUID in `pool.devices`, persisted
    `devid: None`) returns `Ok(true)`;
  - missing-on-both-sides (UUID's persisted `devid` is in
    `pool.missing_devids`) returns `Ok(true)`;
  - gone-without-trace (UUID not in `pool.devices`, persisted `devid`
    `None` or not in `pool.missing_devids`) returns `Ok(false)`;
  - foreign live UUID (`dev.luks_uuid` not in membership map)
    returns `Ok(false)`;
  - duplicate devid in `pool.missing_devids` resolving via
    `Err(DuplicateDevid)` returns
    `Err(JournaledSnapshotError::DuplicateDevid { devid: d, members:
    [...] })` -- explicitly NOT `Ok(false)`. A second test at the
    journal-clearing-gate caller
    (`execute_remove_missing_pool_mutation_recovery`) asserts the
    structured error translates into
    `RecoverError::DuplicateDevidDuringReplay` (not the generic
    topology-mismatch text). Both tests together pin the corruption-
    routing fix.
  - `Ok(None)` for a devid in `pool.missing_devids` returns
    `Err(JournaledSnapshotError::NoMemberForDevid { devid: d })` --
    explicitly NOT `Ok(false)`. Companion call-site test asserts
    translation into `RecoverError::NoMemberForJournaledDevid`.
  - **live/missing intersection (the disjoint-clause pin)**: a member
    `U_M` whose persisted `devid = Some(d)` AND `d` appears in
    `pool.missing_devids` AND `U_M` is also present in `pool.devices`
    (the rescue-mid-recovery case: a previously-missing disk has been
    re-attached and is live again, but btrfs has not yet forgotten the
    missing devid). `live_uuids ∪ missing_uuids == expected` holds,
    so the union clause alone would return `Ok(true)`; the disjoint
    clause `live_uuids ∩ missing_uuids == ∅` is what catches this
    case. The helper MUST return `Ok(false)`, and the caller MUST
    surface the topology-mismatch error rather than clearing the
    journal. Pins the disjoint requirement against a future
    "simplify to a union check" regression.
  These tests are the structural floor under every journal-clearing
  decision in `execute_remove_missing_pool_mutation_recovery` and
  `finish_uncommitted_replace_recovery`.

Lock:

- Drifted member mapper in `pool.devices` is in `member_owned` with the observed
  mapper name.
- Drifted member mapper execution issues `CryptsetupClose { mapper:
  "braid-wrong" }` and does not close the reconstructed expected mapper.
- **Drifted member mapper appears in `BtrfsDeviceScanForget` set
  under observed name, not reconstructed name**: under
  `LockSnapshot::Full`, with `pool.devices[0].mapper = "braid-WRONG"`
  carrying a member-matching UUID, the `forget_devs` set passed to
  `BtrfsDeviceScanForget` (both in `compile_lock_steps` and the
  `LockPlan::execute` site) contains `/dev/mapper/braid-WRONG` and
  does NOT contain `/dev/mapper/braid-<member.name>`. Pins the
  planner-side close-set wiring required by the forget-set
  observed-mapper rule.
- Close ordering: when both `member_owned` and `orphan_mappers` are non-empty,
  `execute` closes all `member_owned` entries first and then all
  `orphan_mappers`; the `BtrfsDeviceScanForget` input is built from that same
  ordered close set.
- Stranded but member-owned `braid-*` mapper outside `pool.devices` is
  reclassified as member-owned by UUID.
- True orphan mapper is classified as orphan.
- Null-underlying mapper with matching persisted devid is member-owned.
- Null-underlying mapper without a matching persisted devid is classified
  as orphan, and the existing orphan-mapper warning is emitted for it
  (matches today's `scan_orphan_mappers` behavior for braid-prefixed
  mappers that no membership lookup recognizes).
- `/dev/mapper` scan failure preserves already-classified member-owned closes.
- **Per-stranded-mapper `cryptsetup status`/`luksUUID` failure degrades to
  orphan and continues scanning**: under `LockSnapshot::Full`, with at
  least two stranded `braid-*` mappers outside `pool.devices` (e.g.
  `braid-stuck` and `braid-good`), force the per-mapper resolution for
  `braid-stuck` to fail (`cryptsetup status` or `cryptsetup luksUUID`
  returns a `CmdError`). Assert: (a) the lock scan does not abort; (b)
  `braid-stuck` is classified into `orphan_mappers` with a warning
  emitted to stderr; (c) `braid-good`'s observed UUID is resolved
  cleanly and -- if it matches membership -- `braid-good` is
  classified into `member_owned`, or into `orphan_mappers` otherwise;
  (d) the rest of `LockCloseSets` reflects the partial-failure state
  (every other classification still landed). Pins the per-mapper
  degrade behavior specified in the lock section so a regression that
  propagates the error and aborts the whole scan fails this test.
- `probe_pool` per-device failure falls back to `probe_fsid`, still runs
  `require_lock_preflight`, preserves orphan coverage, and emits a warning
  whose body contains the pinned substring `Mapper drift detection is
  disabled for this run.` (the full warning is pinned in the lock section's
  `LockSnapshot::FsidOnly` text).
- `ProbeError` variant fallback policy is asserted by table: one test per
  variant in `cli/src/probe.rs:58-88` (`Cmd`, `Parse`, `PoolDevice`,
  `UnsupportedLuksVersion`, `MapperConflict`, `MountInfo`) returns
  `LockSnapshot::FsidOnly`; the `NotBtrfs` variant aborts. The test is
  parameterized to fail-compile when a new `ProbeError` variant is added
  without an explicit policy choice (via a `match` on `ProbeError` inside
  the test helper, mirroring the production match contract).
- Paused-balance preflight still refuses lock in the fallback path.

### VM Tests

Update existing VM tests that inspect `pool.json` or journal shape. The
following are known affected:

- `tests/cli/braid-discover.{nix,py}`;
- `tests/cli/braid-add-disk.py`;
- `tests/cli/braid-add-warnings.py`;
- `tests/cli/luks-label.{nix,py}`;
- `tests/cli/unlock-uuid-mismatch.{nix,py}`;
- `tests/cli/replace-luks-label.{nix,py}`;
- `tests/cli/replace-preserves-devid.{nix,py}`;
- `tests/cli/recover-bootstrap-crash.py`;
- `tests/cli/recover-replace-completed.py`;
- `tests/cli/recover-replace-not-started.py`;
- `tests/cli/config-name-immutability.{nix,py}` if it asserts pool.json
  shape;
- every test found by
  `rg -n 'pool.json|"disks"|luks_uuid|pending-op\.json' tests`.

Discover-vs-mutating-command race coverage: the obligation introduced by
`905e9ca` is already pinned by
`tests/module/pool-lock-discover-contention.{nix,py}`, which holds
`/run/braid-pool.lock` with a generic `flock` and asserts both `braid
discover --write` and bare `braid discover` fail fast with the documented
contention message and leave `pool.json` absent. The migration must not
break this test. No new race test is required, for two reasons:

1. The lock is BSD-style `flock` at the wrapper layer (`braid-wrapper.sh`);
   kernel semantics are holder-agnostic, so a `flock`-as-holder fixture
   exercises the same contention path as `braid add`-as-holder. The
   identity of the holder is not what's being asserted.
2. The static fact that `add` (and every other mutating command)
   participates in the wrapper's flock set is enforced by the
   `case` statement in `braid-wrapper.sh`. The "In-Flight Work" section
   already constrains the migration to not alter that case statement or
   remove `discover` from the flock set, so a reading of the wrapper
   script after the migration is sufficient to verify participation.

What the migration adds to the test plan instead is a regression assertion
inside `tests/module/pool-lock-discover-contention.py`: before the
contended `discover --write` attempt, a prior mutating command (e.g.
`braid add` against the lock-free setup, or whatever the existing
fixture uses to produce real on-disk state) MUST have produced a
real UUID-keyed `pool.json` on disk. The test then records that
file's byte content, takes the lock, runs the contended
`discover --write`, asserts the documented contention message and
exit code, and asserts the `pool.json` byte content is unchanged.
Asserting "unchanged" against an absent or empty `pool.json` is not
acceptable -- the prior UUID-keyed state is mandatory, not a fallback,
so the unchanged-bytes assertion has a real before-state to pin
against torn writes. If the existing test scaffolding cannot produce
that prior state in-line, rebuild the fixture setup (or add a sibling
case that does); do not skip the prior-state precondition. This pins
the new on-disk shape against torn writes, which today's test cannot
pin because `pool.json` is name-keyed on master.

Single-user cutover coverage:

- Old name-keyed `pool.json` fails `load_membership` with the exact
  `MembershipError::Corrupt` remediation suffix pinned in `Membership API`.
- `discover --write` rebuilds UUID-keyed `pool.json` from attached LUKS2 disks
  with `Label: braid-<name>` and observable LUKS UUIDs.
- The rebuilt membership contains only the intended disks and preserves their
  expected names. These checks can live in existing discover/old-shape tests;
  no standalone migration-runbook VM test is required.

`scripts/braid-destroy.sh` coverage (no standalone VM test file is
required; fold these assertions into an existing destroy-aware VM test
if one exists, otherwise into the cutover-coverage block above):

1. Build a real two-disk pool via `braid add` so a real UUID-keyed
   `pool.json` exists on disk.
2. Run `scripts/braid-destroy.sh` against that `pool.json` and assert:
   (a) exit status 0; (b) for each UUID-keyed entry, the recording of
   issued commands (or `journalctl` / direct disk probes after the fact)
   shows one `cryptsetup close` and one `wipefs`-equivalent step
   addressed at the entry's persisted `by_id` and the mapper name
   derived from the entry's persisted `name`; (c) the iteration
   touches every UUID-keyed entry exactly once.
3. Sniff regression: write a synthetic old-shape `pool.json` (top-level
   disk-name keys) into a temp path, run `scripts/braid-destroy.sh
   <path>`, and assert (a) non-zero exit status; (b) stderr contains
   the pinned literal substring `is not in UUID-keyed format`; (c)
   the recording layer shows zero `cryptsetup close` and zero
   `wipefs` calls.

Pins both the positive UUID-keyed read path and the schema-sniff
fail-closed gate specified in the `scripts/braid-destroy.sh` section
under `Other Rust Touch Points`. A regression that reverts to
name-keyed parsing would either silently no-op on the new shape or
silently destroy on the old shape; the pair of assertions catches
both.

Add a mapper-drift VM test, `tests/cli/luks-mapper-drift.{nix,py}`:

1. Build a two-disk pool.
2. Lock it.
3. Externally open disk 1 as `braid-WRONG`; open disk 2 under its normal mapper.
4. Mount the pool so `probe_pool` observes disk 1's UUID under `braid-WRONG`.
5. Run `braid lock`.
6. Assert the wrong observed mapper is closed and the reconstructed expected
   mapper is not targeted.
7. Run `braid unlock` and assert braid returns to the normal expected mapper
   names.

Keep the test preamble clear that mapper drift is different from LUKS-header
label drift.

Add a label-drift behavioral assertion to `tests/cli/luks-label.{nix,py}`
(extend the existing test or add a sibling case in the same Nix
fixture; do not leave label-drift coverage at the migration-touch-up
level):

1. Build a pool with at least one member; record its persisted name
   (e.g. `toshiba1`) and its `/dev/disk/by-id/<id>` path.
2. Lock the pool.
3. Out-of-band: `cryptsetup config --label braid-WRONG /dev/disk/by-id/<id>`
   (the on-disk LUKS label now drifts from `braid-<name>`).
4. Run `braid status`. Assert the displayed name for that member is
   still the persisted `toshiba1`, not `WRONG` and not any error
   string; assert the output does not contain `braid-WRONG`.
5. Run `braid unlock`. Assert it succeeds without renaming the member,
   without rejecting the disk, and without surfacing a label-mismatch
   error; assert the resulting `pool.json` is byte-for-byte unchanged
   from before the unlock (the label drift is observed but not
   reconciled into membership).
6. Assert the post-unlock mapper is `/dev/mapper/braid-toshiba1` (the
   reconstructed mapper from the persisted name), not
   `/dev/mapper/braid-WRONG`.

This pins the "label drift does not break identity" invariant in CI
rather than only in the Manual VM Verification step; a regression that
re-introduces label parsing for identity correlation must fail this
test.

### Existing Suites

Run:

- `just test-rust`;
- `just test-vm`;
- `just test-parsers`;
- `cargo clippy --workspace -- -D warnings`.

Regenerate snapshots only after reviewing the diff:

```sh
INSTA_UPDATE=always cargo test
git diff cli/src/snapshots cli/src/tui/view/snapshots cli/src/browse/snapshots
```

Accept snapshot changes only when they are display-label movements or expected
shape changes from the new model.

### Manual VM Verification

Run a real VM flow:

1. `braid add toshiba1=/dev/disk/by-id/...`.
2. `braid status` still displays `toshiba1`.
3. `cryptsetup luksDump <by-id>` shows `Label: braid-toshiba1`.
4. `/var/lib/braid/pool.json` is keyed by the LUKS UUID and stores
   `name: "toshiba1"`.
5. Change the LUKS label with `cryptsetup config --label braid-WRONG <by-id>`;
   confirm status/unlock correlation still works by UUID.
6. Hot-unplug a mounted member; confirm status/TUI display the correct name via
   devid fallback.
7. Exercise add recovery, remove, remove-missing, and replace end to end:
   - `braid add <name>=<by-id>` for a fresh disk; assert the new pool.json
     entry is keyed by the LUKS UUID written into the new disk's header.
   - `braid remove <name>` for one disk; assert its UUID entry is gone
     from pool.json and the pool reports the correct membership shape.
   - `braid replace --old <name> --new <name>=<by-id>` for a live source;
     assert the old UUID is gone from pool.json and a fresh UUID is
     present, matching the new disk's header.
   - Synthetic mid-add crash: interrupt an `add` between `luksFormat` and
     `btrfs device add` (e.g. SIGKILL the braid process); `braid recover`
     completes the operation, and the resulting pool.json contains the
     pre-generated UUID rather than a re-randomized one.
8. Try `--luks-format-arg=--uuid=...` and
   `--luks-format-arg=--label=...`; each rejects before writing
   `pending-op.json`.

## Definition of Done

- UUID-keyed `pool.json` and UUID-keyed `pending-op.json` are the only accepted
  schemas.
- No production identity decision depends on a name parsed from a mapper path or
  LUKS label outside the explicit discover/add-adoption carve-outs.
- `CmdRequest::CryptsetupLuksFormat` has structured `uuid`, `label`, and
  `LuksFormatExtraOpts` fields.
- User-supplied format extras cannot override `--uuid` or `--label`.
- `LuksUuid`, `DiskName`, `ByIdPath`, and `LuksFormatExtraOpts` cannot be
  constructed with invalid data in production code or deserialization.
- `LuksUuidMap` rejects duplicate canonical UUID keys.
- `scripts/braid-destroy.sh` reads UUID-keyed `pool.json` and the
  positive-and-fail-closed VM coverage specified in `Test Plan >
  scripts/braid-destroy.sh coverage` passes.
- Docs, decisions, manual, and README describe UUID identity consistently.
- `just test-rust`, `just test-vm`, `just test-parsers`, and clippy pass.
- New mapper-drift VM test passes.
- Old-shape `pool.json` rejection and `discover --write` UUID-keyed rebuild
  coverage pass.
- Existing `tests/module/pool-lock-discover-contention.{nix,py}` passes
  against the new UUID-keyed `pool.json` shape, with the added
  unchanged-bytes regression assertion described in the Test Plan.
  The unchanged-bytes assertion runs against a real on-disk
  UUID-keyed `pool.json` produced by a prior mutating command in the
  same test scenario; an "unchanged" assertion against an absent or
  empty `pool.json` does not satisfy this bullet.
- Snapshot diffs are reviewed. Re-render only after a human reviewer
  confirms each structural delta is intentional.
- Every new `pub`/`pub(crate)` item carries a `///` doc comment per
  `AGENTS.md`. Enforcement is a reviewer audit, not a compiler gate:
  the `cli` crate does not enable `#![warn(missing_docs)]`, so
  `cargo doc --no-deps` will not catch undocumented items. Before
  merge, run `git diff master...HEAD -- 'cli/src/**/*.rs'`, grep the
  diff for new `pub`/`pub(crate)` items, and confirm every one has a
  `///` doc comment justifying why it exists at that boundary. Items
  exempt under AGENTS.md (trait impls for `Display`/`Debug`/etc.,
  `#[cfg(test)]` fixtures) do not need a comment.
- Decision 024 is written and registered in `docs/index.md`; principles 2
  and 5 are amended; manual pages and README show the UUID-keyed shape;
  `JournalError::Parse` remediation phrase appears in
  `docs/luks-unlock.md` and `manual/guides/recovery-scenarios.md` verbatim.
- Parser-critical fixtures (`cli/tests/fixtures/nixos-25.11/cryptsetup-luks-dump.txt`
  and `.json`, plus the unstable mirrors) are regenerated via
  `just capture-all-fixtures` if any of the cryptsetup dump parsers'
  surface changes, and `just test-rust` passes against the regenerated
  fixtures.

## Out Of Scope

- Removing LUKS labels or mapper names.
- Adding `braid rename`.
- Adding compatibility migrations for old state files.
- Removing `pool.json`.
- Removing `DiskMember.devid` or `added_at`.

## Discovered TODO Log

Use this append-only log for missing plan work found during refinement. Before
implementation starts, every open Blocking item must be folded into the main
plan or explicitly closed as not applicable.

Rules:

- Do not use this log as a substitute for updating the plan.
- Give every item a stable ID so future agents can refer to it without line
  numbers.
- Keep scratch detail short; put durable implementation instructions in the
  relevant plan section.

No discovered TODOs are currently open.

Template for future entries:

```md
- [ ] TODO-001: <short title>
  - Type: Blocking | Polish | Follow-up
  - Evidence: <file/symbol/doc or review finding>
  - Required action: <single concrete plan edit or decision>
  - Status: Open
```

## Risk Register

- Journal single-source UUID violation: remove every value-side duplicate UUID
  and pin with JSON shape tests.
- Managed format option override: reject `--uuid` and `--label` at the
  `LuksFormatExtraOpts` boundary and test add, replace, and journal
  replay paths.
- Canonical duplicate key loss: use `LuksUuidMap` instead of default
  `BTreeMap` deserialize.
- Stale persisted devid ambiguity: enforce uniqueness at load and fail closed
  in `by_devid`.
- Missing persisted devid on degraded operations: fail with an actionable
  message instead of guessing the member.
- Discover collision regression: keep alias dedup and label-collision checks
  before membership insertion.
- Lock drift leak: make the observed close set drive both preview and execute.
- Post-commit close double drift (Replace and Remove): between
  plan/recovery and post-commit close, the operator could re-open a
  foreign disk under the journaled mapper name. Mitigated by the
  defense-in-depth `cryptsetup luksUUID` probe before
  `CryptsetupClose` at `replace.rs:707`, `recover.rs:2935`, AND
  `remove.rs:180-183`; pinned by the close UUID-probe regression
  tests on the Replace and Remove paths. Remove has no
  recovery-replay mirror because `OpKind::Remove` is intentionally
  skipped by `replay_post_mutation`. `LockPlan::execute`'s
  member-owned close set is intentionally excluded -- see the
  "Accepted risk: in-process member-owned close double-drift"
  paragraph in the `lock.rs` section for the rationale (in-process
  window, foreign-mapper close has no btrfs-write hazard, requires
  concurrent operator misuse).
- Replace ExistingLuks new-target swap at the open boundary: between
  Replace planning and execution, the operator could swap the
  physical disk at `new_target.by_id` (USB shuffle, wrong slot).
  Mitigated by the open-boundary UUID re-probe specified in
  `replace.rs > ExistingLuks new-target UUID re-verification at the
  open boundary`; pinned by the open-boundary regression test on
  both the live and recovery-replay paths.
- Lock fallback regression: per-device probe failure must not skip FSID
  preflight or orphan cleanup.
- Recovery fixture churn: add test helpers first, then mechanically rekey
  fixtures by UUID.
- Documentation drift: implementation is incomplete until decision docs,
  principles, README, and manual all show the UUID-keyed model.
- Discovery vs mutating-command races: `905e9ca` made discovery acquire the
  pool lock; rekeying `pool.json` must preserve that acquisition. A
  regression would let `discover --write` overwrite an in-flight mutation's
  membership. Pinned by the discover-race VM test above.
- TUI display caches: `tui/mod.rs` builds `disk_names: Vec<String>` and
  related name-keyed caches from membership. Switching the source map to
  UUID keys could change cache rebuild order or contents; verify the TUI
  golden snapshots and hot-unplug behaviors before accepting snapshot diffs.
- `pool.json` key ordering changes: the on-disk file is now ordered by UUID
  (essentially random per disk) instead of by name. Any operator script or
  test that assumes name-sorted order in `jq` output breaks. Sweep
  `scripts/`, `tests/`, and the manual for assumptions about order.
- Doc-comment burden: the migration introduces many new `pub` items
  (`DiskName`, `LuksFormatExtraOpts`, `LuksUuidMap`, expanded `MembershipError`,
  helper methods on `PoolMembership`, `MemberOwnedClose`). `AGENTS.md`
  requires a `///` for each. Budget for this and gate the cutover on
  the explicit reviewer audit defined in the Definition of Done
  (`cargo doc --no-deps` cannot enforce this -- `missing_docs` is not
  enabled in the `cli` crate).
- Reference-source drift on `cryptsetup luksFormat`: the managed-flag reject
  list is pinned to cryptsetup 2.8.4 (`OPT_UUID`/`OPT_LABEL` both
  `'\0'` short name per `cryptsetup_arg_list.h:{109,217}`),
  audit closed in `New Data Model > LuksFormatExtraOpts`. A future
  nixpkgs bump that adds a short alias for any managed flag must
  extend the reject list and matching tests in the same change
  (parser-critical fixture-refresh event per AGENTS.md). The risk is
  the absence of an automated tripwire: nothing in the test matrix
  fails when an upstream alias appears, so the reviewer audit on
  fixture-refresh events is the mitigation.
- Probe failure cascades: `plan_lock` now calls `probe_pool`, which can fail
  for per-device cryptsetup reasons `probe_fsid` cannot. The `FsidOnly`
  fallback is the mitigation; tests must cover it explicitly (already in
  the lock test list above).
- Journal-as-identity trust surface: post-migration the journaled
  `pre_membership`/`target_membership` snapshots are load-bearing for
  recovery-side identity decisions (`devid -> LuksUuid` resolution and
  rebuild-path admission). A hand-edited `pending-op.json` can in
  principle inject a foreign UUID or substitute a devid value.
  Accepted-risk paragraph in `Journal Schema` enumerates the structural
  defenses (`deny_unknown_fields`,
  `RecoverError::DuplicateDevidDuringReplay`/
  `NoMemberForJournaledDevid`, `live_pool_matches_membership` corruption
  routing) and the operator-facing contract (do not hand-edit;
  reconcile via the recovery flow). The mitigation is documentation
  plus structural defenses; there is no broad live-corroboration
  cross-check because legitimate post-commit recovery has
  `pre_membership` UUIDs with no live observation by design.
