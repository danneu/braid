# Test: braid-enroll
#
# What: Verifies that `braid enroll` enrolls a binary keyfile into
# LUKS slot 1 on all pool disks, and that `braid unlock --key-file` can
# subsequently open them.
#
# Why: The keyfile enrollment path uses different cryptsetup semantics than
# passphrase (raw bytes, explicit slot, no PBKDF). If enrollment silently
# fails or targets the wrong slot, auto-unlock breaks at 3 AM.
{ braid }:
{
  name = "braid-enroll";

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
      ];

      environment.systemPackages = [
        braid
        pkgs.cryptsetup
        pkgs.btrfs-progs
        pkgs.jq
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-enroll.py;
}
