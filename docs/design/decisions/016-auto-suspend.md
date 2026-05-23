---
intent: "Record the Auto-Suspend via autosuspend + `braid idle` decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Auto-Suspend via autosuspend + `braid idle`


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
- Fail-closed behavior (probe failures map to `Busy(Unknown)` -> exit 1 -> block suspend; setup/config errors stay at exit 2 and also block via `!`) is easier to get right in Rust than in shell
- Testable with unit tests via MockRunner + a `Filesystem` mock

### Exit code inversion

`braid idle` follows natural Unix convention (exit 0 = success = "yes, idle"). autosuspend's ExternalCommand convention is inverted (exit 0 = activity detected). The NixOS module bridges this with `bash -c '! braid idle'`:

- braid exit 0 (idle) -> `!` -> exit 1 -> autosuspend: allow suspend
- braid exit 1 (busy or probe failure) -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)
- braid exit 2 (setup error) -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)
- braid idle signal-killable overrun >10s -> `timeout -k 2 10` (inside bash) returns non-zero -> `!` -> exit 0 -> autosuspend: block suspend (fail-closed)

`timeout` must be **inside** `bash -c` so its non-zero overrun result is inverted by `!`. An outer `timeout` (`timeout -k 2 10 bash -c '! braid idle'`) would fail open: bash gets killed before `!` runs, autosuspend sees the non-zero timeout result and treats it as no activity. Coreutils' `timeout` sends TERM at the main deadline and `-k 2` escalates to KILL two seconds later for processes that ignore or delay TERM (see `reference/coreutils/src/timeout.c`).

Scope of the timeout invariant: this covers signal-killable command overruns (parser regression, slow userspace probe, network-FS latency). Uninterruptible kernel waits (process in `D` state on a wedged ioctl) are not bounded by `timeout(1)` and remain a separate failure mode; under that condition the autosuspend tick itself stalls until the syscall returns, so the system stays awake by virtue of not deciding.

### Mount probe reads `/proc/self/mountinfo` directly

`braid idle`'s initial mount-presence check (`is_btrfs_mounted`) reads `/proc/self/mountinfo` via the existing `Filesystem` abstraction rather than shelling out to `findmnt`. Rationale: the mount probe is a fail-closed safety gate; any subprocess fallback path that maps "non-zero exit + empty stderr" to "no mount" reintroduces the fail-open seam this gate exists to prevent. The kernel-maintained mountinfo file gives a direct answer in one syscall, with no fork/exec.

Octal-escaped mount-point fields (`\040`, `\011`, `\012`, `\134`) are decoded before comparison so configured mount paths containing whitespace match correctly.

IO errors (file unreadable, EIO), malformed mountinfo lines, and ambiguous duplicate target entries surface as `Busy(BusyReason::Unknown)`, exit 1, and block suspend. "Don't know" never becomes "allow suspend".

### Exclusive-op probe scans `/sys/fs/btrfs/*` directly

After the mount check passes, `cmd_idle` reads `exclusive_operation` from every entry under `/sys/fs/btrfs/` via `preflight::check_any_btrfs_exclusive_op` and returns busy as soon as any one is non-`none`. No `findmnt` or `btrfs filesystem show` subprocesses are invoked on this path; only the scrub probe (`btrfs scrub status`) remains, because scrub is not part of the kernel's `exclop_def[]` set (`reference/btrfs-progs/common/utils.c:1186-1194`).

Semantics: any in-flight exclusive op on any btrfs filesystem on the host counts as busy. On a typical braid host (one btrfs filesystem, the pool) this is identical to a fsid-scoped check. On a host with btrfs root alongside the pool the reported `BusyReason` may name an op on the non-pool fs, but the suspend decision is still correct -- autosuspend's job is to err conservative, and "do not suspend while any btrfs is mid-balance/replace/etc." is the right answer regardless of which fs is busy.

Pseudo-dir skip is by name allowlist (`features`, `debug`), not by "absorb any NotFound on read." The kernel only creates `exclusive_operation` under per-fsid `<uuid>/` dirs (`reference/linux/fs/btrfs/sysfs.c:29-47`), but treating a missing attribute on any other listed entry as "must have been a pseudo-dir" would silently swallow a real failure mode: a fsid dir whose attribute disappears mid-scan during a concurrent unmount race. Under the allowlist, that race surfaces as `ExclusiveOpError::Read` and blocks suspend.

Fail-closed branches: `list_dir("/sys/fs/btrfs")` IO errors, any read error on a non-allowlisted entry's `exclusive_operation` (including `NotFound`), unrecognized parser values, and an empty `/sys/fs/btrfs/` after the mount check passed all surface as `Busy(BusyReason::Unknown)` and exit 1.

The scrub probe is held to the same contract: a `parse_btrfs_scrub_status` result of `ScrubState::Unknown` (empty stdout or an unrecognized `Status:` word) surfaces as `Busy(BusyReason::Unknown)` and exits 1. Parser drift must not silently allow suspend.

`probe::probe_fsid` is no longer reached from `cmd_idle`. It remains in use by non-idle callers (`lock.rs` and the preflight pipelines that need a UUID for other purposes), and is out of scope for this gate.

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
