# Test: scrub-lifecycle
#
# What: Verifies lifecycle-bound scrub catch-up, lock-time cancellation,
# pool-online resume, and coalescing when an overdue timer fire and resumable
# pool-online state both target braid-scrub.service.
#
# Why: Config tests verify unit properties (BindsTo, Persistent, etc.) but only
# a behavioral test proves the catch-up actually fires, the cancellation path
# works end-to-end, the resume trigger engages, and the single-runner topology
# prevents duplicate btrfs scrub processes.
#
# Scenario: Four nodes with 2-disk RAID1 pools.
#   catchup:     real scrub service, seeded overdue stamp -> Persistent triggers
#                immediate scrub on unlock.
#   cancel:      fake long-running scrub (holds mount busy via open FD), lock
#                while scrub runs -> Rust dispatch stops timer+service, CLI unmounts.
#   resume:      real scrub service on dm-delay-backed disks, cancel mid-scrub,
#                unlock with trigger masked, then resume via the pool-online trigger.
#   concurrency: dm-delay-backed pool with saved scrub progress + overdue timer
#                stamp; on unlock, both activation paths target braid-scrub.service.
#                Proves systemd coalesces the starts into one resumed scrub run.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];

  # Shared node config for both catchup and cancel nodes.
  commonNode =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for scrub lifecycle tests";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is
      # no pool membership file. braid unlock requires it.
      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
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
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  resumeNode =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
      ];

      braid = {
        enable = true;
        package = braid;
      };

      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.lvm2
      ];
    };
in
{
  name = "scrub-lifecycle";

  nodes.catchup = commonNode;

  nodes.cancel =
    { pkgs, lib, ... }:
    {
      imports = [ commonNode ];

      # Override ExecStart to simulate a long-running scrub that holds the
      # mount busy. Opens an FD on the pool mount, then sleeps. This makes
      # the cancellation test deterministic — no timing race with a real
      # scrub that completes in milliseconds on tiny test disks.
      systemd.services.braid-scrub.serviceConfig.ExecStart = lib.mkForce (
        toString (
          pkgs.writeShellScript "fake-scrub" ''
            exec 3>/mnt/storage/.scrub-lock
            sleep 300
          ''
        )
      );
    };

  nodes.resume = resumeNode;

  # Same dm-delay-backed setup as `resume`; the concurrency subtest needs slow
  # I/O so any accidental second scrub has time to become visible.
  nodes.concurrency = resumeNode;

  testScript =
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./scrub-lifecycle.py;
}
