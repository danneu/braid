---
intent: Record braid's shared systemd unit hardening profile, per-unit exceptions, and the behavior tests that guard them.
status: Active
---

# Decision: Systemd unit hardening

> Principle: [Resilient by default](../principles.md#1-resilient-by-default)

## Context

Several braid-defined systemd units run as root because they inspect btrfs state,
write braid state, manage alerting, or perform lifecycle handoffs. Running those
units without an exec sandbox gives any parser bug or shell mistake the full
ambient authority of root.

The highest-frequency case is `braid-monitor.service`: it runs every five
minutes, parses external btrfs tool output, writes alert state, and may start an
alert unit. That unit also takes `/run/braid-pool.lock` in Rust dispatch before
monitor work begins. A `ProtectSystem=strict` sandbox must therefore grant both
`/var/lib/braid` and the exact lock file, or the monitor fails before doing any
health check.

## Decision

braid has a shared systemd hardening base in `modules/braid/hardening.nix`. The
base contains only directives that are safe for every consumer:

- `NoNewPrivileges=true`
- `ProtectSystem=strict`
- `ProtectHome=true`
- `PrivateTmp=true`
- `ProtectControlGroups=true`
- `ProtectKernelModules=true`
- `ProtectKernelLogs=true`
- `RestrictNamespaces=true`
- `LockPersonality=true`
- `MemoryDenyWriteExecute=true`
- `SystemCallArchitectures=native`
- `RestrictSUIDSGID=true`

Every unit applies per-unit deltas at its call site. `ProtectSystem=strict` means
each writer must name the narrow `ReadWritePaths` it needs, and capability
bounding stays explicit per unit.

| Unit | Profile |
|------|---------|
| `braid-monitor.service` | base, `ReadWritePaths=/var/lib/braid /run/braid-pool.lock`, `CapabilityBoundingSet=CAP_SYS_ADMIN`, `RestrictAddressFamilies=AF_UNIX`, ordered after `systemd-tmpfiles-setup.service`; no `PrivateDevices` |
| `braid-ups-secrets.service` | base, `ReadWritePaths=/var/lib/braid`, empty `CapabilityBoundingSet`, `PrivateNetwork=true`, `PrivateDevices=true` |
| `braid-seal-mountpoint.service` | base, `ReadWritePaths=<mountpoint parent>`, `CapabilityBoundingSet=CAP_LINUX_IMMUTABLE`, `PrivateNetwork=true`, `PrivateDevices=true` |
| `braid-alert.service` | intentionally unsandboxed oneshot+RAE orchestrator; runs the bounded operator `alertCommand`, wants `braid-pcspkr-load.service` and `braid-beep.service` when beeping is enabled |
| `braid-alert-advisory.service` | intentionally unsandboxed oneshot+RAE Warning path; runs only the bounded operator `alertCommand`, no beep |
| `braid-beep.service` | base plus `CapabilityBoundingSet=CAP_SETUID CAP_SETGID`, `PrivateNetwork=true`, `ProtectKernelTunables=true`, `ProtectClock=true`, `RestrictAddressFamilies=AF_UNIX`, `RestrictRealtime=true`, `Restart=always`, `StartLimitIntervalSec=0`; no `PrivateDevices` |
| `braid-scrub-failed.service` | base, `ReadWritePaths=/var/lib/braid`, empty `CapabilityBoundingSet`, `RestrictAddressFamilies=AF_UNIX` |
| `braid-pcspkr-load.service` | base with `ProtectKernelModules=false`, `CapabilityBoundingSet=CAP_SYS_MODULE`, `PrivateNetwork=true`; no `RemainAfterExit` so alert starts can re-run it |
| `hddfancontrol-braid.service` | base plus realtime scheduling and restart policy; no `PrivateDevices` |
| `braid-fan-reload.service` | base, empty `CapabilityBoundingSet`, `RestrictAddressFamilies=AF_UNIX` |

`systemd.tmpfiles` pre-creates `/run/braid-pool.lock` with `0600 root root`.
`ReadWritePaths=/run/braid-pool.lock` bind-mounts the same inode into the
monitor's private mount namespace, so `flock` still contends with host-namespace
pool mutators. `tests/module/monitor-lifecycle.py` proves that premise
behaviorally.

`braid-seal-mountpoint.service` deliberately makes `dirOf braid.mountPoint`
writable, not the mountpoint itself. `ReadWritePaths` creates private bind mounts;
if the exception is exactly the guarded path, `statx(STATX_ATTR_MOUNT_ROOT)` sees
that private bind mount and the seal correctly refuses to touch flags. Making the
parent writable keeps the inode ioctl available without invalidating the
mount-root safety check.

## Capability reasoning

`braid-monitor.service` keeps `CAP_SYS_ADMIN` because its alert probe runs
`cryptsetup status` on each live mapper, and the device-mapper status path fails
without that capability. Its btrfs reads do not drive the cap: linux 6.18.33,
`fs/btrfs/ioctl.c` (`btrfs_ioctl_get_dev_stats`) gates `CAP_SYS_ADMIN` only when
the reset flag is requested, and the monitor does not reset device stats. The
filesystem, device, and space-info paths are read-only query ioctls.

A note for anyone re-deriving these caps around `btrfs scrub status`, which is
deceptive: linux 6.18.33, `fs/btrfs/ioctl.c` (`btrfs_ioctl_scrub_progress`)
does require `CAP_SYS_ADMIN`, but btrfs-progs v6.19.1, `cmds/scrub.c`
(`is_scrub_running_in_kernel` and `cmd_scrub_status`) treats a failed progress
ioctl as "not running" and does not propagate that failure to the command exit.
A caller that reads only the persisted scrub status file therefore gains nothing
from `CAP_SYS_ADMIN`. `braid-scrub.service` is in the unsandboxed exception set
below for unrelated reasons (it mutates the pool), so this does not currently
gate any unit -- it is recorded because the ioctl's cap requirement invites the
wrong conclusion.

`braid-scrub-failed.service` also uses an empty set. It writes the durable
scrub-failed flag and hands off to PID1 over the local systemd socket; neither
operation needs a capability.

`braid-seal-mountpoint.service` needs `CAP_LINUX_IMMUTABLE` to set the immutable
flag on the offline mountpoint. `braid-pcspkr-load.service` needs
`CAP_SYS_MODULE` because it is the only runtime module-load path for enabling
audible alerts without a reboot.

`braid-beep.service` keeps only `CAP_SETUID` and `CAP_SETGID`. Its shared
`braid-beep-probe` wrapper uses
`setpriv --reuid=nobody --regid=beep --groups=beep`, which calls the uid/gid and
supplementary-group syscalls before execing `beep`. An empty bounding set would
make that drop fail, and the loop suppresses probe stderr by design so the
failure would otherwise be silent. The loop does not get `CAP_SYS_MODULE`;
runtime module loading stays in `braid-pcspkr-load.service`.

## Alert split

`alertCommand` is an operator escape hatch. It may send webhooks, read scripts
from `/home`, write root-owned files, use interpreters or JITs, or execute
non-native ABI binaries. A systemd sandbox on the command path would silently
break valid notifiers, so both command-running units are intentionally
unsandboxed.

That unsandboxed surface is short-lived. `braid-alert.service` and
`braid-alert-advisory.service` are oneshot+RAE latches, and both run
`alertCommand` through the shared bounded timeout wrapper. The persistent
surface is instead `braid-beep.service`, which contains no operator input and
uses the hardened profile above.

This split is why the module does not carry a "light alert profile" anymore.
The command path is honest about being an escape hatch; the days-long loop gets
the sandbox.

## Device visibility

`PrivateDevices` is never part of the shared base. It is a per-unit opt-in after
the unit's dependencies prove it safe.

`braid-monitor.service` omits it. The alert probe runs `cryptsetup status` for
each btrfs mapper and treats `device: (null)` as a missing backing device. With
`PrivateDevices=true`, cryptsetup cannot see the real device tree and reports a
healthy mapper as null-backed, which would raise a false `MissingDevice` alert.

`hddfancontrol-braid.service` is the counterexample. hddfancontrol 2.1.1,
`src/cl.rs` (`DriveSelector::to_drive_paths`) scans `/dev/disk/by-id` for
interface selectors such as `ata`, and `src/device/drive.rs` (`Drive::new`)
validates that each resolved path is a block device. `PrivateDevices=true`
would hide the real disk tree and break drive discovery, so the fan-control VM
test asserts it remains off.

`braid-beep.service` also omits it. `PrivateDevices=true` hides the real input
device tree, including the PC Speaker evdev node under `/dev/input`, so the
hardened beep loop must keep host device visibility. The authorization boundary
is the udev rule that grants the `beep` group access to the PC Speaker node,
combined with the wrapper's `nobody:beep` drop.

## Unsandboxed units

The units still not covered by this hardening base are the ones whose purpose is
to mutate the pool or system mount/device state, plus the operator-command alert
escape hatches: `braid-unlock.service`, `braid-auto-unlock.service`,
`braid-online.service`, `braid-scrub.service`, `braid-alert.service`,
`braid-alert-advisory.service`, and the process-less `braid-pool.target`. These
need device-mapper, mount propagation, broad btrfs operation authority,
lifecycle ownership, or intentionally unrestricted operator command execution
that does not fit this base profile.

`braid-fan-reload.service` and `braid-scrub-failed.service` are not in that
exception set. They are dispatch shells, but they do not mount, open
device-mapper, or need broad btrfs mutation authority. They use the base with an
empty capability set and AF_UNIX only for their `systemctl` handoff.

## Test strategy

The tests combine directive-presence assertions with behavioral guards. The
presence checks pin security invariants; the behavioral checks catch namespace
setup failures and subtle regressions that `systemctl show` cannot observe.

`systemd-analyze security` is not an assertion. It is absent from the test VMs
and its scoring changes across systemd releases.

## See

- `modules/braid/hardening.nix`
- `modules/braid/monitor.nix`
- `modules/braid/storage.nix`
- `modules/braid/ups.nix`
- `modules/braid/fan-control.nix`
- `tests/module/monitor-lifecycle.py`
- `tests/module/braid-alert.py`
- `tests/module/braid-alert-hardened.py`
- `tests/module/scrub-alert.py`
- `tests/module/fan-control.py`
- [ADR 018: Systemd lifecycle](018-systemd-lifecycle.md)
- [ADR 026: Pool lock rust-owned](026-pool-lock-rust-owned.md)
