# Decision: LUKS Mapper Naming

Status: Active

> Principle: [Stable identifiers](../principles.md#5-stable-identifiers)

## Context

Each LUKS device needs a mapper name for `/dev/mapper/<name>`. The module creates LUKS entries at eval time; the script opens LUKS devices at runtime. They must agree on the name without explicit coordination.

## Options considered

1. **`btrnas-` prefix** — e.g., `btrnas-ata-Toshiba_MN07_XXXX`. Clear provenance but longer, and the prefix adds nothing since btrfs finds devices by UUID internally.
2. **by-id basename** — e.g., `ata-Toshiba_MN07_XXXX`. Both module and script derive it from the same `/dev/disk/by-id/` path via `builtins.baseNameOf` (Nix) or `basename` (bash). No coordination needed.

## Decision

Option 2. Mapper name = `baseNameOf` of the by-id path. The original plan had a `btrnas-` prefix; it was dropped for simplicity.

Both the module (`builtins.baseNameOf`) and the script (`basename "$disk"`) derive the mapper name from the same source — the `/dev/disk/by-id/` path declared in `btrnas.disks`. No mapping table or shared constant needed.

## systemd unit escaping

`systemd-cryptsetup-generator` escapes hyphens in mapper names when creating unit instance names. For example, `virtio-disk1` becomes `systemd-cryptsetup@virtio\x2ddisk1.service`.

When referencing these units in `After=`, `Wants=`, `Before=`, etc., the escaped form is required:

```nix
cryptsetupUnit = name:
  "systemd-cryptsetup@${builtins.replaceStrings ["-"] ["\\x2d"] name}.service";
```

Production mapper names (e.g., `ata-Toshiba_MN07_XXXX`) and VM names (e.g., `virtio-disk1`) both contain hyphens. Some tests avoid this by using simple names (`disk1`, `disk2`).

## See

- `modules/btrnas/storage.nix` — `cryptsetupUnit` helper and LUKS device generation
- `scripts/btrnas-add-disk.sh` — `basename` derivation of mapper name
- [archive/design-docs/2-btrnas-module.md](../../archive/design-docs/2-btrnas-module.md) — original module plan
