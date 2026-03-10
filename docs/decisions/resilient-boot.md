# Decision: Resilient by Default

Status: Active

> Principle: [Resilient by default](../principles.md#1-resilient-by-default)

## Context

The OS lives on an internal SSD. Data drives are separate. Nothing about the data drives — bad config, dead drive, unplugged cable — should prevent the system from booting. The data pool is an external resource, like a network mount. The module tries to bring it up, but if it fails, the box is still a working Linux machine you can SSH into and fix.

## Options considered

1. **Hard dependencies** — LUKS required, mount required. Any failure blocks boot. Simple but means a dead drive = unreachable NAS.
2. **Degraded toggle** — add an option like `braid.allowDegraded = true`. Default to hard failure, opt in to resilience. Adds complexity and a wrong default.
3. **Resilient by default** — `nofail`, `wants` everywhere. Zero cost when healthy, graceful in every failure case. No toggle. Degraded mounts require explicit opt-in (`--allow-degraded` or `autoUnlock.allowDegraded`) to prevent silent zero-redundancy operation.

## Decision

Option 3. Resilience is the default, not an option.

## Implementation

LUKS unlock is strictly stage-2 — `braid-unlock` or `braid-auto-unlock` opens LUKS and mounts the pool. The module does not generate `boot.initrd.luks.devices`.

Every layer has a specific resilience mechanism:

- **Mount**: `nofail` in mount options. A total failure doesn't block boot. Degraded mounts require explicit `--allow-degraded` (or `autoUnlock.allowDegraded` for unattended use) — braid refuses to silently mount with zero redundancy.
- **btrfs-device-scan**: Stage-2 service referenced by the mount unit's `x-systemd.requires`. Scans for btrfs multi-device filesystems after LUKS mappers are opened.

### Three-tier failure model

| Scenario | What happens | User sees |
|----------|-------------|-----------|
| All drives healthy | Normal boot | Everything works |
| One drive dead | `braid unlock` refuses by default; user must pass `--allow-degraded` or configure `autoUnlock.allowDegraded` | Pool stays locked until explicit opt-in |
| All drives dead / wrong config | 10s timeout, mount fails | System boots, SSH works, no /mnt/storage |

## Key discoveries

### udev SYSTEMD_READY=0 risk

When btrfs has a missing member, udev may mark remaining devices as not ready (systemd/systemd#36886), blocking mount. Not yet hit in testing. Fallback: custom udev rule or moving mount into the scan service script.

### Timeout values

10s in VM tests (no spin-up delay). 30s in production (real drives may be slow to enumerate on a cold DAS).

## Constraint

This is not configurable. There is no `braid.resilient` option. Every braid deployment gets resilient boot.

## See

- `modules/braid/storage.nix` — LUKS, mount, and device-scan config
- `tests/module/` — module tests validate boot with all drives healthy
- [archive/plans/test-boot-degraded.md](../../archive/plans/test-boot-degraded.md) — original plan and research
