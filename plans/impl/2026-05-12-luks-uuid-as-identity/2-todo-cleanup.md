# Mapper Command Boundary and Lock Close Refactor

## Summary

Refactor cryptsetup command plumbing so mapper identity stays typed as
`MapperName` until argv rendering, and replace the lock path's parallel
member/orphan vectors with one ordered `LockCloseSet`. This removes the
deferred post-migration TODOs without changing user-visible command output.

## Key Changes

- Change every `CmdRequest` field named `mapper` from `String` to
  `MapperName`: `CryptsetupStatus`, `CryptsetupLuksOpen`,
  `CryptsetupLuksOpenKeyFile`, and `CryptsetupClose`.
- Add `MapperName::as_str()` and render mapper argv with `mapper.as_str()`
  only inside command argv construction.
- Update production and test call sites to pass `MapperName` values directly;
  use `mapper.clone()` / `mapper.as_str()` instead of reaching through
  `mapper.0.clone()` / `mapper.0.as_str()` once a value is typed.
- Update close helpers to take typed mapper identity:
  - `close_mapper_with_retry(..., mapper: &MapperName, ...)`
  - `close_mapper_best_effort(..., mapper: &MapperName, disk_label: &str, ...)`
  - `CloseMapperCtx::close_one(..., mapper: &MapperName, disk_label: &str, ...)`
- Replace `MemberOwnedClose` and `OrphanMapper` with:
  - `LockMapperClose { mapper: MapperName, kind: LockMapperCloseKind }`
  - `LockMapperCloseKind::MemberOwned { display_name: DiskName }`
  - `LockMapperCloseKind::Orphan { disk_name: String }`
- Fold `StrandedClass` into `LockMapperCloseKind`; `classify_stranded_mapper`
  returns `Result<LockMapperCloseKind, CmdError>` and computes orphan
  `disk_name` from the mapper basename.
- Replace `LockCloseSets` with singular `LockCloseSet { entries: Vec<LockMapperClose> }`.
- Use `LockCloseSet::from_classified(members, orphans)` as the only
  constructor for classified lock closes; it chains members first, then
  orphans. Do not add a separate builder type.
- Add these close-set methods with exact signatures:
  - `pub fn from_classified(members: Vec<LockMapperClose>, orphans: Vec<LockMapperClose>) -> Self`
  - `pub fn entries(&self) -> &[LockMapperClose]`
  - `pub fn is_empty(&self) -> bool`
  - `pub fn mapper_names(&self) -> HashSet<&str>`
  - `pub fn member_names(&self) -> HashSet<&DiskName>`
  - `pub fn forget_paths(&self) -> Vec<String>`
- Update `LockPlan` to hold `pub close_set: LockCloseSet` instead of
  `member_owned` plus `orphan_mappers`.
- Drop the cached `LockPlan.steps` field; have `preview()` derive steps from
  `close_set` and `mount_point` at the preview boundary via
  `compile_lock_steps(self.pool_was_mounted, &self.close_set, &self.mount_point)`.

## Behavioral Requirements

- Preserve members-first / orphans-second close ordering.
- Preserve orphan closability for malformed `braid-*` basenames by keeping
  orphan `disk_name: String`.
- The membership "already closed" prelude must skip a member when either its
  `DiskName` appears in `close_set.member_names()` or its expected
  `mapper_name(member.name)` appears in `close_set.mapper_names()`. The drift
  case, where a member-owned mapper is observed under a non-default mapper
  name, must not produce contradictory `already closed` plus `locking...`
  output for the same disk.
- Dry-run preview, real execution, and `btrfs device scan --forget` must all
  consume the same ordered `LockCloseSet`.
- `LockPlan` carries no cached `Step` vector; preview steps are re-derived from
  `close_set` on each `preview()` call so dry-run and execute cannot drift
  through separate cached close renderings.
- The unified execute loop must pass `entry.is_orphan()` into
  `CloseMapperCtx::close_one` so per-kind status/error behavior is preserved.
- Member-owned close failures use `StatusTag::Fail` without an `orphan: `
  prefix. Orphan close failures also use `StatusTag::Fail`, but include the
  existing `orphan: ` prefix. Both fatal kinds contribute to
  `first_mapper_error`.
- Preserve the existing umount-stuck busy-close special case: it reports
  `StatusTag::Warn`, uses member/orphan-specific wording, and lets the
  deferred umount error remain the returned error.
- Each new Rust CLI `pub` / `pub(crate)` item carries a `///` doc comment per
  `AGENTS.md`; variant-level docs are required when the variant carries a
  distinct invariant such as validated member names or raw orphan basenames.
- Remove the now-obsolete post-migration TODO comments for mapper retyping and
  close-set unification.

## Test Plan

- Run targeted Rust tests around lock close planning/execution and mapper close
  helpers.
- Update compile errors across add/remove/replace/recover/mount/unlock tests
  caused by cryptsetup mapper command requests now taking `MapperName`.
- Use direct tuple-struct construction at test sites; do not add constructor
  helpers or rely on `Into` magic:

  ```rust
  // Before
  CmdRequest::CryptsetupClose { mapper: "braid-aaa".into() }
  CmdRequest::CryptsetupLuksOpen { device: "/dev/...".into(), mapper: "braid-aaa".into() }
  CmdRequest::CryptsetupStatus { mapper: "braid-aaa".into() }
  CmdRequest::CryptsetupLuksOpenKeyFile {
      device: "/dev/...".into(),
      mapper: "braid-aaa".into(),
      key_file_path: "/run/key".into(),
  }

  // After
  CmdRequest::CryptsetupClose { mapper: MapperName("braid-aaa".into()) }
  CmdRequest::CryptsetupLuksOpen {
      device: "/dev/...".into(),
      mapper: MapperName("braid-aaa".into()),
  }
  CmdRequest::CryptsetupStatus { mapper: MapperName("braid-aaa".into()) }
  CmdRequest::CryptsetupLuksOpenKeyFile {
      device: "/dev/...".into(),
      mapper: MapperName("braid-aaa".into()),
      key_file_path: "/run/key".into(),
  }
  ```

- Verify existing assertions that inspected `plan.member_owned` are rewritten
  according to intent:
  - use `plan.close_set.is_empty()` when checking no closes,
  - use `entries().all(...)` when checking member/orphan classification.
- Extend `tests/cli/luks-mapper-drift.py` so `braid lock` output does not
  contain `disk disk1: already closed` while still emitting the existing
  `disk disk1: locking` / `locked` lines and closing `/dev/mapper/braid-WRONG`.
- Run one targeted VM test only after the refactor compiles, per user
  preference; do not run the full VM suite.

## Assumptions

- After this refactor, `CmdRequest` has no remaining `mapper: String` fields;
  raw parser/probe/status structs may still use `String` where they model
  external tool output rather than command input.
- Snapshot output should remain byte-identical because `MapperName`
  display/serialization is transparent and argv rendering still emits the same
  mapper text.
- This plan starts from the deferred mapper-close TODOs and includes the
  matching `CmdRequest` mapper-boundary cleanup needed to keep command typing
  consistent; unrelated VM wording failures are separate migration follow-up
  work.
