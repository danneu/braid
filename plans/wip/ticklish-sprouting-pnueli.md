# Plan: VM test for LUKS UUID mismatch on unlock

## Context

The UUID mismatch check in `cli/src/mount.rs:81-91` is braid's guard against silently mounting wrong data from a swapped or reformatted drive. It's only covered by Rust unit tests (`mount_luks_uuid_mismatch_closed`, `mount_luks_uuid_mismatch_already_open`). A VM test is needed to verify the real `cryptsetup luksUUID` probe → comparison → fatal error pipeline end-to-end.

## Change

Create a **standalone VM test** with its own setup — not appended to `braid-unlock.py`. This avoids hidden dependencies on earlier subtests and avoids leaving a destroyed pool at the tail of a shared script.

### New files

**`tests/cli/unlock-uuid-mismatch.nix`** — VM config (2 disks suffice):
```nix
{ braid }:
{
  name = "unlock-uuid-mismatch";
  nodes.machine = { pkgs, ... }: {
    virtualisation.emptyDiskImages = [
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 1024; driveConfig.deviceExtraOpts.serial = "disk2"; }
    ];
    environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs ];
    environment.etc."braid/config.json".text = builtins.toJSON {
      mount_point = "/mnt/storage";
    };
  };
  testScript = builtins.readFile ./unlock-uuid-mismatch.py;
}
```

**`tests/cli/unlock-uuid-mismatch.py`** — test script with self-contained setup:

1. **Setup:** `braid add` disk1 and disk2 to create a 2-disk RAID1 pool
2. **Enrich:** Tear down (unmount + close LUKS). Run `braid unlock` to enrich `pool.json` with `luks_uuid` fields. Assert `disk2.luks_uuid` is present.
3. **Tear down again:** unmount + close all LUKS mappers
4. **Reformat disk2:** `echo -n {passphrase} | cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk2` — gives disk2 a new UUID, same passphrase
5. **Read new UUID:** `cryptsetup luksUUID /dev/disk/by-id/virtio-disk2`, sanity-assert it differs from stored
6. **Attempt unlock:** `braid unlock` → expect failure

**Assertions:**
- Non-zero exit code
- Output contains `"LUKS UUID mismatch"`
- Output contains `"disk2"` (identifies the right disk)
- Output contains the original UUID (expected) and the new UUID (found)
- Pool is NOT mounted (`mountpoint -q /mnt/storage` fails)
- No LUKS mappers opened for any disk (`/dev/mapper/braid-{disk1,disk2}` both absent)

### Modified file

**`flake.nix`** — register the new test after `braid-unlock` (around line 304):
```nix
unlock-uuid-mismatch = pkgs.testers.nixosTest (
  import ./tests/cli/unlock-uuid-mismatch.nix {
    braid = linuxCrane.braid;
  }
);
```

### Why no mappers should be open

The probe loop in `open_and_mount_pool` iterates `BTreeMap` keys in order: disk1, disk2. disk1 passes (UUID matches), then disk2 triggers the mismatch error, returning immediately — before any `cryptsetup open` calls in step 4 of the function. Both mappers should be absent because the error fires during probing, not after partial opening.

## Files

- `tests/cli/unlock-uuid-mismatch.nix` — **new** — VM config (2 disks)
- `tests/cli/unlock-uuid-mismatch.py` — **new** — test script
- `flake.nix` — add test registration (~line 304)
- `cli/src/mount.rs:81-91` — the code path under test (read-only reference)

## Verification

```
just test unlock-uuid-mismatch
```

If it fails, add `-v` for VM logs:
```
just test unlock-uuid-mismatch -v
```
