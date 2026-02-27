{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];

  braid = pkgs.runCommand "braid-module" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p $out/bin
    makeWrapper ${cfg.package}/bin/braid $out/bin/braid \
      --prefix PATH : ${lib.makeBinPath toolPackages}
  '';

  # Config JSON uses snake_case to match Rust serde field names.
  configJson = builtins.toJSON {
    disks = lib.mapAttrs (_name: disk: {
      by_id = disk.byId;
    }) cfg.disks;
    mount_point = cfg.mountPoint;
  };
in
{
  config = lib.mkIf cfg.enable {
    environment.etc."braid/config.json".text = configJson;

    environment.systemPackages = lib.optional (cfg.package != null) braid;

    # Shell completion registration (dynamic, via clap_complete CompleteEnv).
    # Each shell sources a small registration script on startup; the actual
    # candidates are computed by calling back into the braid binary on each
    # tab-press, so they always reflect the current config.
    programs.bash.interactiveShellInit = ''
      source <(COMPLETE=bash ${braid}/bin/braid)
    '';
    programs.zsh.interactiveShellInit = ''
      source <(COMPLETE=zsh ${braid}/bin/braid)
    '';
    programs.fish.interactiveShellInit = ''
      COMPLETE=fish ${braid}/bin/braid | source
    '';

    # Advisory activation script — prints guidance on nixos-rebuild switch.
    # Best-effort: failures silenced, never mutates. UUID comparison is robust
    # across mapper name changes.
    system.activationScripts.braidAdvisory.text = ''
      if mountpoint -q ${cfg.mountPoint} 2>/dev/null && command -v btrfs >/dev/null 2>&1; then
        # Get pool member LUKS UUIDs
        pool_uuids=""
        for mapper_path in $(btrfs filesystem show ${cfg.mountPoint} 2>/dev/null | ${pkgs.gnugrep}/bin/grep -oP '/dev/mapper/\S+'); do
          mapper=$(basename "$mapper_path")
          underlying=$(cryptsetup status "$mapper" 2>/dev/null | ${pkgs.gnugrep}/bin/grep -oP 'device:\s+\K\S+' || true)
          if [ -n "$underlying" ]; then
            uuid=$(cryptsetup luksUUID "$underlying" 2>/dev/null || true)
            if [ -n "$uuid" ]; then
              pool_uuids="$pool_uuids $uuid"
            fi
          fi
        done

        # Get config disk LUKS UUIDs
        config_uuids=""
        ${lib.concatMapStringsSep "\n" (name: let disk = cfg.disks.${name}; in ''
          if [ -e ${disk.byId} ]; then
            uuid=$(cryptsetup luksUUID ${disk.byId} 2>/dev/null || true)
            if [ -n "$uuid" ]; then
              config_uuids="$config_uuids $uuid"
            else
              echo "braid: uninitialized: ${name} -> run: sudo braid add ${name}"
            fi
          fi
        '') (builtins.attrNames cfg.disks)}

        # Check for config disks not in pool
        for cuuid in $config_uuids; do
          if ! echo "$pool_uuids" | ${pkgs.gnugrep}/bin/grep -qw "$cuuid"; then
            # Find the name for this UUID
            ${lib.concatMapStringsSep "\n" (name: let disk = cfg.disks.${name}; in ''
              this_uuid=$(cryptsetup luksUUID ${disk.byId} 2>/dev/null || true)
              if [ "$this_uuid" = "$cuuid" ]; then
                echo "braid: new disk: ${name} -> run: sudo braid add ${name}"
              fi
            '') (builtins.attrNames cfg.disks)}
          fi
        done

        # Check for pool disks not in config
        for puuid in $pool_uuids; do
          if ! echo "$config_uuids" | ${pkgs.gnugrep}/bin/grep -qw "$puuid"; then
            echo "braid: pool has disk not in config (UUID: $puuid) -> consider removing"
          fi
        done
      fi
    '';
  };
}
