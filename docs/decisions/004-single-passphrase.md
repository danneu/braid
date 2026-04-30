# Decision: Single Passphrase

Status: Active

> Principle: [Single passphrase](../principles.md#4-single-passphrase)

## Context

`braid unlock` and `braid add` prompt for a passphrase that unlocks all LUKS devices. If each drive had a different passphrase, the user would need to type N passphrases on every unlock. The UX must be: one passphrase, all drives unlock.

## Options considered

1. **Shared keyfile on boot disk** — store a keyfile on the SSD, encrypt the SSD with a passphrase. Unlocking the SSD exposes the keyfile, which unlocks data drives. More complex boot chain, keyfile is at-rest on disk.
2. **Same passphrase, no enforcement** — tell users to use the same passphrase. They'll forget or mistype. Boot breaks silently.
3. **Same passphrase, enforced at format time** — `braid add` verifies the passphrase matches relevant existing pool members before formatting. Catches mismatches immediately.

## Decision

Option 3. Enforcement at format time.

### How it works

- **First disk**: prompt for passphrase twice (confirm match). Standard new-passphrase flow.
- **Subsequent disks**: prompt once, then verify every reachable existing LUKS device that will remain in or enter post-operation pool membership via `cryptsetup luksOpen --test-passphrase`. Fresh-format disks are excluded because they have no existing slot 0. The live-replace source is excluded when other retained members exist, so a divergent slot 0 on the disk being replaced does not block its own replacement. If verification fails, refuse to proceed with a clear error.

### Finding a verification target

The CLI reads which devices are in the btrfs pool and verifies the supplied passphrase against each relevant underlying LUKS device before opening or mutating disks. The same result-membership rule applies to keyfile credentials used by `mount`, `unlock`, and `recover`: every planned LUKS target is verified before any mapper is opened with that credential.

For `add`, this widened preflight changes one mixed-failure precedence case. If a non-first pool member has a divergent slot 0 and a closed `PresentLuks { mapper_open: false }` candidate would later surface a foreign-FSID or no-btrfs identity error during `execute` Pass 1, the pool-member credential error now wins. The old identity-first ordering in that shape came from the former first-disk-only verify and was not a documented invariant. A divergent slot 0 is a pool-wide integrity issue that affects future operations; surfacing it before the candidate's one-off selection error is intentional.

Identity errors found during planning are unchanged. `compile_add_steps_multi` still validates every `PresentLuks` candidate's braid label and classifies already-open candidates before `AddPlan::execute` runs, before any passphrase read, and before the widened verify.

## Scope

This decision governs the shared passphrase: one passphrase, enrolled in
LUKS key slot 0 on every pool disk, enforced at format time. Additional
unlock mechanisms (USB keyfiles, TPM, etc.) are orthogonal — they use
separate LUKS key slots and do not weaken or replace the passphrase
requirement.

## See

- `cli/src/` — passphrase prompt and verification logic in the Rust CLI
- `design-docs/1-braid-add-disk.md` — original script design (historical)
