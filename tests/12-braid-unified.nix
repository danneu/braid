# Test: braid unified CLI
#
# What: Exercises `braid status` (human, --json, --verbose), and verifies
# that standalone scripts (braid-remove-disk) still work.
#
# Why: The unified CLI must produce correct results and the remaining
# standalone scripts must continue to function.
#
# Dependencies: braid init-disk + braid apply (pool setup).
{
  name = "braid-unified";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
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
        name = "braid-remove-disk";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq ];
        text = builtins.readFile ../scripts/braid-remove-disk.sh;
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

  testScript = builtins.readFile ./braid-unified.py;
}
