---
intent: "Record the Resilient by Default decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: Resilient by Default


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

LUKS unlock is strictly stage-2 — `braid-unlock` or `braid-auto-unlock` opens LUKS and mounts the pool. The module does not generate `boot.initrd.luks.devices`, data-pool `fileSystems` entries, or LUKS device declarations. The pool is brought online entirely by the CLI at runtime.

Resilience mechanisms:

- **No boot-blocking mount units**: The module generates no data-pool `fileSystems` or LUKS entries. The CLI (`braid unlock`) handles LUKS open + btrfs mount directly. Nothing referencing data drives can block boot. (The one build-time `fileSystems` entry is the optional `autoUnlock` USB-key mount at `/run/braid-key/mnt`, marked `noauto`/`nofail` so it never blocks boot and references the key device, not the pool.)
- **Degraded mount**: Requires explicit `--allow-degraded` (or `autoUnlock.allowDegraded` for unattended use) — braid refuses to silently mount with zero redundancy.

### Three-tier failure model

| Scenario | What happens | User sees |
|----------|-------------|-----------|
| All drives healthy | Normal boot | Everything works |
| One drive dead | `braid unlock` refuses by default; user must pass `--allow-degraded` or configure `autoUnlock.allowDegraded` | Pool stays locked until explicit opt-in |
| All drives dead / no pool.json | `braid unlock` fails (no devices to probe) | System boots, SSH works, no /mnt/storage |

### Identity enforcement

`braid unlock` uses authoritative pool membership from `pool.json` and probes only those configured members. `--allow-degraded` only bypasses degraded-mount refusal; it does not change which disks are considered pool members.

## Key discoveries

### udev SYSTEMD_READY=0 risk

When btrfs has a missing member, udev may mark remaining devices as not ready (systemd/systemd#36886), blocking mount. Not yet hit in testing. Fallback: custom udev rule or moving mount into the scan service script.

### Timeout values

10s in VM tests (no spin-up delay). 30s in production (real drives may be slow to enumerate on a cold DAS).

## Constraint

This is not configurable. There is no `braid.resilient` option. Every braid deployment gets resilient boot.

## See

- `modules/braid/storage.nix` — `braid-online.service`, `braid-pool.target`
- `tests/module/` — module tests validate boot with all drives healthy
- `archive/plans/test-boot-degraded.md` — original plan and research (preserved in git history; last present at commit `9df91f9`)
