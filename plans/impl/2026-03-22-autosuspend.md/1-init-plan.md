# Plan: Auto-suspend with `braid idle` + autosuspend

## Context

HDDs in a btrfs RAID1 NAS can't rely on per-drive spindown — btrfs housekeeping, smartd, and braid-monitor wake them frequently. The solution is whole-system suspend-to-RAM when idle, with Wake-on-LAN for on-demand access and RTC wakeup for scheduled maintenance (scrub).

This plan adds two things:
1. **`braid idle`** — a CLI command that checks if any btrfs exclusive operations (scrub, balance, replace) are running
2. **`sleep.nix`** — a NixOS module that wires autosuspend (existing nixpkgs daemon) to `braid idle` plus SSH/SMB/NFS checks

autosuspend handles: daemon loop, idle countdown, suspend/wake, systemd inhibitors, built-in checks. braid handles: btrfs-specific checks. They connect via autosuspend's `ExternalCommand` check type.

smartd and braid-monitor run opportunistically during wake windows — they don't wake the system and don't prevent suspend.

---

## Part 1: `braid idle` CLI command

### New file: `cli/src/idle.rs`

**Signature** (follows `monitor.rs` pattern):
```rust
pub fn cmd_idle<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<IdleResult, IdleError>
```

**Result/Error types:**
```rust
pub enum IdleResult {
    /// Pool is idle — no exclusive operations running.
    Idle,
    /// Pool not mounted — nothing to protect — allow suspend.
    PoolOffline,
    Busy(BusyReason),
}

pub enum BusyReason {
    ScrubRunning { pct: Option<u8> },
    BalanceRunning { pct_left: u8 },
    BalancePaused { pct_left: u8 },
    ReplaceRunning { pct: f64 },
}

pub enum IdleError {
    Cmd(CmdError),
    Parse(ParseError),
}
```

**Logic:**
1. Run `FindmntJson` — if mount point absent or not btrfs → `Ok(PoolOffline)`. (Simpler than `probe_pool` which also enumerates devices/LUKS — unnecessary here.)
2. Run `BtrfsScrubStatus` → parse → if `ScrubState::Running` → `Ok(Busy(ScrubRunning))`
3. Run `BtrfsBalanceStatus` → parse → if `Running` or `Paused` → `Ok(Busy(...))`
4. Run `BtrfsReplaceStatus` → parse → if `Running` → `Ok(Busy(ReplaceRunning))`
5. All clear → `Ok(Idle)`

**Fail-closed:** Any command/parse error → `Err(IdleError)` → exit 2.

**Display for BusyReason:** Human-readable, e.g. `"scrub running (45%)"`, `"balance paused (58% left)"`.

### Paused balance = busy

A paused balance holds the btrfs exclusive op lock. `check_no_exclusive_op` in `preflight.rs:27` already treats paused as "refuse." Same logic here — don't suspend mid-pause.

### Exit codes

| Code | Meaning | autosuspend sees (via `!`) |
|------|---------|---------------------------|
| 0 | idle | exit 1 → no activity → allow suspend |
| 1 | busy | exit 0 → activity → block suspend |
| 2 | error | exit 0 → activity → block suspend (fail-closed) |

Natural Unix convention (0 = success = "yes, idle"). Inverted in the ExternalCommand wrapper.

### Wire into main.rs

Add `Idle` to `Commands` enum (no args — same as `Monitor`, `Lock`). Dispatch:
```rust
Commands::Idle => {
    let config = match config_read(Path::new(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let runner = RealRunner;
    match braid_cli::idle::cmd_idle(&runner, config.mount_point().as_str()) {
        Ok(braid_cli::idle::IdleResult::PoolOffline) => {
            println!("idle: pool is offline");
            std::process::exit(0);
        }
        Ok(braid_cli::idle::IdleResult::Idle) => {
            println!("idle: pool is idle");
            std::process::exit(0);
        }
        Ok(braid_cli::idle::IdleResult::Busy(reason)) => {
            println!("busy: {reason}");
            std::process::exit(1);
        }
        Err(e) => {
            print_cli_error(&e.to_string());
            std::process::exit(2);
        }
    }
}
```

### Wire into lib.rs

Add `pub mod idle;`

### Existing code reused

| What | Location |
|------|----------|
| `parse_findmnt_json` | `cli/src/parse/findmnt.rs` |
| `parse_btrfs_scrub_status` | `cli/src/parse/btrfs_scrub_status.rs` |
| `parse_btrfs_balance_status` | `cli/src/parse/btrfs_balance_status.rs` |
| `parse_btrfs_replace_status` | `cli/src/parse/btrfs_replace_status.rs` |
| `CmdRequest::{FindmntJson, BtrfsScrubStatus, BtrfsBalanceStatus, BtrfsReplaceStatus}` | `cli/src/cmd.rs` |
| `MockRunner` | `cli/src/cmd.rs:750` |
| `ScrubState, BalanceState, ReplaceState` | `cli/src/parse/types.rs` |

### Unit tests (in `idle.rs`)

Each test uses `MockRunner` with seeded outputs. Only `FindmntJson` needed for pool state (no BtrfsFilesystemShow/cryptsetup mocks — we skip `probe_pool`).

1. **idle when pool offline** — findmnt returns empty → `PoolOffline`
2. **idle when all ops quiet** — mounted + scrub=Completed + balance=None + replace=None → `Idle`
3. **busy: scrub running** — scrub=Running{pct:Some(45)} → `Busy(ScrubRunning{pct:Some(45)})`
4. **busy: balance running** — balance=Running{pct_left:70} → `Busy(BalanceRunning{pct_left:70})`
5. **busy: balance paused** — balance=Paused{pct_left:58} → `Busy(BalancePaused{pct_left:58})`
6. **busy: replace running** — replace=Running{pct:45.3} → `Busy(ReplaceRunning{pct:45.3})`
7. **error on probe failure** — no mock for scrub → `Err(IdleError::Cmd(..))`
8. **short-circuits on first busy** — scrub=Running, no balance/replace mocks → returns `Busy` without querying further (validates short-circuit)

---

## Part 2: `sleep.nix` NixOS module

### New file: `modules/braid/sleep.nix`

**Header** (same `let` block pattern as `monitor.nix`):
```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.braid;
  braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
in
```

**Options** — just `enable` + `idleTime`. Interval is hardcoded at 60s:
```nix
options.braid.sleep = {
  enable = lib.mkEnableOption "auto-suspend when NAS is idle";  # default: false

  idleTime = lib.mkOption {
    type = lib.types.ints.positive;
    default = 900;  # 15 minutes
    description = "Seconds of idle time before suspending.";
  };
};
```

`enable` defaults to false — auto-suspend has a hard prerequisite (WoL in BIOS/NIC firmware) that NixOS can't configure. User must opt in.

**Config block** (`lib.mkIf (cfg.enable && cfg.sleep.enable)`):

Sets `services.autosuspend` options. Verified against actual source:
- NixOS module: `nixos/modules/services/misc/autosuspend.nix` in nixpkgs
- `settings` = freeform INI attrs for `[general]` section (has typed `suspend_cmd`/`wakeup_cmd` with defaults)
- `checks` = `attrsOf checkType` where checkType has typed `enabled` (default: true), `class` (nullOr enum), plus freeform attrs
- `wakeups` = `attrsOf wakeupType`, same pattern
- Config generation: `{ general = cfg.settings; } // checks // wakeups` → INI file

Python source config keys verified:
- `ExternalCommand`: reads `config["command"]` (`checks/command.py:28`)
- `SystemdTimer` wakeup: reads `config["match"]`, compiled as regex (`checks/systemd.py:60`)
- `ActiveConnection`: reads `config["ports"]`, comma-separated ints (`checks/linux.py:43`)
- `Smb`: zero-config, ignores config section entirely (`checks/smb.py:13-15`)

```nix
services.autosuspend = {
  enable = true;

  settings = {
    interval = 60;
    idle_time = cfg.sleep.idleTime;
  };

  checks = lib.mkMerge [
    {
      # btrfs exclusive ops (scrub, balance, replace).
      # Fully qualified paths — autosuspend runs this outside braid's wrapper,
      # so PATH is not guaranteed to include coreutils or a shell.
      BraidPool = {
        class = "ExternalCommand";
        command = "${pkgs.coreutils}/bin/timeout 10 ${pkgs.bash}/bin/bash -c '! ${braidWrapped}/bin/braid idle'";
      };
      # SSH sessions always block suspend — braid requires SSH for unlock,
      # and an active session means someone is working on the machine.
      SSH = {
        class = "ActiveConnection";
        ports = "22";
      };
    }
    (lib.mkIf config.services.samba.enable {
      Smb = {
        class = "Smb";
      };
    })
    (lib.mkIf config.services.nfs.server.enable {
      NfsConnections = {
        class = "ActiveConnection";
        ports = "2049";
      };
    })
  ];

  wakeups = {
    BtrfsScrub = {
      class = "SystemdTimer";
      match = "btrfs-scrub@.*";
    };
  };
};
```

**Exit code inversion:** `timeout 10 bash -c '! braid idle'` (fully qualified in actual config)
- braid exit 0 (idle) → `!` → exit 1 → autosuspend: no activity → allow suspend
- braid exit 1 (busy) → `!` → exit 0 → autosuspend: activity → block suspend
- braid exit 2 (error) → `!` → exit 0 → autosuspend: activity → block suspend (fail-closed)
- timeout (exit 124) → autosuspend: activity → block suspend (fail-closed)

### Register in `modules/braid/default.nix`

Add `./sleep.nix` to imports list.

---

## Part 3: Testing

### Rust unit tests

In `cli/src/idle.rs` (8 tests described above). Run with `just test-rust`.

### NixOS VM test: `braid idle` CLI integration

**`tests/cli/braid-idle.nix`** + **`tests/cli/braid-idle.py`**

Lives in `tests/cli/` (CLI command test, not module config test). Uses initrd-fixture with 2-disk RAID1 pool. Pattern matches existing `tests/cli/braid-monitor.nix`.

Tests:
1. `braid idle` exits 0 when pool offline
2. Unlock pool, `braid idle` exits 0 when idle
3. Start scrub, `braid idle` exits 1 (racy on small VM disks — lenient assertion; unit tests are authoritative for busy path)

### NixOS VM test: `sleep.nix` module config

**`tests/module/braid-sleep.nix`** + **`tests/module/braid-sleep.py`**

Tests NixOS config generation. Needs a minimally valid braid config to evaluate (the `braid.sleep` options live under `braid`, which requires `braid.enable`, `braid.package`, and `braid.disks`). Pattern matches `tests/module/smartd-config.nix`.

```nix
braid = {
  enable = true;
  package = braid;
  disks = lib.genAttrs diskNames (d: { byId = "/dev/disk/by-id/virtio-${d}"; });
  sleep.enable = true;
};
services.samba = {
  enable = true;
  settings.storage = {
    path = "/mnt/storage";
    browseable = "yes";
    "read only" = "no";
    "guest ok" = "yes";
  };
};
virtualisation.emptyDiskImages = [
  { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
  { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
];
```

Tests:
1. `autosuspend` service unit exists and is active
2. Config file contains `[check.BraidPool]` with `braid idle`
3. BraidPool command uses fully qualified `/nix/store/` paths for timeout and bash (regression guard)
4. Config file contains `[check.SSH]` with port 22 (always present)
5. Config file contains `[check.Smb]` (auto-detected from samba)
6. Config file contains `[wakeup.BtrfsScrub]` with `SystemdTimer`

### Register in `flake.nix`

Add both tests to `checksFor`:
```nix
braid-idle = pkgs.testers.nixosTest (import ./tests/cli/braid-idle.nix { ... });
braid-sleep = pkgs.testers.nixosTest (import ./tests/module/braid-sleep.nix { ... });
```

---

## Part 4: Documentation

### README.md

Add auto-suspend section with minimal config example and WoL note.

### `docs/decisions/016-auto-suspend.md`

Status: Active. Document:
- Why autosuspend (external daemon) rather than custom braid timer
- Why `braid idle` as a separate command (keeps btrfs logic in Rust, clean interface)
- Fail-closed exit code design
- SSH always-on, SMB/NFS auto-detection rationale
- smartd/monitor run opportunistically — no wakeup, no block

---

## Implementation sequence

1. `cli/src/idle.rs` — implement `cmd_idle` + unit tests
2. `cli/src/lib.rs` — add `pub mod idle;`
3. `cli/src/main.rs` — add `Idle` variant + dispatch
4. `just test-rust` — verify unit tests
5. `modules/braid/sleep.nix` — implement module
6. `modules/braid/default.nix` — add import
7. `tests/cli/braid-idle.nix` + `.py` — CLI integration test
8. `tests/module/braid-sleep.nix` + `.py` — module config test
9. `flake.nix` — register tests
10. `just test braid-idle braid-sleep` — verify VM tests
11. `README.md` — add sleep section
12. `docs/decisions/016-auto-suspend.md` — decision record

---

## Files modified/created

| File | Action |
|------|--------|
| `cli/src/idle.rs` | **Create** — core idle check + unit tests |
| `cli/src/lib.rs` | Edit — add `pub mod idle;` |
| `cli/src/main.rs` | Edit — add `Idle` command variant + dispatch |
| `modules/braid/sleep.nix` | **Create** — NixOS module |
| `modules/braid/default.nix` | Edit — add `./sleep.nix` to imports |
| `tests/cli/braid-idle.nix` | **Create** — test config |
| `tests/cli/braid-idle.py` | **Create** — test script |
| `tests/module/braid-sleep.nix` | **Create** — test config |
| `tests/module/braid-sleep.py` | **Create** — test script |
| `flake.nix` | Edit — register tests |
| `README.md` | Edit — add sleep section |
| `docs/decisions/016-auto-suspend.md` | **Create** — ADR |
