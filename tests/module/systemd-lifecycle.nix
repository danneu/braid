# Test: systemd-lifecycle
#
# What: Verifies the systemd state machine — braid-pool.target as entry
# point, braid-online.service as lifecycle owner, and CLI wrapper
# synchronization with service activation state.
#
# Why: Existing tests cover CLI behavior and auto-unlock but don't directly
# verify systemd unit state transitions. A broken wrapper or misconfigured
# dependency could silently break automatic locking on shutdown.
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
  name = "systemd-lifecycle";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for lifecycle tests";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is
      # no pool membership file.  braid unlock requires it.
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
        # Extra disk for the braid-add activation path test (subtest 6).
        # Not in pool.json — added via `braid add` during the test.
        { size = 512; driveConfig.deviceExtraOpts.serial = "disk3"; }
      ];
      virtualisation.memorySize = 2048;

      # Persist journal across reboots so the shutdown subtest can assert
      # on the previous boot's log via journalctl -b -1.
      services.journald.extraConfig = "Storage=persistent";

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./systemd-lifecycle.py;
}
