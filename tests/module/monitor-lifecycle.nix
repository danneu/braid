# Test: monitor-lifecycle
#
# What: Verifies the end-to-end systemd monitoring chain — braid-monitor.service
# detects degraded state, triggers braid-alert.service, braid ack clears the
# alert via systemd, and the ConditionPathIsMountPoint gate prevents monitoring
# from running when the pool is not mounted.
#
# Why: Existing tests cover the CLI alert model (braid-monitor) and alert unit
# plumbing (braid-alert, braid-alert-no-beep) in isolation. Nothing exercises
# the systemd integration path: braid-monitor.service starting braid-alert.service
# on a degraded pool, the mount-bound lifecycle, or braid ack stopping the alert
# service through the real systemd path.
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
  name = "monitor-lifecycle";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture for monitor lifecycle";
        })
      ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
        monitor.beep = false;
        monitor.alertCommand = "touch /root/alert-fired";
      };

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is
      # no pool membership file.  braid unlock requires it.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"},"disk2":{"by_id":"/dev/disk/by-id/virtio-disk2"},"disk3":{"by_id":"/dev/disk/by-id/virtio-disk3"}}}''
      ];

      # Override braid-unlock.service script to avoid interactive
      # systemd-ask-password — VM tests have no TTY agent.
      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./monitor-lifecycle.py;
}
