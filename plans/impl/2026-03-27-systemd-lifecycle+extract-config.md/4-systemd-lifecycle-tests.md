# Plan: Systemd lifecycle state machine VM tests

## Context

The systemd lifecycle model has three moving parts — `braid-pool.target` (entry point), `braid-unlock.service` (orchestrator), and `braid-online.service` (lifecycle owner) — synchronized by the CLI wrapper (`braid-wrapper.sh`). Existing tests cover CLI behavior (`braid-unlock.py`, `braid-lock.py`) and auto-unlock (`auto-unlock-key-present.py`), but none directly verify systemd unit state transitions. A broken wrapper or misconfigured dependency could leave `braid-online.service` out of sync with actual pool state, causing silent failure of automatic locking on shutdown.

## Deliverables

### 1. New test: `tests/module/systemd-lifecycle.nix` + `.py`

Module test (imports braid NixOS module, receives `braid-cli-unwrapped`). One VM boot, seven subtests.

**NixOS config** (`systemd-lifecycle.nix`):
- Import `../../modules/braid` and `./lib/initrd-fixture.nix` with 2 disks (`disk1`, `disk2`)
- `braid.enable = true; braid.package = braid;`
- Seed `pool.json` via `systemd.tmpfiles.rules` (same pattern as `auto-unlock-key-present.nix:65-68`) — initial pool has `disk1` + `disk2` only
- Override `systemd.services.braid-unlock.script` with `lib.mkForce` to replace `systemd-ask-password` with inline passphrase — VM tests have no TTY agent
- 3 virtual disks: 2x512MB (`disk1`, `disk2`) for the initial pool + 1x512MB (`disk3`) for the `braid add` test
- 2048MB memory

**Test script** (`systemd-lifecycle.py`):

```
Subtest 1 — Precondition: pool offline after boot
  - fail("mountpoint -q /mnt/storage")
  - fail("systemctl is-active braid-online.service")
  - fail("test -e /dev/mapper/braid-disk1")
  - fail("test -e /dev/mapper/braid-disk2")

Subtest 2 — braid-pool.target brings pool online
  - succeed("systemctl start braid-pool.target")
  - succeed("mountpoint -q /mnt/storage")
  - succeed("systemctl is-active braid-online.service")
  - succeed("test -e /dev/mapper/braid-disk1")
  - succeed("test -e /dev/mapper/braid-disk2")

Subtest 3 — Stopping braid-online.service locks pool
  - succeed("systemctl stop braid-online.service")
    ExecStop runs `braid lock` → unmounts, closes LUKS.
    Note: wrapper's post-lock `systemctl stop braid-online` is a no-op
    (service already deactivating) — harmless.
  - fail("mountpoint -q /mnt/storage")
  - fail("test -e /dev/mapper/braid-disk1")
  - fail("test -e /dev/mapper/braid-disk2")
  - fail("systemctl is-active braid-online.service")
  - succeed("systemctl stop braid-unlock.service")
    Cleanup: reset unlock service state (RemainAfterExit=true keeps it
    "active (exited)" otherwise). Not strictly needed for subtest 4
    (uses CLI directly), but prevents fragility if subtests are reordered.

Subtest 4 — CLI wrapper synchronization (unlock)
  - succeed("printf '%s\n' 'testpassphrase' | braid unlock --passphrase-stdin")
  - succeed("systemctl is-active braid-online.service")
  - succeed("mountpoint -q /mnt/storage")

Subtest 5 — CLI wrapper synchronization (lock)
  - succeed("braid lock")
  - fail("systemctl is-active braid-online.service")
  - fail("mountpoint -q /mnt/storage")
  - fail("test -e /dev/mapper/braid-disk1")

Subtest 6 — braid add activates braid-online.service
  Pool is offline from subtest 5. Manually open existing LUKS mappers
  and mount pool (bypassing wrapper) so braid-online stays inactive.
  Then run `braid add disk3=...` through the wrapper and verify it
  activates braid-online.service — proving the add path works
  independently of unlock.
  - succeed("printf ... | cryptsetup open virtio-disk1 braid-disk1")
  - succeed("printf ... | cryptsetup open virtio-disk2 braid-disk2")
  - succeed("btrfs device scan /dev/mapper/braid-disk1 /dev/mapper/braid-disk2")
  - succeed("mount /dev/mapper/braid-disk1 /mnt/storage")
  - succeed("mountpoint -q /mnt/storage")
  - fail("systemctl is-active braid-online.service")
  - succeed("printf ... | BRAID_LUKS_OPTS='--pbkdf pbkdf2 --pbkdf-force-iterations 1000' braid add disk3=/dev/disk/by-id/virtio-disk3 --passphrase-stdin --yes")
  - succeed("systemctl is-active braid-online.service")
  - succeed("mountpoint -q /mnt/storage")
  - Cleanup: succeed("braid lock")

Subtest 7 — Wrapper warns but succeeds when braid-online.service masked
  - succeed("systemctl mask braid-online.service")
  - execute("printf '%s\n' 'testpassphrase' | braid unlock --passphrase-stdin 2>&1")
  - Assert exit code == 0
  - Assert "WARNING" and "braid-online" in combined output
  - succeed("mountpoint -q /mnt/storage")
  - fail("systemctl is-active braid-online.service")
  - Cleanup: succeed("braid lock"), succeed("systemctl unmask braid-online.service")
```

### 2. Modify `tests/module/auto-unlock-key-present.py`

Add one subtest after "Pool is mounted after auto-unlock" (line 33):

```python
with subtest("braid-online.service is active after auto-unlock"):
    machine.succeed("systemctl is-active braid-online.service")
```

Verifies the wrapper activation path works for `braid-auto-unlock` too, not just manual CLI unlock.

### 3. Register in `flake.nix`

Add after the auto-unlock tests (~line 400):

```nix
systemd-lifecycle = pkgs.testers.nixosTest (
  import ./tests/module/systemd-lifecycle.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

## Files to create/modify

| File | Action |
|------|--------|
| `tests/module/systemd-lifecycle.nix` | Create |
| `tests/module/systemd-lifecycle.py` | Create |
| `tests/module/auto-unlock-key-present.py` | Add 1 subtest |
| `flake.nix` | Add test registration |

## Key patterns to reuse

- `tests/module/auto-unlock-key-present.nix` — module test structure with initrd-fixture and pool.json seeding
- `tests/module/lib/initrd-fixture.nix` — LUKS+btrfs pool creation in initrd
- `tests/cli/braid-unlock.py` — passphrase piping pattern (`printf '%s\n'`)
- `tests/cli/braid-lock.py` — mapper existence checks, idempotency assertions

## Design decisions

**One file, one VM boot:** All lifecycle subtests share the same pool and VM config. Separate files would each need a 30-60s VM boot for the same setup. Follows the pattern of `braid-unlock.py` (8 subtests, 1 boot).

**`lib.mkForce` on unlock service script:** Replaces `systemd-ask-password` with inline passphrase. The alternative (spawning a TTY agent to answer the prompt) is fragile and tests the wrong thing. We want to test the systemd dependency chain, not `systemd-ask-password`.

**Negative path via `systemctl mask`:** Masking the service causes `systemctl start` to fail, triggering the wrapper's WARNING path. Cleaner than overriding ExecStart to fail, and fully reversible with `unmask`.

## Verification

```bash
# Run just the new lifecycle test
just test systemd-lifecycle

# Run the modified auto-unlock test
just test auto-unlock-key-present

# If either fails, re-run with verbose
just test systemd-lifecycle -v
```

## Commits

1. **add systemd lifecycle state machine VM test** — new `systemd-lifecycle.nix` + `.py`, flake.nix registration
2. **assert braid-online.service active after auto-unlock** — one-line addition to `auto-unlock-key-present.py`
