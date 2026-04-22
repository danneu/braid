# Repro: kernel journal on bad-sector-style read failure
#
# Proves that a read-side bad block injected below LUKS is surfaced in the
# Linux kernel journal with enough context to study how the stack reports the
# failure.
#
# Uses 1 disk (512 MiB), no braid dependency.
{
  name = "repro-kernel-journal-bad-sector";

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
        pkgs.e2fsprogs
        pkgs.kmod
        pkgs.lvm2
        pkgs.util-linux
      ];
    };

  testScript = builtins.readFile ./kernel-journal-bad-sector.py;
}
