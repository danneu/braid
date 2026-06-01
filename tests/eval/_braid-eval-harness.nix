{ linuxPkgs, nixpkgs, linuxSystem, lockSystemdStopDeadlineSecs }:
nixpkgs.lib.nixosSystem {
  system = linuxSystem;
  modules = [
    ../../modules/braid
    {
      boot.loader.grub.devices = [ "nodev" ];
      fileSystems."/" = {
        device = "none";
        fsType = "tmpfs";
      };
      system.stateVersion = "26.05";

      braid = {
        enable = true;
        package = linuxPkgs.writeShellScriptBin "braid" "exit 0";
        inherit lockSystemdStopDeadlineSecs;
      };
    }
  ];
}
