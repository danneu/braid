# Repro: bounded-rate scrub launch keeps every device at the configured rate
#
# Locks the tool properties the throttled scrub-window tests rest on: the
# scrub_speed_max sysfs knob bounds scrub rate; `scrub limit -a -l <rate>` +
# `scrub start --limit <same rate>` keeps every device at that rate for the
# whole run while a plain `scrub start` reverts all but devid 1 to unlimited
# (progs' revert-before-join ordering); and the configured limit survives a
# run. Manual pin-bump gate named in docs/dev/parser-compatibility.md -- the
# full story, and what to do when the restore-ordering canary fires, is in the
# .py preamble.
#
# Two 1024 MiB disks, 400 MiB payload: at 20 MiB/s per device the throttled
# scrub runs ~20s, the window the wall-time floor and per-device knob samples
# are measured against.
{
  name = "repro-btrfs-scrub-limit-bounds-rate";

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
      ];

      environment.systemPackages = [ pkgs.btrfs-progs ];
    };

  testScript = builtins.readFile ./btrfs-scrub-limit-bounds-rate.py;
}
