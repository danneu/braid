---
intent: "Record the HDD defaults decision and rationale. Read before changing related behavior or docs."
status: Active
---
# Decision: HDD defaults


> Principle: [HDD defaults](../principles.md#11-hdd-defaults)

## Context

braid manages a NAS pool of LUKS-encrypted btrfs RAID1 drives. The typical deployment is bulk storage on large-capacity spinning drives (e.g., 12–16 TB HDDs). Several defaults already assume rotational media:

- `cryptsetup open` omits `--allow-discards`, so TRIM/discard requests from btrfs never reach the underlying device. btrfs also exposes a mount-layer discard knob (`discard=async`, the kernel default since 6.2 on devices that advertise discard support), but braid's LUKS layer gates it: without `--allow-discards`, the mapped device never reports discard support upward, so the kernel default never activates and any explicit `discard=async` would be silently dropped.
- `noatime` avoids relatime's read-triggered metadata write-amplification on
  every RAID1 copy.
- The 30-days-since-the-last-scrub freshness window is tuned for spinning
  disk wear and noise.

Making braid flash-aware would mean adding `--allow-discards` (with its security tradeoff of leaking block-usage patterns through the encryption layer), flash-specific scrub/balance scheduling, and flash-targeted test coverage. None of this is warranted for the target use case.

Note: braid already handles flash media in its monitoring paths — NVMe SMART parsing (`cli/src/parse/smartctl.rs`) and transport-type detection (`cli/src/tui/probe.rs`) work with any drive type. This decision is about operational defaults, not monitoring.

## Decision

Defaults are chosen for HDD NAS deployments. Flash media (SSDs, NVMe, USB sticks) may function but are not a validated or optimized target.

## Tradeoffs accepted

- **No TRIM passthrough** — braid pins discard off at the LUKS layer by omitting `--allow-discards` and, by consequence, at the btrfs mount layer because no effective `discard=async` can pass through regardless of kernel default. SSDs used with braid experience increased write amplification and performance degradation over time.
- **No flash-specific testing** — flash-related issues in LUKS or mount configuration may go unnoticed.

## Alternatives considered

### Default-on btrfs compression (compress=zstd:1)

Rejected. braid targets HDD bulk-storage NAS pools where the dominant content is media and archives: video, audio, photos, and other formats already compressed at the application layer. Transparent filesystem compression usually saves little or no space on that mix, while the btrfs heuristic that skips incompressible extents still spends CPU on each write. On low-power NAS hardware, that cost is not free.

Reversal is also partial. Removing `compress=...` affects future writes only; extents already written compressed stay compressed until the data is rewritten or explicitly defragmented. Making compression the default would bake that conversion cost into pre-v1.0 software for users who later discover their workload does not benefit. For that reason, `cli/src/cmd.rs` `base_mount_options()` intentionally omits compression.

Fedora's `compress=zstd:1` precedent is workstation root filesystems on SSDs: binaries, logs, configs, and package payloads. That precedent does not transfer cleanly to HDD bulk-storage NAS workloads. Users with compression-friendly data, such as text, code, or document servers, can opt specific paths into compression today with `btrfs property set <path> compression zstd`; `reference/btrfs-progs/Documentation/btrfs-property.rst` documents this modern per-inode interface. This is preferable to legacy `chattr +c`, which uses ext2-style flags and defaults to zlib. No braid feature gate is needed for this per-path opt-in.

### Periodic or automatic defrag

Rejected. braid ships no defrag of any kind: no `defrag` command, no periodic defrag timer or service, and no `autodefrag` mount option (`cli/src/cmd.rs#base_mount_options` sets only `noatime`, `skip_balance`, and `subvolid=5`). The target workload is an HDD media/archive NAS where large, mostly-sequential files fragment little, so a scheduled defrag buys little; the upstream `btrfsmaintenance` toolbox ships its defrag job off by default for the same reason.

More decisively, a blanket `btrfs filesystem defrag` unshares reflink and snapshot extents -- it rewrites shared extents into private copies. On a snapshot-capable pool, that unsharing can sharply increase real space usage, turning a routine maintenance pass into an ENOSPC incident. An automatic or periodic defrag would therefore be actively harmful, not merely unnecessary. An operator who hits a genuine fragmentation problem can still defrag a specific path by hand, accepting the one-time unsharing cost for just that path.

## See

- `cli/src/cmd.rs` — `CryptsetupLuksOpen` and `CryptsetupLuksOpenKeyFile` omit `--allow-discards`
- `cli/src/cmd.rs` — `base_mount_options()` omits any `discard` option, relying on the kernel default that is itself gated by the LUKS layer
- `cli/src/cmd.rs#base_mount_options` -- sets noatime to avoid relatime's
  read-triggered metadata write-amplification on RAID1.
- [Sane defaults](005-sane-defaults.md) — scrub cadence tuned for spinning disks
- [ADR 035: Scrub scheduling by freshness](035-scrub-scheduling-by-freshness.md) --
  the cadence is a freshness window, and a sub-7-day one warns at eval time on
  the wear grounds above
- [ADR 031: Drive-wake posture](031-drive-wake-posture.md) -- mounted drives are
  treated as awake; `noatime` is not spindown management.
