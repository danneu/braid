# Test: braid init-disk
#
# What: Exercises the init-disk command — argument parsing, safety checks,
# LUKS formatting, passphrase validation, and force/confirmation gates.
#
# Why: init-disk is the ONLY path that may call cryptsetup luksFormat.
# All safety checks from section 4.1 of the final proposal must hold:
# declared-disk requirement, pool-membership refusal, isLuks refusal,
# force-confirmation, passphrase check, format execution.
#
# Dependencies: LUKS primitives (cryptsetup), btrfs basics.
{
  name = "braid-init-disk";

  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];

    environment.systemPackages = [
      (pkgs.writeShellApplication {
        name = "braid";
        runtimeInputs = [ pkgs.cryptsetup pkgs.btrfs-progs pkgs.util-linux pkgs.jq pkgs.coreutils ];
        text = builtins.readFile ../scripts/braid.sh;
      })
      pkgs.cryptsetup
      pkgs.btrfs-progs
      pkgs.jq
    ];
  };

  testScript = builtins.readFile ./braid-init-disk.py;
}
