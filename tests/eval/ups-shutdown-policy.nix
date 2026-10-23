# Intent: verifies the braid.ups shutdown-policy options end to end at eval
#   time: the defaults install braid-ups-guard.service and write 300 s / 50 %
#   into config.json; setting both options to null installs no guard and
#   writes nulls; a zero timer and a charge floor outside 1-99 fail
#   evaluation.
# Why it exists: the guard reads its policy only from config.json, and the
#   module is the only place that decides whether the guard runs at all. A
#   dropped config key would leave a running guard with no triggers; a guard
#   installed with both triggers off would crash-loop; and an unchecked 0 or
#   100 would fire on every outage or never. The production unit's user and
#   preserved runtime directory are pinned here because the clock and the
#   no-capability signalling path both depend on them.
# Scenario: a host on the defaults, a host that turned early shutdown off,
#   and three typos.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  lib = nixpkgs.lib;

  systemWith =
    upsCfg:
    import ./_braid-eval-harness.nix {
      inherit linuxPkgs nixpkgs linuxSystem;
      extraModules = [ { braid.ups = { enable = true; } // upsCfg; } ];
    };

  defaults = systemWith { };
  off = systemWith {
    shutdownAfter = null;
    shutdownBelowCharge = null;
  };

  guardOf = sys: sys.config.systemd.services.braid-ups-guard or null;
  configJson = sys: sys.config.environment.etc."braid/config.json".source;

  rejects =
    upsCfg:
    !(builtins.tryEval (
      builtins.deepSeq [
        (systemWith upsCfg).config.braid.ups.shutdownAfter
        (systemWith upsCfg).config.braid.ups.shutdownBelowCharge
      ] true
    )).success;

  defaultGuard = guardOf defaults;
  problems =
    (lib.optional (defaultGuard == null || !defaultGuard.enable) "defaults must install braid-ups-guard.service")
    ++ (lib.optional (
      defaultGuard != null && defaultGuard.serviceConfig.User != "nutmon"
    ) "braid-ups-guard must run as the upsmon user")
    ++ (lib.optional (
      defaultGuard != null && defaultGuard.serviceConfig.RuntimeDirectoryPreserve != true
    ) "braid-ups-guard must preserve its runtime directory across restarts")
    ++ (lib.optional (
      defaultGuard != null && defaultGuard.serviceConfig.CapabilityBoundingSet != ""
    ) "braid-ups-guard must run with an empty capability set")
    ++ (lib.optional (
      guardOf off != null && (guardOf off).enable
    ) "both triggers null must not install braid-ups-guard.service")
    ++ (lib.optional (!rejects { shutdownAfter = 0; }) "shutdownAfter = 0 must fail evaluation")
    ++ (lib.optional (!rejects { shutdownBelowCharge = 0; }) "shutdownBelowCharge = 0 must fail evaluation")
    ++ (lib.optional (
      !rejects { shutdownBelowCharge = 100; }
    ) "shutdownBelowCharge = 100 must fail evaluation");
in
pkgs.runCommand "eval-ups-shutdown-policy" { nativeBuildInputs = [ pkgs.jq ]; } (
  if problems == [ ] then
    ''
      jq -e '.ups.shutdown == {"after_secs": 300, "below_charge_pct": 50}' ${configJson defaults} \
        || { echo 'defaults: config.json ups.shutdown is wrong' >&2; exit 1; }
      jq -e '.ups.shutdown == {"after_secs": null, "below_charge_pct": null}' ${configJson off} \
        || { echo 'both null: config.json ups.shutdown is wrong' >&2; exit 1; }
      echo ok
      touch $out
    ''
  else
    ''
      echo 'braid.ups shutdown policy is wrong:' >&2
      echo '${builtins.toJSON problems}' >&2
      exit 1
    ''
)
