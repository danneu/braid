# Test: enroll-uuid-mismatch-midprompt
#
# What: Verifies `braid enroll --generate` fatally errors when a disk's
# live LUKS UUID changes after discovery but before execute-time mutation.
#
# Why: Enroll mutates LUKS slot 1. The UUID guard must fire after passphrase
# input and before any keyfile creation or slot mutation so a swapped or
# reformatted disk cannot receive the operator's auto-unlock keyfile.
{ braid }:
{
  name = "enroll-uuid-mismatch-midprompt";

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
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./enroll-uuid-mismatch-midprompt.py;
}
