# Test: bootstrap crash recovery (escape instructions)
#
# Intent: Verify `braid recover` detects a bootstrap crash (LUKS format
# succeeded, mkfs.btrfs never ran) and prints actionable escape instructions
# instead of a cryptic mount error.
#
# Why it exists: First-time users are the most likely to hit a crash during
# bootstrap (unfamiliar setup, experimenting). The detection logic in
# recover.rs probes each target device with `btrfs filesystem show` to
# confirm no superblock exists before printing the escape message — this
# probe path is untested end-to-end.
#
# Scenario: A single-disk bootstrap add was interrupted after LUKS format
# but before mkfs.btrfs. The disk has a LUKS header but no btrfs inside.
# A pending-op.json with empty pre_membership is injected to simulate the
# crash. `braid recover` should open LUKS, fail to mount, probe the mapper,
# confirm NoBtrfs, and print escape instructions naming the pending-op.json
# path, the disk's by-id path, and wipefs.
{ braid }:
{
  name = "recover-bootstrap-crash";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./recover-bootstrap-crash.py;
}
