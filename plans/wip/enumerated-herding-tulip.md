# Qualify braid-online synchronization guarantee

## Context

The decision doc and storage.nix both assert `"braid-online active" ⟺ "pool mounted"` as an absolute invariant. In reality, neither enforcement layer (wrapper mount check, ConditionPathIsMountPoint) detects post-activation out-of-band state changes — an external mount or unmount after wrapper activation will desynchronize systemd state. `braid lock` handles this gracefully (exits 0 on already-unmounted pool), so this is a documentation precision issue, not a code bug.

## Changes

### 1. Rewrite invariant #2 in the decision doc

**File:** `docs/decisions/systemd-lifecycle.md:131`

Replace the current equality claim:

> **"braid-online active" = "pool mounted."** Enforced at two layers: the wrapper only activates it after `mountpoint -q` succeeds, and `ConditionPathIsMountPoint` on the unit itself causes systemd to skip activation (unit stays inactive) on direct `systemctl start` when unmounted.

With a scoped synchronization guarantee:

> **Wrapper-synchronized lifecycle.** For wrapper-managed operations, the wrapper keeps `braid-online` synchronized with pool mount state: it activates the service only after `mountpoint -q` succeeds, and deactivates it after a successful lock. `ConditionPathIsMountPoint` on the unit is defense-in-depth against direct `systemctl start` when unmounted. Out-of-band mount or unmount bypasses the wrapper and can leave `braid-online` stale; `braid lock` handles already-unmounted pools gracefully.

### 2. Update the comment in storage.nix

**File:** `modules/braid/storage.nix:38-42`

Replace:

```nix
      # Guard against direct `systemctl start braid-online.service` bypassing
      # the wrapper. When the condition is not met, systemd skips activation
      # (unit stays inactive, systemctl returns 0). The wrapper's own
      # mountpoint -q check (braid-wrapper.sh) is the primary gate; this is
      # defense-in-depth for the invariant: braid-online active ⟺ pool mounted.
```

With:

```nix
      # Guard against direct `systemctl start braid-online.service` bypassing
      # the wrapper. When the condition is not met, systemd skips activation
      # (unit stays inactive, systemctl returns 0). The wrapper's own
      # mountpoint -q check (braid-wrapper.sh) is the primary gate; this is
      # defense-in-depth. Out-of-band mount/unmount can leave this stale.
```

## Verification

- Read both modified locations and confirm the wording is consistent
- Grep the repo for the old `⟺` wording to ensure no other stale references remain
