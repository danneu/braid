# Pin the add-path RAID1 device-count thresholds (test-only + comments)

## Context

An `/ultrareview` finding (Low, Testing) claimed the live-pool balance gate
in `AddPlan::execute` (`cli/src/add.rs`) has no unit test pinning its
`self.pool.devices.len() + mapper_paths.len() >= 2` boundary, and proposed a
positive test ("exactly one `BtrfsBalanceRaid1`") plus a companion negative
("a single-device-result pool does not balance").

Investigation confirmed the gap is real but reframed it:

- **The cited balance gate is a defensive lower-bound, not a two-sided
  branch -- so its negative side is not worth a test.** It lives inside
  `else { /* self.pool.mounted */ }` (executor) and `else { /* self.pool_was_mounted */ }`
  (preview render). On the normal add-to-existing path the existing pool has
  a present device and the branch always adds >= 1 (`.expect()` at
  `add.rs:1447` guarantees `needs_pool_add` non-empty), so `total_after >= 2`
  holds and the balance fires. The gate is not *provably* dead: `probe_pool`
  records a hot-unplugged mapper as null-underlying and excludes it from
  `PoolState.devices` (`probe.rs:447`-`468`), so a degenerate all-null mounted
  pool could in principle leave `devices` empty. But that is not a state a
  normal add drives, and fabricating it just to force the false branch would
  test an impossible-in-practice `PoolState`, not real behavior. So the
  finding's companion negative buys nothing here; the reachable, meaningful
  pin is the positive (balance IS issued).
- **The executor balance positive is genuinely untested at the unit level.**
  The only `BtrfsBalanceRaid1` references in `add.rs` are the preview step
  builder (`add.rs:767`) and two mock handlers (`add.rs:4260`, `add.rs:5152`).
  The success test at `add.rs:4466` *reaches* the balance but asserts only the
  unlock status rows and uses a non-recording runner, so a regression that
  stopped issuing the balance would not fail any unit test. (The VM test
  `tests/module/ups-lb-during-balanced-add.nix` is the slow integration
  backstop; this adds the fast unit pin.)
- **The genuinely two-sided `>= 2` device-count threshold is the *bootstrap*
  mkfs choice** (`mapper_paths.len() >= 2` -> `MkfsBtrfsRaid1` vs `MkfsBtrfs`),
  not the balance gate -- and **neither side is actually pinned today.** The
  single-disk render test (`dry_run_render_fresh_single_disk_bootstrap`)
  asserts only `contains("mkfs.btrfs")`, which a RAID1 render
  (`"mkfs.btrfs RAID1 ..."` / `mkfs.btrfs -d raid1 ...`) *also* satisfies -- so
  a `>= 2` -> `>= 1` regression (one disk rendered as RAID1) would slip
  through. The 2-disk RAID1 render side is pinned by **no** test at all. To pin
  the boundary, the single side must assert the *single profile*
  (`-d single -m dup`) and absence of RAID1, and the RAID1 side must assert
  `-d raid1 -m raid1`.

Intended outcome: a fast unit test that pins the executor balance issuance,
render tests that pin *both* sides of the bootstrap-mkfs boundary, and
clarifying comments so the balance gate's lower-bound nature stops generating
this finding. **No production behavior changes** -- this is tests plus two
mock arms plus comments.

## Approach (Scope B, fresh disk)

### 1. Executor balance positive test (fresh disk)

Add a `#[test]` near the existing execute tests (after `add.rs:4501`) that
drives `AddPlan::execute` end-to-end for a fresh disk added to a mounted
1-device pool, then asserts via `RequestRecordingRunner` that **exactly one**
`CmdRequest::BtrfsBalanceRaid1` was issued.

Fixture (mirror the proven recoverable-add test at `add.rs:4386`-`4501`, but
with a `Fresh` target):

- **Pool**: build inline a mounted 1-device `PoolState` matching
  `RecoverableAddRunner::pool_show()` -- mapper `braid-disk1`, devid 1,
  underlying `/dev/vdb`, luks uuid `1111...`, `fsid = POOL_FSID`,
  `mounted: true`. (Do **not** use `pool_mounted_with_fsid`; it uses
  `braid-existing`/`/dev/vda`, which the runner's `pool_show` does not match.)
- **Plan**: `plan_for_execute_target(AddTargetWork::Fresh(fresh_target("disk2",
  "/dev/disk/by-id/virtio-disk2", "2222...")), journal_targets_with(uuid2,
  fresh_journal_target(&target)), pool)` -- helpers at `add.rs:2824`, `2838`,
  `2847`, `fresh_journal_target` at `add.rs:1902` (produces `FreshLuks` mode,
  required by execute Pass 2).
- **Runner**: `RequestRecordingRunner::new(RecoverableAddRunner::new())`
  (`add.rs:4314`, `4181`).
- **Params**: `AddParams { dry_run: false, yes: true, passphrase_file:
  Some(pass_path), progress: Off, sleep_inhibitor: &RecordingInhibitor::new(),
  passphrase_reader: &RealTty, backing_path_resolver:
  mock_virtio_offset_backing_path_resolver(), .. }`, `fs =
  AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()])`, paths/pass_path from
  `execute_fixture()` (`add.rs:2907`).
- **Assert**: `result.is_ok()`, then
  `runner.requests().iter().filter(|r| matches!(r,
  CmdRequest::BtrfsBalanceRaid1 { .. })).count() == 1`.

The gate reads `total_after = self.pool.devices.len() (1) + mapper_paths.len()
(1) = 2`, so the balance fires. This is the regression guard against "stops
converting single -> RAID1, leaves new data unprotected".

### 2. Extend `RecoverableAddRunner` for the fresh path (2 additive arms)

`RecoverableAddRunner` (`add.rs:4181`-`4284`) already handles the full
live-pool add for disk2 -- the stateful post-add probe flip (`disk2_added`),
`CryptsetupStatus`/`CryptsetupLuksOpen`, `CryptsetupLuksUuid` for
`/dev/disk/by-id/virtio-disk2` -> `2222...`, `BtrfsDeviceAdd`, and
`BtrfsBalanceRaid1`. A fresh disk additionally dispatches
`CryptsetupLuksFormat` (`luks::luks_format`) and `CryptsetupLuksHeaderBackup`
(`luks::backup_luks_header_to`, `luks.rs:480`). Add two arms inside its `run`
match:

- `CryptsetupLuksFormat { .. } => Ok(mock_ok("cryptsetup luksFormat", ""))`.
- `CryptsetupLuksHeaderBackup { backup_path, .. } => { ... }` -- must **create
  the backup file**, because `backup_luks_header_to` does `set_permissions` +
  `durable_rename` on it after the command returns. Copy the proven pattern
  from `AddRecordingRunner` (`add.rs:~7070`): `create_dir_all(parent)` then
  `std::fs::write(backup_path, b"")`, then `Ok(mock_ok(...))`.

Both arms are additive: the existing `pass1_recoverable_...` test (4386) never
issues either command, so its behavior is unchanged.

### 3. Bootstrap mkfs render tests (both sides; cheap, no runner)

Pin both sides of the `mapper_paths.len() >= 2` boundary so a regression in
*either* direction is caught. The mkfs commands are confirmed: `MkfsBtrfs`
renders `mkfs.btrfs -d single -m dup -O block-group-tree <device>`
(`cmd.rs:694`) and `MkfsBtrfsRaid1` renders
`mkfs.btrfs -d raid1 -m raid1 -O block-group-tree <devices...>` (`cmd.rs:706`).

**3a. Strengthen the existing single-disk test.** In
`dry_run_render_fresh_single_disk_bootstrap`, tighten the existing
`$ mkfs.btrfs` command-line assertion -- the `lines[7].contains("$ mkfs.btrfs")`
check -- to require `-d single -m dup` **and** forbid `raid1` (e.g.
`assert!(lines[7].contains("-d single -m dup")); assert!(!lines[7].contains("raid1"))`).
Today it asserts only `contains("mkfs.btrfs")`, which a RAID1 render also
satisfies; this strengthening is what catches a `>= 2` -> `>= 1` regression
(one disk wrongly rendered as RAID1). Add a sentence to that test's preamble
noting it now pins the single-profile side of the bootstrap boundary.

**3b. Add the 2-disk RAID1 render test.** New `#[test]` next to
`dry_run_render_add_to_existing_pool_with_balance` (`add.rs:6832`) covering the
RAID1 bootstrap branch (`render_steps`, `add.rs:706`-`713`):

- `runner = MockRunner::default()`; `pool = pool_unmounted()` (`add.rs:2518`);
  `probed` = two `PresentConfigDiskState::PresentNotLuks` disks (disk1, disk2).
- `build_add_work_plan(...).render_steps()` then `Step::render_dry_run(&steps)`.
- Assert the rendered command contains `-d raid1 -m raid1` (and the
  `"mkfs.btrfs RAID1"` Step description, `add.rs:709`). This catches a
  `>= 2` -> `>= 3` regression (two disks wrongly rendered as single).
- Assert `!output.contains("btrfs balance to RAID1")` -- bootstrap reaches
  RAID1 via the mkfs profile, never a balance step.

Together 3a and 3b pin both sides of the boundary at the render layer.

### 4. Clarifying comments on the balance gates

Add a one-line comment at both `if total_after >= 2` sites describing it as a
**defensive lower-bound guard** for the normal add-to-existing path (the
existing pool's present devices plus the >= 1 device this add commits), kept
for parallel structure with the bootstrap mkfs gate. Do **not** claim every
mounted `PoolState` has a present device -- `probe_pool` excludes
null-underlying mappers from `PoolState.devices` (`probe.rs:447`-`468`), so the
guard is a real lower-bound, not a tautology:

- Preview render: `add.rs:762`-`763`.
- Executor: `add.rs:1477`-`1478`.

Rationale: this dissolves the reviewer confusion that produced the finding,
without touching the mutating path (removal was considered and rejected -- it
de-parallelizes from the bootstrap/preview gates and discards a lower-bound
guard that is not provably dead).

## Critical files

> **Line numbers in this plan are approximate.** They were captured before
> commit `0ccced3` ("fix(add): fail closed on empty journal targets") landed in
> `add.rs` mid-planning, which shifted everything below it by ~11-15 lines
> (e.g. `RecoverableAddRunner` is now ~4192, `RequestRecordingRunner` ~4325,
> `dry_run_render_add_to_existing_pool_with_balance` ~6847,
> `dry_run_render_fresh_single_disk_bootstrap` ~6543). Every *symbol name* in
> this plan is verified-current -- locate each target by `rg '<symbol>'`, not
> by line number. (The repo follows this norm already: commit `1fe9651`
> dropped rust line-number refs from comments.)

- `cli/src/add.rs` -- all changes (two new tests, one strengthened test, two
  added runner arms, two comments). No other files.
- Read-only references during implementation: `cli/src/luks.rs:480`-`518`
  (header-backup dispatch), `cli/src/cmd.rs:694` (single-profile mkfs args) and
  `cli/src/cmd.rs:706` (RAID1 mkfs args).

## Reuse (do not write new versions of these)

- `RequestRecordingRunner` (`add.rs:4314`) -- the command-log mechanism.
- `RecoverableAddRunner` (`add.rs:4181`) -- the stateful live-pool-add runner;
  extend, do not clone.
- `AddRecordingRunner`'s header-backup arm (`add.rs:~7070`) -- copy the
  file-creating mock pattern.
- `plan_for_execute_target` / `fresh_target` / `journal_targets_with` /
  `fresh_journal_target` / `execute_fixture` / `AddMockFs` /
  `mock_virtio_offset_backing_path_resolver` -- plan + fixture builders.
- `dry_run_render_add_to_existing_pool_with_balance` and
  `dry_run_render_fresh_single_disk_bootstrap` -- render test structure and the
  `// Intent: / Why: / Scenario:` preamble form.

## Verification

This is a Rust-unit-test + comment change; no VM tests, no fixtures, no
parser-critical tool versions are involved.

1. `just test-rust` -- full CLI unit suite stays green.
2. Run each new/strengthened test by name to confirm it passes for the right
   reason -- one filter per `cargo test` invocation (libtest takes a single
   filter before harness args):
   `cargo test -p braid-cli <balance_test>` then
   `cargo test -p braid-cli <single_disk_render_test>` then
   `cargo test -p braid-cli <raid1_render_test>`. (Or just run `just test-rust`.)
3. Mutation-check each guard, then revert (manual, do not commit):
   - Executor balance: change `>= 2` to `>= 3`; the balance test must fail
     (zero balances issued).
   - Bootstrap boundary low side: change the bootstrap `>= 2` to `>= 1`; the
     strengthened single-disk render test must fail (one disk rendered RAID1).
   - Bootstrap boundary high side: change the bootstrap `>= 2` to `>= 3`; the
     2-disk RAID1 render test must fail (two disks rendered single).
4. Do **not** run `cargo fmt` / `just fmt`; keep edits narrow per repo policy.

## Out of scope (considered, rejected)

- Executor-level bootstrap mkfs issuance tests (Scope C): already covered by
  `tests/module/add-bootstrap.nix`; high mock cost (bootstrap-capable runner)
  for a trivial 2-line `if/else`.
- A live-pool "single-result does not balance" negative test: not reachable on
  any normal add path; forcing it needs a fabricated all-null `PoolState` (see
  Context).
- Removing the `>= 2` balance guards: no behavioral gain on the normal path,
  touches a mutating path, breaks parallel structure, and discards a
  lower-bound guard that is not provably dead.
