# Test: scrub-lifecycle
#
# What: Verifies freshness-driven scrub scheduling end to end -- a never-scrubbed
# pool gets scrubbed by the timer's post-unlock poke, a pool scrubbed inside the
# freshness window does not, a pool outside it does, lock-time cancellation
# works while a scrub holds the mount busy, an aborted scrub is resumed by the
# next poke, and a poke landing on a running scrub starts no second scrub and
# raises no alert.
#
# Why: Config tests verify unit properties (BindsTo, OnActiveSec, etc.) but only
# a behavioral test proves that btrfs's own scrub record actually drives the
# decision -- that a hand scrub or a just-finished scheduled scrub really does
# suppress the next one, which is the whole point of ADR 035 -- and that the
# cancellation and resume paths still work without the deleted resume trigger.
#
# Scenario: Four nodes with 2-disk RAID1 pools.
#   freshness:   real scrub service; unlock scrubs a never-scrubbed pool, the
#                next unlock does not re-scrub it, and a shrunken freshness
#                window makes the same record stale so it scrubs again.
#   cancel:      fake long-running scrub (holds mount busy via open FD), lock
#                while scrub runs -> Rust dispatch stops timer+service, CLI unmounts.
#   resume:      real scrub service on dm-delay-backed disks, cancel mid-scrub,
#                then resume via the timer's post-unlock poke.
#   concurrency: dm-delay-backed pool with a real scrub in flight; a poke during
#                that scrub must start no second scrub and raise no alert.
{ braid }:
{ pkgs, lib, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];

  # Shared node config for both freshness and cancel nodes.
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

  nodes.freshness = commonNode;

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
  # I/O so a poke can land while a real scrub is still in flight.
  nodes.concurrency = resumeNode;

  testScript =
    builtins.readFile ./dm_delay_helpers.py + "\n\n" + builtins.readFile ./scrub-lifecycle.py;
}
