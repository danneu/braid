# Intent: verifies the braid.autoScrub.intervalDays warning boundaries fire on
#   both sides and stay quiet inside them: 6 warns, 7 does not, 180 does not,
#   181 warns -- and every one of them still evaluates.
# Why it exists: these are warnings, not assertions, precisely because both
#   extremes are legitimate for someone (a tiny SSD pool, an archive powered on
#   twice a year) while being far more often a typo. That only holds if the
#   boundaries are exact -- an off-by-one would either nag a deliberate weekly
#   scrub forever or stay silent on a value that leaves the pool unscrubbed for
#   a year -- and if a warned config still builds.
# Scenario: an operator tunes intervalDays and lands on, or just inside, each
#   guardrail.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  # (intervalDays, should this value warn?)
  cases = [
    {
      days = 6;
      warns = true;
    }
    {
      days = 7;
      warns = false;
    }
    {
      days = 180;
      warns = false;
    }
    {
      days = 181;
      warns = true;
    }
  ];

  systemFor =
    days:
    import ./_braid-eval-harness.nix {
      inherit linuxPkgs nixpkgs linuxSystem;
      extraModules = [ { braid.autoScrub.intervalDays = days; } ];
    };

  # The monitor-disabled warning is unrelated and does not fire here (monitor
  # defaults on), but match on the option name anyway so this test cannot pass
  # by counting some other warning.
  intervalWarnings =
    days:
    builtins.filter (
      w: nixpkgs.lib.hasInfix "autoScrub.intervalDays = ${toString days}" w
    ) (systemFor days).config.warnings;

  problems = builtins.concatMap (
    case:
    let
      found = intervalWarnings case.days;
      builds = (builtins.tryEval (systemFor case.days).config.system.build.toplevel).success;
    in
    (
      if case.warns && found == [ ] then
        [ "intervalDays = ${toString case.days} should warn but did not" ]
      else if !case.warns && found != [ ] then
        [ "intervalDays = ${toString case.days} must not warn, got ${builtins.toJSON found}" ]
      else
        [ ]
    )
    ++ (if builds then [ ] else [ "intervalDays = ${toString case.days} must still evaluate" ])
  ) cases;
in
pkgs.runCommand "eval-auto-scrub-interval-warnings" { } (
  if problems == [ ] then
    ''
      echo ok
      touch $out
    ''
  else
    ''
      echo 'autoScrub.intervalDays warning boundaries are wrong:' >&2
      echo '${builtins.toJSON problems}' >&2
      exit 1
    ''
)
