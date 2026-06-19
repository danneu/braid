# Test: braid monitor — proactive ENOSPC risk (Warning tier)
#
# What: Validates the full proactive-capacity-alert lifecycle through the real
# systemd path: a filling RAID1 pool crosses the ENOSPC threshold, `braid
# monitor` exits 3 (Warning, not the Critical beeping exit 1), the wrapper
# routes that to the non-beeping `braid-alert-advisory.service` (alertCommand
# only), `braid status` shows the NOTICE banner + enospc_risk cause, `braid ack`
# clears it and stops the advisory unit, a re-arm cycle exits 0, and a degraded
# pool raises MissingDevice (Critical) but never EnospcRisk.
#
# Why: Unit tests cover the state machine in isolation; only a VM check proves
# the exit-3 wrapper routing, the advisory systemd unit, and the real `systemctl
# stop` on ack. (The keyed-baseline key-mismatch invalidation is covered by the
# unit test cmd_monitor_stale_baseline_key_mismatch_fires_and_clears across all
# three axes; it is not driven end-to-end here because `braid add` runs a RAID1
# balance that either ENOSPCs on a full pool or relieves the risk on a non-full
# one.)
#
# Scenario: 2-disk RAID1 pool (disk1, disk2). Fill below the per-device
# threshold, then drive the Warning lifecycle end to end.
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

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is no
      # pool membership file.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      # Override braid-unlock.service to avoid interactive systemd-ask-password —
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
