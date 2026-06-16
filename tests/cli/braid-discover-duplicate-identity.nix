# Test: braid-discover-duplicate-identity
#
# What: Boots two blank disks, then formats them at runtime into the two
# structural hazards discover must refuse -- (1) distinct braid labels sharing
# one LUKS UUID (dd-cloned disk), and (2) two distinct disks sharing one braid
# label. For each, asserts `braid discover` and `braid discover --write` exit 1,
# print no preview rows to stdout, emit the remediation wording on stderr, and
# write no pool.json.
#
# Why: discover's DuplicateUuid / LabelCollision refusals (the cloned-disk
# hazard discover exists to catch) are proven only at the scanner unit-test
# level. The Err arm of drain_warnings in main.rs -- print_cli_error -> exit 1 --
# has no end-to-end coverage; braid-discover-empty-scan.py exercises the
# structurally separate Ok(empty) -> NoMembersDiscovered path instead. This is
# the discover sibling of braid-add/replace-cloned-luks-header-rejected.
{ braid }:
{
  name = "braid-discover-duplicate-identity";

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
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 256;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      environment.systemPackages = [ pkgs.cryptsetup ];
    };

  testScript = builtins.readFile ./braid-discover-duplicate-identity.py;
}
