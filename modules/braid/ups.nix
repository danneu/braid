# UPS integration -- opinionated wrapper over nixpkgs `power.ups`.
#
# Enabling `braid.ups.enable = true` gives a home NAS:
#  - orderly shutdown on low-battery (SHUTDOWNCMD = systemctl poweroff)
#    unwinds `braid-online.service` ExecStop -> btrfs umount -> LUKS close
#    (see decisions/018-systemd-lifecycle.md).
#  - preflight refusal of pool-mutating commands while the UPS is on
#    battery (see cli/src/preflight.rs).
#  - early shutdown in a long outage: `braid-ups-guard.service` forces a
#    shutdown through upsmon after `shutdownAfter` on battery or below
#    `shutdownBelowCharge` (see cli/src/ups_guard.rs). Firmware LB stays the
#    backstop.
#
# The upsmon<->upsd credential is generated at activation time by a
# oneshot `braid-ups-secrets.service`; the token lives at
# /var/lib/braid/upsmon.pass outside the Nix store. See
# decisions/020-ups-integration.md for the scope and safety contract.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.braid;
  ups = cfg.ups;
  inherit (import ./hardening.nix { }) base;
  grammar = import ./grammar.nix { inherit lib; };
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
  guardEnabled = ups.shutdownAfter != null || ups.shutdownBelowCharge != null;
in
{
  options.braid.ups = {
    enable = lib.mkEnableOption "UPS support via NUT (single-host standalone)";

    name = lib.mkOption {
      type = lib.types.str;
      default = "ups";
      description = ''
        Identifier used by upsd and by `upsc <name>` at runtime. Also
        written to /etc/braid/config.json so the CLI can find the UPS
        without a separate flag.
      '';
    };

    driver = lib.mkOption {
      type = lib.types.str;
      default = "usbhid-ups";
      description = ''
        NUT driver for the connected UPS. Defaults to `usbhid-ups`, which
        covers the vast majority of home-NAS USB UPSes. Non-USB drivers
        (apcsmart, snmp-ups) are an escape hatch and are not first-class
        supported by braid -- see decisions/020-ups-integration.md.
      '';
    };

    port = lib.mkOption {
      type = lib.types.str;
      default = "auto";
      description = ''
        The `port` value for the configured driver. For `usbhid-ups`,
        `auto` means "find the first matching USB UPS".
      '';
    };

    shutdownAfter = lib.mkOption {
      type = lib.types.nullOr lib.types.ints.positive;
      default = 300;
      example = 600;
      description = ''
        Seconds on battery after which braid shuts the host down, through
        upsmon, so the pool locks cleanly while the battery still has charge
        to spare. Outages shorter than this ride through. Size it as
        about half the UPS's runtime at the NAS's real load, minus the time a
        clean shutdown takes, minus a minute of margin; if that comes out
        under about two minutes, the UPS is too small for this load. Null
        turns the timer off. The UPS firmware's own low-battery signal still
        shuts the host down if it comes first.
      '';
    };

    shutdownBelowCharge = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 1 99);
      default = 50;
      example = 60;
      description = ''
        Battery charge, in percent, below which braid shuts the host down
        while on battery, whichever of this and `shutdownAfter` comes first.
        It covers an aging battery or a grown load that drains faster than
        the timer assumes. Needs a UPS that reports `battery.charge`;
        `braid doctor` warns when it does not. Null turns the floor off.
      '';
    };
  };

  config = lib.mkIf (cfg.enable && ups.enable) {
    assertions = [
      {
        assertion = grammar.isValidUpsName ups.name;
        message = "braid.ups.name must be 1-32 characters of letters, digits, '_', '.', or '-', and must not start with '-'.";
      }
    ];

    # braid-ups-secrets.service generates the upsmon<->upsd credential at
    # /var/lib/braid/upsmon.pass (outside the Nix store). `before` +
    # `requiredBy` on upsd.service / upsmon.service means those units hard
    # fail if secret creation fails, rather than racing it.
    systemd.services.braid-ups-secrets = {
      description = "Generate upsmon password file for braid-managed NUT";
      before = [
        "upsd.service"
        "upsmon.service"
      ];
      requiredBy = [
        "upsd.service"
        "upsmon.service"
      ];
      path = [ pkgs.coreutils ];
      serviceConfig = base // {
        Type = "oneshot";
        RemainAfterExit = true;
        ReadWritePaths = [ "/var/lib/braid" ];
        CapabilityBoundingSet = "";
        PrivateNetwork = true;
        PrivateDevices = true;
        ExecStart = pkgs.writeShellScript "braid-ups-secrets" ''
          set -euo pipefail
          if [ ! -s /var/lib/braid/upsmon.pass ]; then
            umask 077
            head -c 24 /dev/urandom | base64 > /var/lib/braid/upsmon.pass
            chmod 0600 /var/lib/braid/upsmon.pass
          fi
        '';
      };
    };

    power.ups = {
      enable = true;
      mode = "standalone";
      package = cfg.packages.nut;

      ups.${ups.name} = {
        driver = ups.driver;
        port = ups.port;
        description = "braid-managed UPS";
      };

      # Production upsmon user: minimal. upsmon = "primary" grants exactly
      # the actions upsmon needs. No `actions = [ "SET" ]` -- per
      # reference/nut/docs/man/upsd.users.txt SET is only required by
      # upsrw clients; granting it to upsmon is unnecessary privilege.
      # Test-only users with SET are provisioned in tests/module/ups-*.nix.
      users.${ups.name} = {
        upsmon = "primary";
        passwordFile = "/var/lib/braid/upsmon.pass";
      };

      upsmon.monitor.${ups.name} = {
        system = "${ups.name}@localhost";
        powerValue = 1;
        user = ups.name;
        type = "primary";
        passwordFile = "/var/lib/braid/upsmon.pass";
      };

      # Override nixpkgs' default `shutdown now` so the shutdown runs
      # through systemd's standard stop sequence, unwinding
      # braid-online.service (decision 018) -> btrfs umount -> LUKS close.
      # nixpkgs uses mkDefault, so a plain assignment wins.
      upsmon.settings.SHUTDOWNCMD = "${pkgs.systemd}/bin/systemctl poweroff";
    };

    # Early on-battery shutdown. Not pool-bound: it must run whether or not
    # the pool is online. It forces shutdown only through `upsmon -c fsd`, so
    # POWERDOWNFLAG / ups-killpower and the braid-online unwinding happen as
    # on LB. It runs as the upsmon user because signalling upsmon needs the
    # same uid and nothing else -- no capabilities.
    systemd.services.braid-ups-guard = lib.mkIf guardEnabled {
      description = "Shut down early on UPS battery (braid)";
      wantedBy = [ "multi-user.target" ];
      after = [
        "upsd.service"
        "upsmon.service"
      ];
      # nut's sbin holds upsmon; the wrapper's PATH only has its bin.
      path = [ cfg.packages.nut ];
      startLimitIntervalSec = 0;
      serviceConfig = base // {
        Type = "simple";
        User = config.power.ups.upsmon.user;
        ExecStart = "${braidWrapped}/bin/braid ups guard";
        Restart = "always";
        RestartSec = 5;
        # Holds the on-battery clock. Preserved across restarts so a rebuild
        # switch or crash keeps the outage's deadline; /run is cleared at boot.
        RuntimeDirectory = "braid-ups-guard";
        RuntimeDirectoryPreserve = true;
        CapabilityBoundingSet = "";
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictRealtime = true;
        # upsc reaches upsd on loopback only.
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        IPAddressAllow = [ "localhost" ];
        IPAddressDeny = "any";
      };
    };
  };
}
