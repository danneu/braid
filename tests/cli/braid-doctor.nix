# Test: braid doctor
#
# What: Validates the braid doctor subcommand against valid, missing, malformed,
# and schema-invalid config files in both human and JSON output modes.
#
# Why: Doctor is the diagnostic entry point. It must correctly detect config
# problems and report them in a structured way for both human operators and
# automated monitoring.
#
# Dependencies: Rust braid binary.
{ braid }:
{
  name = "braid-doctor";

  nodes.machine =
    { pkgs, ... }:
    {
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

      environment.systemPackages = [
        braid
        pkgs.jq
        pkgs.cryptsetup
        pkgs.btrfs-progs
      ];

      environment.etc."braid/config.json".text = builtins.toJSON {
        mount_point = "/mnt/storage";
      };
    };

  testScript = builtins.readFile ./braid-doctor.py;
}
