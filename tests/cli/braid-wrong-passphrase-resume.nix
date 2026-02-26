# Test: braid-wrong-passphrase-resume
#
# What: Verifies that a wrong passphrase during `braid add` resume fails with
# a clear error and leaves the checkpoint file intact.
#
# Why: This guards against a regression where a wrong passphrase silently
# corrupts or clears the checkpoint on resume.
#
# Scenario: User interrupts `braid add disk2`, fixes the env, then retries
# with the wrong passphrase. The checkpoint must survive so the user can
# retry again with the correct passphrase.
{ braid }:
{
  name = "braid-wrong-passphrase-resume";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    environment.systemPackages = [
      braid
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.coreutils
    ];

    environment.etc."braid/config.json".text = builtins.toJSON {
      disks = {
        disk1 = { by_id = "/dev/disk/by-id/virtio-disk1"; };
        disk2 = { by_id = "/dev/disk/by-id/virtio-disk2"; };
      };
      mount_point = "/mnt/storage";
    };
  };

  testScript = builtins.readFile ./braid-wrong-passphrase-resume.py;
}
