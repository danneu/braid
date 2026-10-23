# Test: bootstrap crash recovery (clear an uncommitted bootstrap)
#
# Intent: Verify `braid recover` clears a bootstrap crash (LUKS format
# succeeded, mkfs.btrfs never ran) and names the disk to wipe, instead of
# failing to mount a pool that does not exist.
#
# Why it exists: First-time users are the most likely to hit a crash during
# bootstrap (unfamiliar setup, experimenting). Recover opens each formatted
# disk and probes its mapper with `btrfs filesystem show` before it clears
# the journal; that probe path needs a real end-to-end run.
#
# Scenario: A single-disk bootstrap add was interrupted after LUKS format
# but before mkfs.btrfs. The disk has a LUKS header but no btrfs inside.
# A pending-op.json with empty pre_membership is injected to simulate the
# crash. `braid recover` should open LUKS, find no btrfs, close the mapper,
# clear the journal, and name the disk's by-id path and wipefs.
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
