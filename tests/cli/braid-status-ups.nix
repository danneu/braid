# Test: braid-status-ups -- parser-canary for `braid ups status`
#
# What: Boots a VM with `braid.ups.enable = true` + a dummy-ups driver
# (.dev file), then exercises `braid ups status` and `braid ups status
# --json` against the live NUT stack. Asserts the parser round-trips
# the expected status flag on the live-tool output for the currently
# pinned `nut` package.
#
# Why: NUT joins btrfs-progs / cryptsetup / util-linux as a pinned
# parser-critical tool (see docs/design/decisions/010-toolchain-pinning.md).
# Fixture-backed golden tests lock in the contract against captured
# output; this canary is the live-tool mirror that confirms the pin
# actually still parses when the wrapped `upsc` runs end-to-end through
# the CLI. Without it, a refactor that silently broke `cmd_ups_status`
# would pass golden tests and only fail at runtime on a real host.
#
# Companion to `braid-status-rust` etc.; included in the `just
# test-parsers` invocation so a single command runs the whole CLI
# parser-canary surface.
#
# We deliberately reuse the same single-.dev + fixture pattern that
# `tests/module/lib/ups-fixture.nix` uses, but without any pool
# machinery -- this is a pure parser canary, not a shutdown test.
{ braid }:
{
  name = "braid-status-ups";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
        ups = {
          enable = true;
          name = "ups";
          driver = "dummy-ups";
          # .dev extension selects dummy-once mode (see
          # reference/nut/docs/man/dummy-ups.txt:90,100). dummy-once
          # is the right mode here: we capture upsc output while the
          # driver is stable, no timed transitions needed.
          port = "ups.dev";
        };
      };

      # Seed the dummy-ups input with an OL fixture that covers the
      # rich-model keys the parser consumes. Using the same shape the
      # live capture-ups-fixtures test generates keeps the canary and
      # the golden fixtures exercising the same key coverage.
      environment.etc."nut/ups.dev".text = ''
        battery.charge: 100
        battery.charge.low: 10
        battery.runtime: 1800
        battery.runtime.low: 120
        battery.type: PbAc
        battery.voltage: 27.0
        battery.mfr.date: 2023/04/12
        device.mfr: APC
        device.model: Back-UPS ES 550G
        device.serial: 3B1234X56789
        device.type: ups
        input.voltage: 120.0
        input.voltage.nominal: 120
        input.transfer.low: 88
        input.transfer.high: 142
        input.sensitivity: medium
        ups.load: 17
        ups.mfr: APC
        ups.model: Back-UPS ES 550G
        ups.realpower.nominal: 330
        ups.status: OL
        ups.test.result: Done and passed
      '';

      # Secondary dummy UPS for the empty-status JSON warning path. The
      # dummy-ups driver initializes a missing ups.status to OL, so this
      # fixture must explicitly set an empty status line.
      power.ups.ups.emptyups = {
        driver = "dummy-ups";
        port = "emptyups.dev";
        description = "empty-status UPS";
      };

      environment.etc."nut/emptyups.dev".text = ''
        battery.charge: 55
        battery.runtime: 900
        input.voltage: 120.0
        ups.load: 12
        ups.mfr: APC
        ups.model: Back-UPS ES 550G
        ups.realpower.nominal: 330
        ups.status:
      '';

      environment.systemPackages = [ pkgs.jq ];

      virtualisation.memorySize = 1024;
    };

  testScript = builtins.readFile ./braid-status-ups.py;
}
