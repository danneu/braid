# Plan: Replace size-based timing payloads with dm-delay

## Context

`just test-vm` currently takes 40 minutes on an M1 Mac. A material slice of
the CPU/IO inside each VM is spent writing, syncing, and checksumming
multi-hundred-MiB to multi-GiB random payloads whose only purpose is to make
a `btrfs replace`/`balance`/`scrub` slow enough that a test can observe an
intermediate state. Per the existing speed-up todo and findings docs
(`todo/2026-05-21-speed-up-vm-tests.md`, `findings-speed-up-tests.md`), this
is Tier A #4 -- the biggest per-test cost left to attack.

Across the affected tests, full runs write **~16 GiB of timing-only data**
that does not contribute to any correctness assertion beyond "the op was
in-flight long enough to observe".

The intended outcome: replace size-as-timer with **dm-delay**, which
deterministically slows IO by injecting per-bio latency on a chosen subset of
backing devices, so the same window-of-observability is available with tens
of MiB of payload. dm-delay is already in production use in two braid VM
tests (`tests/progress-monitoring.py`, `tests/module/scrub-lifecycle.py`),
so this is a pattern-extension, not a greenfield introduction.

## Why dm-delay, not scsi_debug

- **dm-delay** is per-device: wraps an existing virtio backing disk, slows
  only that one, runtime-tunable via `dmsetup suspend/reload/resume`.
- **scsi_debug** has a _global_ `delay`/`ndelay` knob across every
  scsi_debug LU on the system (`sdebug_jdelay`/`sdebug_ndelay` are file-scope
  statics in `drivers/scsi/scsi_debug.c`). It cannot asymmetrically slow one
  disk in a RAID1, which is the exact shape needed for `btrfs replace`.

For every test in this plan, dm-delay is the right tool. Keep `scsi_debug`
for the one test (`tests/cli/monitor-hot-unplug.py`) where the requirement
is "fabricate a brand-new fake disk not in the VM config".

## Critical gotchas (must respect in every conversion)

1. **Use the 9-arg dm-delay table form with `flush_delay=0`.** The 3-arg and
   6-arg forms delay `REQ_PREFLUSH` at the same rate as reads/writes. btrfs
   commits every 30 s with a flush; a 500 ms delay on flushes can add
   minutes of wall-clock to longer tests. Both existing braid users use 3-
   and 6-arg forms today; the shared helper introduced here will default to
   9-arg with `flush_delay_ms=0`.
2. **HZ=250 means 4 ms granularity.** `msecs_to_jiffies()` rounds up; on the
   NixOS default `delay=1` actually fires after ~4 ms. Use delays >= 50 ms
   for stable behavior (this also crosses dm-delay's internal
   kthread-vs-workqueue threshold).
3. **Wrap disks FIRST, then route every disk-targeting command through the
   `braid-test-*-delay` symlink. Delay starts at 0 and is reloaded only
   for the slow phase.** dm-delay slows IO only when the _upper layer's_
   bios actually pass through the dm-delay mapper. If `cryptsetup
luksFormat` runs against `/dev/disk/by-id/virtio-diskN` first, the LUKS
   table targets the raw device; a later `dm_delay_create` wrapping the
   same raw device builds a sibling mapper that the already-open LUKS
   device does not see. Subsequent btrfs IO then bypasses dm-delay
   entirely and the test slows nothing. The existing precedent in
   `tests/module/scrub-lifecycle.py:91-108` does the right thing: it
   `dm_delay_create`s with delay=0 (plus the `braid-test-*-delay` by-id
   symlink) **before** any `cryptsetup luksFormat`/`open`, then runs
   format/open against the symlink, then writes `pool.json` recording the
   symlink as the disk's `by_id`. Every braid CLI invocation
   (`braid init`/`add`/`replace`/`unlock`) and every direct `cryptsetup`
   call for a wrapped disk must use `/dev/disk/by-id/braid-test-*-delay`,
   never the raw `virtio-*` path. Setup runs at delay=0 (fast), and the
   `dmsetup suspend/reload/resume` cycle injects the real delay only
   right before the slow phase.
4. **dm-delay is transparent for LUKS bytes but NOT for braid's persisted
   by-id paths.** LUKS bytes pass through to the backing virtio device
   (because we set up with delay=0), so the LUKS UUID survives a reboot on
   `/dev/disk/by-id/virtio-diskN`. However, when braid operations run
   against `/dev/disk/by-id/braid-test-diskN-delay` during the slow phase,
   braid records that symlink path in `pool.json` (see
   `cli/src/discover.rs:666` for the serialized `by_id` field, and
   `tests/module/scrub-lifecycle.py:117` for the existing precedent of
   recording the `braid-test-*-delay` symlink). After reboot, dm-delay
   mappers and their symlinks are gone -- discovery will hit a dangling
   path before it ever falls back to the LUKS UUID. **Wave-3 tests must
   recreate dm-delay mappers (with delay=0) and `braid-test-*-delay`
   symlinks after reboot before running `braid recover`.** The delay can
   stay at 0; recovery itself does not need to be slowed.
5. **dm-delay adds per-bio latency, not throughput cap.** Large sequential
   IO coalesced into few bios is barely slowed; many small bios get heavily
   penalised. btrfs replace/balance/scrub all issue many small bios -- this
   is exactly the case where dm-delay shines.

## Shared helper module

Create `tests/module/dm_delay_helpers.py`. Convention follows
`tests/cli/inhibitor_helpers.py`:

- Concatenated into the test script at Nix-eval time via `builtins.readFile`
  in the companion `.nix` (it is not Python-importable from a VM-test
  driver).
- Functions reference a passed-in `node` argument (matches
  `scrub-lifecycle.py`, not `progress-monitoring.py`).
- Default to the 9-arg form with `flush_delay_ms=0`.

API:

```python
def dm_delay_table(node, name, *, read_delay_ms=0, write_delay_ms=0,
                   flush_delay_ms=0): ...
def dm_delay_create(node, name, *, by_id_symlink=True): ...
def dm_delay_activate(node, names, *, read_delay_ms=0, write_delay_ms=0,
                      flush_delay_ms=0): ...
def dm_delay_deactivate(node, names): ...
def dm_delay_remove(node, names): ...
```

`name` corresponds to the virtio serial (`disk1`, `disk2`, ...), so each
helper resolves to `/dev/disk/by-id/virtio-{name}` and creates a mapper at
`{name}-delay`. `by_id_symlink=True` creates the `braid-test-{name}-delay`
by-id symlink already used by `scrub-lifecycle.py`.

After this helper exists, **migrate `progress-monitoring.py` and
`scrub-lifecycle.py` to use it** so we have one canonical implementation.
Both already pass; migration is a near-mechanical refactor and proves the
helper before any new test depends on it.

## Conversion target list

The Explore pass identified these candidates. Reboot-bearing tests are
hardest; converted last.

**Delay direction rule.** btrfs `balance`/`add`/`remove`/`replace` are all
relocation operations: they read data from a source set and write it into a
destination set. The progress of these operations is dominated by writes
to the destination. Use **`write_delay_ms` on the destination device(s)**;
do not bother with read_delay for these ops. `scrub`-style read workloads
(present only in `scrub-lifecycle.py` and not converted in this plan) keep
their existing `read_delay_ms`.

**When the destination set is not deterministic, wrap every plausible
destination.** Some btrfs operations do not filter writes to a specific
device. The pre-remove single-profile balance (`btrfs balance start
-dconvert=single -mconvert=dup -f`, emitted by `cli/src/remove.rs:191-194`
via `BtrfsBalanceSingle` at `cli/src/cmd.rs:669-675`) ships no `devid=`
filter, so btrfs picks the target devices itself; delaying only one disk
may not reliably widen the window. In these cases wrap _every_
participating disk and activate `write_delay_ms` on all of them.

### Wave 1 -- inhibitor tests (no reboot, simplest)

| Test                                       | Today                        | After                                                                                                                                                                                                                                       |
| ------------------------------------------ | ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tests/cli/add-inhibits-suspend.py:84`     | 400 MiB + balance            | 16 MiB + dm-delay 200 ms write on `disk2` (the new disk, destination of the post-add RAID1 conversion)                                                                                                                                      |
| `tests/cli/remove-inhibits-suspend.py:92`  | 400 MiB + pre-remove balance | 16 MiB + dm-delay 200 ms write on **both** `disk1` and `disk2` during the pre-remove single-profile balance (the `-dconvert=single -mconvert=dup -f` step ships no devid filter, so btrfs picks targets -- wrap all plausible destinations) |
| `tests/cli/replace-inhibits-suspend.py:81` | 400 MiB + replace            | 16 MiB + dm-delay 200 ms write on the replace target (the `--new` disk)                                                                                                                                                                     |

Out of scope: `tests/cli/remove-missing-inhibits-suspend.py` (already only
20 MiB; its real issue is polling-vs-window race, not payload size).

### Wave 2 -- paused-balance setups (no reboot)

| Test                                          | Today                                    | After                                                                                                                 |
| --------------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `tests/cli/braid-status-during-balance.py:36` | 512 MiB + balance + pause race           | 32 MiB + dm-delay 500 ms write on both disks (RAID1 conversion writes to both); pause-race window widens dramatically |
| `tests/cli/braid-unlock.py:553`               | 512 MiB + paused balance survives unlock | Same pattern                                                                                                          |

### Wave 3 -- UPS LB matrix (reboot + recovery)

| Test                                                   | Today                                      | After                                                                                                                                                    |
| ------------------------------------------------------ | ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `tests/module/ups-lb-during-replace.py:80`             | 3000 MiB + replace + LB shutdown + recover | 100 MiB + dm-delay 500 ms write on replace target during the in-flight window; recreate dm-delay+symlinks at delay=0 after reboot before `braid recover` |
| `tests/module/ups-lb-during-balanced-add.py:72`        | 3000 MiB + add+balance                     | 100 MiB + dm-delay 500 ms write on the newly added disk; same post-reboot recreate                                                                       |
| `tests/module/ups-lb-during-remove.py:79`              | 3000 MiB + remove                          | 100 MiB + dm-delay 500 ms write on the remaining disk(s) where data evacuates to; same post-reboot recreate                                              |
| `tests/module/ups-lb-during-remove-missing.py:127,159` | 512 + 3000 MiB                             | 100 MiB + dm-delay 500 ms write on the remaining disks during the post-remove-missing soft balance; same post-reboot recreate                            |

For each wave-3 test, the payload checksum is still verified after recovery.
100 MiB is plenty to catch any data-loss bug a 3 GiB payload would. The
post-reboot dm-delay reconstruction is critical (see Gotcha #4): braid's
`pool.json` records the `braid-test-diskN-delay` symlink path that the slow
phase used, so the symlinks must exist again before `braid recover` opens
LUKS for discovery.

### Already on the pattern (refactor only, do not change behavior)

- `tests/progress-monitoring.py` -- migrate to shared helper, 9-arg form.
- `tests/module/scrub-lifecycle.py` -- migrate to shared helper, 9-arg form.

## Conversion recipe (used in every wave)

For each target test:

1. Add `pkgs.lvm2` to the `.nix` machine config if it is not already
   present (it provides `dmsetup`).
2. In the `.nix`, include the helper file via `builtins.readFile` of
   `dm_delay_helpers.py`, concatenated ahead of the test script (same
   idiom as `inhibitor_helpers.py`). The relative path depends on where
   the `.nix` lives:
   - From `tests/<name>.nix` (root-level tests, e.g.
     `tests/progress-monitoring.nix`): `./module/dm_delay_helpers.py`.
   - From `tests/cli/<name>.nix` (all Wave 1 and Wave 2 tests):
     `./../module/dm_delay_helpers.py`.
   - From `tests/module/<name>.nix` (Wave 3 tests and `scrub-lifecycle`):
     `./dm_delay_helpers.py`.
3. In the test script, restructure setup so dm-delay wraps the disk
   **before** LUKS or braid ever touches it (see Gotcha #3 for why).
   For each disk that will need to be slowed at any point in the test:
   - 3a. _Before_ any `cryptsetup` / `braid init` / `braid add` /
     `braid replace` invocation that targets that disk, call
     `dm_delay_create(machine, "diskN")` (delay=0, `by_id_symlink=True`).
     Order matters: this must happen before LUKS sees the raw device.
   - 3b. Rewrite every braid CLI invocation and every direct cryptsetup
     call that previously named `/dev/disk/by-id/virtio-diskN` for this
     disk to instead name `/dev/disk/by-id/braid-test-diskN-delay`.
     Disks not being slowed continue to use the raw `virtio-*` path;
     the test mixes both schemes as needed (this matches
     `scrub-lifecycle.py`, which wraps disk1 and disk2 but not disk3).
   - 3b-i. **Hand-seeded state must use the same symlink path.** If the
     test writes `pool.json` or `pending-op.json` directly (e.g.
     `tests/module/ups-lb-during-remove-missing.py:188-217` seeds
     `pool.json` by hand, and `tests/module/scrub-lifecycle.py:113-120`
     does the same via `setup_resume_pool`), the `by_id` field for
     every wrapped disk must be
     `/dev/disk/by-id/braid-test-diskN-delay`, not
     `/dev/disk/by-id/virtio-diskN`. Otherwise braid's mapper-open
     backing-path check (`cli/src/replace.rs:99` -- "open mapper
     backing-path check failed") fires when it sees an active mapper
     backed by `/dev/mapper/diskN-delay` while `pool.json` records the
     raw virtio path.
   - 3c. Replace the inline `dd ... count=NNN` payload with a small payload
     (see per-wave sizes in the target list above). Payload write runs
     at native speed because delay is still 0.
   - 3d. Immediately before the slow phase, call
     `dm_delay_activate(machine, ["diskN", ...], write_delay_ms=M,
flush_delay_ms=0)`. For balance/add/remove/replace, list the
     destination disk(s) here and use `write_delay_ms` only (see the
     delay direction rule under "Conversion target list").
   - 3e. Run the operation as today; the existing in-flight polling still
     works because the per-bio window is now wider, not the payload.
   - 3f. After the assertion, `dm_delay_deactivate(machine, [...])` to
     reload delay back to 0. For wave-3 tests this is implicit via the
     forced shutdown.
4. Verify nothing in the test directly references `/dev/vdN`; all current
   targets use `/dev/disk/by-id/virtio-...`, which is unchanged for
   non-wrapped disks. For wrapped disks the test uses
   `/dev/disk/by-id/braid-test-diskN-delay` (which symlinks to
   `/dev/mapper/diskN-delay`). LUKS format and braid CLI both run against
   the symlink path.

**Wave-3 (reboot) tests have an extra step after `machine.start()` and
before `braid recover`**: reload the `dm-delay` kernel module and call
`dm_delay_create(machine, "diskN")` for each disk that was previously
wrapped. delay=0 is correct here; the goal is only to re-establish the
`/dev/mapper/diskN-delay` mapper and the `braid-test-diskN-delay` by-id
symlink that braid's persisted `pool.json` references. Without this, the
first thing `braid recover` does -- LUKS-open by stored `by_id` path --
hits a dangling symlink. See Gotcha #4.

## Step-by-step todo list

Each step verifies the touched test still passes and records before/after
wall-clock with `time just test-vm <file>`. Record results inline in this
plan as each step completes, then promote to `plans/impl/` on close.

Convention: run `time just test-vm <test>` (no `-v` unless the
non-verbose run fails to explain a failure). Record the `real` time.

### Phase 0 -- shared helper + migrate existing users

- [ ] **0a.** Create `tests/module/dm_delay_helpers.py` per the API above.
- [ ] **0b.** Refactor `tests/progress-monitoring.py` to import the shared
      helper. Drop the in-file `dm_delay_table`/`dm_delay_create`/
      `dm_delay_activate` defs. Use 9-arg form with `flush_delay_ms=0`. - Baseline: `time just test-vm progress-monitoring` - After: `time just test-vm progress-monitoring`
- [ ] **0c.** Refactor `tests/module/scrub-lifecycle.py` likewise. - Baseline: `time just test-vm scrub-lifecycle` - After: `time just test-vm scrub-lifecycle`

Phase 0 changes **must not change wall-clock meaningfully**; both tests
already use dm-delay. Any regression here is a helper-API bug.

### Phase 1 -- inhibitor tests (no reboot)

- [ ] **1a.** Baseline timings: - `time just test-vm add-inhibits-suspend` - `time just test-vm remove-inhibits-suspend` - `time just test-vm replace-inhibits-suspend`
- [ ] **1b.** Convert `tests/cli/add-inhibits-suspend.py`: 400 MiB -> 16 MiB,
      dm-delay `write_delay_ms=200, flush_delay_ms=0` on `disk2` (the new
      disk, destination of the post-add RAID1 conversion). - `time just test-vm add-inhibits-suspend` (must pass; record delta)
- [ ] **1c.** Convert `tests/cli/remove-inhibits-suspend.py`: wrap **both**
      `disk1` and `disk2` with dm-delay (delay=0) at setup; activate
      `write_delay_ms=200, flush_delay_ms=0` on **both** during the
      pre-remove single-profile balance. Single-mode balance does not
      filter by devid (see `cli/src/cmd.rs:669-675`), so wrapping only
      the nominal destination is unreliable. - `time just test-vm remove-inhibits-suspend`
- [ ] **1d.** Convert `tests/cli/replace-inhibits-suspend.py`: dm-delay
      `write_delay_ms=200, flush_delay_ms=0` on the replace target (the
      `--new` disk). - `time just test-vm replace-inhibits-suspend`
- [ ] **1e.** Run the three together once to confirm no shared-state
      issues:
      `time just test-vm add-inhibits-suspend remove-inhibits-suspend replace-inhibits-suspend`

### Phase 2 -- paused-balance setups (no reboot)

- [ ] **2a.** Baseline timings: - `time just test-vm braid-status-during-balance` - `time just test-vm braid-unlock`
- [ ] **2b.** Convert `tests/cli/braid-status-during-balance.py`: 512 MiB ->
      32 MiB, dm-delay `write_delay_ms=500, flush_delay_ms=0` on both
      disks for the balance phase only (RAID1 conversion writes to both). - `time just test-vm braid-status-during-balance`
- [ ] **2c.** Convert the paused-balance block inside
      `tests/cli/braid-unlock.py` (around line 553) with the same
      write-delay-on-both-disks pattern. - `time just test-vm braid-unlock`

### Phase 3 -- UPS LB matrix (reboot + recovery; do these last)

- [ ] **3a.** Baseline timings: - `time just test-vm ups-lb-during-replace` - `time just test-vm ups-lb-during-balanced-add` - `time just test-vm ups-lb-during-remove` - `time just test-vm ups-lb-during-remove-missing`
- [ ] **3b.** Convert `tests/module/ups-lb-during-replace.py`: 3000 MiB ->
      100 MiB, dm-delay `write_delay_ms=500, flush_delay_ms=0` on the
      replace target during the in-flight window. After
      `machine.start()` post-reboot but **before** `braid recover`, add a
      `modprobe dm-delay` and `dm_delay_create(machine, "diskN")` (with
      `by_id_symlink=True`, delay=0) for each previously-wrapped disk so
      the persisted `braid-test-*-delay` paths resolve. - `time just test-vm ups-lb-during-replace` - Verify post-recover checksum still validates and the
      `wait_for_shutdown` race is still won (window is now driven by
      dm-delay, not size, so it should be more reliable).
- [ ] **3c.** Convert `tests/module/ups-lb-during-balanced-add.py`. Apply
      `write_delay_ms=500` to the newly added disk; same post-reboot
      dm-delay reconstruction as 3b. - `time just test-vm ups-lb-during-balanced-add`
- [ ] **3d.** Convert `tests/module/ups-lb-during-remove.py`. Apply
      `write_delay_ms=500` to the remaining disk(s) where data evacuates
      to; same post-reboot reconstruction. - `time just test-vm ups-lb-during-remove`
- [ ] **3e.** Convert `tests/module/ups-lb-during-remove-missing.py` (both
      payload sites at lines 127 and 159). Apply `write_delay_ms=500` to
      the remaining disks during the post-remove-missing soft balance;
      same post-reboot reconstruction. **Critical: this test seeds
      `pool.json` by hand at lines 191-217.** Update that JSON blob to
      record `/dev/disk/by-id/braid-test-diskN-delay` (not the raw
      `virtio-*`) for every wrapped disk -- otherwise the mapper-open
      backing-path check fails (see recipe step 3b-i). - `time just test-vm ups-lb-during-remove-missing`

### Phase 4 -- full-suite sanity + close

- [ ] **4a.** Full unscoped run: `time just test-vm`. Compare against the
      pre-change baseline (separate run captured before Phase 0a).
- [ ] **4b.** Record total wall-clock delta in this plan. Promote to
      `plans/impl/` with `/promote-plan`.

## Files to modify

- New: `tests/module/dm_delay_helpers.py`.
- Modified (Python test scripts): `tests/progress-monitoring.py`,
  `tests/module/scrub-lifecycle.py`, `tests/cli/add-inhibits-suspend.py`,
  `tests/cli/remove-inhibits-suspend.py`,
  `tests/cli/replace-inhibits-suspend.py`,
  `tests/cli/braid-status-during-balance.py`, `tests/cli/braid-unlock.py`,
  `tests/module/ups-lb-during-replace.py`,
  `tests/module/ups-lb-during-balanced-add.py`,
  `tests/module/ups-lb-during-remove.py`,
  `tests/module/ups-lb-during-remove-missing.py`.
- Modified (Nix companions): the matching `.nix` for each script above, to
  (a) include `pkgs.lvm2` in `environment.systemPackages` when missing,
  and (b) prepend `builtins.readFile` of `dm_delay_helpers.py` ahead of
  the test script (using the inhibitor-helper concatenation pattern).
  The relative path of the helper depends on where the `.nix` lives:
  `./module/dm_delay_helpers.py` from `tests/progress-monitoring.nix`,
  `./../module/dm_delay_helpers.py` from each `tests/cli/*.nix`,
  `./dm_delay_helpers.py` from each `tests/module/*.nix`.
  `progress-monitoring.nix` and `scrub-lifecycle.nix` already include
  `lvm2`; the other nine `.nix` files may or may not.

## Verification

Per-step: `time just test-vm <test>` records both correctness (exit code)
and timing (wall-clock).

End-to-end: after each phase, run the touched tests as a group to catch
shared-VM-host issues:

- Phase 1: `time just test-vm add-inhibits-suspend remove-inhibits-suspend replace-inhibits-suspend`
- Phase 2: `time just test-vm braid-status-during-balance braid-unlock`
- Phase 3: `time just test-vm ups-lb-during-replace ups-lb-during-balanced-add ups-lb-during-remove ups-lb-during-remove-missing`

Final: `time just test-vm` (full suite). The expectation is a meaningful
wall-clock reduction concentrated in these eleven tests; remaining suite
runtime is dominated by other Tier S/A items in
`todo/2026-05-21-speed-up-vm-tests.md` (linux-builder sizing,
`nix-fast-build`, fixture-check separation), which are out of scope here.

## Out of scope

- Any other speed-up item from
  `todo/2026-05-21-speed-up-vm-tests.md` (linux-builder sizing,
  `nix-fast-build` default, `useNixStoreImage`, fixture-check separation,
  Rust `cmd_lock_wrapper_uses_real_sleeper` sleep removal, udev-repro fixed
  sleeps). They are tracked separately and have their own risk/reward.
- `tests/cli/remove-missing-inhibits-suspend.py`. Payload is already 20
  MiB; its real issue is a polling-vs-window race, distinct from the
  size-as-timing problem.
- `tests/repro/kernel-journal-{write-error,bad-sector}.py` and
  `tests/cli/monitor-hot-unplug.py`. They use dm-flakey / dm-dust /
  scsi_debug for failure injection, not for timing; they are correctly
  modelled.

## References

- Existing dm-delay precedent: `tests/progress-monitoring.py:15-55`,
  `tests/module/scrub-lifecycle.py:50-87`.
- Helper concatenation idiom: `tests/cli/inhibitor_helpers.py` (loaded via
  `builtins.readFile` in companion `.nix`).
- Kernel doc: <https://docs.kernel.org/admin-guide/device-mapper/delay.html>
  and `reference/linux/drivers/md/dm-delay.c`.
- Existing analysis: `todo/2026-05-21-speed-up-vm-tests.md` (Tier A #4),
  `findings-speed-up-tests.md` (#2, #4, #5, #7).

## Implementation notes

Implementor #1's notes:

- `ups-lb-during-remove-missing`: a 100 MiB degraded write did not
  reliably create `Data, single` by itself because btrfs could reuse
  existing RAID1 chunk space. The implementation keeps the 100 MiB test
  payload but first creates a temporary 900 MiB RAID1 reservation, then
  removes it after the `Data, single` assertion.
- `ups-lb-during-remove`: delaying only destination writes was not enough
  with the smaller payload. The implementation also delays reads from the
  removed disk and uses a temporary 1100 MiB reservation so the removed
  disk owns live extents that must be relocated.
- The delay-direction rule above is still the default, but `remove` is the
  exception found during implementation: source-side read delay was needed
  to make the relocation window deterministic.
- `ups-lb-during-remove` now gates the LB trigger on the kernel
  `exclusive_operation=device remove` signal instead of waiting for the
  removed disk's btrfs usage to decrease. With the reduced payload, usage
  accounting did not decrease early enough to be a reliable in-flight
  signal.
- Baseline timings were captured before conversion only for
  `progress-monitoring` and `scrub-lifecycle`. The remaining timings from
  the implementation pass are post-change focused/group runs, so there is
  no clean pre-change full-suite baseline attached to this plan.
- Practical Nix gotcha: a new helper file must be tracked or staged before
  VM tests that read it through `builtins.readFile`; otherwise flake source
  evaluation omits the untracked file and fails before the VM boots.

Implementor #2's notes:

- Replaced the fixed `ups-lb-during-remove-missing` 900 MiB RAID1
  reservation with `ensure_degraded_data_single_chunks()`: after the 100
  MiB degraded payload write, the test now allocates bounded 64 MiB filler
  files only until `btrfs filesystem df` reports `Data, single`. The filler
  remains in place so those extents are part of the soft-balance work that
  `braid recover` must drain.
- Replaced the fixed `ups-lb-during-remove` 1100 MiB reservation with
  `ensure_disk3_relocation_work()`: before `braid remove disk3`, the test
  checks `/dev/mapper/braid-disk3` in `btrfs device usage --raw` and
  allocates bounded 64 MiB filler files only until disk3 owns `Data,RAID1`
  extents. dm-delay controls the timing window; the filler only proves the
  source device has real relocation work.
- Added the same source-work guard to `ups-lb-during-replace` via
  `ensure_replace_source_work("braid-disk2")`, so the reduced 100 MiB
  payload cannot accidentally leave the replace source without observable
  `Data,RAID1` work.
- Source review informed the shape but not the test contract:
  `btrfs_rm_device` relocates device extents via `btrfs_shrink_device`, and
  balance/replace operate on existing chunks. The tests now prepare and
  assert those observable kernel-reported states instead of encoding
  allocator-size guesses.
- Verification for the follow-up cleanup:
  `just test-vm ups-lb-during-remove-missing`,
  `just test-vm ups-lb-during-remove`,
  `just test-vm ups-lb-during-replace`, and
  `just test-vm ups-lb-during-replace ups-lb-during-remove ups-lb-during-remove-missing ups-lb-during-balanced-add`
  all passed.

Implementor #3's notes:

- Audited every `dm_delay_activate` call introduced by this migration and
  moved deactivation earlier in the non-UPS observability tests. The rule is:
  keep dm-delay active only until the test has captured the slow state it
  needs, then immediately restore the mapper to zero delay so the rest of
  the operation can complete at native VM speed.
- Updated `add-inhibits-suspend`, `remove-inhibits-suspend`, and
  `replace-inhibits-suspend` so they deactivate right after the relevant
  in-flight/inhibitor assertion, instead of waiting for the add/remove/
  replace command to finish.
- Updated `braid-status-during-balance` so it deactivates immediately after
  the balance has been paused with remaining work. The status assertions run
  against the paused balance and do not need slow I/O.
- Updated `progress-monitoring` so read delay is disabled immediately after
  capturing the running scrub fixtures, and write delay is disabled
  immediately after capturing the device-remove progress fixture. The test
  exits after fixture capture; it does not need to slow completion.
- Updated `scrub-lifecycle` to deactivate after the coalesced timer/resume
  scrub run completes. The earlier scrub-cancel setup already deactivated
  before offline setup work; this closes the remaining long-lived delay.
- Tightened `add-inhibits-suspend`'s balance observation loop while moving
  deactivation earlier: `exclusive_operation=none` can mean "not started
  yet", not only "already completed". The loop now treats the balance as
  completed only after `pending-op.json` has cleared.
- Intentionally did not add early deactivation to the UPS LB matrix tests.
  Those tests need the mutation to stay slow through `wait_for_shutdown()`
  so upsmon interrupts real in-flight work; disabling delay after the first
  observation would let the operation finish before the forced-shutdown
  path is exercised.
- Verification after the deactivation audit:
  `just test-vm add-inhibits-suspend remove-inhibits-suspend replace-inhibits-suspend braid-status-during-balance progress-monitoring scrub-lifecycle`
  passed.
- Timing reruns were measured by deleting only the relevant cached VM check
  outputs, then running focused `just test-vm` commands without `-rebuild`
  (because `-rebuild` runs the VM but fails the nondeterminism comparison
  for these tests). Results:
  `progress-monitoring` 40.48s, `scrub-lifecycle` 315.43s,
  add/remove/replace inhibitor trio 35.23s,
  `braid-status-during-balance` plus `braid-unlock` 42.44s,
  `ups-lb-during-remove-missing` 99.16s,
  `ups-lb-during-remove` 75.38s, and full UPS matrix 104.16s.
  The non-UPS groups are all faster than the Implementor #1 timings; the
  UPS matrix remains timing-sensitive by design because delay stays active
  until shutdown.
