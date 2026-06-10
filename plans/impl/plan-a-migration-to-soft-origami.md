# Seal `MapperName` and `MountPoint`; add `MapperName::dev_path()`

## Context

`MapperName` (`cli/src/types.rs#MapperName`) and `MountPoint`
(`cli/src/types.rs#MountPoint`) are the only two newtypes in the module that
still expose a `pub` inner field. Every other validated newtype --
`LuksUuid`, `DiskName`, `ByIdPath`, `LuksLabel` -- seals its string behind
named accessors. Those two holes cause two concrete defects:

- **Prefix drift.** Production sites hand-roll the `/dev/mapper/` prefix in
  **two spellings**: `format!("/dev/mapper/{}", mapper.0)` (~24, `recover.rs`
  alone does it 11 times) and `Display` interpolation
  `format!("/dev/mapper/{mapper}")` / `format!("/dev/mapper/{}", X.mapper)`
  (a further ~10 across `lock.rs`, `mount.rs`, `remove.rs`, `replace.rs`,
  `add.rs`, `doctor.rs`). The literal is re-typed at each one, so a typo or a
  future convention change cannot be made in one place. Sealing `.0` only
  catches the first spelling -- the second is invisible to the compiler (see
  category A2 below), which is the subtle part of this migration.
- **`.0` leak.** The basename escapes the type as a bare `String` at ~26
  `MapperName` sites and ~49 `MountPoint` sites. Once it is a loose string,
  nothing distinguishes a basename from a full device path, and nothing stops
  future code from re-introducing the hand-rolled prefix.

Outcome: one blessed way to get the device path (`dev_path()`), one blessed
way to read the basename (`as_str()`), sealed inner fields so the leak cannot
recur, and `MapperName`/`MountPoint` brought in line with the rest of the
module's type discipline.

## Scope (decided)

Full seal of **both** types. `dev_path()` returns a `String` (not a
`MapperPath` newtype).

**Why no `MapperPath` newtype:** the path-consuming layer is intentionally
block-device-path-agnostic. `CmdRequest` variants (`BtrfsReplaceStart`,
`BtrfsDeviceWipefs`, `Mount`, ...), `fs.exists`, and the scan helpers all
accept a path that may be a `/dev/mapper/<x>` mapper path **or** a
`/dev/disk/by-id/<y>` underlying path. The domain truth there is "some block
device," already modeled as `String` (with `ByIdPath` for the by-id subtype).
A `MapperPath` would be unwrapped back to `String` one call later at the
`CmdRequest` boundary, paying newtype churn for safety that never
materializes. `dev_path() -> String` hands off cleanly to that layer.

## Type changes (`cli/src/types.rs`)

**`MapperName`** -- make `.0` private; add a constructor and `dev_path()`:

```rust
pub struct MapperName(String);              // was: pub String

impl MapperName {
    /// Wrap a mapper basename observed from system output (btrfs show,
    /// cryptsetup status, a `/dev/mapper/` scan). Unvalidated on purpose:
    /// these names come from the kernel, not user input. `config::mapper_name`
    /// is the canonical `braid-<disk>` derivation; this is the observation door.
    pub fn from_basename(name: String) -> Self { MapperName(name) }

    pub fn as_str(&self) -> &str { &self.0 }  // unchanged

    /// The absolute `/dev/mapper/<name>` device path. Single source of the
    /// `/dev/mapper/` prefix so the convention cannot drift across the call
    /// sites that hand it to mount, btrfs, and `fs.exists`.
    pub fn dev_path(&self) -> String { format!("/dev/mapper/{}", self.0) }
}
```

**`MountPoint`** -- make `.0` private; add a constructor (it already wraps a
full path, so no `dev_path()` is needed):

```rust
pub struct MountPoint(String);              // was: pub String

impl MountPoint {
    /// Sole construction door now that the inner mount path is sealed.
    pub fn new(path: String) -> Self { MountPoint(path) }

    pub fn as_str(&self) -> &str { &self.0 }  // unchanged
}
```

Derives stay as-is. `#[serde(transparent)]` + `derive(Serialize, Deserialize)`
work unchanged with a private field (the derive lives in the same module), so
**no serde code changes**.

**`cli/src/config.rs#mapper_name`** -- route through the new constructor:

```rust
MapperName::from_basename(format!("braid-{}", name.as_str()))
```

## Migration mapping

Apply by category. Sealing `.0` makes the compiler flag the read migrations
(B, C) and the `.0`-based path builds (A1) -- those need no manual tracking.
The `Display`/`as_str` path builds (A2) compile fine after sealing, so they
are found by **grep, not the compiler**; the sweep in Verification is
mandatory, not optional.

**A. Path build (the drift) -> `dev_path()`.** Every production site that
builds a `/dev/mapper/<name>` path from a `MapperName` migrates to
`X.dev_path()`. Two spellings exist; they differ in whether the compiler can
see a missed site.

**A1 -- via `.0` (sealing-caught).** `format!("/dev/mapper/{}", X.0)` ->
`X.dev_path()`, including `config::mapper_name(name).0` ->
`config::mapper_name(name).dev_path()`. Sites: `recover.rs` (~11:
`render_into`, `relock_and_remount`, `execute_add_pool_mutation_recovery`,
...), `add.rs`, `mount.rs#plan_open_pool_inner`, `status.rs#build_disk_views`.
A miss here is a compile error after the seal.

**A2 -- via `Display`/`as_str` (NOT sealing-caught; grep-swept).** The
`MapperName` is interpolated whole (`{mapper}` / `{}, X.mapper`) or read as a
basename, so `.0` is never touched and `cargo build` stays green even if the
site is missed. Confirmed production sites:
- `lock.rs#LockCloseSet::forget_paths`: `format!("/dev/mapper/{}", entry.mapper)`
  -> `entry.mapper.dev_path()`.
- `lock.rs#members_known_closed`: same shape -> `entry.mapper.dev_path()`.
- `lock.rs#scan_braid_mapper_candidates`: `format!("/dev/mapper/{entry}")` over
  a raw dir-entry `String` -> wrap first
  (`let mapper = MapperName::from_basename(entry);`), then
  `fs.exists(&mapper.dev_path())`, and push `mapper`.
- `mount.rs#close_opened_mappers`: `format!("/dev/mapper/{mapper}")` ->
  `mapper.dev_path()`.
- `remove.rs#RemoveWorkPlan::render_steps`:
  `format!("/dev/mapper/{}", self.target_mapper)` ->
  `self.target_mapper.dev_path()`; and the sibling
  `format!("/dev/mapper/{mapper_str}")` (where
  `mapper_str = work_plan.target_mapper.as_str()`) ->
  `work_plan.target_mapper.dev_path()`, keeping `mapper_str` only for the
  basename status line (`pool: removing {mapper_str}...`).
- `replace.rs#build_replace_work_plan`: the `new_mapper` device path inside the
  `"btrfs replace start ..."` step description and
  `format!("/dev/mapper/{new_mapper}")` -> `self.new_mapper.dev_path()`.
- `add.rs#fresh_target` / `add.rs#recoverable_target`:
  `format!("/dev/mapper/{}", mapper_name(&name))` ->
  `mapper_name(&name).dev_path()`.
- `doctor.rs` foreign-recovery builder: the paste-ready
  `btrfs device remove /dev/mapper/{mapper} ...` recipe -> `mapper.dev_path()`
  (the drift criterion applies to copy-paste recovery commands too).

**B. Basename use (non-path) -> `as_str()`.**
- Report `String` fields / clones: `X.0.clone()` -> `X.as_str().to_owned()`
  (`status.rs#build_disk_views`, `membership.rs`, and the
  `mapper_name(&cd.name).0` sites in `status.rs` -> `...as_str().to_owned()`).
- Header filename: `luks.rs` `format!("{}.luksheader", mapper.0)` ->
  `format!("{}.luksheader", mapper.as_str())` (basename, NOT a dev path).
- Equality vs `&str`: `pool.rs` `m.0 == target_mapper` ->
  `m.as_str() == target_mapper` (`target_mapper: &str`).

**C. Display in format args -> drop `.0`.** `MapperName: Display` already
exists, so `format!("... {}", dev.mapper.0)` becomes
`format!("... {}", dev.mapper)` (`recover.rs` error messages: ~1662, ~1882,
~2247).

**MountPoint reads (~49) -> `as_str()`.** `mount_point.0.clone()` ->
`mount_point.as_str().to_owned()` (dominated by the `cmd.rs` argv builder,
~38 sites; also `probe.rs`, `tui/view/mod.rs`). `config.rs`
`mount_point.0.is_empty()` -> `mount_point.as_str().is_empty()`. JSON/format
uses -> `mount_point.as_str()`. Note the `probe.rs` `mapper: mount_point.0.clone()`
sites populate a `String` error field named `mapper` from a `MountPoint` --
they migrate under this MountPoint rule, not the MapperName one.

**Construction seal (~985 sites, mostly tests).** Mechanical replace:
`MapperName(` -> `MapperName::from_basename(` and `MountPoint(` ->
`MountPoint::new(`, excluding the two struct definitions in `types.rs`.
Verified safe: there are **no destructuring patterns** -- every occurrence is
a construction, `let x = Type(...)`, or `&Type(...)`, never a `match`-arm
`Type(s) =>` pattern. Concentrated in test fixtures (`probe.rs`, `luks.rs`,
`lock.rs`, `recover.rs`, `status.rs`, `cli/src/test_fixtures/`).

## Milestones (resumable stopping points)

Each milestone ends at a **green `cargo build` + `just test-rust`** and is a
clean commit boundary, so a fresh agent can be handed *this plan file + the
next milestone* and nothing else. The sequence is ordered so production call
sites are migrated **before** the fields are sealed: while `.0` is still
public the tree compiles continuously, and the only step that breaks the build
wholesale (M3) touches construction sites alone -- a single mechanical sweep,
not a mix of reads and constructions.

**Resuming from a milestone (handoff protocol).** A continuing agent should:
(1) confirm the prior milestone's exit gate still holds (run its
build/test/grep); (2) execute *only* the named milestone; (3) stop at its exit
gate and commit. Do not run ahead into the next milestone -- the stop is the
point.

### M1 -- Additive API (no seal yet)
- Add to `types.rs`: `MapperName::from_basename`, `MapperName::dev_path`,
  `MountPoint::new` (+ doc comments). Fields stay `pub` for now.
- Point `config.rs#mapper_name` at `MapperName::from_basename`.
- Add the `types.rs` unit tests (see Tests): they exercise `dev_path`,
  `from_basename`/`as_str` round-trip, and `MountPoint::new`, so no new `pub`
  item is dead code.
- **Exit gate:** `cargo build` + `just test-rust` green. Purely additive --
  nothing else in the tree changes.
- Commit: `feat(types): add MapperName::dev_path + sealed-field constructors`.

### M2 -- Migrate production call sites (fields still public)
- Apply Migration-mapping categories **A1, A2, B, C** and the **MountPoint
  reads** across production code. `.0` is still public, so the tree compiles
  the whole way -- migrate file-by-file, building as you go.
- Run the **A2 grep sweep** (see Verification) until every production
  `/dev/mapper/{` build is `dev_path()` and only `dev_path` itself + inline
  test/mock strings remain.
- **Internally resumable:** every file boundary is green, so a fresh agent may
  stop after any file. Record finished files in the commit body or a scratch
  note so the next agent knows where to pick up.
- **Exit gate:** `cargo build` + `just test-rust` green; A2 sweep clean; no
  production `.0` *reads* remain (test reads and `Type(...)` constructions may
  still use `.0` -- those fall to M3).
- Commit: `refactor(cli): route mapper/mount paths through dev_path/as_str`.

### M3 -- Seal the fields + mechanical construction sweep
- Flip both inner fields private: `MapperName(String)`, `MountPoint(String)`.
- Global replace `MapperName(` -> `MapperName::from_basename(` and
  `MountPoint(` -> `MountPoint::new(` (exclude the two struct defs in
  `types.rs`; no destructuring patterns exist, verified). This breaks ~985
  construction sites at once -- all mechanical, overwhelmingly tests.
- Let `cargo build` enumerate any remaining `.0` (leftover test reads ->
  `as_str()`) until clean.
- **Exit gate:** `cargo build` + `just test-rust` green; full Verification
  passes (`.0` grep empty, A2 sweep clean, ASCII check).
- Commit: `refactor(types): seal MapperName/MountPoint inner fields`.

## Tests

- Add `#[cfg(test)] mod tests` cases in `types.rs` (Intent/Why/Scenario
  preamble, matching `config.rs#mapper_name_for_disk`):
  - `dev_path()` returns `/dev/mapper/braid-x` for a representative name.
  - `from_basename` -> `as_str` round-trips the basename verbatim.
- Update the existing `config.rs#mapper_name_for_disk` assertions to
  `MapperName::from_basename("braid-toshiba".into())` (swept by the global
  replace).
- This is a pure refactor: `dev_path()` emits byte-identical strings to the
  old `format!`, so no NixOS VM test logic changes and **no fixture refresh**
  is required.

## Verification

- `cargo build` -- the seal turns any missed `.0` into a hard compile error.
  Note: a clean build proves the A1/B/C/MountPoint migration is complete, but
  says **nothing** about the A2 `Display` path builds (they never referenced
  `.0`). The grep below is the gate for those.
- `just test-rust` (`cargo test --lib --bin braid ...`) passes.
- **Scoped drift sweep (the real A2 gate).** Match path-*building*
  interpolation only -- the prefix immediately followed by a `{` brace:
  `rg -n '/dev/mapper/\{' cli/src --type rust -g '!**/test_fixtures/**'`.
  The `\{` anchor is the build-vs-check discriminator: `/dev/mapper/{...}` is
  an interpolated build, whereas `/dev/mapper/"` is a prefix check. So this
  pattern deliberately does **not** match the legitimate
  `starts_with("/dev/mapper/")` / `strip_prefix("/dev/mapper/")` prefix
  handling in `probe.rs#probe_pool` and `add.rs`, nor bare literal device
  paths -- those are correct as-is, leave them alone. It is also robust to
  multi-line `format!(` and mid-string interpolation, so it catches the A2
  sites that a same-line `format!\(...` regex would silently miss
  (`replace.rs:350`, `doctor.rs:968`).
  After migration the only legitimate remaining matches are:
  1. the single `format!("/dev/mapper/{}", self.0)` inside
     `MapperName::dev_path` in `types.rs`; and
  2. **inline test/mock output** -- `#[cfg(test)]` strings that reproduce
     external tool text and must keep the literal prefix: mock cryptsetup/btrfs
     output (`"/dev/mapper/{mapper} is inactive"`, `"... is active and is in
     use ..."`, btrfs-show `path /dev/mapper/{mapper}` lines) and assertion
     strings (e.g. `doctor.rs:5718`).
  `cli/src/test_fixtures/**` is excluded by `-g` (mock output by
  construction). Any **non-test** hit other than `dev_path` is unmigrated
  drift.
- `rg '\.0\b' cli/src --type rust | rg -iw 'mapper|mount_point'` returns
  nothing.
- `python3 scripts/docs/check-output-ascii.py` clean -- `dev_path()` emits
  plain ASCII; no Unicode is introduced.

## Non-goals

- No `MapperPath` newtype (rationale above).
- No behavior change and no serde change.
- No new validation on observed basenames (kernel-sourced, deliberately
  unvalidated, mirroring how probe labels stay `Option<String>`).
