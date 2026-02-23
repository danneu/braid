{ braid-rust }:
{
  name = "braid-apply-rust";

  nodes.machine = { pkgs, ... }: let
    braid-cli = pkgs.writeShellApplication {
      name = "braid";
      runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
      text = builtins.readFile ../scripts/braid.sh;
    };
  in {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk4"; }
    ];

    environment.systemPackages = [ braid-cli braid-rust pkgs.cryptsetup pkgs.btrfs-progs pkgs.jq ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = [
        "/dev/disk/by-id/virtio-disk1"
        "/dev/disk/by-id/virtio-disk2"
      ];
      mountPoint = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-apply-rust.py;
}
