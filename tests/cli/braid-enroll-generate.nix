# Test: braid-enroll-generate
#
# What: Verifies that `braid enroll --generate` atomically creates a
# keyfile and enrolls it into all pool disks, and that it refuses to overwrite
# an existing keyfile.
#
# Why: The --generate flag replaces manual dd/chmod steps. If generation is not
# atomic or does not respect create_new semantics, users could silently lose
# their existing keyfile or create one with wrong permissions.
{ braid }:
{
  name = "braid-enroll-generate";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
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

  testScript = builtins.readFile ./braid-enroll-generate.py;
}
