# Plan: VM test for concurrent `braid add` flock serialization

## Context

The wrapper script (`modules/braid/braid-wrapper.sh:34-41`) serializes `unlock|add|recover` via flock on `/run/braid-pool.lock`. The existing concurrent unlock test (`tests/module/systemd-lifecycle.py:116-148`) covers the unlock path, where the loser re-checks `mountpoint -q` and exits early. But the add path is structurally different: after acquiring the lock, the loser runs the **full add command** — reads pool.json, writes journal, adds device, writes pool.json. No test verifies this path. A regression that breaks flock for `add` (e.g., refactoring the wrapper's `case` to only cover `unlock`) would silently allow concurrent adds to race on pool.json.

## Approach

Extend `tests/module/systemd-lifecycle` with a concurrent-add subtest. This keeps all wrapper-lock behavior in one authoritative module lifecycle test. The NixOS module is required because the flock wrapper (`wrapper.nix` → `braid-wrapper.sh`) is only present when the module is enabled; bare CLI tests use the unwrapped binary.

## Files to modify

1. **`tests/module/systemd-lifecycle.nix`** — add disk4 and disk5 to `virtualisation.emptyDiskImages`
2. **`tests/module/systemd-lifecycle.py`** — add concurrent-add subtest after subtest 8 (recover), before the shutdown test
3. **`tests/module/systemd-lifecycle.py`** — update top-of-file block comment to mention disk4, disk5 and the concurrent-add scenario

No new files. No flake.nix change.

## Implementation details

### systemd-lifecycle.nix

Add two virtual disks after the existing disk3 entry:

```nix
{ size = 512; driveConfig.deviceExtraOpts.serial = "disk4"; }
{ size = 512; driveConfig.deviceExtraOpts.serial = "disk5"; }
```

Two extra disks are needed because the test requires two concurrent mutations on distinct raw disks. After subtest 8 (recover), disk3 is already in the pool — disk4 and disk5 are the two available raw disks.

### systemd-lifecycle.py — new subtest

Insert between the recover subtest (current last subtest before shutdown) and the shutdown setup. Follow existing concurrent-unlock subtest as structural template.

```python
# --- Subtest N: Concurrent add attempts serialize via flock ---

with subtest("Concurrent add attempts serialize via flock"):
    # Intent: Two concurrent `braid add` invocations must serialize via the
    # wrapper's flock — the winner adds its disk, then the loser acquires the
    # lock, reads the updated pool.json, and adds its own disk.
    #
    # Why it exists: The unlock flock test (subtest 6) covers the early-exit
    # re-check path. The add path is structurally different — the loser runs
    # the full add command after acquiring the lock. Without a test, a
    # regression that breaks flock for add would let concurrent adds race on
    # pool.json: the second write could lose the first's disk entry.
    #
    # Scenario: A script bug launches two `braid add` commands at the same
    # time. The flock serializes them so both complete and pool.json reflects
    # all disks.

    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed("mountpoint -q /mnt/storage")

    # Launch two concurrent adds, capture PIDs for independent exit checks.
    machine.succeed(
        f"printf '%s\\n' {pq} | BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk4=/dev/disk/by-id/virtio-disk4 --passphrase-stdin --yes "
        f">/tmp/add-a 2>&1 & pid_a=$! ; "
        f"printf '%s\\n' {pq} | BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add disk5=/dev/disk/by-id/virtio-disk5 --passphrase-stdin --yes "
        f">/tmp/add-b 2>&1 & pid_b=$! ; "
        f"wait $pid_a ; echo $? > /tmp/exit-a ; "
        f"wait $pid_b ; echo $? > /tmp/exit-b"
    )

    # Assert each add succeeded independently.
    exit_a = int(machine.succeed("cat /tmp/exit-a").strip())
    exit_b = int(machine.succeed("cat /tmp/exit-b").strip())
    out_a = machine.succeed("cat /tmp/add-a")
    out_b = machine.succeed("cat /tmp/add-b")
    assert exit_a == 0, f"add-a failed (exit {exit_a}):\n{out_a}"
    assert exit_b == 0, f"add-b failed (exit {exit_b}):\n{out_b}"

    # pool.json must contain all 5 disks.
    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    for d in ["disk1", "disk2", "disk3", "disk4", "disk5"]:
        assert d in pool["disks"], f"{d} missing from pool.json: {set(pool['disks'].keys())}"

    # btrfs must show 5 devices.
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 5, f"Expected 5 btrfs devices, got {devid_count}:\n{fi_show}"

    # No residual journal.
    machine.fail("test -f /var/lib/braid/pending-op.json")

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")
```

**Key design decisions for the test oracle:**

- **Explicit PID capture + separate exit checks**: `wait` without PIDs returns the exit status of the last waited-for process only, not an aggregate. Capturing `pid_a=$!` and `pid_b=$!` then `wait $pid_a; echo $? > /tmp/exit-a` ensures each add's exit code is checked independently.

- **Why "both exit 0 + correct final state" proves serialization**: Without flock, two concurrent adds would both read the same pool.json, both write journals (second overwrites first), and both write pool.json — the second write would lose the first's disk entry. The pool would end up with 4 disks (not 5), or one add would fail due to a journal conflict. Both succeeding AND pool.json having all 5 disks is only possible if they ran serially.

### systemd-lifecycle.py — update header comment

Update the top-of-file `Scenario:` line to mention disk4, disk5 and the concurrent-add scenario:

```
# Scenario: 2-disk RAID1 pool pre-created by initrd fixture (disk1, disk2),
# plus spare disks (disk3 for the add test, disk4+disk5 for the concurrent-add
# test). Tests exercise:
# ...
# (7) two concurrent braid add invocations serialize via the wrapper's flock
#     and both complete successfully.
```

## Verification

```
just test systemd-lifecycle
```

If it fails, run with verbose for VM logs:

```
just test systemd-lifecycle -v
```
