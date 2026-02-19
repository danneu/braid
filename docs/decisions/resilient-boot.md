# Decision: Resilient by Default

Status: Active

> Principle: [Resilient by default](../principles.md#1-resilient-by-default)

## Context

The OS lives on an internal SSD. Data drives are separate. Nothing about the data drives — bad config, dead drive, unplugged cable — should prevent the system from booting. The data pool is an external resource, like a network mount. The module tries to bring it up, but if it fails, the box is still a working Linux machine you can SSH into and fix.

## Options considered

1. **Hard dependencies** — LUKS required, mount required. Any failure blocks boot. Simple but means a dead drive = unreachable NAS.
2. **Degraded toggle** — add an option like `braid.allowDegraded = true`. Default to hard failure, opt in to resilience. Adds complexity and a wrong default.
3. **Resilient by default** — `nofail`, `wants`, `degraded` everywhere. Zero cost when healthy, graceful in every failure case. No toggle.

## Decision

Option 3. Resilience is the default, not an option.

## Implementation

Every layer has a specific resilience mechanism:

- **LUKS devices**: `crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s" ]`. A missing drive times out in 10s (tests) or 30s (production) and boot continues.
- **btrfs-device-scan**: `wants` (not `requires`) on cryptsetup units. A failed unlock doesn't cascade to the scan service.
- **Mount**: `nofail` + `degraded` in mount options. A partial pool still mounts. A total failure doesn't block boot.
- **Two-service pattern**: `btrfs-device-scan` must exist in both initrd and stage-2 because `x-systemd.requires` in mount options persists across switch-root.

### Three-tier failure model

| Scenario | What happens | User sees |
|----------|-------------|-----------|
| All drives healthy | Normal boot | Everything works |
| One drive dead | 10s timeout, btrfs mounts degraded | Pool accessible in degraded mode |
| All drives dead / wrong config | 10s timeout, mount fails | System boots, SSH works, no /mnt/storage |

## Key discoveries

### crypttabExtraOpts

Hidden NixOS option (`listOf singleLineStr`), systemd initrd only. Appends to the crypttab options column. `nofail` makes the cryptsetup unit use `Wants=` instead of `Requires=` toward `cryptsetup.target`.

### neededForBoot tension

`neededForBoot = true` is required for the btrfs mount to survive into the real root (LUKS mapper devices are only available in initrd). But `neededForBoot` normally makes a failed mount fatal. The combination of `neededForBoot + nofail` resolves this: the mount is attempted in initrd (where mapper devices exist) but failure doesn't block boot.

### udev SYSTEMD_READY=0 risk

When btrfs has a missing member, udev may mark remaining devices as not ready (systemd/systemd#36886), blocking mount. Not yet hit in testing. Fallback: custom udev rule or moving mount into the scan service script.

### Timeout values

10s in VM tests (no spin-up delay). 30s in production (real drives may be slow to enumerate on a cold DAS).

## Constraint

This is not configurable. There is no `braid.resilient` option. Every braid deployment gets resilient boot.

## See

- `modules/braid/storage.nix` — LUKS, mount, and device-scan config
- `tests/braid-module/` — module tests validate boot with all drives healthy
- `tests/4-degraded-boot.nix` — validates degraded boot with a bricked drive
- [archive/plans/test-boot-degraded.md](../../archive/plans/test-boot-degraded.md) — original plan and research
