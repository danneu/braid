# Intent: verifies that safe canonical braid.mountPoint values still evaluate.
# Why it exists: the mount-point assertion is a safety guard, not a restriction
#   to the default path alone.
# Scenario: an operator chooses a canonical alternate pool mount point, a single
#   trailing slash, or a hidden path segment.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  validMountPoints = [
    "/srv/pool"
    "/mnt/storage/"
    "/mnt/.snapshots"
  ];

  systems = map (
    mountPoint:
    import ./_braid-eval-harness.nix {
      inherit
        linuxPkgs
        nixpkgs
        linuxSystem
        mountPoint
        ;
    }
  ) validMountPoints;
in
pkgs.runCommand "eval-mountpoint-accepts-valid"
  {
    toplevels = builtins.concatStringsSep " " (
      map (system: toString system.config.system.build.toplevel) systems
    );
  }
  ''
    echo ok
    touch $out
  ''
