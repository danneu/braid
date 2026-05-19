{ pkgs, linuxPkgs, nixpkgs, linuxSystem }:
let
  braidOnlineStopTimeoutSecs =
    (import ../../modules/braid/constants.nix).braidOnlineStopTimeoutSecs;
  bad = import ./_braid-eval-harness.nix {
    inherit linuxPkgs nixpkgs linuxSystem;
    lockSystemdStopDeadlineSecs = braidOnlineStopTimeoutSecs;
  };
  expectedMessage = "braid.lockSystemdStopDeadlineSecs (${toString braidOnlineStopTimeoutSecs}) must be strictly less than braid-online.service TimeoutStopSec (${toString braidOnlineStopTimeoutSecs}).";
  matching = builtins.filter (
    a:
    let
      assertion = builtins.tryEval a.assertion;
      message = builtins.tryEval a.message;
    in
    assertion.success && assertion.value == false && message.success && message.value == expectedMessage
  ) bad.config.assertions;
  ours = if matching == [ ] then null else builtins.head matching;
in
pkgs.runCommand "eval-lock-systemd-stop-deadline-fails" { } ''
  ${if ours == null then ''
    echo "no assertion with the expected message found" >&2
    exit 1
  '' else ''
    echo ok
    touch $out
  ''}
''
