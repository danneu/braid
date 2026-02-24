{ config, lib, pkgs, ... }:
let
  cfg = config.braid.daemon;
in
{
  options.braid.daemon = {
    enable = lib.mkEnableOption "braid daemon";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = config.braid.package;
      defaultText = lib.literalExpression "config.braid.package";
      description = "The braid package (must provide 'braid daemon' subcommand).";
    };

    socketPath = lib.mkOption {
      type = lib.types.path;
      default = "/run/braid/daemon.sock";
      description = "Path to the braid Unix socket.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = cfg.package != null;
      message = "braid.daemon.package must be set when braid.daemon.enable is true";
    }];

    systemd.sockets.braid = {
      description = "braid daemon socket";
      wantedBy = [ "sockets.target" ];
      socketConfig = {
        ListenStream = cfg.socketPath;
        Accept = false;
      };
    };

    systemd.services.braid = {
      description = "braid daemon";
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/braid daemon";
        Restart = "on-failure";
      };
    };
  };
}
