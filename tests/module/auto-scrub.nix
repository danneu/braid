# Test: braid.autoScrub option mapping
#
# What: Verifies that braid.autoScrub maps to braid-owned scrub systemd units
# with correct lifecycle binding to braid-online.service, that disabling removes
# the units, that a custom interval is passed through, and that a busy-skipped
# scrub (exit 4) is a unit success systemd retries rather than an alert.
#
# Why: braid owns the scrub timer to bind its lifecycle to the pool's online
# state. A broken mapping could create units without lifecycle binding
# (degrading to the nixpkgs always-on behavior), miss the pool mount point,
# or leave stale units when disabled.
#
# Scenario: Three nodes — default config (enabled, monthly, 1h retry), disabled
# (no units), and weekly (custom interval and retry interval). Verify unit
# properties via systemctl show.
{ braid }:
{ ... }:
{
  name = "braid-auto-scrub";

  nodes.defaults =
    { ... }:
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
    };

  nodes.disabled =
    { ... }:
    {
      imports = [ ../../modules/braid ];
      braid = {
        enable = true;
        package = braid;
        autoScrub.enable = false;
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
    };

  nodes.weekly =
    { ... }:
    {
      imports = [ ../../modules/braid ];
      braid = {
        enable = true;
        package = braid;
        autoScrub.interval = "weekly";
        autoScrub.retryInterval = "10m";
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
    };

  testScript = builtins.readFile ./auto-scrub.py;
}
