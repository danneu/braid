# Test: braid doctor — PC speaker probe (beep_path check)
#
# What: Validates that the doctor `beep_path` check plays the alert test
# tone in human mode (Ok when the wrapper works, Fail when it does not,
# recovery when the underlying issue is resolved) and skips silently in
# `--json` mode regardless of speaker state.
#
# Why: Without an active alert, a broken PC speaker is invisible — the alert
# service's `|| true` swallows beep failures and the user only discovers the
# problem when a real disk alert produces no sound. doctor exists precisely
# to surface this kind of latent breakage. `--json` mode must never produce
# audible side effects so scripts piping doctor output stay silent.
{ braid }:
{
  name = "braid-doctor-beep";

  nodes.machine = { pkgs, ... }: {
    imports = [ ../../modules/braid ];

    # Flag-file-controlled mock so one VM can exercise both healthy and broken
    # branches by touching/removing the flag, instead of needing two VMs with
    # two different overlay-pinned beep packages.
    nixpkgs.overlays = [
      (final: prev: {
        beep = prev.writeShellScriptBin "beep" ''
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
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 512; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];

    # /etc/braid/config.json is written by the braid NixOS module from the
    # default `braid.mountPoint` (/mnt/storage). No explicit override needed.
  };

  testScript = builtins.readFile ./braid-doctor-beep.py;
}
