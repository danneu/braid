# Repro: cryptsetup close fails when filesystem is mounted on the LUKS device
#
# Proves that `cryptsetup close` returns a non-zero exit code with a "busy"
# error when a filesystem is still mounted on the LUKS device. After umount,
# close succeeds.
#
# Uses 1 disk (512 MiB), no braid dependency.
{
  name = "repro-cryptsetup-close-mounted";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
    ];

    environment.systemPackages = [
      pkgs.cryptsetup
      pkgs.btrfs-progs
    ];
  };

  testScript = builtins.readFile ./cryptsetup-close-mounted.py;
}
