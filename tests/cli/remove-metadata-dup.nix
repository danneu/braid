# Test: metadata profile after RAID1 → single conversion
#
# What: When braid removes a disk that reduces the pool to 1 device,
# the metadata profile must convert to DUP (not single).
#
# Why: DUP keeps two copies of metadata on the same device, protecting
# against localized corruption. Single metadata means one bad sector can
# lose the entire filesystem. btrfs defaults to DUP for metadata on
# single-device mkfs, and we must preserve that invariant after conversion.
#
# Dependencies: braid add, braid remove.
{ braid }:
{
  name = "remove-metadata-dup";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
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

  testScript = builtins.readFile ./remove-metadata-dup.py;
}
