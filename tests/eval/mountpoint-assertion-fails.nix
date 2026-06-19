# Intent: verifies that unsafe braid.mountPoint values fail module evaluation.
# Why it exists: cfg.mountPoint is interpolated into root-context shell,
#   tmpfiles, and systemd slots, so unsafe paths must be rejected before those
#   consumers are generated.
# Scenario: an operator accidentally configures a whitespace, shell-like, or
#   non-canonical pool mount point and receives the module assertion message.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  badMountPoints = [
    "/mnt/my storage"
    "/mnt/a\nb"
    "/mnt/$(reboot)"
    "/mnt/x;y"
    "/"
    "/mnt//storage"
    "/mnt/./storage"
    "/mnt/../storage"
  ];

  expectedMessage =
    mountPoint:
    "braid.mountPoint must be a canonical absolute path: segments of letters, digits, '_', '.', '-' separated by single '/', with no empty/'.'/'..' segments, spaces, newlines, or shell metacharacters. Got: '${mountPoint}'.";

  hasExpectedAssertion =
    mountPoint:
    let
      bad = import ./_braid-eval-harness.nix {
        inherit
          linuxPkgs
          nixpkgs
          linuxSystem
          mountPoint
          ;
      };
      matching = builtins.filter (
        a:
        let
          assertion = builtins.tryEval a.assertion;
          message = builtins.tryEval a.message;
        in
        assertion.success
        && assertion.value == false
        && message.success
        && message.value == expectedMessage mountPoint
      ) bad.config.assertions;
    in
    matching != [ ];

  slipped = builtins.filter (mountPoint: !(hasExpectedAssertion mountPoint)) badMountPoints;
in
pkgs.runCommand "eval-mountpoint-rejects-bad-chars" { } ''
  ${
    if slipped == [ ] then
      ''
        echo ok
        touch $out
      ''
    else
      ''
        echo 'mountPoint assertion did not reject expected bad values:' >&2
        echo '${builtins.toJSON slipped}' >&2
        exit 1
      ''
  }
''
