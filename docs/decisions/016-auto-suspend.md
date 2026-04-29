# Auto-Suspend via autosuspend + `braid idle`

Status: Active

## Context

HDDs in a btrfs RAID1 NAS can't rely on per-drive spindown — btrfs periodic commits (every 30s), smartd polling, and braid-monitor health checks wake drives frequently. The user wants the NAS to be quiet and low-power when not in use, and responsive when needed.

## Decision

### Whole-system suspend-to-RAM

The entire NixOS machine suspends when idle. This preserves LUKS keys and the mounted btrfs pool in RAM — no re-unlock ceremony on wake. Drives stop, CPU stops, fans stop. Wake via Wake-on-LAN or RTC alarm.

### autosuspend as the daemon

[autosuspend](https://github.com/languitar/autosuspend) is an existing Python daemon in nixpkgs that handles idle countdown, periodic activity checks, and RTC wakeup scheduling. When the host is idle, it executes the configured suspend command (typically `systemctl suspend`). systemd/logind then applies the actual sleep request semantics, including honoring active high-level `sleep` inhibitor locks. Writing a custom daemon for this would reimplement what autosuspend already does well.

braid configures autosuspend via the existing NixOS module (`services.autosuspend`). The user writes `braid.autoSuspend.enable = true;` and gets sensible defaults.

### `braid idle` as the btrfs check

A separate CLI command (`braid idle`) checks for an in-flight scrub plus any kernel exclusive operation (`balance`, `balance paused`, `device add`, `device remove`, `device replace`, `resize`, `swap activate`). The exclusive-operation states are read from `/sys/fs/btrfs/<fsid>/exclusive_operation` -- the same source `preflight.rs` uses for mutating commands -- so the two code paths cannot disagree about what counts as busy. Scrub is read separately via `btrfs scrub status` because scrub is not in the kernel's exclusive-operation set (see `reference/btrfs-progs/common/utils.c:1188-1197`). autosuspend calls `braid idle` via `ExternalCommand` check.

Why a separate command rather than inline shell in autosuspend config:
- braid already has the parser for `btrfs scrub status` and the sysfs read helper
- Fail-closed behavior (exit 2 on any probe error -> block suspend) is easier to get right in Rust than in shell
- Testable with unit tests via MockRunner + a `Filesystem` mock

### Exit code inversion

`braid idle` follows natural Unix convention (exit 0 = success = "yes, idle"). autosuspend's ExternalCommand convention is inverted (exit 0 = activity detected). The NixOS module bridges this with `bash -c '! braid idle'`:

- braid exit 0 (idle) → `!` → exit 1 → autosuspend: allow suspend
- braid exit 1 (busy) → `!` → exit 0 → autosuspend: block suspend
- braid exit 2 (error) → `!` → exit 0 → autosuspend: block suspend (fail-closed)

### SSH always on, SMB/NFS auto-detected

SSH check is unconditional — braid requires SSH for unlock, and an active SSH session means someone is working. SMB and NFS checks are auto-detected from `config.services.samba.enable` and `config.services.nfs.server.enable` to avoid false positives on systems that don't run those services.

### smartd and braid-monitor run opportunistically

Neither smartd nor braid-monitor should wake the system or prevent suspend. They run naturally during wake windows (user access, scrub wakeup). SMART counters accumulate in drive firmware regardless of polling. The only scheduled wakeup is for the monthly btrfs scrub timer.

### Paused balance = busy

A paused balance holds the btrfs exclusive operation lock. `check_no_exclusive_op` in preflight.rs already treats paused as "refuse." Same logic in `braid idle` — don't suspend mid-pause.

### WoL managed by braid

`braid.autoSuspend.wolInterface` is required when sleep is enabled. braid sets `networking.interfaces.<iface>.wakeOnLan.enable = true` on the specified interface. A build-time assertion prevents enabling sleep without WoL — otherwise the NAS suspends and becomes unreachable until someone physically presses the power button. The BIOS-side WoL setting is the user's responsibility (can't be automated from NixOS).

### Fully qualified store paths

The ExternalCommand command string uses absolute `/nix/store/` paths for `timeout`, `bash`, and `braid`. autosuspend runs the command outside braid's wrapper, so PATH is not guaranteed to include these tools.
