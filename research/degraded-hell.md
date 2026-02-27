# The `degraded` in fstab Problem

## Background: What Happens During Degraded Mode

When btrfs mounts RAID1 in degraded mode (a device is missing), all new block
group allocations use `single` profile — **one copy, zero redundancy**. Any data
written while degraded has no mirror. If you then lose another drive, that data
is gone permanently.

The `degraded` mount option tells btrfs "go ahead and mount even with missing
devices."

Other key degraded-mode facts:

- btrfs **refuses to mount degraded by default** — requires explicit `-o degraded`
- **Second degraded RW mount becomes read-only** — you get one shot
- After recovery, you **must** rebalance to convert single-profile chunks back:
  ```
  btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/pool
  ```
- Without that rebalance, single-profile chunks silently persist — a data-loss
  time bomb

## The Problem in braid

`storage.nix:36` hardcodes `"degraded"` in the `fileSystems` mount options:

```nix
options = [
  "degraded"    -- this is the problem
  "nofail"
  "x-systemd.requires=btrfs-device-scan.service"
  "x-systemd.after=btrfs-device-scan.service"
];
```

Having it hardcoded in the mount unit means **any path that triggers the systemd
mount could silently mount degraded without the user knowing**. The user's NAS
keeps running, services keep writing data, and none of that data has RAID1
protection.

## What braid currently does

braid has **two mount paths**, and they disagree:

### Path 1: `braid unlock` CLI (correct)

`unlock.rs:146-177` intelligently detects missing devices and only uses
`-o degraded` when `any_absent || any_not_luks`. It prints skip messages for
each missing disk, giving the user visibility:

```rust
let mount_result = if any_absent || any_not_luks {
    // Some disks missing -> degraded mount
    runner.run(&CmdRequest::MountWithOptions {
        device: ...,
        mount_point: mount_point.to_owned(),
        options: vec!["degraded".to_owned()],
    })?
} else {
    // All disks present -> normal mount
    runner.run(&CmdRequest::Mount {
        device: ...,
        mount_point: mount_point.to_owned(),
    })?
};
```

### Path 2: systemd mount unit (dangerous)

The NixOS `fileSystems` entry always has `degraded` in the options. If LUKS
devices happen to be open when systemd tries to mount (e.g., auto-unlock opened
them, or test fixtures pre-open them), the mount succeeds silently with degraded
mode — no warning, no user decision.

The comment on the entry says it is "not authoritative for mounting" and exists
so "NixOS knows about the mount point." But systemd will still *try* to mount
it. Today it usually fails because LUKS isn't open yet. But with
`braid-auto-unlock`, there's a race: if auto-unlock opens LUKS devices and then
anything triggers the mount unit (e.g., a dependency ordering change, `mount -a`,
manual `systemctl start`), the pool mounts degraded silently.

## What braid SHOULD do

Remove `"degraded"` from the `fileSystems` options:

```nix
options = [
  "nofail"
  "x-systemd.requires=btrfs-device-scan.service"
  "x-systemd.after=btrfs-device-scan.service"
];
```

This makes the systemd mount unit **fail when a device is missing** — which is
correct. It forces the user through `braid unlock`, which:

1. Detects exactly which devices are missing
2. Prints clear status for each disk
3. Makes a deliberate decision to mount degraded
4. Gives the user visibility into the degraded state

When all devices are present, `degraded` is unnecessary — btrfs mounts fine
without it. So removing it from `fileSystems` has **zero impact** on the happy
path.

## What a test should look like

The test should verify that **the systemd mount unit alone cannot silently mount
a degraded pool** — the user must go through `braid unlock`:

```python
with subtest("systemd mount unit refuses to mount degraded pool"):
    """
    Intent: verify that the NixOS-generated systemd mount unit does NOT
    silently mount the pool when a device is missing.

    Why it exists: if the fileSystems entry includes 'degraded', systemd
    can mount RAID1 with a missing device without any user awareness.
    All new writes get single-profile chunks (zero redundancy). This is
    a silent data-loss time bomb.

    Scenario: 3-disk RAID1 pool. Disk3's LUKS header is bricked. LUKS
    is opened on disk1 and disk2 (simulating auto-unlock). The systemd
    mount unit should FAIL because it doesn't have 'degraded'. Then
    'braid unlock' should succeed with proper degraded handling.
    """

    # Pre-condition: pool not mounted
    machine.fail("mountpoint -q /mnt/storage")

    # Open LUKS on surviving disks (simulate auto-unlock opening them)
    machine.succeed(
        "cryptsetup open /dev/disk/by-id/virtio-disk1 braid-disk1 --key-file /tmp/key"
    )
    machine.succeed(
        "cryptsetup open /dev/disk/by-id/virtio-disk2 braid-disk2 --key-file /tmp/key"
    )
    # disk3 is bricked — cannot open

    # btrfs scan so kernel sees the (incomplete) array
    machine.succeed("btrfs device scan")

    # The systemd mount unit should FAIL — no 'degraded' option
    machine.fail("systemctl start mnt-storage.mount")

    # Pool should NOT be mounted
    machine.fail("mountpoint -q /mnt/storage")

    # Now use braid unlock — it detects the missing disk and mounts degraded
    machine.succeed("braid unlock --key-file /tmp/key")

    # NOW the pool is mounted (degraded, but through the proper path)
    machine.succeed("mountpoint -q /mnt/storage")

    # braid status should report degraded
    status = machine.succeed("braid status")
    assert "DEGRADED" in status
```

The key assertion: `machine.fail("systemctl start mnt-storage.mount")` — the
systemd mount unit **must fail** when a device is missing. That failure is the
safety mechanism that forces users through braid's aware, instrumented path.
