# Plan: Remove RemainAfterExit from braid-unlock.service

## Context

`braid-unlock.service` has `RemainAfterExit = true`, which keeps it "active (exited)" after a successful unlock. This blocks re-use of `systemctl start braid-pool.target` as a re-unlock path after `braid lock`, because systemd sees the service as already active and skips it. The existing test (`systemd-lifecycle.py:76-81`) already works around this with a manual `systemctl stop braid-unlock.service`. The `ConditionPathIsMountPoint = !mountPoint` guard already prevents double-unlock, so RAE adds no value — `braid-online` is the sole status signal.

Precedent: `braid-auto-unlock.service` intentionally omits RAE for exactly this reason (`systemd-lifecycle.md:65`).

## Changes

### 1. Remove RAE from braid-unlock — `modules/braid/storage.nix:63`

Delete `RemainAfterExit = true;` from the `braid-unlock` serviceConfig. The `Type = "oneshot"` stays.

### 2. Update decision doc — `docs/decisions/systemd-lifecycle.md`

- **Line 21** (ASCII diagram): Change `oneshot, RAE` to `oneshot` for braid-unlock.
- **Line 54**: Rewrite "runs once, stays 'active (exited)' to prevent re-run" — the condition guard prevents re-run, and the service now returns to inactive after completion. Note this enables `systemctl start braid-pool.target` as a re-unlock path after lock.

### 3. Update AGENTS.md systemd table

Change `Interactive passphrase unlock (oneshot, RAE)` to `Interactive passphrase unlock (oneshot)`.

### 4. Update test — `tests/module/systemd-lifecycle.py`

- **Remove** lines 76-81: the `systemctl stop braid-unlock.service` workaround and its comment. No longer needed since braid-unlock returns to inactive on its own.
- **Add new subtest** between current subtests 3 and 4 (after "Stopping braid-online.service locks pool"): re-unlock via `systemctl start braid-pool.target` after lock. This is the regression test for the exact round-trip that was previously broken.

New subtest:

```python
with subtest("braid-pool.target re-unlock after lock"):
    # After stopping braid-online (which ran braid lock), the pool is offline.
    # braid-unlock.service should be inactive (no RemainAfterExit), so
    # re-starting braid-pool.target must trigger a fresh unlock cycle.
    machine.fail("mountpoint -q /mnt/storage")
    machine.fail("systemctl is-active braid-online.service")

    machine.succeed("systemctl start braid-pool.target")

    machine.succeed("mountpoint -q /mnt/storage")
    machine.succeed("systemctl is-active braid-online.service")

    # Tear down via lifecycle owner (systemd path, not CLI path).
    machine.succeed("systemctl stop braid-online.service")
    machine.fail("systemctl is-active braid-online.service")
    machine.fail("mountpoint -q /mnt/storage")
```

### 5. Update README — `README.md:273-281`

Add a note that `systemctl start braid-pool.target` also works as a re-unlock after `braid lock` or a lifecycle-owner stop. Current text only frames it as a post-boot action.

After the existing sentence "One passphrase prompt opens all available LUKS devices and mounts the pool." add: "The same command re-unlocks the pool after `braid lock`."

## Files modified

- `modules/braid/storage.nix` — remove one line
- `docs/decisions/systemd-lifecycle.md` — update diagram + description
- `AGENTS.md` — update table
- `tests/module/systemd-lifecycle.py` — remove workaround, add re-unlock subtest
- `README.md` — note re-unlock via same command

## Verification

`just test systemd-lifecycle` — the new subtest exercises the exact re-unlock path that was previously broken by RAE.
