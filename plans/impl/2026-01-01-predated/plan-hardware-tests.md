# Plan: Hardware Canary Suite

## Context

All braid tests run in NixOS VMs with 256–4096 MiB virtual disks. VMs are the behavioral source of truth and cover CLI validation, error paths, JSON shape, argument handling, and edge cases well. What VMs cannot model: real `/dev/disk/by-id` paths, real USB controller behavior, actual LUKS/btrfs performance at scale, and near-full behavior on 500 GB drives.

This adds a small **hardware canary suite** — not a second full test suite. Each canary explicitly traces back to a VM test it revalidates on real hardware, and one hardware-only stress test exercises the operation most likely to differ from VMs under real-disk space pressure.

**Environment:** NixOS machine with 3×500 GB HDDs. Braid installed via nix (the NixOS system config is unchanged).

**Drives needed:** 3 for all canaries.

**Host state contract:** The runner uses `braid --config /tmp/braid-hw-test/config.json` for all commands, so `/etc/braid/config.json` (NixOS-managed) is never touched. Only `/var/lib/braid/` (runtime state: disk-map, LUKS header backups) is written to and reset between tests — this is ephemeral state equivalent to what VMs discard.

## Implementation

### Step 1: `tests/hw/harness.py` — lightweight test helpers

Standalone helpers for running shell commands and managing test state. Not a NixOS VM API shim.

```python
run(cmd, timeout=300)   # run, assert exit 0, return stdout (default 5min timeout)
run_fail(cmd, timeout=300)  # run, assert non-zero exit, return combined output
run_capture(cmd, timeout=300)  # run, return (exit_code, combined_output)
disk(n)                 # return nth device path from BRAID_HW_DISKS env
disk_name(n)            # return "diskN"
cleanup()               # best-effort umount /mnt/storage + close mappers for test disk names only
section(name)           # context manager — prints name, PASS/FAIL
```

All `run*` functions accept a `timeout` parameter (seconds). On timeout, the subprocess is killed and a `TimeoutError` is raised with captured partial output. This prevents the stress test (or any hung btrfs operation) from blocking indefinitely.

`cleanup()` is scoped: it only closes mappers named `braid-disk1`, `braid-disk2`, ... for the disk names in the test config. It never touches other `braid-*` mappers on the system.

Also includes shared command builders that mirror the VM test patterns and inject `--config`:
```python
CONFIG = "/tmp/braid-hw-test/config.json"

add_cmd(key, passphrase, luks_opts)       # includes --config CONFIG
replace_cmd(old, new, passphrase, luks_opts, extra="")
unlock_cmd(passphrase, extra="")
remove_cmd(key, extra="")
```
Every braid invocation includes `--config {CONFIG}` so the host's `/etc/braid/config.json` is never read or written.

### Step 2: `tests/hw/runner.py` — test orchestrator

- CLI: positional device paths + `--tests` filter + `--yes-destroy-these-disks`
- Validates devices are block devices, prints model/size
- Writes test config to `/tmp/braid-hw-test/config.json` (never touches `/etc/braid/`)
- Sets `BRAID_HW_DISKS` env var (colon-separated device paths)
- For each test:
  1. `cleanup()` — umount `/mnt/storage`, close mappers for test disk names only (`braid-disk1`, `braid-disk2`, ...)
  2. Wipe all disks (`wipefs -a`, `dd if=/dev/zero bs=1M count=10`)
  3. Reset runtime state (`rm -rf /var/lib/braid`)
  4. Re-write `/tmp/braid-hw-test/config.json`
  5. Run test as subprocess
- Reports: per-test pass/fail, summary

### Step 3: Hardware canary tests (4 tests)

Each test starts with a block comment per AGENTS.md convention (intent, why it exists, scenario) and explicitly names the VM test it revalidates.

#### `tests/hw/test_add_canary.py`
**Revalidates:** `tests/cli/braid-add-disk.py` phases 1–3

Mirrors the exact command flow from the VM test: `add_cmd("disk1")` → assert single profile + DUP metadata + data write → `add_cmd("disk2")` → assert RAID1 + data survives → `add_cmd("disk3")` → assert 3 devids in pool + all data survives. Uses the same `add_cmd()` pattern with `--passphrase-stdin --yes` and `BRAID_LUKS_OPTS`.

Hardware-specific value: real udev `/dev/disk/by-id` symlink resolution, real LUKS format timing on 500 GB drives, real btrfs balance on physical media.

#### `tests/hw/test_lock_unlock_canary.py`
**Revalidates:** `tests/cli/braid-lock.py` tests 1–2 + `tests/cli/braid-unlock.py` tests 1–2

Build 3-disk pool with `braid add`, write data. Lock: `braid lock` → assert pool unmounted + all mappers closed. Lock again: idempotent exit 0. Unlock: `braid unlock --passphrase-stdin` → assert pool remounted + all mappers open + data intact. Unlock again: idempotent exit 0. Same assertion patterns as the VM tests.

Hardware-specific value: real `cryptsetup close`/`open` sequencing and real btrfs device scan + mount on physical devices.

#### `tests/hw/test_replace_live_canary.py`
**Revalidates:** `tests/cli/replace-live-disk.py` phase 1

Build 2-disk RAID1 pool (disk1+disk2) with test data. `braid replace --old disk2 --new disk3 --passphrase-stdin --yes`. Assert: disk3 in pool, disk2 gone, no missing devices, 2 devids, RAID1 profile, old mapper closed, data intact, disk-map updated. Same assertions as `replace-live-disk.py` phase 1.

Hardware-specific value: full add→balance→remove→close pipeline on real disks with real I/O timing and 500 GB block device sizes.

#### `tests/hw/test_remove_under_pressure.py` (hardware-only)
**No VM equivalent.** VM ENOSPC tests (`tests/cli/braid-remove-enospc.py`) use 512 MiB drives where filling is instant and chunk allocation is trivial. This tests the same pre-flight rejection at real capacity where btrfs allocation has real fragmentation and real timing.

**Contract:** fill until braid's own pre-flight rejects `--dry-run`, then assert real remove also cleanly rejects.

**Strategy:** use `braid remove disk3 --yes --dry-run` as the oracle. This runs the exact same pre-flight checks (`cli/src/preflight.rs:check_raid1_relocation_space` — per-type Data/Metadata/System, RAID1 pairing constraint) without mutating the pool. No threshold reimplementation needed.

1. Build 3-disk RAID1 pool.
2. Fill loop: write 1 GB via `dd`, sync, then run `braid remove disk3 --yes --dry-run --config ...`.
   - If dry-run succeeds → keep filling.
   - If dry-run fails with "not enough space" → threshold crossed, stop filling.
   - If `dd` fails (pool full) → also stop.
3. Run real `braid remove disk3 --yes --config ...` (with 30-minute timeout).
4. Assert **clean ENOSPC rejection**: non-zero exit, error contains "not enough space".
5. Assert **pool unchanged**: all 3 devices still present in `btrfs fi show`.
6. Assert **filesystem still writable**: `touch /mnt/storage/test-write` succeeds.

Single deterministic outcome. The oracle is braid's own code, so the test cannot drift from the pre-flight contract.

Hardware-specific value: real btrfs chunk allocation patterns, real fragmentation from iterative writes, real pre-flight computation against actual device usage on 500 GB drives.

**Note:** This test will be slow (iterative fill takes minutes). The harness prints progress after each fill iteration.

### Step 4: `justfile` — add `test-hw` recipe

```just
# Run hardware canary tests (requires root, DESTRUCTIVE to specified drives)
test-hw *args:
    sudo python3 tests/hw/runner.py {{args}}
```

Usage:
```bash
just test-hw \
  /dev/disk/by-id/usb-ABC \
  /dev/disk/by-id/usb-DEF \
  /dev/disk/by-id/usb-GHI \
  --yes-destroy-these-disks

# Run one specific canary
just test-hw \
  /dev/disk/by-id/usb-ABC /dev/disk/by-id/usb-DEF /dev/disk/by-id/usb-GHI \
  --tests test_remove_under_pressure \
  --yes-destroy-these-disks
```

## Files

| File | Purpose |
|------|---------|
| `tests/hw/harness.py` | Test helpers: `run()`, `cleanup()`, `section()`, command builders |
| `tests/hw/runner.py` | Orchestrator: arg parse, disk wipe, config gen, run, report |
| `tests/hw/test_add_canary.py` | Revalidates: `cli/braid-add-disk` phases 1–3 |
| `tests/hw/test_lock_unlock_canary.py` | Revalidates: `cli/braid-lock` + `cli/braid-unlock` |
| `tests/hw/test_replace_live_canary.py` | Revalidates: `cli/replace-live-disk` phase 1 |
| `tests/hw/test_remove_under_pressure.py` | Hardware-only: remove pre-flight rejection at measured threshold |
| `justfile` | Modified: add `test-hw` recipe |

## Verification

1. `just test` — existing VM tests unaffected (no files changed)
2. `just test-hw <3 drives> --yes-destroy-these-disks` — all 4 canaries pass
3. `just test-hw <3 drives> --tests test_remove_under_pressure --yes-destroy-these-disks` — stress test fills until dry-run rejects, then asserts clean ENOSPC rejection with unchanged pool and writable filesystem
