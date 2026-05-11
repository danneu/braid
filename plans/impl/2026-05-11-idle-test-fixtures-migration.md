# Plan: Migrate `cli/src/idle.rs` test scaffolding to `test_fixtures::idle`

**Status: Draft**

## Goals

Simplify `cli/src/idle.rs::tests` by moving repeated idle-shaped setup into a
focused `cli/src/test_fixtures/idle.rs` module while preserving the contracts
that make the current autosuspend tests useful:

- pool offline returns `IdleResult::PoolOffline`
- mounted pool with scrub finished and `exclusive_operation=none` returns
  `IdleResult::Idle`
- running scrub short-circuits before any `/sys/fs/btrfs` scan and reports
  scrub progress from `btrfs scrub status`
- each known kernel exclusive-operation string maps to the matching
  `BusyReason`
- unknown exclusive-operation values fail closed as `BusyReason::Unknown`
- sysfs read and list failures fail closed
- scrub probe failures fail closed
- mountinfo read and parse failures fail closed
- `features` and `debug` sysfs pseudo-dirs are skipped by name
- a missing `exclusive_operation` file for a listed real fsid is not silently
  skipped
- any busy btrfs fsid blocks suspend, not only the pool fsid
- empty `/sys/fs/btrfs` after a mounted-btrfs check fails closed
- `cmd_idle` does not reintroduce `BtrfsBalanceStatus`,
  `BtrfsReplaceStatus`, or `BtrfsFilesystemShow` subprocess probes

This is a test-side refactor only. Do not change `cmd_idle`,
`is_btrfs_mounted`, `check_any_btrfs_exclusive_op`, `ExclusiveOp::parse`, or
exit-code behavior.

## Current-State Inventory

`cli/src/idle.rs` is compact. The `#[cfg(test)] mod tests` block starts at
line 144 and contains 17 tests plus about 200 lines of local scaffolding.

### Local Helpers

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `FSID` / `FSID_OTHER` | 151-152 | Canonical sysfs fsid names used to seed `/sys/fs/btrfs/<fsid>/exclusive_operation`, including multi-fsid order-sensitive tests. | Move to `test_fixtures::idle` as `IDLE_FSID` / `IDLE_FSID_OTHER`. Export because several tests need explicit fsid ordering and one intentionally leaves `IDLE_FSID_OTHER` unseeded. |
| `mp` | 154-156 | Canonical `/mnt/storage` mount point. | Promote as `idle_mp()`. Do not reuse `status_mp`, `ack_mp`, or `monitor_mp`; idle's runner outputs and mountinfo shape should stay idle-scoped. |
| `MOUNTINFO_WITH_BTRFS_TARGET` | 158-159 | Mounted btrfs at `/mnt/storage`, source `/dev/mapper/braid-disk1`. | Move private to the idle fixture module. Expose through `IdleMockFs::mounted_btrfs_only()` and `IdleMockFs::with_exclop(...)` rather than re-exporting the raw body. |
| `MOUNTINFO_WITHOUT_TARGET` | 160-161 | Rootfs-only mountinfo; no configured target. | Move private and expose through `IdleMockFs::offline_mountinfo()`. |
| local strict `MockFs` | 163-292 | HashMap-backed filesystem mock. `read_to_string` and `list_dir` are strict: unseeded paths return `NotFound` with the path in the message. | Promote as `IdleMockFs`. Preserve strict unseeded read/list behavior. Do not use `shared::MockFs`; it is too broad in some places and cannot model `/sys/fs/btrfs` listings/errors precisely. |
| `MockFs::empty` | 173-179 | Starts with no reads or listings seeded. | Promote as `IdleMockFs::empty()`. Needed for mountinfo-read failure and deliberate short-circuit tests. |
| `seed_mountinfo` | 181-185 | Seeds `/proc/self/mountinfo`. | Promote as a chainable method. Keep public enough for the malformed-mountinfo parser test. |
| `seed_btrfs_listing` | 187-193 | Seeds `list_dir("/sys/fs/btrfs")`. | Promote as a chainable method for pseudo-dir, multi-fsid, and empty-list tests. |
| `seed_btrfs_listing_error` | 195-198 | Makes `/sys/fs/btrfs` listing fail. | Promote as a chainable method for list-dir fail-closed coverage. |
| `seed_exclop` | 200-206 | Seeds one fsid's `exclusive_operation` body with trailing newline. | Promote as a chainable method. |
| `seed_exclop_error` | 208-214 | Makes one fsid's `exclusive_operation` read fail. | Promote as a chainable method. |
| `with_exclop` | 216-224 | Common mounted-btrfs + one fsid + exclop body setup. | Promote as `IdleMockFs::with_exclop(body)`. This is the main sysfs-shape helper, but it remains narrow: one fsid, one explicit body. |
| `with_read_error` | 226-235 | Mounted-btrfs + one fsid whose exclop read returns `PermissionDenied`. | Promote as `IdleMockFs::with_exclop_read_error(kind)` or `IdleMockFs::with_read_error()`. Prefer the parameterized name so the error kind is visible at call sites where useful. |
| `with_offline_mountinfo` | 237-241 | Mountinfo lacks the configured target. | Promote as `IdleMockFs::offline_mountinfo()`. |
| `with_no_mountinfo` | 243-247 | No `/proc/self/mountinfo` seed. | Do not add a special helper beyond `IdleMockFs::empty()`. The test reads clearer when the missing seed is obvious. |
| `with_mountinfo` | 249-253 | Custom mountinfo parser input. | Promote as `IdleMockFs::with_mountinfo(content)` or use `IdleMockFs::empty().seed_mountinfo(content)`. Keep malformed content inline in the test. |
| `scrub_finished` | 294-311 | `(CmdRequest, RawCommandOutput)` for completed scrub. | Promote as `idle_scrub_finished()`. Use `shared::mock_ok` internally if it keeps the output builder shorter. |
| `scrub_running` | 313-334 | `(CmdRequest, RawCommandOutput)` for running scrub with computed bytes and percentage. | Promote as `idle_scrub_running(pct)`. Keep the percentage-driven payload, because the test asserts the parser/pct contract. |
| `runner_with_scrub_finished` | 336-339 | `MockRunner` seeded with only `BtrfsScrubStatus`. | Promote as `idle_runner_with_scrub_finished()`. It must not seed balance, replace, or filesystem-show probes. |
| `assert_busy_unknown` | 341-346 | Repeated assertion that the result is `Busy(Unknown(_))`. | Promote as `assert_idle_busy_unknown(result)`. This is idle-specific because it depends on `IdleResult` and `BusyReason`. |
| `ready_for_sysfs_check` | 391-395 | Common runner+fs pair that reaches sysfs after mount and scrub-clean gates. | Promote as `idle_ready_for_sysfs_check(exclop) -> (MockRunner, IdleMockFs)`. It remains narrow: one fsid and one explicit exclop body. |

### Behavior Families

| Family | Tests | Migration concern |
|---|---|---|
| Baseline mount/scrub flow | `idle_when_pool_offline`, `idle_when_all_ops_quiet`, `busy_when_scrub_running` | Preserve the ordering: mountinfo first, scrub second, sysfs only after finished scrub. The running-scrub test must keep sysfs deliberately unseeded. |
| Kernel exclop mapping | `busy_when_balance`, `busy_when_balance_paused`, `busy_when_device_add`, `busy_when_device_remove`, `busy_when_device_replace`, `busy_when_resize`, `busy_when_swap_activate` | Preserve one-body-per-test clarity. A table-driven test would be acceptable, but this plan does not require it; fixture migration should not combine behavior assertions unless the implementer chooses to. |
| Unknown / probe fail-closed | `busy_unknown_on_unrecognized_exclop`, `busy_unknown_on_sysfs_read_failure`, `busy_unknown_on_scrub_probe_failure`, `mountinfo_read_failure_is_busy_unknown`, `mountinfo_malformed_target_line_is_busy_unknown` | Preserve exact missing-mock and strict missing-path behavior. Do not replace these with broad runners or filesystem defaults. |
| Removed subprocess probes | `no_balance_or_replace_subprocess_calls` | Preserve `MissingMock` coverage by keeping the runner seeded with only `BtrfsScrubStatus`. The idle fixture must not seed `BtrfsBalanceStatus`, `BtrfsReplaceStatus`, or `BtrfsFilesystemShow`. |
| Sysfs scan edge cases | `idle_skips_features_and_debug_pseudo_dirs`, `idle_unknown_entry_notfound_is_fail_closed`, `idle_any_busy_blocks_suspend_multi_btrfs`, `idle_zero_fsid_dirs_after_mount_check_is_busy_unknown`, `idle_list_dir_io_error_is_fail_closed` | Preserve explicit listing order, intentionally unseeded reads, empty-list failure, and any-busy semantics. |

## Existing Fixture Modules

- `shared::MockFs` should not be reused for idle. It has useful mountinfo and
  generic `exclusive_operation` support for command preflights, but it cannot
  seed `/sys/fs/btrfs` entries, cannot make that listing fail, returns an
  empty list for unrelated directories, and answers every
  `*/exclusive_operation` read with the same value. Idle needs a stricter
  per-path mock so unseeded sysfs reads/listings expose unexpected access.
- `lock_fs` wraps `shared::MockFs` and derives `/dev/mapper` listings from
  paths. It is lock-shaped and does not model `/sys/fs/btrfs` scanning.
- `monitor_fs_btrfs`, `ack_fs_btrfs`, and `status_fs_mounted` are
  mountinfo-only fixtures. They are good examples of scoped filesystem
  helpers, but idle cannot reuse them because most idle tests need both
  mountinfo and sysfs behavior.
- `mount_fs` is path-existence oriented; its mountinfo body is not part of the
  mount-test call graph. It is not useful for idle.
- `shared::mock_ok` can be used inside `test_fixtures::idle` for scrub output
  factories, but no new shared helper is required.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/idle.rs` as a flat idle-scoped module. Register
it in `cli/src/test_fixtures.rs` with `mod idle;` and facade re-exports.

Do not create a broad `IdlePool`, topology installer, or `MockRunner`
handler. The current idle tests are valuable because missing mocks and missing
filesystem seeds are observable. The fixture should make common setup shorter,
not answer extra probes.

### Public Fixture Surface

```rust
pub(crate) const IDLE_FSID: &str;
pub(crate) const IDLE_FSID_OTHER: &str;

pub(crate) struct IdleMockFs;

impl IdleMockFs {
    pub(crate) fn empty() -> Self;
    pub(crate) fn mounted_btrfs_only() -> Self;
    pub(crate) fn offline_mountinfo() -> Self;
    pub(crate) fn with_mountinfo(content: &str) -> Self;
    pub(crate) fn with_exclop(body: &str) -> Self;
    pub(crate) fn with_exclop_read_error(kind: std::io::ErrorKind) -> Self;

    pub(crate) fn seed_mountinfo(self, content: &str) -> Self;
    pub(crate) fn seed_btrfs_listing(self, entries: &[&str]) -> Self;
    pub(crate) fn seed_btrfs_listing_error(self, kind: std::io::ErrorKind) -> Self;
    pub(crate) fn seed_exclop(self, fsid: &str, body: &str) -> Self;
    pub(crate) fn seed_exclop_error(self, fsid: &str, kind: std::io::ErrorKind) -> Self;
}

pub(crate) fn idle_mp() -> MountPoint;

pub(crate) fn idle_scrub_finished() -> (CmdRequest, RawCommandOutput);
pub(crate) fn idle_scrub_running(pct: u8) -> (CmdRequest, RawCommandOutput);
pub(crate) fn idle_runner_with_scrub_finished() -> MockRunner;

pub(crate) fn idle_ready_for_sysfs_check(exclop: &str) -> (MockRunner, IdleMockFs);

pub(crate) fn assert_idle_busy_unknown(result: IdleResult);
```

Implementation notes:

- `IdleMockFs` should preserve the local mock's strict map-backed behavior:
  unseeded `read_to_string` and `list_dir` return `ErrorKind::NotFound` with
  the unexpected path in the message.
- `exists` and `is_block_device` can keep the current local behavior
  (`false`) unless the implementation deliberately chooses to tighten them.
  The load-bearing strictness today is read/list strictness.
- `mounted_btrfs_only()` should seed only `/proc/self/mountinfo`, with no
  `/sys/fs/btrfs` listing. The running-scrub short-circuit test should use
  this helper so the absence of sysfs remains visible.
- `with_exclop(body)` should seed exactly one fsid (`IDLE_FSID`) and exactly
  one exclop body. It should not add pseudo-dirs or any second fsid.
- `idle_runner_with_scrub_finished()` should seed only
  `CmdRequest::BtrfsScrubStatus { mount_point: idle_mp() }`.
- Keep the raw mountinfo constants private. The malformed-mountinfo test can
  pass its custom body through `IdleMockFs::with_mountinfo(...)` so the bad
  parser input remains inline.
- The fixture module may use `shared::mock_ok` privately, but it should not
  introduce a new public raw-output helper unless a test needs one.

### Facade Exports

Add an idle block to `cli/src/test_fixtures.rs`:

```rust
mod idle;

#[allow(unused_imports)]
pub(crate) use idle::{
    IDLE_FSID, IDLE_FSID_OTHER, IdleMockFs, assert_idle_busy_unknown, idle_mp,
    idle_ready_for_sysfs_check, idle_runner_with_scrub_finished,
    idle_scrub_finished, idle_scrub_running,
};
```

Update the module-level comment in `cli/src/test_fixtures.rs` with one idle
bullet: flat idle helpers, strict mountinfo/sysfs filesystem mock, scrub
output factories, and no broad runner because missing mocks and missing
sysfs seeds are load-bearing.

### What Stays Local

- The malformed mountinfo string in
  `mountinfo_malformed_target_line_is_busy_unknown` stays inline. It is the
  parser-failure scenario, not reusable setup.
- The comment in `busy_when_scrub_running` explaining that sysfs is
  deliberately not seeded should stay next to the test. It is the point of
  the test.
- The comments in `idle_skips_features_and_debug_pseudo_dirs`,
  `idle_unknown_entry_notfound_is_fail_closed`, and
  `idle_any_busy_blocks_suspend_multi_btrfs` should stay local because they
  explain exact absence/order contracts in those tests.
- No new helper belongs in `shared` for this migration. If another future
  scope needs the same strict seeded-read/list filesystem, generalize after a
  second consumer exists.

## Staged Migration

Each sub-commit should keep
`cargo test --manifest-path cli/Cargo.toml --lib idle::tests` and
`just test-rust` green.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(idle): add idle fixture module` | Add `cli/src/test_fixtures/idle.rs`, register facade exports, and update the fixture facade doc comment. No `idle.rs` call sites change yet. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |
| 2 | `test(idle): migrate mount and scrub fixtures` | Migrate `idle_when_pool_offline`, `idle_when_all_ops_quiet`, `busy_when_scrub_running`, and `busy_unknown_on_scrub_probe_failure` to `idle_mp`, `IdleMockFs`, `idle_scrub_running`, `idle_runner_with_scrub_finished`, and `idle_ready_for_sysfs_check` where appropriate. Preserve the scrub-running test's unseeded sysfs surface. | Run the four tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |
| 3 | `test(idle): migrate exclusive-op mapping fixtures` | Migrate the seven known-exclop tests plus `busy_unknown_on_unrecognized_exclop` and `no_balance_or_replace_subprocess_calls` to `idle_ready_for_sysfs_check` and `assert_idle_busy_unknown`. Preserve the removed-subprocess test's runner with only scrub status seeded. | Run the nine tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |
| 4 | `test(idle): migrate mountinfo and sysfs failure fixtures` | Migrate `busy_unknown_on_sysfs_read_failure`, `mountinfo_read_failure_is_busy_unknown`, `mountinfo_malformed_target_line_is_busy_unknown`, `idle_zero_fsid_dirs_after_mount_check_is_busy_unknown`, and `idle_list_dir_io_error_is_fail_closed`. Keep the malformed mountinfo body inline and use `IdleMockFs::empty()` for the missing-mountinfo case. | Run the five tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |
| 5 | `test(idle): migrate multi-fsid sysfs edge fixtures` | Migrate `idle_skips_features_and_debug_pseudo_dirs`, `idle_unknown_entry_notfound_is_fail_closed`, and `idle_any_busy_blocks_suspend_multi_btrfs` to `IDLE_FSID`, `IDLE_FSID_OTHER`, and chainable `IdleMockFs` sysfs seed methods. Preserve explicit listing order and intentionally unseeded reads. | Run the three tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |
| 6 | `refactor(idle): delete local test scaffolding` | Delete local `FSID`, `FSID_OTHER`, `mp`, mountinfo constants, local `MockFs`, scrub output factories, `runner_with_scrub_finished`, `assert_busy_unknown`, and `ready_for_sysfs_check` from `idle.rs::tests`. Clean unused imports. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib idle::tests`; `just test-rust` |

## Risks

- **Weakening strict filesystem coverage.** Reusing `shared::MockFs` or adding
  default sysfs answers would hide unexpected reads/listings. Mitigation: keep
  `IdleMockFs` map-backed and strict for unseeded `read_to_string` and
  `list_dir`.
- **Breaking the scrub short-circuit proof.** If the running-scrub test uses a
  helper that seeds `/sys/fs/btrfs`, it no longer proves sysfs was skipped.
  Mitigation: use `IdleMockFs::mounted_btrfs_only()` and keep the local comment.
- **Losing `MissingMock` subprocess coverage.** A broad idle runner that seeds
  balance, replace, or filesystem-show probes would make
  `no_balance_or_replace_subprocess_calls` weaker. Mitigation: the only runner
  helper seeds `BtrfsScrubStatus`.
- **Hiding any-busy semantics.** A helper that always checks only `IDLE_FSID`
  could make multi-fsid behavior less visible. Mitigation: expose fsid
  constants and chainable listing/exclop seed methods for the multi-fsid tests.
- **Overfitting malformed mountinfo.** Moving the bad mountinfo line into a
  named fixture would obscure the parser shape under test. Mitigation: keep
  that string inline.
- **Overprescribing test structure.** The implementation may choose to leave
  the seven known-exclop tests as separate tests or table-drive them. The
  migration plan requires preserving behavior and strict fixtures, not a
  specific assertion layout.

## Verification

Use filtered Rust tests during each sub-commit:

```sh
cargo test --manifest-path cli/Cargo.toml --lib idle::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib idle::tests
```

Run the full Rust gate after each sub-commit:

```sh
just test-rust
```

Before promoting the plan or implementation, run:

```sh
cargo check --manifest-path cli/Cargo.toml --tests
cargo test --manifest-path cli/Cargo.toml --lib idle::tests
just test-rust
```

No VM fixture capture is required. This migration does not change parser
fixtures, nixpkgs inputs, or production parser behavior.
