# Intent: verifies the Nix predicates accept and reject the same sample matrix
#   as the Rust argv-boundary newtypes.
# Why it exists: mountPoint, ups.name, and wolInterface are interpolated into
#   module-generated command text before Rust runs, so the Nix and Rust
#   grammars must not drift.
# Scenario: a future grammar edit weakens one side only, and the direct
#   predicate parity check fails during flake checks.
{ pkgs }:
let
  grammar = import ../../modules/braid/grammar.nix { lib = pkgs.lib; };

  cases = [
    {
      name = "mountPoint";
      pred = grammar.mountPointOk;
      accept = [
        "/mnt/storage"
        "/mnt/tank-1"
        "/mnt/storage/"
        "/mnt/.snapshots"
      ];
      reject = [
        "mnt/storage"
        "-o"
        ""
        "/"
        "/mnt//storage"
        "/mnt/./storage"
        "/mnt/../storage"
        "/mnt/my drive"
        "/mnt/x;touch"
      ];
    }
    {
      name = "upsName";
      pred = grammar.isValidUpsName;
      accept = [
        "ups"
        "my-ups"
        "ups_1"
        "abcdefghijklmnopqrstuvwxyzabcdef"
      ];
      reject = [
        "ups@host:3493"
        "ups:1"
        "-x"
        "with space"
        "abcdefghijklmnopqrstuvwxyzabcdefg"
      ];
    }
    {
      name = "interface";
      pred = grammar.isValidInterface;
      accept = [
        "eno1"
        "br0"
        "eth0.100"
      ];
      reject = [
        "eth/0"
        "eth:0"
        "."
        ".."
        "abcdefghijklmnop"
        "-i"
        "with space"
      ];
    }
  ];

  failures = builtins.concatMap (
    case:
    let
      accepted = builtins.filter (sample: !(case.pred sample)) case.accept;
      rejected = builtins.filter (sample: case.pred sample) case.reject;
    in
    pkgs.lib.optionals (accepted != [ ]) [
      {
        inherit (case) name;
        kind = "accepted samples rejected";
        samples = accepted;
      }
    ]
    ++ pkgs.lib.optionals (rejected != [ ]) [
      {
        inherit (case) name;
        kind = "rejected samples accepted";
        samples = rejected;
      }
    ]
  ) cases;
in
pkgs.runCommand "eval-grammar-parity" { } ''
  ${
    if failures == [ ] then
      ''
        echo ok
        touch $out
      ''
    else
      ''
        echo 'grammar parity failures:' >&2
        echo '${builtins.toJSON failures}' >&2
        exit 1
      ''
  }
''
