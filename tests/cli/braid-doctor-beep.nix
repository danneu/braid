# Test: braid doctor -- PC speaker probe (beep_path check)
#
# Intent: Validates that the doctor `beep_path` check skips by default, plays
# the alert test beep only when `--beep` is passed, reports wrapper failures
# for explicit `--beep`, and rejects the conflicting `--json --beep`
# combination before invoking the beep wrapper.
#
# Why: Without an active alert, a broken PC speaker is invisible -- the alert
# service's `|| true` swallows beep failures and the user only discovers the
# problem when a real disk alert produces no sound. doctor exists to surface
# this kind of latent breakage, but the audible test must be opt-in. `--json`
# mode must never produce audible side effects, so scripts piping doctor
# output stay silent and clap rejects an audible side-effect request.
#
# Scenario: NixOS machine with braid.monitor.beep = true and pkgs.beep
# replaced by a flag-file-gated mock. The test toggles /tmp/beep-broken and
# checks /tmp/beep-invoked to distinguish skip from real wrapper execution.
{ braid }:
{
  name = "braid-doctor-beep";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      # Flag-file-controlled mock so one VM can exercise both healthy and broken
      # branches by touching/removing the flag, instead of needing two VMs with
      # two different overlay-pinned beep packages.
      nixpkgs.overlays = [
        (final: prev: {
          beep = prev.writeShellScriptBin "beep" ''
            touch /tmp/beep-invoked
            if [ -f /tmp/beep-broken ]; then
              echo "mock beep: failing per /tmp/beep-broken" >&2
              exit 1
            fi
            exit 0
          '';
        })
      ];

      braid = {
        enable = true;
        package = braid;
        monitor.enable = true;
        monitor.beep = true;
      };

      virtualisation.emptyDiskImages = [
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk1";
        }
        {
          size = 512;
          driveConfig.deviceExtraOpts.serial = "disk2";
        }
      ];

      # /etc/braid/config.json is written by the braid NixOS module from the
      # default `braid.mountPoint` (/mnt/storage). No explicit override needed.
    };

  testScript = builtins.readFile ./braid-doctor-beep.py;
}
