# Plan: pivot -- VM test for Add::PoolMutation mixed-batch recovery

## Context

A `/ultrareview` finding flagged that no NixOS VM test pins the multi-disk
partial-add cleanup contract end-to-end across `braid recover`. Today the
control flow inside `execute_add_pool_mutation_recovery`
(`cli/src/recover.rs:2389`) is well-covered by Rust unit tests with a mock
runner -- in particular `live_add_recovery_drops_ghosts_for_mixed_batch`
(`cli/src/recover.rs:6060`) pins that recovery replays `BtrfsDeviceAdd` only
for the missing target when one target is already live. So the cited
regression -- dropping the `live_member_uuids` skip at
`cli/src/recover.rs:2410` / `:2497` -- is already caught at the unit level.

The genuine residual gap is the *integration* shape: no VM test exercises
this recovery path against real btrfs, real LUKS, real `pool_add_device`,
the post-mutation sweep at `sweep_recovered_add_acked_stats`
(`cli/src/recover.rs:2700`), pool.json rebuild, and the soft-balance
handoff. The closest existing tests are:

- `tests/cli/braid-add-persists-before-balance.py` -- happy single-disk
  add, non-failing path.
- `tests/cli/recover-bootstrap-crash.py` -- Add::PoolMutation, 1 target,
  escape-instructions path (no replay).
- `tests/cli/recover-replace-completed.py` -- Replace::PoolMutation,
  pool.json rebuild from live state, not Add.

The finding's proposed mechanism (prime `btrfs device add` to fail via
udev or reservation) is fragile and racy. The repo's established pattern
for testing recover paths is **journal injection plus hand-prepared live
state** (`recover-bootstrap-crash.py`, `recover-replace-completed.py`).
This plan pivots to that pattern.

## Scope (chosen)

Mixed-batch happy path + ack hygiene, single test. Pins:

1. The replay-loop `live_member_uuids` skip at
   `cli/src/recover.rs:2497` is honoured end-to-end -- disk2 is
   already in btrfs, recovery does not re-format or re-add it.
   (The earlier pre-scan skip at `:2410` is NOT meaningfully pinned
   here: with disk2 already open as `braid-disk2`, removing it only
   adds idempotent re-probe/re-scan work; that path stays covered at
   the unit level by `live_add_recovery_drops_ghost_under_drifted_mapper_*`
   at `cli/src/recover.rs:5882` and `:6000`.)
2. The missing target is fully replayed -- disk3 is LUKS-formatted,
   opened, added to btrfs.
3. Final ack hygiene -- `sweep_recovered_add_acked_stats` removes ack
   entries for **both** target devids (disk2's actual devid and the
   devid btrfs assigns to disk3 during replay) while leaving an
   unrelated control entry untouched.
4. pool.json is rebuilt with all three members.
5. The journal is cleared and the post-add soft-balance replay runs.
6. Data written before the crash window is intact across a subsequent
   lock/unlock cycle.

Out of scope: UUID-mismatch rejection, dry-run preview shape (already
covered by `tests/cli/braid-recover.py`), the bootstrap-crash variant,
re-failure during recover.

## Files to create

- `tests/cli/recover-add-mixed-batch.nix` -- NixOS test config, 3 virtual
  disks at 1024 MiB each. Mirror `tests/cli/recover-replace-completed.nix`
  (drop the 4th disk).
- `tests/cli/recover-add-mixed-batch.py` -- test script (see Test design
  below).

## Files to modify

- `flake.nix` -- register the new test in the `checksFor` block at
  approximately line 497-521, next to the other `recover-*` entries.
  Pattern to copy verbatim:

  ```nix
  recover-add-mixed-batch = pkgs.testers.nixosTest (
    import ./tests/cli/recover-add-mixed-batch.nix {
      braid = linuxCrane.braid;
    }
  );
  ```

## Test design

### Preamble (per `docs/testing.md`)

Three-section comment at the top of the `.py` file:

- **Intent:** `braid recover` on an Add::PoolMutation journal with one
  target already in btrfs (mid-loop crash after first per-target commit)
  must skip the live target in the replay loop, replay only the missing
  target, sweep ack entries for both target devids while leaving
  unrelated entries alone, rebuild pool.json, and run the post-add
  balance -- end-to-end against real btrfs + LUKS.
- **Why it exists:** the control flow is unit-tested
  (`live_add_recovery_drops_ghosts_for_mixed_batch`), but the integration
  with live btrfs membership, real LUKS, the sweep at
  `sweep_recovered_add_acked_stats`, pool.json rebuild, and balance
  handoff is not. A regression in any of those layers would slip past
  unit tests while breaking the recovery contract.
- **Scenario:** 1-disk pool with disk1. Operator runs
  `braid add disk2 disk3`; the loop commits disk2 to btrfs, then crashes
  before disk3's pool_add_device (so pool.json still says {disk1}, the
  journal carries targets={disk2,disk3}, and disk3 is untouched raw).
  On reboot, `braid recover` must finish the work.

### State setup

1. `braid add disk1` (normal bootstrap, pool mounted, pool.json={disk1}).
2. Write a sentinel payload (`echo recover-add-mixed-batch-data > /mnt/storage/testfile.txt; sync`).
3. Capture pool.json post-disk1 (we'll use it for the journal's
   `pre_membership`).
4. Manually pre-arrange disk2's "committed-but-not-bookkept" state.
   **Critical:** every `cryptsetup ... --key-file=-` invocation MUST
   pipe the passphrase via `printf '%s' {pq}` (no trailing newline) --
   cryptsetup reads the entire stdin including any newline as the
   key, while `braid recover --passphrase-stdin` strips the trailing
   newline. A mismatched seed-time vs recover-time key will make
   recovery fail at the disk2 passphrase verification for the wrong
   reason. Canonical pattern with the explanatory comment:
   `tests/cli/replace-new-already-luks.py:83-87`; same form also at
   `tests/cli/recover-bootstrap-crash.py:35-39`,
   `tests/cli/luks-mapper-drift.py:51-55`,
   `tests/cli/enroll-uuid-mismatch.py:98`.

   - `printf '%s' {pq} | cryptsetup luksFormat --batch-mode
     {luks_opts} --uuid <disk2_uuid> --label braid-disk2 --key-file=-
     /dev/disk/by-id/virtio-disk2`
     (use the same `--pbkdf pbkdf2 --pbkdf-force-iterations 1000`
     opts as other tests via `luks_opts`).
   - `printf '%s' {pq} | cryptsetup open --key-file=-
     /dev/disk/by-id/virtio-disk2 braid-disk2`.
   - `btrfs device add /dev/mapper/braid-disk2 /mnt/storage` (real --
     btrfs now knows about disk2; pool.json does not).
   - Capture disk2's devid from `btrfs filesystem show /mnt/storage`
     -- needed for both the ack seed and the post-recover assertion.
   - Capture disk2's LUKS UUID via `cryptsetup luksUUID
     /dev/disk/by-id/virtio-disk2` -- needed to assert recovery did
     not re-format the disk.
5. Generate a fresh, distinct UUID for disk3 (the journal's planned
   identity for the would-be FreshLuks format). Disk3's raw device is
   left untouched.
6. Seed `/var/lib/braid/acked-stats.json` with three entries so the
   sweep contract is fully observable:
   - **disk2's actual devid** (captured in step 4): a stale entry
     keyed at the live target's devid. The sweep must drop this.
   - **disk3's about-to-be-assigned devid**: compute
     `disk3_expected_devid = max(live_devids) + 1` by parsing every
     `devid N` line out of `btrfs filesystem show /mnt/storage`
     immediately after the manual disk2 add in step 4 -- the btrfs
     kernel allocates the next devid as previous-max + 1 (see
     `reference/linux/fs/btrfs/volumes.c:1895-1903`,
     `*devid_ret = found_key.offset + 1`). Bind that value to a
     Python variable (`disk3_expected_devid`) and seed the ack entry
     at exactly that key -- do NOT hard-code "devid 3". The sweep
     must drop this once disk3 is replayed.
   - **An unrelated control entry** at a devid that is neither a
     target nor a live member (e.g. devid 99): the sweep must NOT
     touch this.

   Use the existing wire shape produced by `alert::save_acked_stats`
   -- read `cli/src/alert.rs` once to confirm the on-disk JSON schema
   and the `acked-stats.json` map-key form (devid as string key) before
   authoring the literal JSON. The two-target+control pattern mirrors
   the Rust unit test `live_add_recovery_drops_ghosts_for_mixed_batch`
   at `cli/src/recover.rs:6069-6077` (which seeds devids 1, 4, 5 with
   devid 1 as the surviving control).
7. Inject `/var/lib/braid/pending-op.json` with the journal:

   ```python
   journal = {
       "started_at": "2026-01-01T00:00:00Z",
       "op": {
           "op": "Add",
           "phase": "PoolMutation",
           "targets": {
               disk2_uuid: {
                   "name": "disk2",
                   "by_id": "/dev/disk/by-id/virtio-disk2",
                   "mode": {"FreshLuks": {
                       "extra_opts": ["--pbkdf", "pbkdf2",
                                      "--pbkdf-force-iterations", "1000"],
                       "enroll_key_file": None,
                   }},
               },
               disk3_uuid: {
                   "name": "disk3",
                   "by_id": "/dev/disk/by-id/virtio-disk3",
                   "mode": {"FreshLuks": {
                       "extra_opts": ["--pbkdf", "pbkdf2",
                                      "--pbkdf-force-iterations", "1000"],
                       "enroll_key_file": None,
                   }},
               },
           },
       },
       "pre_membership": pool_json_after_disk1,   # rich form, devid+added_at
       "target_membership": {                     # lean form is fine
           "disks": {
               disk1_uuid: {"name": "disk1", "by_id": "/dev/disk/by-id/virtio-disk1"},
               disk2_uuid: {"name": "disk2", "by_id": "/dev/disk/by-id/virtio-disk2"},
               disk3_uuid: {"name": "disk3", "by_id": "/dev/disk/by-id/virtio-disk3"},
           },
       },
   }
   ```

   Schema reference: `cli/src/journal.rs:16-196` (Journal, OpKind::Add,
   AddJournalTarget, AddJournalMode::FreshLuks).

   Note: pool is left mounted at recover time so `plan_open_pool`
   short-circuits via AlreadyMounted -- mirrors the live-state
   precondition the unit test `live_add_recovery_drops_ghosts_for_mixed_batch`
   uses (`pool_state_disk1_and_disk2_devid4`).

### Recover invocation

```python
machine.succeed(
    f"printf '%s\\n' {pq} | braid recover --passphrase-stdin "
    f">/tmp/recover.out 2>/tmp/recover.err"
)
```

### Assertions

1. **Disk2 untouched on disk:** re-read `cryptsetup luksUUID
   /dev/disk/by-id/virtio-disk2` and assert it equals the pre-recover
   value (recovery did NOT re-format disk2).
2. **Disk2 still in btrfs at same devid:** `btrfs filesystem show
   /mnt/storage` lists braid-disk2 with the same devid captured in
   step 4 (recovery did NOT re-add disk2).
3. **Disk3 now formatted:** `cryptsetup isLuks
   /dev/disk/by-id/virtio-disk3` succeeds; its `luksUUID` equals
   `disk3_uuid` from the journal.
4. **Disk3 in btrfs:** `btrfs filesystem show /mnt/storage` lists
   `/dev/mapper/braid-disk3`.
5. **pool.json rebuilt:** `/var/lib/braid/pool.json` contains
   {disk1, disk2, disk3} with correct by_id paths and devids.
6. **Ack hygiene end-to-end:** resolve disk2's actual devid
   (unchanged from step 4) and disk3's actual devid (from
   `btrfs filesystem show /mnt/storage` or the rebuilt `pool.json`)
   after recovery, then:
   - **First** assert `disk3_actual_devid == disk3_expected_devid`
     (the value computed and seeded in setup step 6). If btrfs ever
     allocates a different devid than previous-max+1, the seed for
     disk3's row would have landed on the wrong key and the ack
     check below would silently lose disk3 coverage -- this guard
     makes the seed-time premise an explicit invariant.
   - Assert against `/var/lib/braid/acked-stats.json`:
     - The disk2 devid entry is absent (per-target sweep covered disk2).
     - The `disk3_expected_devid` entry is absent (per-target sweep
       covered disk3).
     - The unrelated control entry (devid 99) is still present
       (sweep is precise -- only the journaled targets' devids are
       dropped).
   Pins the `sweep_recovered_add_acked_stats` integration end-to-end.
7. **Journal cleared:** `/var/lib/braid/pending-op.json` is absent.
8. **Balance replay ran:** stderr contains the substring
   `replaying post-add RAID1 soft balance` and the matching
   `[ok]   pool: RAID1 soft balance replay complete` row (same
   strings asserted by `tests/cli/braid-recover.py:313-320`).
9. **Pool remained mounted throughout** -- assert
   `mountpoint -q /mnt/storage` succeeds. The recover-remount cycle
   string `recover remount cycle` must NOT appear in stderr (this
   string belongs to replace recovery only; same negative assertion
   as `braid-recover.py:308-310`).
10. **Data intact:** `cat /mnt/storage/testfile.txt` equals the
    pre-crash sentinel.
11. **Normal ops resume:** `braid lock`, `braid unlock`, re-verify
    the sentinel. Mirrors the closing phase of
    `recover-replace-completed.py:215-227`.

### Wire patterns to reuse verbatim

- Journal injection here-doc:
  `recover-bootstrap-crash.py:77-81` / `recover-replace-completed.py:156-160`.
- LUKS UUID capture pattern: `recover-replace-completed.py:91-93`.
- `add_cmd` helper with pinned LUKS opts: any of the existing tests
  (e.g. `braid-recover.py:28-32`).
- Manual `cryptsetup luksFormat` + `open` (no-newline passphrase
  via `printf '%s' {pq} | cryptsetup ... --key-file=-`):
  `tests/cli/replace-new-already-luks.py:83-87` (the canonical pattern
  with the explanatory "braid strips the trailing newline" comment).
  Same form also at `tests/cli/recover-bootstrap-crash.py:35-39`,
  `tests/cli/luks-mapper-drift.py:51-55`, and
  `tests/cli/enroll-uuid-mismatch.py:98`. Do NOT copy the
  newline-emitting `printf '%s\\n'` shape used elsewhere for `braid`
  CLI passphrase entry -- cryptsetup with `--key-file=-` consumes
  the newline as part of the key, breaking the seed/recover key
  match.
- `btrfs device add` against a live pool: standard btrfs CLI form,
  no special wrapper needed.
- Balance replay assertions: `braid-recover.py:313-320`.

### Nix config

Copy `tests/cli/recover-replace-completed.nix` and reduce the
`emptyDiskImages` list from 4 entries to 3 (disk1, disk2, disk3),
all 1024 MiB. Replace the `name` and `testScript` filename. No other
changes -- same `environment.systemPackages` (`braid`, `cryptsetup`,
`btrfs-progs`) and same `environment.etc."braid/config.json"` with
`mount_point = "/mnt/storage"`.

## Verification

Run locally before commit:

1. `just test-vm recover-add-mixed-batch` -- the new test alone.
   Should pass on aarch64-darwin via the Linux builder.
2. `just test-vm recover-add-mixed-batch -v` -- only if the
   non-verbose run fails; use the per-subtest log to diagnose.
3. `just test-rust` -- no Rust changes here, but the run confirms
   nothing in the working tree regressed.

Regression-coverage sanity check (manual, post-implementation): with
the test passing, temporarily comment out the `live_member_uuids`
skip at `cli/src/recover.rs:2497` (the replay loop) and re-run the
new test. It must fail -- most likely at `pool_add_device` for disk2
(btrfs rejects re-adding a live member), or on the
disk2-LUKS-UUID-unchanged assertion if btrfs's re-add somehow
succeeds. Restore the skip before committing. The pre-scan skip at
`:2410` is intentionally NOT part of this check -- removing it only
adds idempotent probe/scan calls under this test's preconditions; its
regression coverage lives in the unit-level drifted-mapper tests.

## Critical files referenced

- `cli/src/recover.rs:2389` -- `execute_add_pool_mutation_recovery`
  (the integration target).
- `cli/src/recover.rs:2497` -- the replay-loop `live_member_uuids`
  skip this VM test pins end-to-end (regression removes it -> btrfs
  rejects re-adding a live member in the FreshLuks branch).
- `cli/src/recover.rs:2410` -- the pre-scan `live_member_uuids` skip
  is NOT meaningfully pinned by this VM test (disk2 already-open
  reduces its removal to idempotent re-probe/re-scan work); covered
  at the unit level by `live_add_recovery_drops_ghost_under_drifted_mapper_*`
  at `cli/src/recover.rs:5882` and `:6000`.
- `cli/src/recover.rs:2700` -- `sweep_recovered_add_acked_stats`
  (covered by assertion 6, including the precision contract).
- `cli/src/recover.rs:6060` -- `live_add_recovery_drops_ghosts_for_mixed_batch`
  (the unit-level analog; the new VM test is its integration twin).
- `cli/src/journal.rs:16-196` -- Journal / OpKind::Add /
  AddJournalTarget / AddJournalMode schema.
- `cli/src/alert.rs` -- read once to confirm `acked-stats.json` on-disk
  shape before seeding the ghosts.
- `tests/cli/replace-new-already-luks.py:83-87` -- canonical
  `printf '%s' | cryptsetup ... --key-file=-` no-newline passphrase
  pattern with explanatory comment.
- `tests/cli/recover-bootstrap-crash.{nix,py}` -- closest .nix template
  and journal-injection prior art.
- `tests/cli/recover-replace-completed.{nix,py}` -- multi-disk .nix
  template and full lock/unlock/verify-data closing phase to mirror.
- `tests/cli/braid-recover.py:313-320` -- soft-balance replay string
  assertions to copy.
- `flake.nix:497-521` -- registration block.
