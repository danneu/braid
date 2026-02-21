# Test: braid plan
#
# What: Exercises the planner diff engine across all scenarios: no-op, add, remove,
# replace, ambiguity refusal, redundancy warning, and JSON schema validation.
#
# Why: The planner is the core of the unified CLI. It must correctly classify every
# desired-vs-live state diff and refuse ambiguous cases.
#
# Dependencies: braid-add-disk (builds the test pool).
{
  name = "braid-plan";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];

    environment.systemPackages = let
      braid-cli = pkgs.writeShellApplication {
        name = "braid";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
        text = builtins.readFile ../scripts/braid.sh;
      };
    in [
      braid-cli
      (pkgs.writeShellApplication {
        name = "braid-add-disk";
        runtimeInputs = [ braid-cli pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-add-disk.sh;
      })
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
        "/dev/disk/by-id/virtio-disk3"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-plan.py;
}
