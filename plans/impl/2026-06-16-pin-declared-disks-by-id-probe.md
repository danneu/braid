# Plan: pin `declared_disks` by-id swap-detection probe (the ideal pivot)

## Context

ADR-024 ([024-luks-uuid-identity.md](../../docs/design/decisions/024-luks-uuid-identity.md), Active)
carves out a deliberate split inside `braid doctor`:

- `smart_self_test` probes the **live backing path** for a present member
  (`PoolState::underlying_for_uuid`) so by-id drift cannot make it read stale
  SMART data -- pinned by two tests (`check_smart_selftest_present_member_queries_live_underlying`,
  `..._warn_hint_uses_by_id`).
- `declared_disks` deliberately keeps probing the **stable by-id handle** so it
  can detect a disk that was swapped/reformatted at the hardware handle -- the
  early swap-detection surface ADR-024 commits to.

The by-id side has **no equivalent regression test**. A future "unify device
selection across doctor" refactor that routed `declared_disks` through
`underlying_for_uuid` (like smart) would silently move swap detection off the
stable handle and still pass every existing test, because the current
declared-disks tests either hand-build `DiskState` (the pure
`summarize_declared_disks_*` tests) or never set up an Online pool with a
present member (`check_declared_disks_warns_when_live_topology_unavailable`).
The `tests/cli/braid-doctor-uuid-swap.py` VM test runs with the pool
**unmounted** (it closes both mappers before reformatting), so it cannot catch
a regression that only affects the mounted/assembled path.

**Why this is a pivot, not just "add a test":** the obvious test cannot be
written against the current code. `classify_disk_state`
([`cli/src/doctor.rs`](../../cli/src/doctor.rs) `#classify_disk_state`) gates on
real `std::fs::metadata(path)` + `file_type().is_block_device()`, bypassing the
`fs: &dyn Filesystem` seam that `DoctorContext` already holds and threads to
`probe::probe_pool`. A unit test cannot make a fake by-id path a real block
device, so the probe returns `DiskState::Missing`, never reaches the runner, and
never yields `Fail`. The function's own doc comment concedes this: it calls the
fs gate "the only untested code path." The fix is to route that gate through the
already-injected `Filesystem` seam, which both **enables the swap test** and
**dissolves the untested-gate class** the doc comment laments.

Intended outcome: `declared_disks`'s by-id swap-detection probe is pinned by a
hermetic unit test that fails if the probe is ever rerouted to the live backing
path, and `classify_disk_state` is fully testable through the doctor fs mock.

## Approach

### 1. Route `classify_disk_state`'s block-device gate through `ctx.fs`

File: [`cli/src/doctor.rs`](../../cli/src/doctor.rs) `#classify_disk_state`
(sole caller is `check_declared_disks`; confirmed by grep).

- Change the signature from `(runner, path, expected_uuid)` to
  `(runner, fs: &dyn Filesystem, path, expected_uuid)`.
- Replace the `std::fs::metadata` match with the existing trait seam
  (`Filesystem` in [`cli/src/probe.rs`](../../cli/src/probe.rs), already in
  scope via `ctx.fs`), preserving the exact `Missing` vs `NotBlock` semantics:

  ```rust
  let device = path.to_string_lossy();
  if fs.is_block_device(&device) {
      classify_luks_identity(runner, &device, expected_uuid)
  } else if fs.exists(&device) {
      DiskState::NotBlock
  } else {
      DiskState::Missing
  }
  ```

  This matches `RealFilesystem` semantics exactly (block device -> proceed;
  exists-but-not-block -> `NotBlock`; absent/dangling symlink -> `Missing`), so
  runtime behavior is unchanged. `classify_luks_identity` (the runner-only LUKS
  identity seam, already covered by three unit tests) is untouched.
- Update the call site in `check_declared_disks` (the `.map` closure) to
  `classify_disk_state(ctx.runner, ctx.fs, Path::new(&by_id), uuid)`. `ctx.fs`
  is a `Copy` shared reference; this is the same borrow shape as the existing
  `ctx.runner` capture in that closure.
- Rewrite the `classify_disk_state` doc comment: it now consults the injected
  `Filesystem` seam (so the block-device gate is testable), removing the "only
  untested code path" claim. Keep it boundary-focused per AGENTS.md doc-comment
  rules.

### 2. Teach `DoctorMockFs` to model all three gate outcomes

File: [`cli/src/test_fixtures/doctor.rs`](../../cli/src/test_fixtures/doctor.rs)
`#DoctorMockFs`.

The gate has three outcomes -- block device (proceed), existing-but-not-block
(`NotBlock`), and absent (`Missing`) -- so the mock must represent each. A
single block-device set cannot express "exists but not a block device", so
mirror the canonical two-set idiom in
[`cli/src/probe.rs`](../../cli/src/probe.rs) `MockFs` (separate `paths` and
`block_devices` `Vec<String>`s, matched with `.contains()`):

- Add two fields (both default empty): `block_devices: Vec<String>` and
  `existing_paths: Vec<String>`. Initialize both to `vec![]` in the existing
  `mounted_btrfs_only()` and `empty()` constructors.
- Add two chainable builders following the `with_*`-returns-`Self` convention
  (e.g. `MockRunner::with_output`, `preflight` `MockFs::with_mountinfo`):
  - `pub(crate) fn with_block_device(mut self, path: &str) -> Self` -- registers
    a block device.
  - `pub(crate) fn with_existing_path(mut self, path: &str) -> Self` -- registers
    a path that exists but is not a block device.
- Implement `is_block_device(p)` as `block_devices.contains(p)`, and `exists(p)`
  as `existing_paths.contains(p) || block_devices.contains(p)` (a block device
  also reports as existing, matching `RealFilesystem`'s coupling). This yields
  the full truth table: `with_block_device` -> proceed; `with_existing_path` ->
  `NotBlock`; neither -> `Missing`.
- Unregistered paths still return `false` from both methods, so all ~27 existing
  `DoctorMockFs` callers are unaffected (none currently exercise
  `is_block_device`/`exists`).

### 3. Add the regression test

File: [`cli/src/doctor.rs`](../../cli/src/doctor.rs) test module, beside
`check_smart_selftest_present_member_queries_live_underlying`.

Test `check_declared_disks_present_member_probes_by_id_not_live` with the
required `//` Intent / Why it exists / Scenario preamble (per AGENTS.md):
swap detection must read the stable by-id handle even when the member is
assembled and a live backing path is available; a "unify with smart's
`underlying_for_uuid`" refactor would silently defeat it.

Setup (reusing existing helpers):
- `save_doctor_membership(&paths, &[(1, "disk1", "/dev/disk/by-id/disk1", Some(Devid::new(1)))])`.
- `pool_state_runner(vec![("braid-disk1", 1, "/dev/vdb", test_uuid(1))], &[])`
  -- establishes a mounted Online pool (registers `MountpointCheck` ok, btrfs
  show, `cryptsetup status braid-disk1 -> /dev/vdb`, `luksUUID /dev/vdb ->
  test_uuid(1)`), so `underlying_for_uuid(test_uuid(1)) == "/dev/vdb"` is
  available to any (mis)routed probe.
- Chain `.with_output(...)` for the **by-id** probe carrying the swapped
  identity: `CryptsetupIsLuks { device: "/dev/disk/by-id/disk1" }` ok and
  `CryptsetupLuksUuid { device: "/dev/disk/by-id/disk1" } -> test_uuid(2)`.
  Reuse the `is_luks_ok` / `luks_uuid_ok` factories
  ([`cli/src/test_fixtures/mount.rs`](../../cli/src/test_fixtures/mount.rs)).
- Counterfactual anchor: also register `CryptsetupIsLuks { device: "/dev/vdb" }`
  ok (its `luksUUID -> test_uuid(1)` is already registered by
  `pool_state_runner`) and mark `/dev/vdb` as a block device. With a comment
  stating: if a refactor wrongly routed this probe through the live backing
  path, `/dev/vdb` carries the **matching** UUID and the check would pass `Ok` --
  so asserting `Fail` pins the by-id selection.
- `fs = DoctorMockFs::mounted_btrfs_only().with_block_device("/dev/disk/by-id/disk1").with_block_device("/dev/vdb")`.
- `ctx = DoctorContext::for_test_parsed_with_fs(&runner, &fs, &paths, valid_config_json())`.

Assertions (behavioral, structure-insensitive -- mirroring how the smart test
asserts on the rendered outcome, not the raw `CmdRequest`):
- `check_declared_disks(&mut ctx).status == CheckStatus::Fail`.
- Message surfaces the swapped (observed) `test_uuid(2)` mismatch for `disk1`
  (mirror the assertion style of `summarize_declared_disks_promotes_to_fail_on_uuid_mismatch`).

The `Fail` outcome can only arise from reading the mismatched by-id handle,
since the live `/dev/vdb` carries the matching UUID -- that is the pin.

**Also pin the other two gate branches** with small direct `classify_disk_state`
unit tests -- now that the gate routes through the mock, all three outcomes are
reachable, so cover them (this is what makes the "gate is fully testable" claim
literally true). Both are idiomatic here (the existing `classify_luks_identity_*`
tests call the classifier directly) and use a `MockRunner::default()` with no
cryptsetup outputs registered: reaching the LUKS probe would error rather than
return the asserted state, so a green assertion proves the gate short-circuits
before touching cryptsetup.

- `classify_disk_state_existing_non_block_renders_not_block`: `fs =
  DoctorMockFs::empty().with_existing_path("/dev/disk/by-id/diskX")`; call
  `classify_disk_state(&runner, &fs, Path::new("/dev/disk/by-id/diskX"), &test_uuid(1))`;
  assert `DiskState::NotBlock`.
- `classify_disk_state_absent_path_renders_missing`: `fs = DoctorMockFs::empty()`
  (nothing registered); same call shape; assert `DiskState::Missing`.

### 4. Sync the authority doc

File: [`024-luks-uuid-identity.md`](../../docs/design/decisions/024-luks-uuid-identity.md)
"## Tests That Enforce This", the `declared_disks` bullet (currently: renders
absent members as `Warn`, keeps UUID mismatch as `Fail`, preserves offline-pool
behavior, warns when topology unavailable).

Extend that bullet to record the new invariant: `declared_disks` issues its
LUKS-identity probe against the persisted **by-id** handle (not the live backing
path) even for an assembled member, so a swap at the stable hardware handle is
detected under a mounted pool -- the deliberate counterpart to the live-path
SMART/TUI bullets. ADR-024 is Active and this is the "Tests That Enforce This"
section (not a frozen ADR or a `## See` block), so the edit is in-bounds; keep
it ASCII and within the existing list style.

## Verification

- `just test-rust` (or `cargo test -p <cli-crate> doctor`) -- the new test
  passes; existing `declared_disks`, `summarize_declared_disks_*`,
  `classify_luks_identity_*`, and the two `check_smart_selftest_*` tests still
  pass (the refactor preserves `RealFilesystem` semantics).
- Guard check: temporarily reroute the `check_declared_disks` probe to
  `topology`/`underlying_for_uuid` (live `/dev/vdb`) and confirm the new test
  flips to red (status `Ok` instead of `Fail`); revert. This proves the test is
  a real regression guard, not incidentally green.
- `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) clean --
  the `--tests` flag lints the new test code and `DoctorMockFs` fixture changes,
  which plain `cargo clippy` can skip.
- `just docs-build` -- mdbook linkcheck passes for the ADR-024 edit;
  `scripts/docs/check-output-ascii.py` unaffected (no user-facing strings
  changed; doc comment stays ASCII).

## Alternatives considered (rejected)

- **Add a `metadata`-style method to the `Filesystem` trait.** Unnecessary
  trait churn -- the existing `exists` + `is_block_device` reproduce the
  `Missing`/`NotBlock` split exactly.
- **Test via a real block device (loop/`/dev/*`).** Non-hermetic, needs
  privileges, fragile in CI; the whole codebase mocks the fs seam instead.
- **Extract a pure `declared_probe_device(member) -> &str` helper and test it.**
  The selection is an unconditional `member.by_id`, so the test would be a
  tautology a refactor would delete; it would not guard the real regression
  (routing to `underlying_for_uuid`). The behavioral `Fail`-under-live-pool test
  is the robust guard.
- **Add a mounted-swap VM subtest to `braid-doctor-uuid-swap.py`.** A mounted
  by-id/live divergence is hard to stage physically and redundant now that the
  unit path is testable; device-selection is the right altitude for a unit test.
