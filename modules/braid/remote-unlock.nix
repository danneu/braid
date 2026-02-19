{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  ru = cfg.remoteUnlock;
in
{
  options.braid.remoteUnlock = {
    enable = lib.mkEnableOption "SSH remote unlock in initrd";

    sshPort = lib.mkOption {
      type = lib.types.port;
      default = 2222;
      description = "SSH port for initrd remote unlock.";
    };

    authorizedKeys = lib.mkOption {
      type = lib.types.nonEmptyListOf lib.types.str;
      description = "Public keys allowed to SSH into initrd for LUKS unlock.";
    };

    hostKeys = lib.mkOption {
      type = lib.types.listOf (lib.types.either lib.types.str lib.types.path);
      default = [ "/etc/secrets/initrd/ssh_host_ed25519_key" ];
      description = "Paths to SSH host keys for the initrd SSH server. Use strings to avoid copying secrets into the Nix store.";
    };
  };

  config = lib.mkIf (cfg.enable && ru.enable) {
    assertions = [{
      assertion = ru.hostKeys != [];
      message = "braid.remoteUnlock requires at least one SSH host key. Set braid.remoteUnlock.hostKeys or generate one with: ssh-keygen -t ed25519 -f /etc/secrets/initrd/ssh_host_ed25519_key -N ''";
    }];

    boot.initrd.systemd.network.enable = true;

    boot.initrd.systemd.users.root.shell = "/bin/sh";
    boot.initrd.systemd.extraBin = {
      cryptsetup = "${pkgs.cryptsetup}/bin/cryptsetup";
    };

    boot.initrd.network = {
      enable = true;
      ssh = {
        enable = true;
        port = ru.sshPort;
        authorizedKeys = ru.authorizedKeys;
        hostKeys = ru.hostKeys;
      };
    };
  };
}
