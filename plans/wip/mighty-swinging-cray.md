# Fix: Guard braid-online.service against direct start while unmounted

## Context

The invariant "braid-online active ⟺ pool is mounted" is documented in `docs/decisions/systemd-lifecycle.md` but only enforced by the wrapper's `mountpoint -q` check. A direct `systemctl start braid-online.service` bypasses the wrapper and activates the unit even when the pool is unmounted. This breaks the invariant and could confuse shutdown behavior.

## Changes

### 1. Add ConditionPathIsMountPoint to braid-online.service

**File:** `modules/braid/storage.nix:39`

Add `unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;` with a comment explaining why.

```nix
systemd.services.braid-online = {
  description = "Braid storage pool online";
  # Guard against direct `systemctl start braid-online.service` bypassing
  # the wrapper. When the condition is not met, systemd skips activation
  # (unit stays inactive, systemctl returns 0). The wrapper's own
  # mountpoint -q check (braid-wrapper.sh) is the primary gate; this is
  # defense-in-depth for the invariant: braid-online active ⟺ pool mounted.
  unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
  serviceConfig = {
    Type = "oneshot";
    RemainAfterExit = true;
    ExecStart = "${pkgs.coreutils}/bin/true";
    ExecStop = "${braidWrapped}/bin/braid lock";
  };
};
```

### 2. Add VM test: direct start skipped while unmounted

**File:** `tests/module/systemd-lifecycle.py` (insert after line 42, before subtest 2)

When a systemd condition fails, `systemctl start` returns 0 (the unit is silently skipped), so we verify the unit stayed inactive afterward.

```python
with subtest("Direct start of braid-online.service skipped when pool unmounted"):
    # ConditionPathIsMountPoint causes systemd to skip activation (exit 0)
    # when the mount point isn't mounted. Verify the unit stays inactive.
    machine.fail("mountpoint -q /mnt/storage")
    machine.succeed("systemctl start braid-online.service")
    machine.fail("systemctl is-active braid-online.service")
```

### 3. Fix subtest 7 sanity check to work with condition guard

**File:** `tests/module/systemd-lifecycle.py:141-142`

The existing sanity check (`machine.fail("systemctl start braid-online.service")`) expects non-zero exit from the ExecStart=/bin/false override. With the new condition guard, the pool is unmounted at this point so the unit is skipped (exit 0) before ExecStart ever runs, breaking the assertion.

Fix: mount a tmpfs at /mnt/storage to satisfy `ConditionPathIsMountPoint` without touching LUKS state, verify ExecStart=/bin/false causes failure, then unmount. This preserves the original test intent — proving the override is effective — while being compatible with the condition guard.

Replace lines 141-142:
```python
    # Verify the override makes the service fail.
    machine.fail("systemctl start braid-online.service")
```

With:
```python
    # Verify the override makes the service fail. Temporarily satisfy
    # ConditionPathIsMountPoint with a tmpfs so ExecStart=/bin/false is
    # actually reached — proving the override works independently of the
    # condition guard.
    machine.succeed("mount -t tmpfs tmpfs /mnt/storage")
    machine.fail("systemctl start braid-online.service")
    machine.succeed("umount /mnt/storage")
```

### 4. Update lifecycle decision doc

**File:** `docs/decisions/systemd-lifecycle.md`

Two updates:

**a) braid-online.service section (line 76):** Insert `ConditionPathIsMountPoint` bullet before the "Not in any dependency chain" bullet.

Add after line 75 (`RemainAfterExit`):
```markdown
- `ConditionPathIsMountPoint = ${mountPoint}` — systemd skips activation when the pool is not mounted (`systemctl start` returns 0 but the unit stays inactive). Defense-in-depth: the wrapper's `mountpoint -q` check is the primary gate, but this condition prevents direct `systemctl start` from leaving the unit active while unmounted.
```

**b) Key design constraint #2 (line 126):** Update to reflect dual enforcement.

Replace:
```
The wrapper only activates it after `mountpoint -q` succeeds. No other path activates it.
```
With:
```
Enforced at two layers: the wrapper only activates it after `mountpoint -q` succeeds, and `ConditionPathIsMountPoint` on the unit itself causes systemd to skip activation (unit stays inactive) on direct `systemctl start` when unmounted.
```

## Files modified

- `modules/braid/storage.nix` — add `unitConfig.ConditionPathIsMountPoint`
- `tests/module/systemd-lifecycle.py` — add new subtest + fix subtest 7 sanity check
- `docs/decisions/systemd-lifecycle.md` — document the condition guard

## Verification

```
just test systemd-lifecycle
```
