# Test: add-enroll-recoverable
#
# What: `braid add --enroll DIR` against a returning braid disk
# (recoverable: braid-labeled, btrfs FSID matches the live pool)
# enrolls the keyfile into LUKS slot 1, then runs the forced
# device-add. Idempotent re-add of the same disk recognises
# `AlreadyEnrolled` and skips the addKey/backup work.
#
# Why: the silent-drop bug fix on the add path. Pre-refactor,
# `add --enroll DIR` against a recoverable disk dropped the keyfile
# (the disk was already LUKS so the format/enroll branch was skipped,
# but no other branch picked up `--enroll`). The refactor routes the
# decision through `plan_single_disk_enrollment`.
{ braid }:
{
  name = "add-enroll-recoverable";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
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

  testScript = builtins.readFile ./add-enroll-recoverable.py;
}
