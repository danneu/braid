# Test: braid.autoScrub option mapping
#
# What: Verifies that braid.autoScrub maps to braid-owned scrub systemd units
# with correct lifecycle binding to braid-online.service, that disabling removes
# the units, that autoScrub.intervalDays reaches the CLI as --fresh-for-secs,
# that the timer is the cheap poll ADR 035 describes (hourly + OnActiveSec, no
# Persistent, no WakeSystem), and that a busy skip (exit 4) is a unit success
# with no retry apparatus of its own.
#
# Why: braid owns the scrub timer to bind its lifecycle to the pool's online
# state. A broken mapping could create units without lifecycle binding
# (degrading to the nixpkgs always-on behavior), miss the pool mount point,
# or leave stale units when disabled. The negative asserts matter as much as
# the positive ones: a re-introduced Persistent= or OnCalendar=monthly would
# restore the second schedule record this design deleted, silently and
# invisibly.
#
# Scenario: Three nodes — default config (enabled, 30-day window), disabled
# (no units), and weekly (custom intervalDays). Verify unit properties via
# systemctl show.
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
        autoScrub.intervalDays = 7;
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
