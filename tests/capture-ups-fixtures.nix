# Test: capture `upsc` output fixtures for the NUT (Network UPS Tools)
# parser-critical surface.
#
# What: Boots a single-host NUT stack with five `dummy-ups` drivers in
# `dummy-once` mode, each reading a distinct `/etc/nut/<state>.dev`
# file seeded with one target UPS state. Captures `upsc <name>` output
# for each state.
#
# Why: NUT joins btrfs-progs / cryptsetup / util-linux as a pinned
# parser-critical tool (see docs/decisions/010-toolchain-pinning.md).
# These fixtures back the golden parser tests in
# `cli/tests/golden_nixos_25_11.rs` (and the nixos-unstable sibling).
# A nixpkgs bump that changes `nut`'s output format must refresh
# these fixtures in lockstep.
#
# Design note -- why 5 .dev files instead of `upsrw -s`:
#
# An earlier revision used one .dev file + `upsrw -s` to flip state
# between captures. That works cleanly on NUT 2.8.3 but regresses on
# 2.8.4 (the nixos-unstable lane): 2.8.4's dummy-ups driver reports
# `driver.state: updateinfo` and periodically re-reads the .dev file
# even in `dummy-once` mode, clobbering upsrw writes before the
# capture fires. The "one .dev per state" approach avoids the race
# entirely: each state's values live in a static file, and the driver
# is free to re-read it without changing the capture.
#
# The `tests/module/lib/ups-fixture.nix` harness still uses the single
# .dev + upsrw pattern, because its matrix tests need the ability to
# flip state at runtime (mid-mutation LB injection). That path is
# deliberately kept on the 2.8.3 contract; if the matrix tests grow a
# 2.8.4 failure mode, that's a separate problem.
#
# Design note -- why we do NOT run upsmon:
#
# Fixture capture only needs `upsd` + the `dummy-ups` drivers so that
# `upsc` can connect. Running upsmon as well means a primary monitor
# would watch the `lowbattery` dummy UPS, declare it critical
# (OB+LB), and race its own SHUTDOWNCMD against our capture loop on
# slower builders. Disabling upsmon via
# `power.ups.upsmon.enable = false` keeps the capture deterministic
# and limits the unit surface the test depends on.
{
  name = "capture-ups-fixtures";

  nodes.machine =
    { pkgs, ... }:
    let
      # Each state's full UPS dump. The seed is an APC Back-UPS-style
      # dump -- enough keys that every typed field the parser cares
      # about (battery, input, load, device, test result) has content.
      mkDev =
        {
          status,
          batteryCharge,
          batteryRuntime,
          inputVoltage,
          upsLoad,
        }:
        ''
          battery.charge: ${toString batteryCharge}
          battery.charge.low: 10
          battery.runtime: ${toString batteryRuntime}
          battery.runtime.low: 120
          battery.type: PbAc
          battery.voltage: 27.0
          battery.mfr.date: 2023/04/12
          device.mfr: APC
          device.model: Back-UPS ES 550G
          device.serial: 3B1234X56789
          device.type: ups
          driver.name: dummy-ups
          driver.parameter.pollfreq: 30
          input.voltage: ${toString inputVoltage}
          input.voltage.nominal: 120
          input.transfer.low: 88
          input.transfer.high: 142
          input.sensitivity: medium
          ups.load: ${toString upsLoad}
          ups.mfr: APC
          ups.model: Back-UPS ES 550G
          ups.realpower.nominal: 330
          ups.status: ${status}
          ups.test.result: Done and passed
        '';

      states = {
        online = {
          status = "OL";
          batteryCharge = 100;
          batteryRuntime = 1800;
          inputVoltage = "120.0";
          upsLoad = 17;
        };
        onbattery = {
          status = "OB";
          batteryCharge = 72;
          batteryRuntime = 1140;
          inputVoltage = "0.0";
          upsLoad = 23;
        };
        lowbattery = {
          status = "OB LB";
          batteryCharge = 8;
          batteryRuntime = 45;
          inputVoltage = "0.0";
          upsLoad = 25;
        };
        "replace-battery" = {
          status = "OL RB";
          batteryCharge = 100;
          batteryRuntime = 1800;
          inputVoltage = "120.0";
          upsLoad = 17;
        };
      };

      mkUpsEntry = _state: spec: {
        driver = "dummy-ups";
        # `.dev` extension selects dummy-once mode per
        # reference/nut/docs/man/dummy-ups.txt:90,100. Distinct file
        # per state so each dummy-ups has its own independent input.
        port = "${_state}.dev";
        description = "braid fixture UPS (${spec.status})";
      };
    in
    {
      environment.systemPackages = with pkgs; [
        coreutils
        nut
      ];

      # Seed every dummy-ups input file from the state definitions above.
      environment.etc = builtins.listToAttrs (
        map (name: {
          name = "nut/${name}.dev";
          value = {
            text = mkDev states.${name};
          };
        }) (builtins.attrNames states)
      );

      power.ups = {
        enable = true;
        mode = "standalone";

        ups = builtins.mapAttrs mkUpsEntry states;

        # upsmon is intentionally NOT started -- see the header
        # "why we do NOT run upsmon" note. upsd + drivers alone are
        # enough for `upsc` to connect; adding upsmon would let it
        # fire SHUTDOWNCMD on the `lowbattery` UPS mid-capture.
        upsmon.enable = false;
      };
    };

  testScript = builtins.readFile ./capture-ups-fixtures.py;
}
