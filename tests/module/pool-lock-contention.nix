# Test: pool-lock-contention
#
# What: Verifies Rust dispatch's pool-lock acquisition is non-blocking and
# fails fast when another process holds /run/braid-pool.lock.
#
# Why: Without -n on the flock call, a wedged holder would silently
# hang any concurrent `braid unlock` invocation forever. This test
# guards the failure layer -- it must fail if dispatch regresses
# to a blocking flock.
# See docs/design/decisions/026-pool-lock-rust-owned.md.
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
  name = "pool-lock-contention";

  nodes.machine =
    { pkgs, lib, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs fixture for lock-contention test";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      # Seed pool.json — the initrd fixture bypasses `braid add`, so there is
      # no pool membership file.  braid unlock requires it.
      systemd.tmpfiles.rules = [
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/virtio-disk1"},"22222222-2222-2222-2222-222222222222":{"name":"disk2","by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

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
      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./pool-lock-contention.py;
}
