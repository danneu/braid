# Intent: verifies that unsafe braid.poolBoundServices entries fail module
#   evaluation.
# Why it exists: service-name typos can otherwise create inert systemd service
#   skeletons or stamp the lifecycle owner itself.
# Scenario: an operator writes a suffixed unit name, a missing service, or
#   braid-online and receives the module assertion before deploying.
{
  pkgs,
  linuxPkgs,
  nixpkgs,
  linuxSystem,
}:
let
  cases = [
    {
      poolBoundServices = [ "samba-smbd.service" ];
      expectedMessage = "braid.poolBoundServices entries must be bare NixOS service names without systemd unit suffixes. Got: [\"samba-smbd.service\"].";
    }
    {
      poolBoundServices = [ "missing-consumer" ];
      expectedMessage = "braid.poolBoundServices entries must name services defined by another NixOS module before braid stamps lifecycle edges. Missing: [\"missing-consumer\"].";
    }
    {
      poolBoundServices = [ "braid-online" ];
      expectedMessage = "braid.poolBoundServices must not include braid-online; braid owns braid-online.service as the pool lifecycle marker.";
    }
  ];

  hasExpectedAssertion =
    testCase:
    let
      bad = import ./_braid-eval-harness.nix {
        inherit
          linuxPkgs
          nixpkgs
          linuxSystem
          ;
        inherit (testCase) poolBoundServices;
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
        && message.value == testCase.expectedMessage
      ) bad.config.assertions;
    in
    matching != [ ];

  slipped = builtins.filter (testCase: !(hasExpectedAssertion testCase)) cases;
in
pkgs.runCommand "eval-pool-bound-services-rejects-bad-names" { } ''
  ${
    if slipped == [ ] then
      ''
        echo ok
        touch $out
      ''
    else
      ''
        echo 'poolBoundServices assertion did not reject expected bad values:' >&2
        echo '${builtins.toJSON slipped}' >&2
        exit 1
      ''
  }
''
