# Repro: btrfs's own scrub record is a usable scheduling anchor
#
# Locks the two upstream facts braid's freshness scheduler rests on (ADR 035):
# any scrub -- including one an operator runs by hand, outside braid entirely --
# moves the `Scrub started:` timestamp `btrfs scrub status` reports, and that
# record survives a reboot. braid reads that timestamp and nothing else to
# decide whether a scrub is owed, so a btrfs-progs change that stopped moving it
# for hand scrubs, or moved the record somewhere that does not persist, would
# silently restore the calendar-scrub behavior this design replaced -- with
# every mocked test still green.
#
# Two 1024 MiB disks with a small payload: this test wants scrubs that *finish*
# quickly, unlike its sibling `btrfs-scrub-start-rejected-during-scrub`, which
# needs a scrub slow enough to collide with.
{
  name = "repro-btrfs-scrub-record-anchors-schedule";

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

  testScript = builtins.readFile ./btrfs-scrub-record-anchors-schedule.py;
}
