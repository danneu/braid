{ config, lib, pkgs, ... }:
let
  cfg = config.btrnas.daemon;
  btrnasd = pkgs.callPackage ../../nix/btrnas-daemon.nix {};
in
{
  options.btrnas.daemon = {
    enable = lib.mkEnableOption "btrnas daemon";

    socketPath = lib.mkOption {
      type = lib.types.path;
      default = "/run/btrnas/daemon.sock";
      description = "Path to the btrnasd Unix socket.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.btrnasd = {
      description = "btrnas daemon";
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${btrnasd}/bin/btrnasd";
        RuntimeDirectory = "btrnas";
        Environment = "BTRNAS_SOCKET=${cfg.socketPath}";
        Restart = "on-failure";
      };
    };
  };
}
