# Decision: Sane Defaults

Status: Active

## Context

Braid should protect the user's data without requiring them to read through NixOS options to find features worth enabling. If a setting is something every NAS should have, braid should turn it on automatically.

The guiding question: **would a knowledgeable admin always enable this?** If yes, braid enables it by default.

## Decision

Braid sets opinionated defaults for the underlying NixOS options using `lib.mkDefault`. Users override them with normal NixOS config — no braid-specific wrapper options needed.

### When to use mkDefault (don't wrap)

Use `lib.mkDefault` to set an underlying NixOS option directly when:

- The NixOS option is **stable and well-known** — wrapping it adds no clarity.
- The meaning **doesn't change** if braid's internals change — braid is always btrfs, so `services.btrfs.autoScrub` isn't leaking an implementation detail.
- The mapping is **1:1** — a braid option would just be a passthrough.

The user overrides by setting the NixOS option in their own config. `mkDefault` gives way automatically.

### When to wrap in a braid option

Create a `braid.*` option when:

- **One braid option maps to many underlying options** — e.g., `braid.shares.media` sets Samba config, permissions, and directory creation.
- **The underlying tech could change** — the abstraction survives an implementation swap.
- **The raw option requires braid-specific context** — e.g., `braid.disks` encodes LUKS + mapper naming + crypttab options + btrfs scan ordering. Exposing the raw options would require the user to understand braid's internals.
- **The mapping is non-obvious or must stay in sync** — e.g., if braid supported multiple pools, scrub `fileSystems` would need to track all mount points automatically.

## Defaults applied

| Setting | Value | Rationale |
|---------|-------|-----------|
| `services.btrfs.autoScrub.enable` | `true` | Scrub detects bit rot before it compounds. Every NAS should do this. |
| `services.btrfs.autoScrub.interval` | `"monthly"` | Btrfs community consensus. Weekly is aggressive for spinning disks; quarterly risks undetected corruption on a small RAID1. TrueNAS defaults to weekly (ZFS); Synology doesn't enable it by default. Monthly is the sweet spot. |
| `services.btrfs.autoScrub.fileSystems` | `[ cfg.mountPoint ]` | Targets braid's pool. Not mkDefault — this must always include the pool. |
| `braid.storageGroup` | `"storage"` | Mount root set to `root:storage 2770`. Users in the group can read/write the mount root. Setgid ensures new entries inherit the group. Same pattern as TrueNAS/OMV. Does not override per-file umask. |

## Alternatives considered

### Wrap scrub in braid.scrub.interval

Rejected. Braid is always btrfs — `services.btrfs.autoScrub` is not an implementation detail. A wrapper adds indirection with no insulation benefit.

### Don't enable scrub by default

Rejected. This is what Synology and Unraid do — scrub is opt-in. Users who don't know about scrub never enable it. Braid's philosophy is that data integrity features should be on by default.

### Weekly scrub (TrueNAS default)

Rejected. TrueNAS runs ZFS on always-on servers. Braid targets home NAS with spinning disks where weekly scrubs add unnecessary wear and noise. Monthly catches bit rot well before it can compound across a 2-3 drive RAID1.

## See

- `modules/braid/storage.nix` — where defaults are applied
- [Resilient by default](resilient-boot.md) — related philosophy: protect by default, no toggles
