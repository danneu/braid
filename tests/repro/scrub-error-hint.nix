# Repro: scrub error hint points at kernel journal details
#
# Proves that braid status prints a copyable journalctl command after a scrub
# reports errors, and that the command finds the kernel scrub messages for a
# deterministic dm-dust data-extent failure.
#
# Uses 1 disk (512 MiB) wrapped in dm-dust below LUKS+btrfs.
{ braid }:
{
  name = "repro-scrub-error-hint";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

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

  testScript = builtins.readFile ./scrub-error-hint.py;
}
