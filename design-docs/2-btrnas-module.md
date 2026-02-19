# Plan: btrnas NixOS module — options + storage + tests

## Context

All existing tests (remote-unlock, degraded-boot, first-boot-single-disk) hand-wire 50+ lines of identical NixOS config for LUKS + btrfs + device-scan. The btrnas module captures this once. This first increment covers the core storage path only — no Samba, no initrd SSH. Stub files for those exist but are inert.

## Files to create

### Module: `modules/btrnas/`

**`modules/btrnas/default.nix`** — entrypoint
```nix
{ imports = [ ./options.nix ./storage.nix ./samba.nix ./remote-unlock.nix ]; }
```

**`modules/btrnas/options.nix`** — option schema
```nix
{ lib, ... }:
{
  options.btrnas = {
    enable = lib.mkEnableOption "btrnas encrypted storage";

    disks = lib.mkOption {
      type = lib.types.nonEmptyListOf lib.types.str;
      description = "Disk paths (/dev/disk/by-id/...) for the LUKS + btrfs pool.";
    };

    mountPoint = lib.mkOption {
      type = lib.types.str;
      default = "/mnt/storage";
      description = "Where to mount the btrfs pool.";
    };
  };
}
```

`disks` uses `nonEmptyListOf` — Nix eval rejects `disks = []` at the type level when `enable = true`. No runtime assertion needed.

**`modules/btrnas/storage.nix`** — LUKS + btrfs mount + device-scan
```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.btrnas;
  mapperNames = map builtins.baseNameOf cfg.disks;
in
{
  config = lib.mkIf cfg.enable {
    boot.initrd = {
      supportedFilesystems = [ "btrfs" ];
      systemd.enable = true;

      luks.devices = lib.genAttrs mapperNames (name: {
        device = "/dev/disk/by-id/${name}";
      });

      systemd.services.btrfs-device-scan = {
        description = "Scan for btrfs multi-device filesystems";
        after = map (n: "systemd-cryptsetup@${n}.service") mapperNames;
        requires = map (n: "systemd-cryptsetup@${n}.service") mapperNames;
        before = [ "initrd-fs.target" ];
        wantedBy = [ "initrd-fs.target" ];
        unitConfig.DefaultDependencies = false;
        serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
        path = [ pkgs.btrfs-progs ];
        script = "btrfs device scan";
      };
    };

    fileSystems.${cfg.mountPoint} = {
      device = "/dev/mapper/${builtins.head mapperNames}";
      fsType = "btrfs";
      neededForBoot = true;
      options = [
        "x-systemd.requires=btrfs-device-scan.service"
        "x-systemd.after=btrfs-device-scan.service"
      ];
    };

    # Stage-2 copy: x-systemd.requires persists across switch-root
    systemd.services.btrfs-device-scan = {
      description = "Scan for btrfs multi-device filesystems";
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ pkgs.btrfs-progs ];
      script = "btrfs device scan";
    };
  };
}
```

**`modules/btrnas/samba.nix`** — stub
```nix
# Samba integration — not yet implemented.
{ }
```

**`modules/btrnas/remote-unlock.nix`** — stub
```nix
# Initrd SSH remote unlock — not yet implemented.
{ }
```

### Tests: `tests/btrnas-module/`

**`00-disabled.nix` / `00-disabled.py`** — Module imported, `enable = false`. VM boots to multi-user. No LUKS, no btrfs mount. Single VM, no disks needed.

**`01-single-disk.nix` / `01-single-disk.py`** — `enable = true`, 1 disk. Initrd fixture pre-formats LUKS + single-disk btrfs. Auto-unlock via keyFile (no SSH). Assert: boots to multi-user, `/mnt/storage` mounted, btrfs shows `Data, single` profile, write/read works.

**`02-raid1.nix` / `02-raid1.py`** — `enable = true`, 3 disks. Initrd fixture pre-formats LUKS + btrfs RAID1. Auto-unlock via keyFile. Assert: boots to multi-user, `/mnt/storage` mounted, all 3 devices in pool, `Data, RAID1` profile, write/read works.

### Test patterns

All module tests share the same shape:

1. Import `../../modules/btrnas` (the `default.nix`)
2. Set `btrnas.enable`, `btrnas.disks`
3. Override `boot.initrd.luks.devices` with `lib.mkVMOverride` + `keyFile` for VM compatibility (qemu-vm.nix blanket-overrides LUKS devices)
4. Add initrd fixture to pre-format disks
5. Python script: `start_all()`, wait for boot, check mount, check btrfs profile

The keyFile is created with `pkgs.writeText "luks-test-key" passphrase` — a Nix store path included in the initrd. systemd-cryptsetup reads it to auto-unlock (no SSH needed).

The `mkVMOverride` is a known VM testing limitation. The module's LUKS config is correct for production but gets clobbered by qemu-vm.nix. The test re-declares the same mapping plus `keyFile`. The rest of the module's config (mount, device-scan, supportedFilesystems) is tested directly.

### flake.nix update

Add 3 new checks:
```nix
btrnas-module-disabled    = pkgs.testers.nixosTest (import ./tests/btrnas-module/00-disabled.nix);
btrnas-module-single-disk = pkgs.testers.nixosTest (import ./tests/btrnas-module/01-single-disk.nix);
btrnas-module-raid1       = pkgs.testers.nixosTest (import ./tests/btrnas-module/02-raid1.nix);
```

## Implementation order

1. Create `modules/btrnas/` — all 5 files (default, options, storage, samba stub, remote-unlock stub)
2. Create `tests/btrnas-module/00-disabled.nix` + `.py` — simplest test, confirms module evaluates
3. Run `make test-one t=btrnas-module-disabled` — expect pass
4. Create `tests/btrnas-module/01-single-disk.nix` + `.py`
5. Run `make test-one t=btrnas-module-single-disk` — expect pass
6. Create `tests/btrnas-module/02-raid1.nix` + `.py`
7. Run `make test-one t=btrnas-module-raid1` — expect pass
8. `make test` — all tests pass (existing + new)

## Verification

```
make test-one t=btrnas-module-disabled
make test-one t=btrnas-module-single-disk
make test-one t=btrnas-module-raid1
```
