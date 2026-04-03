# Test: scrub-lifecycle
#
# What: Verifies the two key behaviors of lifecycle-bound scrub: (1) Persistent
# catch-up fires immediately after pool unlock when the timer stamp is overdue,
# and (2) braid lock succeeds while the scrub service is actively holding the
# mount busy, because the wrapper stops the timer and service before unmounting.
#
# Why: Config tests verify unit properties (BindsTo, Persistent, etc.) but only
# a behavioral test proves the catch-up actually fires and the cancellation path
# works end-to-end. These are the two behaviors that justify owning the scrub
# timer instead of delegating to services.btrfs.autoScrub.
#
# Scenario: Two nodes, each with a 2-disk RAID1 pool (initrd fixture).
#   catchup: real scrub service, seeded overdue stamp → Persistent triggers
#            immediate scrub on unlock.
#   cancel:  fake long-running scrub (holds mount busy via open FD), lock
#            while scrub runs → wrapper stops timer+service, CLI unmounts.
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
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"},"disk2":{"by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      # Override braid-unlock.service script to avoid interactive
      # systemd-ask-password — VM tests have no TTY agent.
      systemd.services.braid-unlock.script = lib.mkForce ''
        printf '%s\n' '${passphrase}' | braid unlock --passphrase-stdin
      '';

      virtualisation.emptyDiskImages = [
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
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
      systemd.services.braid-scrub.serviceConfig.ExecStart = lib.mkForce
        (toString (pkgs.writeShellScript "fake-scrub" ''
          exec 3>/mnt/storage/.scrub-lock
          sleep 300
        ''));
    };

  testScript = builtins.readFile ./scrub-lifecycle.py;
}
