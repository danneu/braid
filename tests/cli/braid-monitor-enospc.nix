# Test: braid monitor - proactive ENOSPC risk (Warning tier)
#
# Intent: Validate the proactive-capacity-alert lifecycle through the real
# systemd path: a filling RAID1 pool crosses the ENOSPC threshold, `braid
# monitor` exits 3, the wrapper routes that to the non-beeping advisory service,
# `braid status` shows the NOTICE banner + enospc_risk cause, `braid ack` clears
# it and stops the advisory unit, a re-arm cycle exits 0, and a degraded pool
# raises MissingDevice (Critical) but never EnospcRisk.
#
# Why it exists: Unit tests cover the state machine in isolation. Only a VM
# check proves the exit-3 wrapper routing, advisory systemd unit, real
# `systemctl stop` on ack, and degraded-pool precedence.
#
# Scenario: 2-disk RAID1 pool (disk1, disk2). Fill below the per-device
# threshold, drive the Warning lifecycle, then remount degraded. See
# braid-monitor-enospc-geometry for keyed-baseline invalidation after a
# same-devid geometry change.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-monitor-enospc";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ../module/lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 fixture for ENOSPC monitor";
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
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      # Override braid-unlock.service to avoid interactive systemd-ask-password:
      # VM tests have no TTY agent.
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
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.jq
      ];
    };

  testScript = builtins.readFile ./braid-monitor-enospc.py;
}
