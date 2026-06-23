# Test: replace-preformatted-luks-passphrase-mismatch
#
# What: New disk is already LUKS-formatted (mapper closed, not in pool) and
# the operator supplies the wrong passphrase on `braid replace`. The command
# must fail before the journal is written -- no pending-op.json, no reformat,
# no pool change.
#
# Why: Decision 019 requires reversible preflight failures to abort cleanly
# without stranding pending-op.json. The closed-LUKS replacement path
# previously deferred passphrase verification to a post-journal
# ensure_luks_open and regressed this invariant.
#
# Dependencies: braid add (builds the test pool), braid replace
# PresentLuks { mapper_open: false } path.
{ braid }:
{
  name = "replace-preformatted-luks-passphrase-mismatch";

  nodes.machine =
    { pkgs, ... }:
    {
      virtualisation.emptyDiskImages = [
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
        {
          size = 1024;
          driveConfig.deviceExtraOpts.serial = "disk3";
        }
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

  testScript =
    builtins.readFile ./member_helpers.py
    + "\n\n"
    + builtins.readFile ./replace-preformatted-luks-passphrase-mismatch.py;
}
