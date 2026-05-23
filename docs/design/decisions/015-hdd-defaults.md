---
intent: "Record the HDD defaults decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: HDD defaults


> Principle: [HDD defaults](../principles.md#11-hdd-defaults)

## Context

braid manages a NAS pool of LUKS-encrypted btrfs RAID1 drives. The typical deployment is bulk storage on large-capacity spinning drives (e.g., 12–16 TB HDDs). Several defaults already assume rotational media:

- `cryptsetup open` omits `--allow-discards`, so TRIM/discard requests from btrfs never reach the underlying device. btrfs also exposes a mount-layer discard knob (`discard=async`, the kernel default since 6.2 on devices that advertise discard support), but braid's LUKS layer gates it: without `--allow-discards`, the mapped device never reports discard support upward, so the kernel default never activates and any explicit `discard=async` would be silently dropped.
- `noatime` mount rationale references HDD spindown prevention.
- Monthly scrub interval is tuned for spinning disk wear and noise.

Making braid flash-aware would mean adding `--allow-discards` (with its security tradeoff of leaking block-usage patterns through the encryption layer), flash-specific scrub/balance scheduling, and flash-targeted test coverage. None of this is warranted for the target use case.

Note: braid already handles flash media in its monitoring paths — NVMe SMART parsing (`cli/src/parse/smartctl.rs`) and transport-type detection (`cli/src/tui/probe.rs`) work with any drive type. This decision is about operational defaults, not monitoring.

## Decision

Defaults are chosen for HDD NAS deployments. Flash media (SSDs, NVMe, USB sticks) may function but are not a validated or optimized target.

## Tradeoffs accepted

- **No TRIM passthrough** — braid pins discard off at the LUKS layer by omitting `--allow-discards` and, by consequence, at the btrfs mount layer because no effective `discard=async` can pass through regardless of kernel default. SSDs used with braid experience increased write amplification and performance degradation over time.
- **No flash-specific testing** — flash-related issues in LUKS or mount configuration may go unnoticed.

## See

- `cli/src/cmd.rs` — `CryptsetupLuksOpen` and `CryptsetupLuksOpenKeyFile` omit `--allow-discards`
- `cli/src/cmd.rs` — `base_mount_options()` omits any `discard` option, relying on the kernel default that is itself gated by the LUKS layer
- `modules/braid/storage.nix` — `noatime` rationale references HDD spindown
- [Sane defaults](005-sane-defaults.md) — scrub interval tuned for spinning disks
