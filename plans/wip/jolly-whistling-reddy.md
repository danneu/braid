# Plan: Add shutdown/reboot test for ExecStop=braid lock

## Context

The systemd lifecycle test (`tests/module/systemd-lifecycle.py`) verifies `systemctl stop braid-online.service` (manual stop), but never tests actual VM shutdown. systemd's shutdown ordering differs from manual stop — `DefaultDependencies` adds implicit `Conflicts=shutdown.target` + timeout enforcement, and `ExecStop` could be skipped or killed mid-execution if ordering is wrong. Since `ExecStop=braid lock` during shutdown is the entire reason `braid-online.service` exists, this is the highest-value missing test.

**Key insight:** After any reboot, LUKS mappers and mounts are always gone regardless of whether ExecStop ran. Post-reboot state checks alone can false-green the exact regression this test catches. The real proof is the previous boot's journal showing `braid lock` completed without timeout.

## Files to modify

1. `tests/module/systemd-lifecycle.nix` — enable persistent journal storage
2. `tests/module/systemd-lifecycle.py` — add shutdown/reboot subtest

No changes to `storage.nix`, `initrd-fixture.nix`, or `braid-wrapper.sh`.

## Changes

### 1. `tests/module/systemd-lifecycle.nix`: persistent journal

Add inside `nodes.machine`:

```nix
# Persist journal across reboots so the shutdown subtest can assert
# on the previous boot's log via journalctl -b -1.
services.journald.extraConfig = "Storage=persistent";
```

Without this, NixOS defaults to volatile journal (in `/run`), and `journalctl -b -1` returns nothing after reboot.

### 2. `tests/module/systemd-lifecycle.py`: update top-level comment

Add scenario `(6)` at line 24:

```
#     cannot be activated.
```
→
```
#     cannot be activated,
# (6) actual VM shutdown/reboot runs ExecStop=braid lock to completion.
```

### 3. `tests/module/systemd-lifecycle.py`: add shutdown/reboot test

Insert between subtest 8's cleanup (line 222) and the current `machine.shutdown()` (line 224). Keep the final `machine.shutdown()`.

```python
# --- Subtest 9: Shutdown runs ExecStop=braid lock ---
#
# Subtests 3/5 test manual stop, but systemd's shutdown ordering differs.
# DefaultDependencies adds Conflicts=shutdown.target + timeout enforcement.
# ExecStop could be skipped or killed if ordering is wrong. Post-reboot
# state (mappers gone, mount gone) proves nothing — a reboot clears those
# regardless. The journal is the real proof.

# Setup: unlock pool, write canary, trigger real shutdown.
machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
machine.succeed("systemctl is-active braid-online.service")
machine.succeed("echo 'shutdown-canary' > /mnt/storage/canary.txt")
machine.succeed("sync")

machine.shutdown()
machine.start()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("ExecStop=braid lock completes during shutdown"):
    # PRIMARY: previous boot's journal proves ExecStop ran to completion.
    # "Stopped Braid storage pool online." means systemd saw clean exit.
    journal = machine.succeed(
        "journalctl -b -1 -u braid-online.service --no-pager"
    )
    assert "Stopped Braid storage pool online" in journal, (
        f"ExecStop did not complete during shutdown. Journal:\n{journal}"
    )
    assert "timed out" not in journal.lower(), (
        f"braid-online.service was killed by timeout. Journal:\n{journal}"
    )

    # SECONDARY: canary file survives — data integrity after clean unmount.
    machine.succeed(f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin")
    content = machine.succeed("cat /mnt/storage/canary.txt").strip()
    assert content == "shutdown-canary", (
        f"Expected 'shutdown-canary', got '{content}'"
    )

    # Cleanup
    machine.succeed("braid lock")
    machine.fail("systemctl is-active braid-online.service")

machine.shutdown()
```

### What each assertion proves

| Assertion | What it proves | Why needed |
|---|---|---|
| `"Stopped Braid storage pool online" in journal` | ExecStop ran and systemd recorded clean deactivation | Primary proof — without this, a reboot hides whether ExecStop was skipped |
| `"timed out" not in journal` | ExecStop wasn't killed by DefaultTimeoutStopSec | Catches the race/ordering bugs the issue describes |
| Canary file content intact | Filesystem was cleanly unmounted, data survived | Secondary — proves the unmount in `braid lock` completed before LUKS close |

### What was dropped from v1

- **`btrfs device stats -c`**: Cumulative device-error counters, not a shutdown-cleanliness signal. Can false-pass (dirty shutdown, no IO errors) or false-fail (unrelated historical counter).
- **Two separate subtests**: One subtest is sufficient — the journal check is the primary gate, canary is a bonus in the same block.
- **Mapper/mount-gone checks as "proof"**: Demoted from primary evidence — a reboot always clears these.

## Verification

```
just test systemd-lifecycle
```

If the test fails, add `-v` to the specific test for VM logs:
```
just test systemd-lifecycle -v
```
