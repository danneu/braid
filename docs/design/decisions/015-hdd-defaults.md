---
intent: "Record the HDD defaults decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: HDD defaults


> Principle: [HDD defaults](../principles.md#11-hdd-defaults)

## Context

braid manages a NAS pool of LUKS-encrypted btrfs RAID1 drives. The typical deployment is bulk storage on large-capacity spinning drives (e.g., 12–16 TB HDDs). Several defaults already assume rotational media:

- `cryptsetup open` omits `--allow-discards`, so TRIM/discard requests from btrfs never reach the underlying device. This is correct for HDDs (TRIM is a no-op) but harmful for SSDs (write amplification, degraded performance, shorter lifespan).
- `noatime` mount rationale references HDD spindown prevention.
- Monthly scrub interval is tuned for spinning disk wear and noise.

Making braid flash-aware would mean adding `--allow-discards` (with its security tradeoff of leaking block-usage patterns through the encryption layer), flash-specific scrub/balance scheduling, and flash-targeted test coverage. None of this is warranted for the target use case.

Note: braid already handles flash media in its monitoring paths — NVMe SMART parsing (`cli/src/parse/smartctl.rs`) and transport-type detection (`cli/src/tui/probe.rs`) work with any drive type. This decision is about operational defaults, not monitoring.

## Decision

Defaults are chosen for HDD NAS deployments. Flash media (SSDs, NVMe, USB sticks) may function but are not a validated or optimized target.

## Tradeoffs accepted

- **No TRIM passthrough** — SSDs used with braid experience increased write amplification and performance degradation over time.
- **No flash-specific testing** — flash-related issues in LUKS or mount configuration may go unnoticed.

## See

- `cli/src/cmd.rs` — `CryptsetupLuksOpen` and `CryptsetupLuksOpenKeyFile` omit `--allow-discards`
- `modules/braid/storage.nix` — `noatime` rationale references HDD spindown
- [Sane defaults](005-sane-defaults.md) — scrub interval tuned for spinning disks
