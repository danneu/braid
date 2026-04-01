# Plan: VM test for basic `braid discover` use case

## Context

`braid discover` is a recovery/bootstrap tool: it scans `/dev/disk/by-id/` for LUKS devices labeled `braid-<name>` and rebuilds `pool.json` when it's absent. The canonical scenario is a NAS user who lost `pool.json` (reinstalled NixOS, migrated to a new box) and needs to recover it from the drives themselves.

We recently changed `discover` to fail immediately if `pool.json` already exists, removed `--dry-run`, and made the hint name the actual path. Existing VM tests (`single-disk.py`, `raid1.py`, `braid-browse.py`) exercise `braid discover --write` incidentally as a setup step, but no dedicated test covers: (a) the read-only mode without `--write`, or (b) the "pool.json already exists" guard.

## What to build

Two new files + one flake.nix registration:

- `tests/cli/braid-discover.nix` — NixOS VM config (2 LUKS-labeled disks, no pool.json)
- `tests/cli/braid-discover.py` — Python test script
- `flake.nix` — register as `braid-discover` in the CLI checks block

## Test steps

**Scenario:** pool.json is missing; user wants to recover it from labeled drives.

1. **discover without --write** — `braid discover`
   - Succeeds (exit 0)
   - Output contains `disk1` and `disk2` entries
   - Output contains `pass --write to save to /var/lib/braid/pool.json`
   - pool.json does NOT exist afterward (read-only)

2. **discover --write** — `braid discover --write`
   - Succeeds (exit 0)
   - Output contains `pool membership written to /var/lib/braid/pool.json`
   - pool.json now exists at `/var/lib/braid/pool.json`
   - pool.json contains `disk1` and `disk2` entries, each with a `/dev/disk/by-id/` path (not asserting the specific alias)
   - Immediately run `braid unlock --passphrase-stdin` — the recovered pool.json must actually work; pool mounts at `/mnt/storage`

3. **discover fails when pool.json exists** — run `braid discover` again
   - Fails (non-zero exit)
   - Output contains `pool.json already exists at /var/lib/braid/pool.json`

## NixOS config

Model on `tests/cli/braid-browse.nix`. Key points:
- 2 virtual disks: `{ size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }` etc.
- Import `initrd-fixture.nix` with `passphrase = "testpassphrase"` and `diskNames = ["disk1" "disk2"]`
  - The fixture LUKS-formats both disks with `--label "braid-disk1"` / `--label "braid-disk2"` and creates a btrfs RAID1 (the btrfs part is harmless for this test)
- Use `linuxCrane.braid` (the PATH-wrapped binary, same as all other CLI tests)
- `memorySize = 2048`
- No pool.json pre-seeded in the config (the point is it's absent)

## Python test script

```python
# Intent: verify braid discover's recovery workflow end-to-end.
# Why it exists: discover is the sole way to rebuild pool.json from labeled drives;
#   a regression here leaves users unable to recover a lost pool config.
# Scenario: user reinstalled NixOS on their NAS; pool.json is gone but drives
#   retain their braid LUKS labels. They run discover to reconstruct pool.json.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("discover lists labeled disks and prints write hint"):
    out = machine.succeed("braid discover 2>&1")
    assert "disk1" in out
    assert "disk2" in out
    assert "pass --write to save to /var/lib/braid/pool.json" in out

with subtest("discover without --write does not create pool.json"):
    machine.fail("test -f /var/lib/braid/pool.json")

with subtest("discover --write creates pool.json"):
    out = machine.succeed("braid discover --write 2>&1")
    assert "pool membership written to /var/lib/braid/pool.json" in out
    machine.succeed("test -f /var/lib/braid/pool.json")

with subtest("pool.json contains disk entries with by-id paths"):
    pool_json = machine.succeed("cat /var/lib/braid/pool.json")
    assert "disk1" in pool_json
    assert "disk2" in pool_json
    assert "/dev/disk/by-id/" in pool_json

with subtest("recovered pool.json is usable — unlock succeeds"):
    machine.succeed("echo testpassphrase | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint -q /mnt/storage")

with subtest("discover fails when pool.json already exists"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out
```

## flake.nix registration

In `checksFor`, in the CLI tests block (around line 102–239), add:

```nix
braid-discover = pkgs.testers.nixosTest (
  import ./tests/cli/braid-discover.nix {
    braid = linuxCrane.braid;
  }
);
```

## Critical files

| Role | Path |
|---|---|
| New test script | `tests/cli/braid-discover.py` |
| New NixOS config | `tests/cli/braid-discover.nix` |
| Template (NixOS config) | `tests/cli/braid-browse.nix` |
| Shared LUKS fixture | `tests/module/lib/initrd-fixture.nix` |
| Discover CLI handler | `cli/src/main.rs` (Commands::Discover) |
| Test registration | `flake.nix` (checksFor, CLI block) |

## Verification

```
just test braid-discover        # run the new test
just test braid-discover -v     # with VM logs if it fails
```
