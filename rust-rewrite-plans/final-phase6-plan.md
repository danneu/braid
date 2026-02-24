# Phase 6: `braid init-disk` Rust Implementation

## Context

The Rust CLI has `plan`, `apply`, and `status` fully implemented and passing VM tests. `init-disk` is the last stubbed command (`main.rs:71` prints "not yet implemented"). `init-disk` is the **only command** that may call `cryptsetup luksFormat` — this is a hard safety invariant (Principle 3). This phase implements the init-disk subcommand, completing the Rust CLI migration.

---

## Safety contract (from bash, 7 steps)

1. **Device exists** as a block device (`-b "$by_id"`)
2. **Declared in config** — `config.disks` contains the path
3. **Not in mounted pool** — if pool mounted AND target is LUKS, check its UUID against pool members' UUIDs
4. **LUKS header probe** — `cryptsetup isLuks`; if already LUKS, refuse unless `--force` + `BRAID_CONFIRM='reformat this disk'`
5. **Require passphrase** — `BRAID_PASSPHRASE` must be set
6. **Single-passphrase check** — find an existing member (open mapper first, then LUKS-formatted config disk), verify via `cryptsetup open --test-passphrase --key-file=-`
7. **Format** — `cryptsetup luksFormat --batch-mode --key-file=- $BRAID_LUKS_OPTS "$by_id"`

---

## New CmdRequest variants

Add to `cli/src/cmd.rs`:

```rust
CryptsetupLuksFormat { device: String, extra_opts: Vec<String> },
CryptsetupTestPassphrase { device: String },
```

**`CryptsetupLuksFormat`**: Must use `run_with_stdin` (passphrase via stdin). `extra_opts` carries parsed `BRAID_LUKS_OPTS`. Like `CryptsetupLuksOpen`, calling `run()` (without stdin) returns an error.

**`CryptsetupTestPassphrase`**: Must use `run_with_stdin`. Runs `cryptsetup open --test-passphrase --key-file=- <device>`. Exit 0 = passphrase matches.

**RealRunner implementations:**
- `LuksFormat`: `cryptsetup luksFormat --batch-mode --key-file=- <extra_opts...> <device>`
- `TestPassphrase`: `cryptsetup open --test-passphrase --key-file=- <device>`

Update `cmd_request_declares_expected_commands` test: bump count from 22 → 24.

---

## Filesystem trait extension

Add `is_block_device()` to `cli/src/probe.rs`:

```rust
pub trait Filesystem {
    fn exists(&self, path: &str) -> bool;
    fn is_block_device(&self, path: &str) -> bool;
}
```

`RealFilesystem`: `std::fs::metadata(path).map(|m| m.file_type().is_block_device()).unwrap_or(false)` — requires `use std::os::unix::fs::FileTypeExt`.

`MockFs`: add a `block_devices: Vec<String>` field. `is_block_device()` returns true only if in that list.

**Downstream impact**: Update all existing `MockFs` definitions (in `probe.rs`, `apply.rs`, `status.rs` tests) to include the new field. Existing tests set `block_devices: vec![]` (they don't call `is_block_device`).

---

## Module: `cli/src/init_disk.rs`

### Error type

```rust
#[derive(Debug, thiserror::Error)]
pub enum InitDiskError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
}
```

### Public API

```rust
/// Entry point: reads env vars, delegates to cmd_init_disk_with.
pub fn cmd_init_disk<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    by_id_path: &str,
    force: bool,
) -> Result<(), InitDiskError>
```

Reads from env:
- `BRAID_PASSPHRASE` → required, error if empty/unset
- `BRAID_CONFIRM` → required when `force`, must equal `"reformat this disk"`
- `BRAID_LUKS_OPTS` → optional, split by whitespace into `Vec<String>`

Delegates to:

```rust
pub(crate) fn cmd_init_disk_with<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    by_id_path: &str,
    force: bool,
    passphrase: &str,
    confirm: &str,
    luks_extra_opts: &[String],
) -> Result<(), InitDiskError>
```

### Staged flow inside `cmd_init_disk_with`

1. **Block device check**: `if !fs.is_block_device(by_id_path)` → error "Device not found or not a block device: ..."
2. **Declared check**: `if !config.disks.iter().any(|d| d.0 == by_id_path)` → error "Disk ... is not declared in config"
3. **Pool membership check** (fail-closed):
   - First, `MountpointCheck { path: config.mount_point }` — if exit ≠ 0 → not mounted, skip membership check entirely
   - If mounted, call `probe_pool(runner, &config.mount_point)`:
     - `Ok(pool)` where `pool.mounted` → proceed with UUID check below
     - `Err(ProbeError::NotBtrfs { .. })` → not a btrfs pool, skip membership check
     - `Err(other)` → **fatal**, return error (fail-closed: cannot verify pool membership)
   - If pool mounted AND target is LUKS (`CryptsetupIsLuks` exit 0):
     - Get target UUID via `CryptsetupLuksUuid`
     - Check if any `pool.devices` has matching `luks_uuid`
     - If match → error "Disk ... is currently part of the mounted pool"
4. **LUKS header probe**: `CryptsetupIsLuks { device }` — if exit 0 (is LUKS):
   - If `!force` → error "Disk ... already has a LUKS header. Use --force to re-format"
   - If `force` but `confirm != "reformat this disk"` → error "--force requires BRAID_CONFIRM='reformat this disk'"
5. **Passphrase requirement**: already validated in `cmd_init_disk` (before calling `_with`)
6. **Single-passphrase check**: `find_passphrase_target(runner, fs, config, by_id_path)` → `Option<String>`:
   - First pass: find any config disk with an open mapper (`fs.exists("/dev/mapper/<name>")` + `CryptsetupStatus` is active) → return that disk's by-id path
   - Second pass: find any config disk (excluding target) that is a block device and passes `CryptsetupIsLuks` → return that disk's by-id path
   - If found: `CryptsetupTestPassphrase { device: member }` with passphrase via stdin. If exit ≠ 0 → error "Passphrase does not match existing pool member ..."
7. **Format**: print "Formatting {by_id_path} with LUKS...", then `CryptsetupLuksFormat { device, extra_opts }` via `run_with_stdin` with passphrase. If error → propagate.
8. **Success**: print "LUKS format complete: {by_id_path}\nNext step: run 'braid apply' to open and add this disk to the pool."

### Private helpers

```rust
fn find_passphrase_target<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    exclude_path: &str,
) -> Option<String>
```

**Fail-closed search** — returns `Result<Option<String>, InitDiskError>`:

Two-pass search:
1. Open mapper: for each config disk, derive mapper name, check `fs.exists("/dev/mapper/{name}")`. If found, check `CryptsetupStatus` — if active, return `Ok(Some(by_id_path))`. If check errors, record the error and continue.
2. LUKS-formatted: for each config disk (except target), check `fs.is_block_device()` + `CryptsetupIsLuks` exit 0 → return `Ok(Some(by_id_path))`. If check errors, record the error and continue.

**Fail-closed rule**: If no candidate was successfully verified BUT at least one candidate existed (mapper was present or disk was a block device) and ALL checks for those candidates errored → return `Err(InitDiskError)`. Only return `Ok(None)` when there are genuinely no candidates (no mapper paths exist, no other block devices).

Note: the bash impl checks open mappers across ALL config disks (including target), then falls back to LUKS-formatted config disks (excluding target). Match this behavior.

---

## Wire into `main.rs`

Replace stub at line 71:

```rust
Commands::InitDisk(args) => {
    let config = match config_read(Path::new(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let runner = RealRunner;
    let fs = RealFilesystem;
    if let Err(e) = braid_cli::init_disk::cmd_init_disk(
        &runner, &fs, &config, &args.by_id_path, args.force,
    ) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
```

Add `pub mod init_disk;` to `cli/src/lib.rs`.

---

## Unit tests in `init_disk.rs`

All use `MockRunner` + `MockFs` with inline data. Use `cmd_init_disk_with` to control env vars.

**Safety gate tests:**
- `init_disk_device_not_found` — `is_block_device` returns false → error "not found or not a block device"
- `init_disk_not_declared` — device exists but not in config.disks → error "not declared"
- `init_disk_in_pool_refuses` — pool mounted, target UUID matches pool device → error "currently part of the mounted pool"
- `init_disk_luks_without_force` — isLuks succeeds, force=false → error "already has a LUKS header"
- `init_disk_force_wrong_confirm` — force=true, confirm="wrong" → error about BRAID_CONFIRM
- `init_disk_force_correct_confirm` — force=true, confirm="reformat this disk" → proceeds to format
- `init_disk_passphrase_mismatch` — existing member exists, test-passphrase fails → error "does not match"
- `init_disk_passphrase_match` — existing member, test-passphrase succeeds → proceeds

**Happy path tests:**
- `init_disk_fresh_no_existing_member` — first disk, no members to verify against → format succeeds
- `init_disk_with_existing_member` — second disk, passphrase matches → format succeeds
- `init_disk_non_luks_target_in_pool_not_checked` — target is not LUKS → pool membership check skipped (can't be a member)

**Pool membership check (fail-closed):**
- `init_disk_pool_probe_error_is_fatal` — `MountpointCheck` succeeds (mounted) + `probe_pool` returns non-NotBtrfs error → `Err`, not silently skipped
- `init_disk_not_btrfs_skips_membership` — `MountpointCheck` succeeds + `probe_pool` returns `NotBtrfs` → no membership error
- `init_disk_not_mounted_skips_membership` — `MountpointCheck` fails → membership check skipped entirely

**Passphrase target search (fail-closed):**
- `find_target_prefers_open_mapper` — open mapper found → returns Ok(Some(disk))
- `find_target_falls_back_to_luks_disk` — no open mapper, but another config disk has LUKS header → returns Ok(Some(disk))
- `find_target_excludes_self` — only config disk is the target → returns Ok(None)
- `find_target_none_when_no_members` — no disks exist → returns Ok(None)
- `find_target_all_candidates_error` — mapper exists but CryptsetupStatus errors for all candidates → returns Err (fail-closed)

---

## NixOS VM test

**`tests/19-braid-init-disk-rust.nix`** — follows existing Rust test pattern:

```nix
{ braid-rust }:
{
  name = "braid-init-disk-rust";
  nodes.machine = { pkgs, ... }: let
    braid-cli = pkgs.writeShellApplication { ... };
  in {
    virtualisation.emptyDiskImages = [
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      { size = 256; driveConfig.deviceExtraOpts.serial = "disk3"; }
    ];
    environment.systemPackages = [ braid-cli braid-rust pkgs.cryptsetup pkgs.btrfs-progs pkgs.jq ];
  };
  testScript = builtins.readFile ./braid-init-disk-rust.py;
}
```

**`tests/braid-init-disk-rust.py`** — mirrors bash test with `braid-rust init-disk`:

Phase 1 — Safety contract:
- Formats declared non-LUKS disk (verify with `cryptsetup isLuks`)
- Refuses undeclared disk
- Refuses already-LUKS without --force
- --force without BRAID_CONFIRM fails
- --force with wrong BRAID_CONFIRM fails
- --force with correct BRAID_CONFIRM succeeds
- Refuses disk currently in pool
- Wrong passphrase against existing member fails
- Correct passphrase succeeds

**`flake.nix`** — register `braid-init-disk-rust` after `braid-status-rust`.

---

## Files modified

| File | Change |
|------|--------|
| `cli/src/init_disk.rs` | **New** — error type, cmd_init_disk, safety gates, format, all unit tests |
| `cli/src/lib.rs` | Add `pub mod init_disk;` |
| `cli/src/main.rs` | Wire `Commands::InitDisk` → `cmd_init_disk(runner, fs, config, by_id_path, force)` |
| `cli/src/cmd.rs` | Add `CryptsetupLuksFormat`, `CryptsetupTestPassphrase` to CmdRequest + RealRunner + MockRunner test |
| `cli/src/probe.rs` | Add `is_block_device()` to Filesystem trait + RealFilesystem impl; update MockFs in tests |
| `cli/src/apply.rs` | Update MockFs in tests to include `block_devices` field |
| `cli/src/status.rs` | Update MockFs in tests to include `block_devices` field |
| `tests/19-braid-init-disk-rust.nix` | **New** — VM test config |
| `tests/braid-init-disk-rust.py` | **New** — VM test assertions |
| `flake.nix` | Register `braid-init-disk-rust` check |

## Acceptance criteria

1. `cargo test -p braid-cli` — all unit tests pass (existing + new init_disk tests)
2. `cargo test -p braid-cli --test golden_nixos_25_11` — golden tests still pass
3. `make test-one t=braid-init-disk-rust` — VM integration test passes
4. `make test` — full suite still passes
5. `plan_no_format_action_exists` test still passes (luksFormat never reachable from plan/apply)
