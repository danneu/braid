# De-flake `braid-add-during-balance` via dm-delay

## Context

`tests/cli/braid-add-during-balance.py` transiently failed in subtest 4
("dry-run during in-flight balance routes notes to stdout only"): the dry-run
rendered the full preview steps but with **no** "waiting for in-flight" note.

Both subtests start a real `btrfs balance`, poll `btrfs balance status` until
it reports `running`, then spawn a fresh `braid` and assert braid observes the
in-flight op (emits the "waiting for in-flight" Info note). The note comes from
the sysfs preflight (`cli/src/add.rs#plan_add` -> `preflight::require_mutation_preflight`
in `cli/src/preflight.rs`, policy `RejectPausedBalanceElseEnqueue`).

On a 512MB payload, the RAID1->single balance completes in well under a second
-- comparable to braid's own startup -- so it can finish before braid reads
`/sys/fs/btrfs/{fsid}/exclusive_operation`. braid then sees `none`, emits no
note, and the assertion flakes. This is a **payload-size-vs-disk-speed race**.
Subtest 3 has the identical race; it just didn't lose this run.

We cannot use the pause-trick that `braid-status-during-balance.py` /
`braid-exclop-paused-balance.py` use, because the `add` preflight **rejects a
paused balance as a hard error** (`RejectPausedBalanceElseEnqueue`,
`cli/src/preflight.rs#check_exclusive_op_with_policy`). The test needs a *running*
(non-paused) balance.

Intended outcome: make "a balance is in flight when braid checks" deterministic,
so the test stops flaking, with a minimal, faithful change and no production-code
edits.

## Approach: dm-delay write delay (the repo's idiom for "make a balance observable")

Put the initial pool members behind `dm-delay` devices and apply a **write**
delay. Balance *writes* are slowed so the balance stays in `running` for many
seconds, while *reads* stay fast (read_delay = 0), so braid's `probe_pool` +
sysfs read still reach the preflight check promptly. This mirrors
`tests/cli/braid-status-during-balance.py`, which already uses the same
`tests/module/dm_delay_helpers.py` helpers for exactly this purpose.

Key kernel fact (verified in `reference/linux/fs/btrfs/`): the kernel sets
`fs_info->exclusive_operation = BTRFS_EXCLOP_BALANCE` and `BTRFS_FS_BALANCE_RUNNING`
at balance-**ioctl entry** (`ioctl.c#btrfs_try_lock_balance` -> `btrfs_exclop_start`
at `ioctl.c:3465`; flag set at `volumes.c:4730`), *before* any relocation write.
So detection succeeds for **any** in-flight balance; the dm-delay's only job is to
keep the balance alive long enough that braid finishes startup before the
relocation loop ends and clears the exclop. (This is why read_delay must stay 0:
delaying reads would only slow braid's own probe, for no benefit.)

Chosen variant: **synchronous add** (smallest, most faithful diff). The delay
stays active across the whole `braid add disk3`, so its `--enqueue` device-add
genuinely enqueues behind the in-flight balance and drains at the delay's pace.
No background-process/log-polling machinery.

### Parameters
- `WRITE_DELAY_MS = 2000` (read_delay = 0, flush_delay unused). ~2-4x margin over
  braid's ~0.3-2s startup. The proven 500ms in `status-during-balance` is for a
  *pause-caught* window; this test has no pause, so the window must self-sustain
  -- 2000ms is the safe default.
- Payload stays **512MB** (`dd ... bs=1M count=512`). Reliably yields >=2 data
  block groups plus metadata/system conversion, so multiple delayed relocations.
  Do not shrink it.
- **Only disk1 + disk2 are delayed** (the initial 2-disk pool). disk3 (added in
  subtest 3) and disk4 (dry-run only) stay **raw `virtio-*`**. Delaying disk1/2
  already gates every balance: each btrfs transaction commit writes a superblock
  to *every* device, so with disk1/2 write-delayed no commit -- hence no balance
  progress past a chunk -- completes faster than `WRITE_DELAY_MS`, regardless of
  where single-profile chunks land. (Source reads are not delayed -- read_delay = 0
  -- and don't affect the window.) The small extra margin from delaying disk3 isn't
  worth keeping disk3's format/lifecycle asymmetric. `DELAYED = ["disk1","disk2"]`
  mirrors `status-during-balance.py`'s `DELAYED_DISKS`.

## Changes

### `tests/cli/braid-add-during-balance.nix`
- Add `pkgs.lvm2` to `environment.systemPackages` (provides `dmsetup`). `pkgs.jq`
  is **not** needed (no JSON parsing here).
- Prepend the helper to the test script, exactly as `braid-status-during-balance.nix`:
  ```nix
  testScript =
    builtins.readFile ./../module/dm_delay_helpers.py + "\n\n"
    + builtins.readFile ./braid-add-during-balance.py;
  ```
- Keep 4 disks at size 4096 and `memorySize = 2048`. No `flake.nix` change (the
  test is already registered).

### `tests/cli/braid-add-during-balance.py`
Reuse helpers `dm_delay_create`, `dm_delay_activate`, `dm_delay_deactivate` from
`tests/module/dm_delay_helpers.py` (in scope via the `.nix` prepend). Preserve
the three-section test preamble (Intent / Why it exists / Scenario), updated to
describe the dm-delay mechanism and the race it removes. **All existing
assertions stay** -- only the balance-liveness mechanism changes.

Structure:

```python
DELAYED = ["disk1", "disk2"]
WRITE_DELAY_MS = 2000  # read_delay stays 0 so braid's probe/sysfs read stay fast

def disk_path(key):
    return (f"/dev/disk/by-id/braid-test-{key}-delay" if key in DELAYED
            else f"/dev/disk/by-id/virtio-{key}")

# Load-bearing wiring: rewrite add_cmd(key) to interpolate disk_path(key) where
# the original (braid-add-during-balance.py current add_cmd) hard-codes
# `{key}=/dev/disk/by-id/virtio-{key}`. This is what routes disk1/disk2 onto the
# dm-delay symlink; disk3/disk4 fall through to virtio. If add_cmd is left
# pointing at raw virtio, dm_delay_activate operates on an unused mapper, no delay
# reaches the balance, and the flake returns -- and a single green run would NOT
# catch it (only the repeat-run verification would).
def add_cmd(key):
    return (f"printf '%s\\n' {pq} | braid add <luks-format-args> "
            f"{key}={disk_path(key)} --passphrase-stdin --yes")

def wait_for_running_balance():           # existing 200 x 0.05s "running" poll
    ...

# subtest 1: dm_delay_create(machine, n) for disk1, disk2; braid add disk1/disk2
#            via disk_path() (delay inactive -> full-speed build).
# subtest 2: write 512MB (delay still inactive -> fast write).
```

**Subtest 3 ("braid add waits for balance and succeeds"):**
1. `dm_delay_activate(machine, DELAYED, write_delay_ms=WRITE_DELAY_MS)`
2. start background `btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage > /tmp/balance.log 2>&1 &`
3. `assert wait_for_running_balance()`
4. `result = machine.execute(add_cmd("disk3") + " 2>&1")` (synchronous; disk3 raw)
5. assert exit 0, `"waiting for in-flight" in output.lower()`, `/dev/mapper/braid-disk3` in `btrfs fi show`
6. `dm_delay_deactivate(machine, DELAYED)` -- runs only **after** the add returns
   (pool is idle then, so the suspend flushes nothing and returns instantly)

**Subtest 4 ("dry-run during in-flight balance routes notes to stdout only"):**
1. `dm_delay_activate(machine, DELAYED, write_delay_ms=WRITE_DELAY_MS)`
2. start background `btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage > /tmp/balance2.log 2>&1 &`
3. `assert wait_for_running_balance()`
4. run `braid add disk4=... --dry-run` capturing `> /tmp/add-dryrun.out 2> /tmp/add-dryrun.err` (disk4 raw)
5. assert `stderr == ""`, `"waiting for in-flight" in stdout.lower()`, and a
   bracketed risk tag (`[safe` / `[destructive` / `[long`) in stdout
6. **Teardown -- cancel, then drain, then deactivate** (a clean, deterministic
   teardown; the order is hygiene, not correctness -- see Implementer notes):
   ```python
   machine.execute("btrfs balance cancel /mnt/storage 2>/dev/null || true")
   # stop the balance, confirm it stopped, then drop the delay. Order is just
   # cleanliness: deactivating mid-balance is equally safe (dm-delay's presuspend
   # flushes pending delayed bios immediately, so there is no delay-length stall).
   for _ in range(200):
       if "no balance" in machine.execute("btrfs balance status /mnt/storage")[1].lower():
           break
       time.sleep(0.1)
   dm_delay_deactivate(machine, DELAYED)
   ```
7. `machine.shutdown()`

## Implementer notes
- **read_delay stays 0.** Pass only `write_delay_ms`. Delaying reads would slow
  braid's own `probe_pool`/`btrfs fi show` for no benefit.
- **Subtest 4 teardown order is hygiene, not correctness.** cancel ->
  drain-poll-until-"no balance" -> deactivate is a clean, deterministic teardown,
  but the reverse order is equally safe: `dm-delay`'s `delay_presuspend` cancels
  its timer and flushes all pending delayed bios *immediately*
  (`flush_delayed_bios(dc, /*flush_all=*/true)` in
  `reference/linux/drivers/md/dm-delay.c#delay_presuspend`), so deactivating
  mid-balance incurs only real (fast) underlying-write latency -- no
  `WRITE_DELAY_MS`-length stall and never a hang.
- **Subtest 3 deactivates only after `braid add disk3` returns.** braid's internal
  post-add `balance -dconvert=raid1` must run through the delay to keep the add
  faithfully synchronous; the pool is idle once the add exits, so the deactivate's
  suspend flushes nothing.
- disk3/disk4 are **never** dm-delay devices: do not `dm_delay_create` them and do
  not add them to `DELAYED`.
- No deadlock: dm-delay sits *under* the LUKS+btrfs stack; suspending it just makes
  upper layers block on I/O briefly. The test holds no dm locks during any add.

## Trade-offs (accepted, documented)
- Wall-clock rises (both balances and braid's restore-balance drain through the
  2000ms delay): subtest 3 ~40-70s, whole test ~1.5-2.5 min vs the old ~30-60s.
  Well within `machine.execute`'s 900s default timeout (verified) and normal for a
  braid VM test. `WRITE_DELAY_MS` is the speed/margin knob (1500ms trims time with
  still-~3x margin) if needed later.
- Subtest 4's note-on-stdout contract is also covered by mock unit tests in
  `cli/src/add.rs` (~`plan_add_preflight_busy_op_becomes_info_note`), but the VM
  subtest uniquely exercises **real-kernel sysfs + real stream routing**, so it
  stays. Scope is de-flake, not coverage cuts.

## Verification
1. Build + run the focused test, several times (it's a timing fix, so repeat to
   build confidence it no longer flakes):
   ```
   just test-vm braid-add-during-balance
   ```
   Repeat ~3-5x (optionally interleave a couple of runs).
2. If a run still misses the window (it should not), the assertions now fail with
   a clear "never observed ... balance running" / "expected 'waiting for in-flight'"
   message rather than silently -- bump `WRITE_DELAY_MS` and note why.
3. No Rust/module/parser changes -> no fixture refresh and no broad-suite run
   required. Touches one test's `.py` + `.nix` only.
