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

  nodes.machine = { pkgs, ... }: {
    environment.systemPackages = [
      braid
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-doctor.py;
}
