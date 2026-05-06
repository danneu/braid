# Test: braid-module-add-locked-pool
#
# Intent: `braid add` for a fresh disk against an existing locked pool
# (members in pool.json, no /dev/mapper/braid-* open) refuses BEFORE any
# destructive step. The new check_pool_unlocked_if_membership_exists
# preflight lives between the pool probe and the mounted-only checks in
# plan_add; without it, the bootstrap branch in add work-plan rendering
# would mkfs.btrfs the new disk single-profile and overwrite pool.json,
# orphaning the existing locked members.
#
# Why it exists: the user's bug report -- locked 2-disk pool, plug fresh
# disk, run `braid add` without first running `braid unlock`. Previously
# this silently destroyed pool data. The behavior must be exercised
# end-to-end (real LUKS, real btrfs, real braid binary) because a unit
# test on the helper alone cannot catch a wiring bug in plan_add, and a
# unit test on plan_add cannot catch a regression in confirmation
# bypass / passphrase plumbing that lets format run anyway.
#
# Scenario: pool.json lists disk1 + disk2 (both LUKS-locked at boot via
# initrd-fixture). disk3 is bare. Operator runs `braid add disk3=...`
# without first unlocking. The command must refuse and leave disk3 bare,
# pool.json untouched, and the pool unmounted. Sanity case at the end:
# unlock the pool, then add disk3 successfully.
{ braid }:
{ pkgs, ... }:
let
  passphrase = "testpassphrase";
  diskNames = [
    "disk1"
    "disk2"
  ];
in
{
  name = "braid-module-add-locked-pool";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [
        ../../modules/braid
        (import ./lib/initrd-fixture.nix {
          inherit passphrase diskNames;
          description = "Prepare LUKS + btrfs RAID1 (disk1 + disk2)";
        })
      ];

      braid = {
        enable = true;
        package = braid;
      };

      # Seed pool.json -- the initrd fixture creates LUKS+btrfs but does
      # not write pool.json. Without this, the new locked-pool refusal
      # has no membership to refuse against, defeating the test's intent.
      systemd.tmpfiles.rules = [
        "d /var/lib/braid 0755 root root -"
        ''f /var/lib/braid/pool.json 0644 root root - {"disks":{"disk1":{"by_id":"/dev/disk/by-id/virtio-disk1"},"disk2":{"by_id":"/dev/disk/by-id/virtio-disk2"}}}''
      ];

      virtualisation.emptyDiskImages = [
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
      ];
      virtualisation.memorySize = 2048;

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
      ];
    };

  testScript = builtins.readFile ./add-locked-pool.py;
}
