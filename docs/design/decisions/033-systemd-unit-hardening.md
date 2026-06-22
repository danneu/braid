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
| `braid-alert.service` without a custom command | base, full capability bounding set so the beep wrapper can drop uid/gid with `setpriv` |
| `braid-alert.service` with a custom command | light alert profile only, leaving filesystem, home, tmp, namespace, W^X, ABI, network, and capability policy open for operator code |
| `braid-alert-advisory.service` | same profile choice as `braid-alert.service`; it runs the same operator command path for warning-only alerts |
| `braid-scrub-failed.service` | base, `ReadWritePaths=/var/lib/braid`, empty `CapabilityBoundingSet`, `RestrictAddressFamilies=AF_UNIX` |
| `braid-pcspkr-load.service` | base with `ProtectKernelModules=false`, `CapabilityBoundingSet=CAP_SYS_MODULE`, `PrivateNetwork=true`; no `RemainAfterExit` so alert starts can re-run it |
| `hddfancontrol-braid.service` | base plus realtime scheduling and restart policy; no `PrivateDevices` |
| `braid-fan-reload.service` | base, empty `CapabilityBoundingSet`, `RestrictAddressFamilies=AF_UNIX` |
| `braid-scrub-resume-trigger.service` | base, empty `CapabilityBoundingSet`, `RestrictAddressFamilies=AF_UNIX` |

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

`braid-scrub-resume-trigger.service` also uses an empty set. The deceptive
detail is `btrfs scrub status`: linux 6.18.33,
`fs/btrfs/ioctl.c` (`btrfs_ioctl_scrub_progress`) does require
`CAP_SYS_ADMIN`, but btrfs-progs v6.19.1, `cmds/scrub.c`
(`is_scrub_running_in_kernel` and `cmd_scrub_status`) treats a failed progress
ioctl as "not running" and does not propagate that failure to the command exit.
The resume decision comes from the persisted scrub status file and ungated
filesystem/device/space-info queries, so granting `CAP_SYS_ADMIN` would add
authority without improving the decision.

`braid-scrub-failed.service` also uses an empty set. It writes the durable
scrub-failed flag and hands off to PID1 over the local systemd socket; neither
operation needs a capability.

`braid-seal-mountpoint.service` needs `CAP_LINUX_IMMUTABLE` to set the immutable
flag on the offline mountpoint. `braid-pcspkr-load.service` needs
`CAP_SYS_MODULE` because it is the only runtime module-load path for enabling
audible alerts without a reboot. The long-lived alert loop does not get that
capability.

## Alert profiles

`braid-alert.service` has two profiles because `alertCommand` is an operator
escape hatch. Without a custom command, the unit runs braid-owned code and uses
the full base. With a custom command, braid keeps only ambient process-family
protections:

- `NoNewPrivileges=true`
- `ProtectKernelModules=true`
- `ProtectControlGroups=true`
- `ProtectKernelLogs=true`
- `LockPersonality=true`
- `RestrictSUIDSGID=true`

The light profile intentionally omits `ProtectSystem`, `ProtectHome`,
`PrivateTmp`, `RestrictNamespaces`, `MemoryDenyWriteExecute`,
`SystemCallArchitectures`, network restrictions, and `CapabilityBoundingSet`.
Those can break normal operator notifiers such as scripts that write root-owned
files, send webhooks, use interpreters/JITs, or execute non-native ABI binaries.

The strong beep branch also omits a restricted `CapabilityBoundingSet`. The
beep wrapper uses `setpriv --reuid=nobody --regid=beep --groups=beep`, and an
empty bounding set would remove the uid/gid-drop capabilities before `setpriv`
can run. The beep command suppresses stderr, so this is guarded by
`tests/module/braid-alert-hardened.py`.

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

## Unsandboxed units

The units still not covered by this hardening base are the ones whose purpose is
to mutate the pool or system mount/device state: `braid-unlock.service`,
`braid-auto-unlock.service`, `braid-online.service`, `braid-scrub.service`, and
the process-less `braid-pool.target`. These need device-mapper, mount
propagation, broad btrfs operation authority, or lifecycle ownership that does
not fit this base profile.

`braid-fan-reload.service`, `braid-scrub-resume-trigger.service`, and
`braid-scrub-failed.service` are not in that exception set. They are dispatch
shells, but they do not mount, open device-mapper, or need broad btrfs mutation
authority. They use the base with an empty capability set and AF_UNIX only for
their `systemctl` handoff.

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
