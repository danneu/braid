# Plan: `braid unlock` command

## Context

After a NixOS rebuild (or missed initrd unlock window), there's no CLI way to open LUKS volumes and mount the btrfs pool. Users must manually run `cryptsetup open` + `btrfs device scan` + `mount -o degraded`. `braid unlock` wraps this into a single idempotent command.

## Algorithm

1. Read config
2. If pool already mounted → print message, exit 0
3. Probe each config disk (`probe_config_disk`):
   - **Absent** → warn, skip
   - **PresentNotLuks** → warn "not initialized, run `braid add`", skip
   - **PresentLuks { mapper_open: true }** → skip (already open)
   - **PresentLuks { mapper_open: false }** → collect for unlocking
4. If no disks collected for unlocking AND no disks have mapper_open → error "no unlockable disks"
5. If disks need opening → prompt passphrase once (`read_passphrase`)
6. Verify passphrase against first collected disk (`verify_passphrase`) — fail fast before opening anything
7. Open each collected disk (`ensure_luks_open`)
8. `btrfs device scan` (`BtrfsDeviceScanAll`)
9. `mkdir -p mount_point`, then mount: if any disks are Absent → `mount -o degraded`; otherwise plain `mount`. No fallback — the probe results already tell us which mode is correct.
10. Print summary lines

### Output format

```
[ok  ]  aaa       unlocked
[skip]  bbb       not found (unplugged?)
[ok  ]  ccc       already open
[ok  ]  pool      mounted /mnt/storage
```

Already done:
```
pool already mounted at /mnt/storage
```

### Args

Only passphrase args — no `--dry-run`, `--yes`, or `--progress` (nothing destructive, nothing to confirm, no long ops).

```
braid unlock [--passphrase-stdin] [--passphrase-file <path>]
```

## Files to modify

### 1. `cli/src/cmd.rs` — add `MountWithOptions` variant

New variant (leaves existing `Mount` untouched):

```rust
MountWithOptions {
    device: String,
    mount_point: String,
    options: Vec<String>,
},
```

`RealRunner::run()` arm:

```rust
CmdRequest::MountWithOptions { device, mount_point, options } => {
    let mut args = Vec::new();
    let opts_str = options.join(",");
    if !options.is_empty() {
        args.push("-o");
        args.push(&opts_str);
    }
    args.push(device);
    args.push(mount_point);
    RealRunner::exec("mount", &args)
}
```

Update `cmd_request_declares_expected_commands` test (count 28→29).

### 2. `cli/src/unlock.rs` — new file

- `UnlockError` enum (wraps `ProbeError`, `LuksError`, `PoolError`, `ConfigError`, `CmdError`)
- `cmd_unlock(runner, fs, config, passphrase_stdin, passphrase_file) -> Result<(), UnlockError>`
- Follows algorithm above
- Mount logic: if any config disks are Absent → `MountWithOptions { options: ["degraded"] }`; if all present → plain `Mount`. Decision based on probe results, no fallback cascade.

Key reuse:
- `luks::read_passphrase()`, `luks::ensure_luks_open()`, `luks::verify_passphrase()` (`cli/src/luks.rs`)
- `probe::probe_config_disk()`, `probe::probe_pool()` (`cli/src/probe.rs`)
- `config::mapper_name()` (`cli/src/config.rs`)
- `CmdRequest::BtrfsDeviceScanAll` (`cli/src/cmd.rs`)

### 3. `cli/src/main.rs` — add subcommand

```rust
/// Unlock LUKS volumes and mount the pool
Unlock(UnlockArgs),
```

`UnlockArgs`: just `passphrase_stdin: bool` and `passphrase_file: Option<PathBuf>`.

Dispatch: read config, call `cmd_unlock(runner, fs, &config, ...)`.

### 4. `cli/src/lib.rs` — register module

Add `pub mod unlock;`

### 5. `flake.nix` — register test

```nix
braid-unlock = pkgs.testers.nixosTest (
  import ./tests/cli/braid-unlock.nix {
    braid = linuxCrane.braid;
  }
);
```

### 6. `tests/cli/braid-unlock.nix` — VM test setup

4 virtual disks (1024MB): disk1-disk3 for the pool, disk4 as a raw unformatted disk for test 6. braid + cryptsetup + btrfs-progs. config.json with disk1/disk2/disk3.

### 7. `tests/cli/braid-unlock.py` — test script

**Setup phase**: Use `braid add` to create a 3-disk RAID1 pool, write test data, then close everything (unmount + cryptsetup close all mappers).

**Test cases**:

| # | Name | Scenario | Assert |
|---|------|----------|--------|
| 1 | Happy path | All 3 locked+unmounted → `braid unlock` | All mappers open, pool mounted, data intact |
| 2 | Idempotent | Run `braid unlock` again | Exit 0, no errors |
| 3 | Partial state | Close 1 mapper, unmount → `braid unlock` | Reopens the 1 closed mapper, remounts |
| 4 | Missing disk | `rm /dev/disk/by-id/virtio-disk3`, close all, unmount → `braid unlock` | Opens 2/3, mounts degraded, data intact |
| 5 | Wrong passphrase | Bad passphrase via stdin | Exit 1, no mappers opened |
| 6 | Not initialized | Write a temp config pointing at disk4 (raw, never `braid add`'d) → `braid unlock --config /tmp/raw.json` | Exit 1, clear error about disk not being initialized |

## Verification

1. `just test-rust` — Rust unit tests pass (new `cmd_request_declares_expected_commands` count)
2. `just test braid-unlock` — NixOS VM test passes all 6 subtests
3. Manual: on the real NAS, `sudo braid unlock` prompts for passphrase, opens LUKS, mounts pool
