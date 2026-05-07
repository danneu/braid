# Test: replace-enroll-existing-luks
#
# What: `braid replace --enroll DIR` against a new disk that is already
# LUKS-formatted (PresentLuks state, slot 1 empty) must enroll the
# keyfile into slot 1, run a post-enroll header backup, then proceed
# with the live btrfs replace. After the replace, the new disk's slot
# 1 must authenticate the keyfile so auto-unlock can open it.
#
# Why: This pins the silent-drop bug fix on the replace path. Pre-
# refactor, `Some(kf) + PresentLuks` was dropped on the floor — the
# keyfile flag was a no-op when the new disk was already LUKS-
# formatted, leaving the pool in an asymmetric state where unlock fell
# back to passphrase for the new disk. The refactor routes that case
# through `plan_single_disk_enrollment` and journals the keyfile so
# crash-recovery can replay enrollment.
{ braid }:
{
  name = "replace-enroll-existing-luks";

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
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk4";
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

  testScript = builtins.readFile ./replace-enroll-existing-luks.py;
}
