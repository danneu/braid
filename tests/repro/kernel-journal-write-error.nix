# Repro: kernel journal on real write I/O error
#
# Proves that a write-side block error injected below LUKS is surfaced in the
# Linux kernel journal with device-identifying metadata that braid can inspect.
#
# Uses 1 disk (512 MiB), no braid dependency.
{
  name = "repro-kernel-journal-write-error";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
      ];

      environment.systemPackages = [
        pkgs.btrfs-progs
        pkgs.cryptsetup
        pkgs.kmod
        pkgs.lvm2
        pkgs.util-linux
      ];
    };

  testScript = builtins.readFile ./kernel-journal-write-error.py;
}
