# Intent: verifies that the retired braid.autoScrub.interval and
#   braid.autoScrub.retryInterval options fail module evaluation, with migration
#   text that names their replacement.
# Why it exists: both options named a systemd time span for a schedule braid no
#   longer has. Dropping them silently would let a config that still sets one
#   evaluate cleanly while the setting did nothing -- leaving an operator
#   believing they had retimed their scrubs, or shortened a retry that no longer
#   exists. A trap turns that into a build failure they read once.
# Scenario: an operator upgrades braid with `autoScrub.interval = "weekly"` (or
#   `retryInterval = "10m"`) still in configuration.nix.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  # Each retired option, with a fragment of the migration text that must reach
  # the operator. Substrings, not whole messages: the fragments are the
  # load-bearing part (the replacement option, and the capability that has no
  # replacement), and pinning the full prose would fail on a wording tweak.
  cases = [
    {
      name = "interval";
      setting = {
        braid.autoScrub.interval = "weekly";
      };
      fragments = [
        "braid.autoScrub.intervalDays"
        "time-of-day expression has no replacement"
      ];
    }
    {
      name = "retryInterval";
      setting = {
        braid.autoScrub.retryInterval = "10m";
      };
      fragments = [ "retried by the next hourly poll" ];
    }
  ];

  evalWith =
    setting:
    builtins.tryEval (
      (import ./_braid-eval-harness.nix {
        inherit linuxPkgs nixpkgs linuxSystem;
        extraModules = [ setting ];
      }).config.system.build.toplevel
    );

  # mkRemovedOptionModule raises its message through an assertion, so the
  # failure surfaces when the config is forced. Check the assertion list rather
  # than the throw text, which tryEval does not hand back.
  failedFor =
    setting:
    let
      system = import ./_braid-eval-harness.nix {
        inherit linuxPkgs nixpkgs linuxSystem;
        extraModules = [ setting ];
      };
      broken = builtins.filter (
        a:
        let
          assertion = builtins.tryEval a.assertion;
        in
        assertion.success && assertion.value == false
      ) system.config.assertions;
    in
    map (a: a.message) broken;

  problems = builtins.concatMap (
    case:
    let
      messages = failedFor case.setting;
      joined = builtins.concatStringsSep "\n" messages;
      missing = builtins.filter (fragment: !(nixpkgs.lib.hasInfix fragment joined)) case.fragments;
    in
    if messages == [ ] then
      [ "braid.autoScrub.${case.name} still evaluates without an assertion failure" ]
    else if missing != [ ] then
      [
        "braid.autoScrub.${case.name} migration text is missing ${builtins.toJSON missing}; got: ${joined}"
      ]
    else
      [ ]
  ) cases;

  # Also prove the retired options do not merely warn: a config that sets one
  # must not produce a buildable system.
  stillBuilds = builtins.filter (case: (evalWith case.setting).success) cases;

  # The report goes through a store file rather than a shell literal: the
  # migration text these problems quote contains backticks and quotes that
  # would otherwise be re-interpreted by the builder's shell.
  report = builtins.toFile "auto-scrub-removed-options-report" (
    builtins.toJSON {
      inherit problems;
      stillBuilds = map (c: c.name) stillBuilds;
    }
  );
in
pkgs.runCommand "eval-auto-scrub-removed-options" { } (
  if problems == [ ] && stillBuilds == [ ] then
    ''
      echo ok
      touch $out
    ''
  else
    ''
      echo 'retired autoScrub options are not trapped as expected:' >&2
      cat ${report} >&2
      echo >&2
      exit 1
    ''
)
