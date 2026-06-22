# Test: braid monitor - ENOSPC baseline invalidation after geometry change
#
# Intent: Verify that a keyed ENOSPC suppression baseline is discarded after a
# same-devid `braid replace` changes the real btrfs device geometry.
#
# Why it exists: Unit tests cover PoolKey mismatches with hand-built keys. This
# VM check proves the live probe -> parser -> PoolKey path observes a real
# replace onto a larger disk and re-fires a still-at-risk pool.
#
# Scenario: A skewed 3-disk RAID1 pool is at risk because one member is much
# smaller than the others. After acknowledgment, disk1 is replaced with a larger
# disk4. The FS UUID and devid set stay fixed, but the device_size for disk1's
# preserved devid changes.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
    "disk3"
  ];
in
{
  name = "braid-monitor-enospc-geometry";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare skewed LUKS + btrfs RAID1 fixture for ENOSPC geometry monitor";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
        monitor.beep = false;
        monitor.alertCommand = "touch /root/alert-fired";
      };

      # Seed pool.json: the initrd fixture bypasses `braid add`, so there is no
      # pool membership file.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"},"33333333-3333-3333-3333-333333333333":{"name":"disk3","by_id":"/dev/disk/by-id/virtio-disk3"}}}''
      ];

      # Override braid-unlock.service to avoid interactive systemd-ask-password:
      # VM tests have no TTY agent.
      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
        {
          size = 8192;
          driveConfig.deviceExtraOpts.serial = "disk4";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.jq
      ];
    };

  testScript = builtins.readFile ./braid-monitor-enospc-geometry.py;
}
