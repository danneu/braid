{ config, lib, pkgs, ... }:
let
  cfg = config.braid.daemon;
  braid = pkgs.buildGoModule {
    pname = "braid";
    version = "0.1.0";
    src = ../../daemon;
    vendorHash = null;
    postInstall = ''
      mv $out/bin/daemon $out/bin/braid
    '';
  };
in
{
  options.braid.daemon = {
    enable = lib.mkEnableOption "braid daemon";

    socketPath = lib.mkOption {
      type = lib.types.path;
      default = "/run/braid/daemon.sock";
      description = "Path to the braid Unix socket.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.braid = {
      description = "braid daemon";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${braid}/bin/braid";
        RuntimeDirectory = "braid";
        Environment = "BRAID_SOCKET=${cfg.socketPath}";
        Restart = "on-failure";
      };
    };
  };
}
