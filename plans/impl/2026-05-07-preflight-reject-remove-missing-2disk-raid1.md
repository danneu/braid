# Plan: pre-flight reject `remove-missing` on 2-disk RAID1 with 1 survivor

## Context

`braid remove-missing` against a 2-disk RAID1 pool with one missing
device (`pool.devices.len() == 1`, `pool.missing_count == 1`) is
guaranteed to fail at the kernel level: `btrfs_rm_device` calls
`btrfs_check_raid_min_devices(fs_info, num_devices - 1)` before any
relocation, and rejects with `BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET`
because going below two devices on RAID1 is forbidden
(`reference/linux/fs/btrfs/volumes.c:2153-2178`,
`reference/linux/fs/btrfs/volumes.c:2026-2050`,
`reference/linux/include/uapi/linux/btrfs.h:1054`).

braid currently has no pre-flight for this case. Today the operator gets
through pending-op preflight, mutation preflight, UPS preflight, the
missing-id validations, and the relocation-space check (currently
gated at `cli/src/remove_missing.rs:405` by `if pool.devices.len() >= 2`,
so 1-survivor scenarios bypass it). Then `RemoveMissingPlan::execute`
prompts "Surviving disk already has all data", writes
`pending-op.json`, acquires the sleep inhibitor, and finally invokes
`pool_remove_device_using` -- which is where the kernel rejection lands.
The user is left with a stranded journal, a raw kernel error, and a
forced trip through `braid recover` for an operation that was never
going to succeed.

The architecture intent in `docs/decisions/012-intent-cli.md` is
already clear: `remove-missing` is **cleanup-only** (e.g. forgetting a
stale entry in a 3+ disk pool), and `braid replace --missing-id <devid>`
is the **repair path** for a dead disk. The symmetric reject already
exists on the live-old side of `replace` at
`cli/src/replace.rs:1277-1286`. This plan adds the missing reject on
the dead-old `remove-missing` side and aligns the documented rationale
with what the kernel actually enforces. Scope is deliberately narrow:
only the exact 2-total-device case is changed. Other single-survivor
states (e.g. 1-present + 2+-missing on a 3+-device pool) are out of
scope and their behavior is unchanged.

The reproduced kernel constraint is already encoded as folklore inline
in `tests/repro/degraded-soft-balance.py:82-84` ("can't remove missing
from a 2-device RAID1 without going below the minimum device count"),
which only completes because it `btrfs device add`s a third disk first.

Intended outcome: braid rejects the impossible operation up-front, with
no journal write and no inhibitor acquisition, and the error names the
two supported recovery paths.

## Files to change

- `cli/src/remove_missing.rs` -- add pre-flight reject; rewrite the
  masking unit test; add a dry-run unit assertion.
- `tests/cli/remove-missing-2disk-rejected.nix` -- new VM test (NixOS
  config).
- `tests/cli/remove-missing-2disk-rejected.py` -- new VM test script.
- `flake.nix` -- register the new VM test in the `checks` set
  (per `docs/testing.md`).
- `docs/decisions/012-intent-cli.md` -- correct the "ENOSPC pre-flight
  check" rationale paragraph for the single-survivor `remove-missing`
  branch.
- `manual/commands/remove-missing.md` -- add the new 2-disk refusal
  case to the "Safety checks / refusal cases" list and name the
  supported recovery paths.

## Change 1 -- Pre-flight reject in `plan_remove_missing`

Insert the new check in `cli/src/remove_missing.rs` immediately after
the existing `!pool.missing_devids.contains(&params.missing_id)` guard
(currently around line 387) and before the `if pool.devices.len() >= 2`
relocation-space block (currently around line 405). Locate the
insertion point structurally rather than by line number: it is the
gap between the last "is the missing-id valid for this pool?" guard
and the relocation-space pre-flight; line numbers will drift, the
ordering will not. Ordering matters: the missing-id identity errors
stay primary; the kernel-constraint reject only fires once the
missing-id is otherwise valid. Notes must be preserved on the `Err`
branch like the surrounding guards (use the same
`std::mem::take(&mut notes)` shape).

Guard precision matters: the kernel rule is on **total filesystem
devices**, not surviving devices. `btrfs_rm_device` reads
`btrfs_num_devices(fs_info)` which is `fs_devices->num_devices` --
`reference/linux/fs/btrfs/volumes.c:2095-2107` and `:2174-2178` -- and
that count includes missing devices. The RAID1 minimum (`devs_min == 2`,
`reference/linux/fs/btrfs/volumes.c:78`) only proves rejection when
total drops below 2; a 1-present + 2-missing pool (total = 3) would
still pass the kernel check and is therefore out of scope for this
guard. Match the kernel constraint exactly. `PoolState.total_devices`
already exists for this purpose at `cli/src/types.rs:61` and is
populated by `probe_pool` from `btrfs filesystem show`'s "Total
devices" line.

Recommended body, matching the wording style of the live-old reject in
`cli/src/replace.rs:1277-1286` and the `--` (double-hyphen) CLI style
from `AGENTS.md`. Note the `--new` value shape: replace's
`parse_disk_spec` (used at `cli/src/replace.rs:887` and exercised in
every replace test fixture, e.g. `cli/src/replace.rs:2713`) requires
`<name>=<by-id-path>`, not bare `<name>`:

```rust
// Pre-flight: reject the exact 2-device RAID1 + 1 missing case. The
// kernel's btrfs_rm_device calls btrfs_check_raid_min_devices on
// `num_devices - 1` (where num_devices is fs_devices->num_devices,
// counting present + missing) and rejects with
// BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET when that drops below devs_min=2.
// Per docs/decisions/012-intent-cli.md, remove-missing is cleanup-only;
// the documented repair path for a dead disk on a 2-disk pool is
// `braid replace --missing-id <devid>`. Pools with total_devices > 2
// are intentionally out of scope here -- the kernel accepts those
// calls, and reasoning about data integrity in multi-missing states
// (where the survivor is not guaranteed to mirror every chunk under
// btrfs RAID1's ncopies=2 layout) is left to existing/future logic.
if pool.total_devices == 2
    && pool.devices.len() == 1
    && pool.missing_count == 1
{
    return RemoveMissingPlanReport {
        notes: std::mem::take(&mut notes),
        result: Err(RemoveMissingError::Validation(format!(
            "cannot remove missing devid {devid} -- this is a 2-disk \
             RAID1 pool with one disk missing, and the kernel refuses \
             to drop a RAID1 pool below two devices. Repair the dead \
             disk with `braid replace --old <missing-name> \
             --new <new-name>=/dev/disk/by-id/<...> --missing-id \
             {devid}`, or run `braid add <new-name>=/dev/disk/by-id/<...>` \
             first and then re-run `braid remove-missing`. \
             Use `braid status` to see device names and IDs.",
            devid = params.missing_id,
        ))),
    };
}
```

Leaves the existing `if pool.devices.len() >= 2` guard around
`check_relocation_space` untouched -- the user chose the tight scope,
so we do not touch dead-code cleanups in this plan, and we also make
no new claims about whether that skip is correct for 1-present +
2+-missing pools (under btrfs RAID1's ncopies=2 layout the survivor
is not guaranteed to hold every chunk in that state). Those non-2-disk
single-survivor cases are out of scope; their behavior is whatever
the existing code does today.

Reuses, do not duplicate:
- `RemoveMissingError::Validation` (file head).
- `RemoveMissingPlanReport` shape (already used by every guard in the
  function).
- `PoolState.total_devices` (`cli/src/types.rs:61`).
- `std::mem::take(&mut notes)` notes-preservation idiom (used by every
  sibling guard in `plan_remove_missing`; current call sites are at
  `cli/src/remove_missing.rs:362, 369, 379, 389, 413`, but locate by
  context rather than by line number).

## Change 2 -- Rewrite the masking unit test

`no_usage_probe_for_single_survivor` (currently at
`cli/src/remove_missing.rs:722`; locate by name, not line number)
proves the broken behavior: it sets up 1 present + 1 missing via
`PoolFixture::two_disk_devids_pinned()` plus
`RemoveMissingPool::two_disk_one_missing()`, calls
`cmd_remove_missing`, `.expect("remove-missing should succeed")`, and
asserts no `BtrfsDeviceUsageRaw` was probed. After the fix, that test
must invert: the same scenario must reject at preflight with no side
effects.

Replace the test using the current fixture harness in
`cli/src/test_fixtures/remove_missing.rs` -- the same shared scaffold
the surrounding tests use (e.g.
`journal_survives_device_remove_failure` at
`cli/src/remove_missing.rs:1272`). Concretely:

- `let f = PoolFixture::two_disk_devids_pinned();`
- `let (runner, _remove_done) = RemoveMissingPool::two_disk_one_missing().install(MockRunner::default());`
- Build params with the existing `RemoveMissingParamsBuilder`:
  `f.remove_missing_params().missing_id(2).build()` for real-run,
  `.missing_id(2).dry_run(true).build()` for dry-run (the
  `.dry_run(bool)` setter is at
  `cli/src/test_fixtures/remove_missing.rs:249`; the same setter is
  already exercised in `remove.rs` tests at
  `cli/src/remove.rs:1012, 1525, ...`).
- `&MockFs::storage(vec![])` for the filesystem mock.
- Assertions read from the fixture (no separate locals): the runner's
  command log is `runner.requests()`; the inhibitor counter is
  `f.inhibitor.acquire_count()`; the journal is
  `journal::load_journal(&f.paths)` (see the existing assertion at
  `cli/src/remove_missing.rs:1297` for the canonical shape, just
  inverted -- `.unwrap().is_none()` instead of `.is_some()`).

New body, with **two assertions** -- one for the real-run path and
one for the dry-run path -- so a future refactor cannot move the
reject from `plan_remove_missing` into `execute()` and silently
regress dry-run behavior:

- **Real-run assertion** (`f.remove_missing_params().missing_id(2).build()`):
  - Call `cmd_remove_missing(&runner, &MockFs::storage(vec![]), &params)`;
    bind the `Result`.
  - Assert `Err(RemoveMissingError::Validation(msg))`, where
    `msg.contains("2-disk RAID1 pool with one disk missing")` and
    `msg.contains("braid replace")` and `msg.contains("--missing-id")`
    (two assertions, since the recommended message body has `--old` /
    `--new` flags between `replace` and `--missing-id`).
  - Assert `f.inhibitor.acquire_count() == 0` -- the reject must land
    before `RemoveMissingPlan::execute` reaches the inhibitor acquire.
  - Assert `journal::load_journal(&f.paths).unwrap().is_none()` --
    no pending-op.json was written.
  - Assert no `BtrfsDeviceRemove` calls landed:
    `assert!(!runner.requests().iter().any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })))`.

- **Dry-run assertion** (`f.remove_missing_params().missing_id(2).dry_run(true).build()`,
  fresh fixture so the inhibitor counter starts at 0):
  - Call `cmd_remove_missing(...)`.
  - Assert the same `Err(RemoveMissingError::Validation(msg))` with
    the same substrings as the real-run case. This pins the invariant that **the
    reject lives in `plan_remove_missing`, not in `execute()`** --
    `cmd_remove_missing` runs `plan_remove_missing` first and bails
    on `Err` before reaching the `if params.dry_run` branch (currently
    around `cli/src/remove_missing.rs:454`, locate by context). If
    someone moves the reject downstream, this assertion fails first.
  - Assert `f.inhibitor.acquire_count() == 0`.
  - Assert no `BtrfsDeviceRemove` calls landed.

Use two sibling tests rather than one combined test -- it reads more
cleanly and matches the convention in the surrounding test module.
Suggested names: `single_survivor_rejected_at_preflight` and
`single_survivor_rejected_in_dry_run`.

Preamble (per `AGENTS.md` "Test Conventions"):
1. **Intent**: `cmd_remove_missing` rejects the exact 2-disk RAID1 +
   1-missing case at preflight, in both real-run and dry-run, with no
   side effects.
2. **Why it exists**: The kernel's
   `btrfs_check_raid_min_devices(num_devices - 1)` rejects going below
   two devices on RAID1; without the preflight braid would strand
   `pending-op.json` and the inhibitor for a doomed call. The dry-run
   pin guards against a future refactor moving the reject into
   `execute()`, which would silently let `--dry-run` print a doomed
   plan.
3. **Scenario**: 2-disk RAID1, disk2 dies. Operator runs
   `braid remove-missing --missing-id 2` (and again with `--dry-run`).
   braid rejects up-front and names the supported repair paths.

The other dead-code-adjacent test, `remove_missing_confirm_single_survivor`
(currently at `cli/src/remove_missing.rs:1511`; locate by name),
exercises the `format_remove_missing_confirm` else branch. Per the
chosen tight scope, leave that test alone -- the formatter is pure
and its else branch survives as defense-in-depth.

## Change 3 -- New VM test (`tests/cli/remove-missing-2disk-rejected.{nix,py}`)

Two-disk RAID1 end-to-end coverage. Models the closest existing tests
for shape and provisioning:

- `.nix` config: model on `tests/cli/remove-missing-inhibits-suspend.nix`
  but provision **two** disks (`disk1`, `disk2`) instead of three. Reuse
  whatever shared module helper provisions a braid-managed pool; do not
  hand-roll braid setup if a helper exists.
- `.py` script: model on `tests/repro/degraded-soft-balance.py:1-54`
  for the disk-death + degraded-mount sequence, then drive `braid` end
  to end:
  1. Bootstrap braid with 2 LUKS+btrfs RAID1 disks.
  2. `braid lock` (or umount + `cryptsetup luksClose disk2`).
  3. Re-mount degraded.
  4. Real-run reject. Use the project-standard pattern for capturing
     both status and combined output (model on
     `tests/cli/braid-destroy.py:62-64` and
     `tests/cli/add-passphrase-mismatch.py:67-69` --
     `machine.fail` returns only the output and is harder to assert
     against precisely):
     ```python
     status, output = machine.execute(
         "braid remove-missing --missing-id <devid> --yes 2>&1"
     )
     assert status != 0, f"expected non-zero exit, got {status}:\n{output}"
     assert "2-disk RAID1 pool with one disk missing" in output, output
     assert "braid replace" in output, output
     assert "--missing-id" in output, output
     ```
  5. Dry-run reject. Same pattern, with `--dry-run` instead of `--yes`:
     ```python
     status, output = machine.execute(
         "braid remove-missing --missing-id <devid> --dry-run 2>&1"
     )
     assert status != 0, f"expected non-zero exit, got {status}:\n{output}"
     assert "2-disk RAID1 pool with one disk missing" in output, output
     assert "braid replace" in output, output
     assert "--missing-id" in output, output
     ```
     Ties the dry-run reject to end-to-end coverage as well as the
     unit test.
  6. Assert the journal was not stranded:
     `machine.fail("test -f /var/lib/braid/pending-op.json")`.
  7. (Optional) Run
     `braid replace --old disk2 --new disk3=/dev/disk/by-id/virtio-disk3 --missing-id <devid>`
     against a third virtual drive to prove the recommended path
     actually completes -- skip if it expands the test runtime
     materially; the unit test plus the stderr assertions above are
     enough.

Test preamble per `AGENTS.md`:
- **Intent**: `braid remove-missing` on a 2-disk RAID1 + 1 missing
  rejects before journaling or inhibiting suspend, and names the
  supported repair path.
- **Why it exists**: Without the preflight, the kernel rejects the
  underlying `btrfs device remove` after braid has stranded
  `pending-op.json` and forced the operator into recovery mode for an
  impossible op.
- **Scenario**: Operator's 2-disk NAS loses disk2. They reach for
  `remove-missing` (a reasonable instinct). braid steers them to
  `replace --missing-id`.

Register the test in `flake.nix` `checks.aarch64-darwin` (or wherever
the existing `remove-missing-*` tests are listed -- match the
neighbouring entry's shape; do not improvise).

## Change 4 -- Doc updates

### `docs/decisions/012-intent-cli.md`

Replace the existing single-survivor `remove-missing` bullet in the
"ENOSPC pre-flight check" section with prose grounded only in the
kernel constraint and the supported repair paths. Do not justify the
new behavior with claims about chunk distribution or "the survivor has
all data" -- degraded writes create single-profile chunks with no
mirror copy at all (proven in `tests/repro/degraded-soft-balance.py:66-72`),
so any chunk-distribution rationale is fragile and not what this plan
actually relies on.

The replacement bullet should say only:

- `braid remove-missing` rejects at preflight when
  `pool.total_devices == 2 && pool.devices.len() == 1 && pool.missing_count == 1`,
  because `btrfs_rm_device` runs `btrfs_check_raid_min_devices(num_devices - 1)`
  and returns `BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET` whenever the
  remaining device count would drop below the RAID1 minimum of 2;
- the supported repair paths for that case are
  `braid replace --missing-id <devid>` (preferred) or `braid add`
  followed by `braid remove-missing`.

Do not add an "applies to 3+ total devices" or other generalized
applicability rule -- the only state this plan changes is the exact
2-total-device case above. All other states keep whatever behavior the
existing code has today; documenting them here would overreach.

Keep the `remove (2->1)` bullet as-is -- that path remains correct
because it pre-balances `RAID1->single` before `btrfs device remove`
(see `cli/src/remove.rs:131-140`), which clears `avail_*_alloc_bits`
for RAID1 and lets the kernel min-devices check pass.

### `manual/commands/remove-missing.md`

Add a single new bullet to the existing "Safety checks / refusal
cases" list, scoped to exactly the case the code now rejects:

- *Refuses on a 2-disk RAID1 pool with one disk missing -- the kernel
  refuses to drop a RAID1 pool below two devices. Use
  `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...> --missing-id <devid>`
  to repair the dead disk, or `braid add` first and then re-run.*

Do not add a generalized "applies to 3+ device pools" or
"`remove-missing` only applies once the pool has N devices" rule to
the intro or "When to use it" -- the code change in this plan is
scoped to the exact 2-total-device case, and a wider rule in the
manual would overstate what the plan actually proves.

## Verification

- `just test-rust` -- runs the rewritten unit test; must pass.
- `cargo build` (via `just test-rust` or directly) -- must compile.
- `just test-vm remove-missing-2disk-rejected` -- the new VM test
  must pass.
- `just test-vm remove-missing-inhibits-suspend add-returned-disk-after-remove-missing` --
  the existing 3-disk `remove-missing` paths must still pass
  unchanged (the new reject is gated on
  `pool.total_devices == 2 && pool.devices.len() == 1 && pool.missing_count == 1`,
  which is false for those scenarios where the pool starts at
  `total_devices == 3`).
- `just test-repro degraded-soft-balance` -- still passes; this test
  bypasses `braid` entirely and drives `btrfs` directly, so it is
  immune to the new CLI guard.
- Spot-check by hand: `cargo run -- remove-missing --missing-id <devid> --dry-run`
  on a fixture with `total_devices == 2`, 1 present + 1 missing should
  print the validation error and exit non-zero, with no `pending-op.json`
  written in the state dir. (The dry-run unit test pins this in CI; the
  spot-check is for confidence on a real machine.)
